// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust `.gitattributes` parser, evaluator, EOL normalizer, and clean/smudge
//! filter driver runner (`filter=lfs` and custom filter drivers).

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::RwLock;

use globset::{GlobBuilder, GlobMatcher};

use crate::diff::is_binary_bytes;
use crate::error::{OzError, Result};
use crate::git_config::{AutoCrlf, EolSetting, GitConfig};

const GIT_LFS_POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";

/// Parsed state of a single Git attribute on a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrState {
    Set,
    Unset,
    Unspecified,
    Value(String),
}

/// Resolved attributes map for a repository-relative path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedAttrs {
    attrs: BTreeMap<String, AttrState>,
}

impl ResolvedAttrs {
    /// Returns the raw attribute state for `name`, if set.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AttrState> {
        self.attrs.get(name)
    }

    /// Returns `true` if `diff` is unset (`-diff` or `binary` macro).
    #[must_use]
    pub fn is_diff_disabled(&self) -> bool {
        matches!(self.attrs.get("diff"), Some(AttrState::Unset))
    }

    /// Returns `true` if `-text` or `binary` disabled text/EOL conversion.
    #[must_use]
    pub fn is_text_unset(&self) -> bool {
        matches!(self.attrs.get("text"), Some(AttrState::Unset))
    }

    /// Returns `true` if `text` is explicitly set (not `text=auto` or `-text`).
    #[must_use]
    pub fn is_text_set(&self) -> bool {
        matches!(self.attrs.get("text"), Some(AttrState::Set))
    }

    /// Returns `true` if `text=auto` is active.
    #[must_use]
    pub fn is_text_auto(&self) -> bool {
        matches!(self.attrs.get("text"), Some(AttrState::Value(v)) if v.eq_ignore_ascii_case("auto"))
    }

    /// Returns configured `eol=lf` or `eol=crlf` setting if explicitly specified.
    #[must_use]
    pub fn eol(&self) -> Option<EolSetting> {
        match self.attrs.get("eol") {
            Some(AttrState::Value(v)) if v.eq_ignore_ascii_case("lf") => Some(EolSetting::Lf),
            Some(AttrState::Value(v)) if v.eq_ignore_ascii_case("crlf") => Some(EolSetting::Crlf),
            _ => None,
        }
    }

    /// Returns the active filter driver name from `filter=<driver>` if set to a string value.
    #[must_use]
    pub fn filter_driver(&self) -> Option<&str> {
        match self.attrs.get("filter") {
            Some(AttrState::Value(v)) if !v.is_empty() => Some(v.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
enum AttrAssignment {
    Set(String),
    Unset(String),
    Unspecified(String),
    Value(String, String),
}

#[derive(Debug, Clone)]
struct AttrRule {
    matcher: GlobMatcher,
    basename_only: bool,
    assignments: Vec<AttrAssignment>,
}

/// Source backend for hierarchical `.gitattributes` files.
#[derive(Debug)]
enum AttrSource {
    /// Reads `<dir>/.gitattributes` on demand from a working directory on disk.
    Workdir(PathBuf),
    /// Uses preloaded `.gitattributes` parsed per directory (e.g. from ODB tree or overlay).
    Preloaded,
}

/// Thread-safe hierarchical `.gitattributes` resolver (`system`, `global`, `.gitattributes`,
/// nested `.gitattributes`, and `<git_common_dir>/info/attributes`).
#[derive(Debug)]
pub struct AttributeResolver {
    source: AttrSource,
    /// Rules sorted lowest-to-highest outside repo `.gitattributes`: `[system, global]`.
    low_priority_rules: Vec<AttrRule>,
    /// Rules from `<git_common_dir>/info/attributes` (highest precedence).
    info_rules: Vec<AttrRule>,
    /// Custom `[attr]<name>` macros + built-in `binary` macro.
    macros: RwLock<HashMap<String, Vec<AttrAssignment>>>,
    /// Per-directory rules keyed by repo-relative directory (`""` for root).
    dir_rules_cache: RwLock<HashMap<String, Vec<AttrRule>>>,
}

impl AttributeResolver {
    /// Builds a resolver reading `.gitattributes` on demand from `workdir_path`.
    pub fn for_workdir(
        workdir_path: &Path,
        git_common_dir: &Path,
        cfg: &GitConfig,
    ) -> Result<Self> {
        let mut macros = builtin_macros();
        let low_priority_rules = load_low_priority_rules(cfg, &mut macros)?;
        let info_rules = load_info_attributes(git_common_dir, &mut macros)?;

        // Preload root `.gitattributes` so top-level `[attr]` macros are visible even for info rules.
        let mut dir_cache = HashMap::new();
        let root_attr_path = workdir_path.join(".gitattributes");
        if root_attr_path.is_file() {
            let content = fs::read_to_string(&root_attr_path)?;
            let rules = parse_attributes_str(&content, true, &mut macros);
            dir_cache.insert(String::new(), rules);
        } else {
            dir_cache.insert(String::new(), Vec::new());
        }

        Ok(Self {
            source: AttrSource::Workdir(workdir_path.to_path_buf()),
            low_priority_rules,
            info_rules,
            macros: RwLock::new(macros),
            dir_rules_cache: RwLock::new(dir_cache),
        })
    }

    /// Builds a resolver from preloaded `(rel_dir, content)` `.gitattributes` blobs
    /// (for ODB tree materialization or FUSE3 overlay inspection without touching FUSE mount).
    pub fn from_preloaded_files(
        mut per_dir_contents: HashMap<String, String>,
        git_common_dir: &Path,
        cfg: &GitConfig,
    ) -> Result<Self> {
        let mut macros = builtin_macros();
        let low_priority_rules = load_low_priority_rules(cfg, &mut macros)?;

        let mut dir_cache = HashMap::new();
        // Parse root first so `[attr]` macros apply cleanly.
        if let Some(root_raw) = per_dir_contents.remove("") {
            let rules = parse_attributes_str(&root_raw, true, &mut macros);
            dir_cache.insert(String::new(), rules);
        }
        let mut entries: Vec<(String, String)> = per_dir_contents.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (dir, raw) in entries {
            let rules = parse_attributes_str(&raw, false, &mut macros);
            dir_cache.insert(dir, rules);
        }
        let info_rules = load_info_attributes(git_common_dir, &mut macros)?;

        Ok(Self {
            source: AttrSource::Preloaded,
            low_priority_rules,
            info_rules,
            macros: RwLock::new(macros),
            dir_rules_cache: RwLock::new(dir_cache),
        })
    }

    /// Loads all `.gitattributes` blobs from a `gix::ObjectId` tree into a preloaded resolver.
    pub fn for_tree(
        gix_repo: &gix::Repository,
        tree_id: Option<gix::ObjectId>,
        git_common_dir: &Path,
        cfg: &GitConfig,
    ) -> Result<Self> {
        let mut per_dir = HashMap::new();
        if let Some(tid) = tree_id {
            let tree = gix_repo.find_tree(tid).map_err(OzError::git_source)?;
            let entries = tree
                .traverse()
                .breadthfirst
                .files()
                .map_err(OzError::git_source)?;
            for entry in entries {
                if !entry.mode.is_blob() {
                    continue;
                }
                let path = String::from_utf8_lossy(entry.filepath.as_slice());
                let is_attr = path == ".gitattributes" || path.ends_with("/.gitattributes");
                if is_attr {
                    let dir = path
                        .strip_suffix("/.gitattributes")
                        .unwrap_or("")
                        .to_string();
                    let blob = gix_repo.find_blob(entry.oid).map_err(OzError::git_source)?;
                    per_dir.insert(dir, String::from_utf8_lossy(&blob.data).into_owned());
                }
            }
        }
        Self::from_preloaded_files(per_dir, git_common_dir, cfg)
    }

    /// Resolves effective attributes for repository-relative POSIX path `rel_path`.
    #[must_use]
    pub fn resolve(&self, rel_path: &str) -> ResolvedAttrs {
        let rel = rel_path.trim_start_matches('/');
        let mut resolved = ResolvedAttrs::default();

        // 1. Lowest precedence: system + global attributes
        self.apply_rules(&self.low_priority_rules, rel, &mut resolved);

        // 2. Repo-level & nested `.gitattributes` along ancestor dirs (`""`, `"a"`, `"a/b"`, ...)
        let ancestors = ancestor_dirs_for(rel);
        for dir in ancestors {
            let sub_rel = if dir.is_empty() {
                rel
            } else {
                rel.strip_prefix(&format!("{dir}/")).unwrap_or(rel)
            };
            self.with_dir_rules(&dir, |rules| {
                self.apply_rules(rules, sub_rel, &mut resolved);
            });
        }

        // 3. Highest precedence: `<git_common_dir>/info/attributes`
        self.apply_rules(&self.info_rules, rel, &mut resolved);

        resolved
    }

    /// Returns `true` if any rules exist anywhere (fast check to avoid overhead in repos without
    /// attributes configuration).
    #[must_use]
    pub fn may_have_rules(&self) -> bool {
        if !self.low_priority_rules.is_empty() || !self.info_rules.is_empty() {
            return true;
        }
        if let Ok(cache) = self.dir_rules_cache.read() {
            return cache.values().any(|v| !v.is_empty());
        }
        true
    }

    /// Returns the newest modified timestamp across `<git_common_dir>/info/attributes` and root
    /// `.gitattributes`, used for status index-stat cache invalidation.
    #[must_use]
    pub fn max_attributes_mtime(
        &self,
        workdir_path: &Path,
        git_common_dir: &Path,
    ) -> Option<std::time::SystemTime> {
        let candidates = [
            workdir_path.join(".gitattributes"),
            git_common_dir.join("info").join("attributes"),
        ];
        candidates
            .iter()
            .filter_map(|p| fs::metadata(p).ok()?.modified().ok())
            .max()
    }

    fn with_dir_rules<F>(&self, dir: &str, f: F)
    where
        F: FnOnce(&[AttrRule]),
    {
        if let Ok(cache) = self.dir_rules_cache.read()
            && let Some(rules) = cache.get(dir)
        {
            f(rules);
            return;
        }

        let loaded = match &self.source {
            AttrSource::Preloaded => Vec::new(),
            AttrSource::Workdir(workdir) => {
                let attr_path = if dir.is_empty() {
                    workdir.join(".gitattributes")
                } else {
                    workdir.join(dir).join(".gitattributes")
                };
                if attr_path.is_file() {
                    if let Ok(raw) = fs::read_to_string(&attr_path) {
                        if let Ok(mut macros) = self.macros.write() {
                            parse_attributes_str(&raw, dir.is_empty(), &mut macros)
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            }
        };

        if let Ok(mut cache) = self.dir_rules_cache.write() {
            let entry = cache.entry(dir.to_string()).or_insert(loaded);
            f(entry);
        } else {
            f(&loaded);
        }
    }

    fn apply_rules(&self, rules: &[AttrRule], sub_rel: &str, out: &mut ResolvedAttrs) {
        let basename = Path::new(sub_rel)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(sub_rel);
        let macros_guard = self.macros.read().ok();

        for rule in rules {
            let target = if rule.basename_only {
                basename
            } else {
                sub_rel
            };
            if rule.matcher.is_match(target) {
                for assign in &rule.assignments {
                    apply_assignment_recursive(assign, macros_guard.as_deref(), out, 0);
                }
            }
        }
    }
}

fn apply_assignment_recursive(
    assign: &AttrAssignment,
    macros: Option<&HashMap<String, Vec<AttrAssignment>>>,
    out: &mut ResolvedAttrs,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    match assign {
        AttrAssignment::Set(name) => {
            if let Some(macro_map) = macros
                && let Some(expanded) = macro_map.get(name)
            {
                for sub in expanded {
                    apply_assignment_recursive(sub, macros, out, depth + 1);
                }
            }
            out.attrs.insert(name.clone(), AttrState::Set);
        }
        AttrAssignment::Unset(name) => {
            out.attrs.insert(name.clone(), AttrState::Unset);
        }
        AttrAssignment::Unspecified(name) => {
            out.attrs.remove(name);
        }
        AttrAssignment::Value(name, val) => {
            out.attrs
                .insert(name.clone(), AttrState::Value(val.clone()));
        }
    }
}

/// Returns `true` if `bytes` looks like a canonical Git LFS v1 pointer file.
#[must_use]
pub fn is_lfs_pointer(bytes: &[u8]) -> bool {
    bytes.len() <= 1024
        && bytes.starts_with(GIT_LFS_POINTER_PREFIX)
        && bytes
            .windows(b"oid sha256:".len())
            .any(|w| w == b"oid sha256:")
}

/// Applies clean (checkin) EOL normalization according to `.gitattributes` and `GitConfig`.
#[must_use]
pub fn clean_eol_bytes<'a>(
    bytes: &'a [u8],
    attrs: &ResolvedAttrs,
    cfg: &GitConfig,
) -> Cow<'a, [u8]> {
    if attrs.is_text_unset() {
        return Cow::Borrowed(bytes);
    }

    let should_clean = if attrs.is_text_set() || attrs.eol().is_some() {
        true
    } else if attrs.is_text_auto()
        || (attrs.get("text").is_none() && cfg.autocrlf() != AutoCrlf::False)
    {
        !is_binary_bytes(bytes)
    } else {
        false
    };

    if !should_clean || !bytes.windows(2).any(|w| w == b"\r\n") {
        return Cow::Borrowed(bytes);
    }

    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\r' && bytes[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Cow::Owned(out)
}

/// Applies smudge (checkout) EOL normalization according to `.gitattributes` and `GitConfig`.
#[must_use]
pub fn smudge_eol_bytes<'a>(
    bytes: &'a [u8],
    attrs: &ResolvedAttrs,
    cfg: &GitConfig,
) -> Cow<'a, [u8]> {
    if attrs.is_text_unset() {
        return Cow::Borrowed(bytes);
    }

    let target_eol = match attrs.eol() {
        Some(EolSetting::Lf) => return Cow::Borrowed(bytes),
        Some(EolSetting::Crlf) => {
            if is_binary_bytes(bytes) {
                return Cow::Borrowed(bytes);
            }
            EolSetting::Crlf
        }
        Some(EolSetting::Native) | None => {
            let text_enabled = attrs.is_text_set()
                || ((attrs.is_text_auto()
                    || (attrs.get("text").is_none() && cfg.autocrlf() == AutoCrlf::True))
                    && !is_binary_bytes(bytes));
            if !text_enabled {
                return Cow::Borrowed(bytes);
            }
            if cfg.autocrlf() == AutoCrlf::True {
                EolSetting::Crlf
            } else {
                cfg.eol().unwrap_or(EolSetting::Native)
            }
        }
    };

    if target_eol != EolSetting::Crlf {
        return Cow::Borrowed(bytes);
    }

    let mut has_bare_lf = false;
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (idx == 0 || bytes[idx - 1] != b'\r') {
            has_bare_lf = true;
            break;
        }
    }
    if !has_bare_lf {
        return Cow::Borrowed(bytes);
    }

    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (idx == 0 || bytes[idx - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(b);
    }
    Cow::Owned(out)
}

/// Cleans working-tree bytes before comparing against HEAD or storing in the ODB:
/// applies EOL cleaning followed by any configured `filter.<driver>.clean` driver.
///
/// When `strict_missing_lfs` is `true` (e.g. `oz commit`), a missing `filter.lfs.clean`
/// driver fails safely with `OzError::CheckFailed` instead of committing raw binary content.
pub fn clean_blob_bytes<'a>(
    rel_path: &str,
    disk_bytes: &'a [u8],
    attrs: &ResolvedAttrs,
    cfg: &GitConfig,
    workdir: &Path,
    git_dir: &Path,
    strict_missing_lfs: bool,
) -> Result<Cow<'a, [u8]>> {
    let after_eol = clean_eol_bytes(disk_bytes, attrs, cfg);

    let Some(driver) = attrs.filter_driver() else {
        return Ok(after_eol);
    };

    let fcfg = cfg.filter_driver(driver);
    let clean_cmd = fcfg.as_ref().and_then(|c| c.clean.as_deref());
    let is_required = fcfg.as_ref().is_some_and(|c| c.required) || driver == "lfs";

    let Some(cmd_template) = clean_cmd else {
        if (driver == "lfs" && strict_missing_lfs) || fcfg.as_ref().is_some_and(|c| c.required) {
            return Err(OzError::CheckFailed {
                message: format!(
                    "path '{rel_path}' requires clean filter '{driver}', but 'filter.{driver}.clean' is not configured"
                ),
                recovery: missing_filter_recovery(driver),
            });
        }
        return Ok(after_eol);
    };

    match run_filter_command(cmd_template, rel_path, &after_eol, workdir, git_dir) {
        Ok(filtered) => Ok(Cow::Owned(filtered)),
        Err(err) if is_required => Err(OzError::CheckFailed {
            message: format!("clean filter '{driver}' failed for '{rel_path}': {err}"),
            recovery: missing_filter_recovery(driver),
        }),
        Err(_) => Ok(after_eol),
    }
}

/// Smudges an ODB blob before writing it into a workspace or checkout:
/// runs any configured `filter.<driver>.smudge` driver followed by EOL smudging.
///
/// Fails safely with `OzError::CheckFailed` when `filter=lfs` (or `required = true`) is active
/// (or blob is an LFS pointer) and `smudge` is missing or fails, preventing writing raw LFS
/// pointers silently.
pub fn smudge_blob_bytes<'a>(
    rel_path: &str,
    blob_bytes: &'a [u8],
    attrs: &ResolvedAttrs,
    cfg: &GitConfig,
    workdir: &Path,
    git_dir: &Path,
) -> Result<Cow<'a, [u8]>> {
    let explicit_driver = attrs.filter_driver();
    let effective_driver = explicit_driver.or_else(|| {
        if is_lfs_pointer(blob_bytes) {
            Some("lfs")
        } else {
            None
        }
    });

    let after_filter: Cow<'a, [u8]> = if let Some(driver) = effective_driver {
        let fcfg = cfg.filter_driver(driver);
        let smudge_cmd = fcfg.as_ref().and_then(|c| c.smudge.as_deref());
        let is_required = fcfg.as_ref().is_some_and(|c| c.required)
            || driver == "lfs"
            || is_lfs_pointer(blob_bytes);

        match smudge_cmd {
            Some(cmd_template) => {
                match run_filter_command(cmd_template, rel_path, blob_bytes, workdir, git_dir) {
                    Ok(out) => Cow::Owned(out),
                    Err(err) if is_required => {
                        return Err(OzError::CheckFailed {
                            message: format!(
                                "smudge filter '{driver}' failed for '{rel_path}': {err}"
                            ),
                            recovery: missing_filter_recovery(driver),
                        });
                    }
                    Err(_) => Cow::Borrowed(blob_bytes),
                }
            }
            None if is_required => {
                return Err(OzError::CheckFailed {
                    message: format!(
                        "path '{rel_path}' uses smudge filter '{driver}', but 'filter.{driver}.smudge' is not configured"
                    ),
                    recovery: missing_filter_recovery(driver),
                });
            }
            None => Cow::Borrowed(blob_bytes),
        }
    } else {
        Cow::Borrowed(blob_bytes)
    };

    Ok(match smudge_eol_bytes(&after_filter, attrs, cfg) {
        Cow::Borrowed(_) => after_filter,
        Cow::Owned(v) => Cow::Owned(v),
    })
}

fn missing_filter_recovery(driver: &str) -> Vec<String> {
    if driver == "lfs" {
        vec![
            "Install `git-lfs` and run `git lfs install`.".to_string(),
            "Verify `filter.lfs.clean` and `filter.lfs.smudge` in Git config.".to_string(),
        ]
    } else {
        vec![format!(
            "Configure `filter.{driver}.clean` and `filter.{driver}.smudge` in Git config."
        )]
    }
}

#[cfg(windows)]
fn find_windows_sh() -> Option<std::path::PathBuf> {
    if let Some(path_env) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let sh = dir.join("sh.exe");
            if sh.is_file() {
                return Some(sh);
            }
        }
        for dir in std::env::split_paths(&path_env) {
            if dir.join("git.exe").is_file() {
                for root in [Some(dir.as_path()), dir.parent()].into_iter().flatten() {
                    for rel in ["usr\\bin\\sh.exe", "bin\\sh.exe"] {
                        let cand = root.join(rel);
                        if cand.is_file() {
                            return Some(cand);
                        }
                    }
                }
            }
        }
    }
    for fallback in [
        r"C:\Program Files\Git\usr\bin\sh.exe",
        r"C:\Program Files\Git\bin\sh.exe",
    ] {
        let p = Path::new(fallback);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    None
}

#[cfg(any(windows, test))]
const WINDOWS_CMD_FORBIDDEN_CHARS: &[char] = &[
    '%', '&', '|', '^', '<', '>', '(', ')', '"', '\n', '\r', '!', ';', '\0',
];

/// Validates a repository path before substituting `%f` on Windows when `sh.exe` is unavailable.
///
/// Rejects all `cmd.exe` shell metacharacters, control/newline separators, NUL bytes, and
/// trailing backslashes (which would escape closing quotes in Windows CRT argv parsing).
#[cfg(any(windows, test))]
fn validate_windows_filter_path(rel_path: &str) -> std::result::Result<(), String> {
    if let Some(bad) = rel_path
        .chars()
        .find(|ch| WINDOWS_CMD_FORBIDDEN_CHARS.contains(ch))
    {
        return Err(format!(
            "refusing to substitute filter path '{rel_path}' containing forbidden Windows shell metacharacter {bad:?}"
        ));
    }
    if rel_path.ends_with('\\') {
        return Err(format!(
            "refusing to substitute filter path '{rel_path}' ending with a backslash"
        ));
    }
    Ok(())
}

/// Execution plan for running a Git clean/smudge filter on Windows when POSIX `sh.exe` is absent.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum WindowsFilterPlan {
    /// Direct `CreateProcessW` execution without invoking `cmd.exe`.
    Direct { program: String, args: Vec<String> },
    /// Restricted `cmd.exe /D /C` fallback for filter templates that themselves use shell operators,
    /// after strictly validating that `rel_path` contains zero shell metacharacters.
    CmdFallback { command_line: String },
}

/// Attempts to parse a simple filter template (such as `git-lfs clean -- %f`) into direct argv
/// without shell operators so Windows can spawn the executable directly without `cmd.exe`.
#[cfg(any(windows, test))]
fn parse_direct_filter_argv(cmd_template: &str, rel_path: &str) -> Option<(String, Vec<String>)> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = cmd_template.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '"' | '\'' => {
                    quote = Some(ch);
                    in_token = true;
                }
                c if c.is_ascii_whitespace() => {
                    if in_token {
                        tokens.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                }
                // Shell operators or variable expansion require a shell rather than direct argv.
                '|' | '&' | ';' | '<' | '>' | '(' | ')' | '^' | '!' | '`' | '$' | '\n' | '\r' => {
                    return None;
                }
                '%' => {
                    if chars.peek() == Some(&'f') {
                        chars.next();
                        current.push_str(rel_path);
                        in_token = true;
                    } else {
                        return None;
                    }
                }
                _ => {
                    current.push(ch);
                    in_token = true;
                }
            },
        }
    }

    if quote.is_some() {
        return None;
    }
    if in_token {
        tokens.push(current);
    }

    // Also substitute `%f` inside quoted tokens (e.g. `my-filter "%f"`), while refusing
    // arbitrary `%VAR%` environment interpolation inside quotes.
    let mut expanded_tokens = Vec::with_capacity(tokens.len());
    for tok in tokens {
        let mut out = String::with_capacity(tok.len());
        let mut tchars = tok.chars().peekable();
        while let Some(ch) = tchars.next() {
            if ch == '%' {
                if tchars.peek() == Some(&'f') {
                    tchars.next();
                    out.push_str(rel_path);
                } else {
                    return None;
                }
            } else {
                out.push(ch);
            }
        }
        expanded_tokens.push(out);
    }

    let mut iter = expanded_tokens.into_iter();
    let program = iter.next()?;
    let args: Vec<String> = iter.collect();
    Some((program, args))
}

#[cfg(any(windows, test))]
fn plan_windows_no_sh_filter(
    cmd_template: &str,
    rel_path: &str,
) -> std::result::Result<WindowsFilterPlan, String> {
    let uses_path = cmd_template.contains("%f");
    if uses_path {
        // Always reject Windows cmd.exe metacharacters in `rel_path` even for direct argv,
        // because Windows `CreateProcessW` implicitly invokes `cmd.exe` for `.bat`/`.cmd` shims.
        validate_windows_filter_path(rel_path)?;
    }

    if let Some((program, args)) = parse_direct_filter_argv(cmd_template, rel_path) {
        return Ok(WindowsFilterPlan::Direct { program, args });
    }

    // Shell pipeline/operator template without `sh.exe`: `rel_path` was already validated to
    // contain zero shell metacharacters, double quotes, newlines, or trailing backslashes.
    let quoted_rel = format!("\"{rel_path}\"");
    let win_expanded = cmd_template
        .replace("\"%f\"", &quoted_rel)
        .replace("%f", &quoted_rel);
    Ok(WindowsFilterPlan::CmdFallback {
        command_line: win_expanded,
    })
}

fn run_filter_command(
    cmd_template: &str,
    rel_path: &str,
    input: &[u8],
    workdir: &Path,
    git_dir: &Path,
) -> std::result::Result<Vec<u8>, String> {
    if rel_path.contains('\0') {
        return Err("refusing to run filter on path containing NUL byte".to_string());
    }
    let quoted_path = shell_quote_single(rel_path);
    let expanded = cmd_template.replace("%f", &quoted_path);
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &expanded]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        if let Some(sh_exe) = find_windows_sh() {
            let mut c = Command::new(sh_exe);
            c.args(["-c", &expanded]);
            c
        } else {
            match plan_windows_no_sh_filter(cmd_template, rel_path)? {
                WindowsFilterPlan::Direct { program, args } => {
                    let mut c = Command::new(program);
                    c.args(args);
                    c
                }
                WindowsFilterPlan::CmdFallback { command_line } => {
                    use std::os::windows::process::CommandExt as _;
                    let mut c = Command::new("cmd.exe");
                    c.raw_arg("/D");
                    c.raw_arg("/C");
                    c.raw_arg(format!("\"{command_line}\""));
                    c
                }
            }
        }
    };
    if workdir.is_dir() {
        cmd.current_dir(workdir);
    }
    cmd.env("GIT_DIR", git_dir);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed spawning '{expanded}': {e}"))?;
    let stdin = child.stdin.take();

    // Feed `stdin` on a scoped worker thread concurrently with `wait_with_output()` draining
    // stdout/stderr so streaming filters on payloads larger than the OS pipe buffer (>64 KiB)
    // never deadlock.
    let (write_res, wait_res) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> std::io::Result<()> {
            if let Some(mut stdin) = stdin {
                stdin.write_all(input)?;
                drop(stdin);
            }
            Ok(())
        });

        let wait_res = child.wait_with_output();
        let write_res = match writer.join() {
            Ok(res) => res,
            Err(_) => Err(std::io::Error::other("filter stdin writer thread panicked")),
        };
        (write_res, wait_res)
    });

    let output = wait_res.map_err(|e| format!("failed waiting on '{expanded}': {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        return Err(if stderr.is_empty() {
            format!("command `{expanded}` exited with status {code}")
        } else {
            format!("command `{expanded}` exited with status {code}: {stderr}")
        });
    }
    if let Err(err) = write_res
        && err.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(format!("failed writing stdin to '{expanded}': {err}"));
    }
    Ok(output.stdout)
}

fn shell_quote_single(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn builtin_macros() -> HashMap<String, Vec<AttrAssignment>> {
    let mut map = HashMap::new();
    map.insert(
        "binary".to_string(),
        vec![
            AttrAssignment::Unset("diff".to_string()),
            AttrAssignment::Unset("merge".to_string()),
            AttrAssignment::Unset("text".to_string()),
        ],
    );
    map
}

fn load_low_priority_rules(
    cfg: &GitConfig,
    macros: &mut HashMap<String, Vec<AttrAssignment>>,
) -> Result<Vec<AttrRule>> {
    let mut rules = Vec::new();
    let sys_path = Path::new("/etc/gitattributes");
    if sys_path.is_file() {
        let raw = fs::read_to_string(sys_path)?;
        rules.extend(parse_attributes_str(&raw, true, macros));
    }
    if let Some(global_path) = resolve_global_attributes_path(cfg)
        && global_path.is_file()
    {
        let raw = fs::read_to_string(&global_path)?;
        rules.extend(parse_attributes_str(&raw, true, macros));
    }
    Ok(rules)
}

fn resolve_global_attributes_path(cfg: &GitConfig) -> Option<PathBuf> {
    if let Some(explicit) = cfg.attributes_file() {
        return Some(explicit);
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("git").join("attributes"));
    }
    std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("git")
            .join("attributes")
    })
}

fn load_info_attributes(
    git_common_dir: &Path,
    macros: &mut HashMap<String, Vec<AttrAssignment>>,
) -> Result<Vec<AttrRule>> {
    let info_path = git_common_dir.join("info").join("attributes");
    if !info_path.is_file() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&info_path)?;
    Ok(parse_attributes_str(&raw, true, macros))
}

fn ancestor_dirs_for(rel_path: &str) -> Vec<String> {
    let mut dirs = vec![String::new()];
    let mut cur = String::new();
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.len() <= 1 {
        return dirs;
    }
    for component in &parts[..parts.len() - 1] {
        if component.is_empty() {
            continue;
        }
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(component);
        dirs.push(cur.clone());
    }
    dirs
}

fn parse_attributes_str(
    raw: &str,
    allow_macros: bool,
    macros: &mut HashMap<String, Vec<AttrAssignment>>,
) -> Vec<AttrRule> {
    let mut rules = Vec::new();
    for line in raw.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line).trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[attr]") {
            if !allow_macros {
                continue;
            }
            let mut tokens = split_attr_tokens(rest);
            if let Some(macro_name) = tokens.next() {
                let assigns: Vec<AttrAssignment> = tokens.filter_map(parse_attr_token).collect();
                macros.insert(macro_name, assigns);
            }
            continue;
        }

        let Some((pattern, rest)) = split_pattern_and_attrs(line) else {
            continue;
        };
        if pattern.starts_with('!') || pattern.is_empty() {
            continue;
        }
        let assignments: Vec<AttrAssignment> = split_attr_tokens(rest)
            .filter_map(parse_attr_token)
            .collect();
        if assignments.is_empty() {
            continue;
        }
        if let Some((matcher, basename_only)) = compile_attr_glob(&pattern) {
            rules.push(AttrRule {
                matcher,
                basename_only,
                assignments,
            });
        }
    }
    rules
}

fn split_pattern_and_attrs(line: &str) -> Option<(String, &str)> {
    let bytes = line.as_bytes();
    let mut pattern = String::new();
    let mut i = 0;

    if bytes.first() == Some(&b'"') {
        i = 1;
        while i < bytes.len() {
            let ch = line[i..].chars().next()?;
            if ch == '\\' {
                i += 1;
                let esc = line[i..].chars().next()?;
                pattern.push(match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    other => other,
                });
                i += esc.len_utf8();
            } else if ch == '"' {
                i += 1;
                break;
            } else {
                pattern.push(ch);
                i += ch.len_utf8();
            }
        }
        return Some((pattern, &line[i..]));
    }

    while i < bytes.len() {
        let ch = line[i..].chars().next()?;
        if ch == '\\' {
            i += 1;
            if i < bytes.len() {
                let esc = line[i..].chars().next()?;
                pattern.push(esc);
                i += esc.len_utf8();
            }
        } else if ch.is_ascii_whitespace() {
            break;
        } else {
            pattern.push(ch);
            i += ch.len_utf8();
        }
    }

    Some((pattern, &line[i..]))
}

fn split_attr_tokens(rest: &str) -> impl Iterator<Item = String> + '_ {
    rest.split_ascii_whitespace().map(ToString::to_string)
}

fn parse_attr_token(tok: String) -> Option<AttrAssignment> {
    if tok.is_empty() {
        return None;
    }
    if let Some(name) = tok.strip_prefix('-') {
        if !name.is_empty() {
            return Some(AttrAssignment::Unset(name.to_string()));
        }
        return None;
    }
    if let Some(name) = tok.strip_prefix('!') {
        if !name.is_empty() {
            return Some(AttrAssignment::Unspecified(name.to_string()));
        }
        return None;
    }
    if let Some((k, v)) = tok.split_once('=') {
        if !k.is_empty() {
            return Some(AttrAssignment::Value(k.to_string(), v.to_string()));
        }
        return None;
    }
    Some(AttrAssignment::Set(tok))
}

fn compile_attr_glob(pattern: &str) -> Option<(GlobMatcher, bool)> {
    let mut pat = pattern.to_string();
    let basename_only = !pat.contains('/');
    if let Some(stripped) = pat.strip_prefix('/') {
        pat = stripped.to_string();
    }
    let converted = convert_posix_negated_char_classes(&pat);
    let glob = GlobBuilder::new(&converted)
        .literal_separator(true)
        .build()
        .ok()?;
    Some((glob.compile_matcher(), basename_only))
}

fn convert_posix_negated_char_classes(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len());
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let mut in_class = false;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            out.push(ch);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if !in_class && ch == '[' {
            in_class = true;
            out.push('[');
            if i + 1 < chars.len() && chars[i + 1] == '^' {
                out.push('!');
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_class && ch == ']' {
            in_class = false;
        }
        out.push(ch);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_config::GitConfigContext;
    use tempfile::TempDir;

    #[test]
    fn resolves_hierarchical_and_info_attributes_and_macros() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("repo");
        let git_common = workdir.join(".git");
        fs::create_dir_all(workdir.join("docs/generated")).unwrap();
        fs::create_dir_all(git_common.join("info")).unwrap();

        fs::write(
            workdir.join(".gitattributes"),
            "* text=auto\n*.sh eol=lf\n*.png binary\n[attr]lock -diff -text\nCargo.lock lock\n",
        )
        .unwrap();
        fs::write(workdir.join("docs/.gitattributes"), "*.md text eol=crlf\n").unwrap();
        fs::write(
            git_common.join("info/attributes"),
            "docs/generated/*.md -text -diff\n",
        )
        .unwrap();

        let cfg = GitConfig::default();
        let resolver = AttributeResolver::for_workdir(&workdir, &git_common, &cfg).unwrap();

        let sh = resolver.resolve("scripts/deploy.sh");
        assert_eq!(sh.eol(), Some(EolSetting::Lf));
        assert!(!sh.is_diff_disabled());

        let png = resolver.resolve("assets/logo.png");
        assert!(png.is_diff_disabled());
        assert!(png.is_text_unset());

        let lock = resolver.resolve("Cargo.lock");
        assert!(lock.is_diff_disabled());
        assert!(lock.is_text_unset());

        let md = resolver.resolve("docs/guide.md");
        assert_eq!(md.eol(), Some(EolSetting::Crlf));
        assert!(md.is_text_set());

        let gen_md = resolver.resolve("docs/generated/api.md");
        assert!(gen_md.is_diff_disabled());
        assert!(gen_md.is_text_unset());
    }

    #[test]
    fn clean_and_smudge_eol_preserve_binary_and_honor_eol_directives() {
        let cfg_auto = GitConfig::parse_str(
            "[core]\n\tautocrlf = true\n",
            None,
            &GitConfigContext::default(),
        )
        .unwrap();

        let mut text_lf = ResolvedAttrs::default();
        text_lf.attrs.insert("text".into(), AttrState::Set);
        text_lf
            .attrs
            .insert("eol".into(), AttrState::Value("lf".into()));

        assert_eq!(
            clean_eol_bytes(b"a\r\nb\r\n", &text_lf, &cfg_auto).as_ref(),
            b"a\nb\n"
        );
        // eol=lf stays LF even when core.autocrlf = true
        assert_eq!(
            smudge_eol_bytes(b"a\nb\n", &text_lf, &cfg_auto).as_ref(),
            b"a\nb\n"
        );

        let mut binary = ResolvedAttrs::default();
        binary.attrs.insert("text".into(), AttrState::Unset);
        binary.attrs.insert("diff".into(), AttrState::Unset);
        assert_eq!(
            clean_eol_bytes(b"hdr\r\n\r\nbody", &binary, &cfg_auto).as_ref(),
            b"hdr\r\n\r\nbody"
        );
        assert_eq!(
            smudge_eol_bytes(b"hdr\nbody", &binary, &cfg_auto).as_ref(),
            b"hdr\nbody"
        );

        let mut crlf = ResolvedAttrs::default();
        crlf.attrs
            .insert("eol".into(), AttrState::Value("crlf".into()));
        assert_eq!(
            smudge_eol_bytes(b"line1\nline2\r\n", &crlf, &GitConfig::default()).as_ref(),
            b"line1\r\nline2\r\n"
        );
    }

    #[test]
    fn missing_lfs_filter_fails_safely_instead_of_corrupting_or_writing_pointers() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();

        let mut attrs = ResolvedAttrs::default();
        attrs
            .attrs
            .insert("filter".into(), AttrState::Value("lfs".into()));
        attrs.attrs.insert("text".into(), AttrState::Unset);

        let empty_cfg = GitConfig::default();
        let err_clean = clean_blob_bytes(
            "weights.bin",
            b"\x00\x01\x02raw-binary",
            &attrs,
            &empty_cfg,
            tmp.path(),
            &git_dir,
            true,
        )
        .unwrap_err();
        assert_eq!(err_clean.error_code(), "check_failed");

        let ptr =
            b"version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef\nsize 123\n";
        assert!(is_lfs_pointer(ptr));
        let err_smudge =
            smudge_blob_bytes("weights.bin", ptr, &attrs, &empty_cfg, tmp.path(), &git_dir)
                .unwrap_err();
        assert_eq!(err_smudge.error_code(), "check_failed");
    }

    #[test]
    fn windows_filter_plan_rejects_cmd_metacharacters_and_uses_direct_argv() {
        // Direct filter command avoids cmd.exe altogether, even with spaces in path.
        let plan = plan_windows_no_sh_filter("git-lfs clean -- %f", "docs/my report.bin")
            .expect("safe path succeeds");
        assert_eq!(
            plan,
            WindowsFilterPlan::Direct {
                program: "git-lfs".to_string(),
                args: vec![
                    "clean".to_string(),
                    "--".to_string(),
                    "docs/my report.bin".to_string(),
                ],
            }
        );

        let quoted_prog = plan_windows_no_sh_filter(
            r#""C:\Program Files\Git LFS\git-lfs.exe" clean -- "%f""#,
            "assets/model.bin",
        )
        .expect("quoted program path succeeds");
        assert_eq!(
            quoted_prog,
            WindowsFilterPlan::Direct {
                program: r"C:\Program Files\Git LFS\git-lfs.exe".to_string(),
                args: vec![
                    "clean".to_string(),
                    "--".to_string(),
                    "assets/model.bin".to_string(),
                ],
            }
        );

        // Shell pipeline falls back cleanly for safe paths.
        let pipe_plan = plan_windows_no_sh_filter("filter-a %f | filter-b", "src/data.txt")
            .expect("safe pipeline succeeds");
        assert_eq!(
            pipe_plan,
            WindowsFilterPlan::CmdFallback {
                command_line: "filter-a \"src/data.txt\" | filter-b".to_string(),
            }
        );

        // Every Windows cmd.exe metacharacter injection vector must be rejected.
        let malicious_paths = [
            "a%PATH%b",
            "a&calc.exe",
            "a|calc.exe",
            "a^&calc.exe",
            "a<in.txt",
            "a>out.txt",
            "a(b)",
            "a\" & calc.exe & \"",
            "a\ncalc.exe",
            "a\rcalc.exe",
            "a!DELAYED!",
            "a;calc.exe",
            "a\0b",
            "trailing\\",
        ];
        for bad in malicious_paths {
            assert!(
                plan_windows_no_sh_filter("git-lfs clean -- %f", bad).is_err(),
                "direct plan must reject malicious path {bad:?}"
            );
            assert!(
                plan_windows_no_sh_filter("filter-a %f | filter-b", bad).is_err(),
                "cmd fallback plan must reject malicious path {bad:?}"
            );
        }
    }

    #[test]
    fn streaming_filter_handles_over_128kib_payload_without_pipe_deadlock() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();

        // 256 KiB payload (> 4x typical 64 KiB OS pipe buffer) to prove concurrent stdin/stdout I/O.
        let payload: Vec<u8> = (0..(256 * 1024usize))
            .map(|i| b'a' + (i % 26) as u8)
            .collect();

        // NUL byte in filename is rejected before spawn on all platforms.
        assert!(run_filter_command("cat", "bad\0name", b"hi", tmp.path(), &git_dir).is_err());

        #[cfg(unix)]
        {
            let out = run_filter_command("cat", "large/blob.dat", &payload, tmp.path(), &git_dir)
                .expect("256 KiB streaming filter must complete without deadlocking");
            assert_eq!(out.len(), payload.len());
            assert_eq!(out, payload);

            // Also verify metacharacter-laden repository filenames cannot inject commands via `%f`.
            let marker = tmp.path().join("pwned_marker");
            let injected_rel = format!("evil'; touch {}; '.txt", marker.display());
            let echoed =
                run_filter_command("printf '%s' %f", &injected_rel, b"", tmp.path(), &git_dir)
                    .expect("POSIX single-quoted path runs safely");
            assert_eq!(echoed, injected_rel.as_bytes());
            assert!(
                !marker.exists(),
                "shell command injection occurred via %f filename!"
            );
        }
        #[cfg(windows)]
        {
            if find_windows_sh().is_some() {
                let out =
                    run_filter_command("cat", "large/blob.dat", &payload, tmp.path(), &git_dir)
                        .expect("256 KiB streaming filter must complete without deadlocking");
                assert_eq!(out, payload);
            }
        }
    }
}
