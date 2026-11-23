// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust Git configuration parser with layered resolution, `include` / `includeIf`,
//! environment overrides (`GIT_CONFIG_COUNT`, `GIT_CONFIG_PARAMETERS`), `-c` overrides,
//! and `url.<base>.insteadOf` / `pushInsteadOf` rewrites.

use std::fs;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;

use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;

const MAX_INCLUDE_DEPTH: usize = 10;

/// Normalization mode for `core.autocrlf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoCrlf {
    #[default]
    False,
    True,
    Input,
}

/// Configured `core.eol` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EolSetting {
    Lf,
    Crlf,
    Native,
}

impl EolSetting {
    /// Returns the line ending byte sequence for the active platform.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Lf | Self::Native => b"\n",
            Self::Crlf => b"\r\n",
        }
    }
}

/// Configuration for a `filter.<driver>` section.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterDriverConfig {
    pub clean: Option<String>,
    pub smudge: Option<String>,
    pub required: bool,
}

/// Raw `remote.<name>` configuration fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteConfig {
    pub url: Option<String>,
    pub pushurl: Option<String>,
    pub fetch: Vec<String>,
}

/// Resolved remote URLs after applying `insteadOf` / `pushInsteadOf`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedRemoteUrls {
    pub configured_url: Option<String>,
    pub configured_pushurl: Option<String>,
    pub fetch_url: Option<String>,
    pub push_url: Option<String>,
}

/// Repository and environment context used while loading [`GitConfig`].
#[derive(Debug, Clone, Default)]
pub struct GitConfigContext {
    pub git_dir: Option<PathBuf>,
    pub git_common_dir: Option<PathBuf>,
    pub workdir_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub overrides: Vec<(String, String)>,
    pub home_dir: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub system_config: Option<PathBuf>,
    pub global_config: Option<PathBuf>,
    pub no_system: Option<bool>,
    pub ignore_env_configs: bool,
}

impl GitConfigContext {
    /// Builds a context from an [`OzoneRepo`] and optional CLI `-c` overrides.
    #[must_use]
    pub fn from_repo(repo: &OzoneRepo, overrides: &[(String, String)]) -> Self {
        Self {
            git_dir: Some(repo.git_dir.clone()),
            git_common_dir: Some(repo.git_common_dir.clone()),
            workdir_path: Some(repo.workdir_path.clone()),
            branch: repo.branch.clone(),
            overrides: overrides.to_vec(),
            ..Default::default()
        }
    }

    fn effective_home(&self) -> Option<PathBuf> {
        self.home_dir
            .clone()
            .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from))
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
    }

    fn effective_xdg_config_home(&self) -> Option<PathBuf> {
        self.xdg_config_home
            .clone()
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
            .or_else(|| self.effective_home().map(|h| h.join(".config")))
    }

    fn is_no_system(&self) -> bool {
        if let Some(flag) = self.no_system {
            return flag;
        }
        std::env::var("GIT_CONFIG_NOSYSTEM")
            .ok()
            .and_then(|v| parse_git_bool(&v))
            .unwrap_or(false)
    }

    fn system_config_path(&self) -> Option<PathBuf> {
        if self.is_no_system() {
            return None;
        }
        if let Some(ref p) = self.system_config {
            return Some(p.clone());
        }
        if !self.ignore_env_configs
            && let Some(env_p) = std::env::var_os("GIT_CONFIG_SYSTEM")
        {
            if env_p.is_empty() {
                return None;
            }
            return Some(PathBuf::from(env_p));
        }
        Some(PathBuf::from("/etc/gitconfig"))
    }

    fn global_config_paths(&self) -> Vec<PathBuf> {
        if let Some(ref p) = self.global_config {
            return vec![p.clone()];
        }
        if !self.ignore_env_configs
            && let Some(env_p) = std::env::var_os("GIT_CONFIG_GLOBAL")
        {
            if env_p.is_empty() {
                return Vec::new();
            }
            return vec![PathBuf::from(env_p)];
        }
        let mut paths = Vec::new();
        if let Some(xdg) = self.effective_xdg_config_home() {
            paths.push(xdg.join("git").join("config"));
        }
        if let Some(home) = self.effective_home() {
            paths.push(home.join(".gitconfig"));
        }
        paths
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigEntry {
    section: String,
    subsection: Option<String>,
    key: String,
    /// `None` denotes an implicit boolean entry without `=`.
    value: Option<String>,
}

/// Layered Git configuration snapshot with `include` / `includeIf` expansion and URL rewriting.
#[derive(Debug, Clone, Default)]
pub struct GitConfig {
    entries: Vec<ConfigEntry>,
    home_dir: Option<PathBuf>,
}

impl GitConfig {
    /// Loads system, global, local, worktree, environment, and CLI `-c` configuration for `repo`.
    pub fn load_for_repo(repo: &OzoneRepo, overrides: &[(String, String)]) -> Result<Self> {
        let ctx = GitConfigContext::from_repo(repo, overrides);
        Self::load_with_context(&ctx)
    }

    /// Loads global/system/environment configuration outside of any repository.
    pub fn load_global(overrides: &[(String, String)]) -> Result<Self> {
        let ctx = GitConfigContext {
            overrides: overrides.to_vec(),
            ..Default::default()
        };
        Self::load_with_context(&ctx)
    }

    /// Loads layered Git configuration according to `ctx`.
    pub fn load_with_context(ctx: &GitConfigContext) -> Result<Self> {
        let mut cfg = Self {
            entries: Vec::new(),
            home_dir: ctx.effective_home(),
        };

        if let Some(sys_path) = ctx.system_config_path() {
            cfg.load_file_if_exists(&sys_path, ctx, 0)?;
        }

        for global_path in ctx.global_config_paths() {
            cfg.load_file_if_exists(&global_path, ctx, 0)?;
        }

        let explicit_git_config = if !ctx.ignore_env_configs {
            std::env::var_os("GIT_CONFIG")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        } else {
            None
        };

        if let Some(env_file) = explicit_git_config {
            cfg.load_file_if_exists(&env_file, ctx, 0)?;
        } else {
            let common_or_git_dir = ctx
                .git_common_dir
                .as_deref()
                .or(ctx.git_dir.as_deref());
            if let Some(common_dir) = common_or_git_dir {
                let local_cfg = common_dir.join("config");
                cfg.load_file_if_exists(&local_cfg, ctx, 0)?;
            }

            if cfg.get_bool("extensions", None, "worktreeconfig").unwrap_or(false)
                && let Some(git_dir) = ctx.git_dir.as_deref()
            {
                let wt_cfg = git_dir.join("config.worktree");
                cfg.load_file_if_exists(&wt_cfg, ctx, 0)?;
            }
        }

        if !ctx.ignore_env_configs {
            cfg.apply_env_overrides()?;
        }

        for (raw_key, val) in &ctx.overrides {
            cfg.apply_override(raw_key, val)?;
        }

        Ok(cfg)
    }

    /// Parses an in-memory Git config string, evaluating any `[include]` / `[includeIf]` directives
    /// relative to `base_dir`.
    pub fn parse_str(
        contents: &str,
        base_dir: Option<&Path>,
        ctx: &GitConfigContext,
    ) -> Result<Self> {
        let mut cfg = Self {
            entries: Vec::new(),
            home_dir: ctx.effective_home(),
        };
        cfg.parse_into(contents, base_dir, None, ctx, 0)?;
        for (raw_key, val) in &ctx.overrides {
            cfg.apply_override(raw_key, val)?;
        }
        Ok(cfg)
    }

    /// Parses a CLI `-c key[=value]` argument into `(key, value)`.
    pub fn parse_cli_override(spec: &str) -> Result<(String, String)> {
        if let Some((k, v)) = spec.split_once('=') {
            validate_canonical_key(k)?;
            Ok((k.to_string(), v.to_string()))
        } else {
            validate_canonical_key(spec)?;
            Ok((spec.to_string(), "true".to_string()))
        }
    }

    /// Applies an in-memory override (`section[.subsection].key = value`).
    pub fn apply_override(&mut self, canonical_key: &str, value: &str) -> Result<()> {
        let (section, subsection, key) = split_canonical_key(canonical_key)?;
        self.entries.push(ConfigEntry {
            section,
            subsection,
            key,
            value: Some(value.to_string()),
        });
        Ok(())
    }

    /// Returns the last configured raw value (`None` if unset; `Some(None)` if valueless bool).
    #[must_use]
    pub fn get_raw(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<Option<&str>> {
        let sec = section.to_ascii_lowercase();
        let k = key.to_ascii_lowercase();
        self.entries
            .iter()
            .rev()
            .find(|e| e.section == sec && e.subsection.as_deref() == subsection && e.key == k)
            .map(|e| e.value.as_deref())
    }

    /// Returns the last configured string value (`""` for valueless implicit bools if `allow_implicit`
    /// is false? Returns `None` if valueless or unset).
    #[must_use]
    pub fn get_str(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<&str> {
        match self.get_raw(section, subsection, key) {
            Some(Some(val)) => Some(val),
            _ => None,
        }
    }

    /// Returns all configured string values for `(section, subsection, key)` in order of priority.
    #[must_use]
    pub fn get_all(&self, section: &str, subsection: Option<&str>, key: &str) -> Vec<&str> {
        let sec = section.to_ascii_lowercase();
        let k = key.to_ascii_lowercase();
        let mut out = Vec::new();
        for entry in &self.entries {
            if entry.section == sec
                && entry.subsection.as_deref() == subsection
                && entry.key == k
                && let Some(val) = entry.value.as_deref()
            {
                if val.is_empty() {
                    out.clear();
                } else {
                    out.push(val);
                }
            }
        }
        out
    }

    /// Returns the last boolean setting for `(section, subsection, key)`.
    #[must_use]
    pub fn get_bool(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<bool> {
        match self.get_raw(section, subsection, key)? {
            None => Some(true),
            Some(v) => parse_git_bool(v),
        }
    }

    /// Returns `core.hooksPath` if configured and non-empty.
    #[must_use]
    pub fn hooks_path(&self) -> Option<&str> {
        self.get_str("core", None, "hookspath")
            .filter(|v| !v.is_empty())
    }

    /// Returns `core.attributesFile` resolved against `HOME` if configured.
    #[must_use]
    pub fn attributes_file(&self) -> Option<PathBuf> {
        let raw = self
            .get_str("core", None, "attributesfile")
            .filter(|v| !v.is_empty())?;
        Some(expand_tilde_with_home(raw, self.home_dir.as_deref()))
    }

    /// Returns `core.autocrlf`.
    #[must_use]
    pub fn autocrlf(&self) -> AutoCrlf {
        match self.get_raw("core", None, "autocrlf") {
            None => AutoCrlf::False,
            Some(None) => AutoCrlf::True,
            Some(Some(v)) => {
                if v.eq_ignore_ascii_case("input") {
                    AutoCrlf::Input
                } else if parse_git_bool(v) == Some(true) {
                    AutoCrlf::True
                } else {
                    AutoCrlf::False
                }
            }
        }
    }

    /// Returns `core.eol`.
    #[must_use]
    pub fn eol(&self) -> Option<EolSetting> {
        let raw = self.get_str("core", None, "eol")?;
        if raw.eq_ignore_ascii_case("lf") {
            Some(EolSetting::Lf)
        } else if raw.eq_ignore_ascii_case("crlf") {
            Some(EolSetting::Crlf)
        } else if raw.eq_ignore_ascii_case("native") {
            Some(EolSetting::Native)
        } else {
            None
        }
    }

    /// Returns `core.sshCommand` if configured and non-empty.
    #[must_use]
    pub fn ssh_command(&self) -> Option<&str> {
        self.get_str("core", None, "sshcommand")
            .filter(|v| !v.is_empty())
    }

    /// Returns `filter.<driver>` configuration if any key in the driver section is present.
    #[must_use]
    pub fn filter_driver(&self, driver: &str) -> Option<FilterDriverConfig> {
        let clean = self
            .get_str("filter", Some(driver), "clean")
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let smudge = self
            .get_str("filter", Some(driver), "smudge")
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let required_opt = self.get_bool("filter", Some(driver), "required");
        if clean.is_none() && smudge.is_none() && required_opt.is_none() {
            return None;
        }
        Some(FilterDriverConfig {
            clean,
            smudge,
            required: required_opt.unwrap_or(false),
        })
    }

    /// Returns `user.name` if configured.
    #[must_use]
    pub fn user_name(&self) -> Option<&str> {
        self.get_str("user", None, "name").filter(|s| !s.is_empty())
    }

    /// Returns `user.email` if configured.
    #[must_use]
    pub fn user_email(&self) -> Option<&str> {
        self.get_str("user", None, "email")
            .filter(|s| !s.is_empty())
    }

    /// Returns `user.signingKey` if configured.
    #[must_use]
    pub fn signing_key(&self) -> Option<&str> {
        self.get_str("user", None, "signingkey")
            .filter(|s| !s.is_empty())
    }

    /// Returns `commit.gpgsign` if configured.
    #[must_use]
    pub fn commit_gpgsign(&self) -> Option<bool> {
        self.get_bool("commit", None, "gpgsign")
    }

    /// Returns `gpg.format` if configured.
    #[must_use]
    pub fn gpg_format(&self) -> Option<&str> {
        self.get_str("gpg", None, "format")
            .filter(|s| !s.is_empty())
    }

    /// Returns raw remote config for `remote.<name>`.
    #[must_use]
    pub fn remote_config(&self, name: &str) -> RemoteConfig {
        RemoteConfig {
            url: self
                .get_str("remote", Some(name), "url")
                .filter(|s| !s.is_empty())
                .map(ToString::to_string),
            pushurl: self
                .get_str("remote", Some(name), "pushurl")
                .filter(|s| !s.is_empty())
                .map(ToString::to_string),
            fetch: self
                .get_all("remote", Some(name), "fetch")
                .into_iter()
                .map(ToString::to_string)
                .collect(),
        }
    }

    /// Rewrites `raw_url` using `url.<base>.pushInsteadOf` (when `for_push && !has_explicit_pushurl`)
    /// and `url.<base>.insteadOf`, matching Git's longest-prefix-wins semantics.
    #[must_use]
    pub fn rewrite_url(&self, raw_url: &str, for_push: bool, has_explicit_pushurl: bool) -> String {
        if for_push
            && !has_explicit_pushurl
            && let Some(rewritten) = self.find_longest_url_rewrite(raw_url, "pushinsteadof")
        {
            return rewritten;
        }
        if let Some(rewritten) = self.find_longest_url_rewrite(raw_url, "insteadof") {
            return rewritten;
        }
        raw_url.to_string()
    }

    /// Resolves effective fetch and push URLs for `remote_name` using `insteadOf` / `pushInsteadOf`.
    #[must_use]
    pub fn resolve_remote_urls(&self, remote_name: &str) -> ResolvedRemoteUrls {
        let remote = self.remote_config(remote_name);
        let fetch_url = remote
            .url
            .as_deref()
            .map(|u| self.rewrite_url(u, false, false));
        let push_url = match remote.pushurl.as_deref() {
            Some(pu) => Some(self.rewrite_url(pu, true, true)),
            None => remote
                .url
                .as_deref()
                .map(|u| self.rewrite_url(u, true, false)),
        };
        ResolvedRemoteUrls {
            configured_url: remote.url,
            configured_pushurl: remote.pushurl,
            fetch_url,
            push_url,
        }
    }

    fn find_longest_url_rewrite(&self, raw_url: &str, key_name: &str) -> Option<String> {
        let mut best: Option<(usize, &str, &str)> = None;
        for entry in &self.entries {
            if entry.section == "url"
                && entry.key == key_name
                && let Some(base) = entry.subsection.as_deref()
                && let Some(prefix) = entry.value.as_deref()
                && !prefix.is_empty()
                && raw_url.starts_with(prefix)
            {
                let len = prefix.len();
                let replace = match best {
                    None => true,
                    Some((best_len, _, _)) => len >= best_len,
                };
                if replace {
                    best = Some((len, base, prefix));
                }
            }
        }
        best.map(|(len, base, _)| format!("{base}{}", &raw_url[len..]))
    }

    fn load_file_if_exists(
        &mut self,
        path: &Path,
        ctx: &GitConfigContext,
        depth: usize,
    ) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let raw = fs::read(path)?;
        let content = String::from_utf8_lossy(&raw);
        let base_dir = path.parent();
        self.parse_into(&content, base_dir, Some(path), ctx, depth)
    }

    fn parse_into(
        &mut self,
        contents: &str,
        base_dir: Option<&Path>,
        source_path: Option<&Path>,
        ctx: &GitConfigContext,
        depth: usize,
    ) -> Result<()> {
        let logical_lines = join_logical_lines(contents);
        let mut current_section: Option<(String, Option<String>)> = None;

        for line in logical_lines {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }

            if trimmed.starts_with('[') {
                let header = parse_section_header(trimmed, source_path)?;
                current_section = Some(header);
                continue;
            }

            let Some((section, subsection)) = current_section.as_ref() else {
                return Err(invalid_cfg(
                    source_path,
                    format!("git config variable outside any [section]: {trimmed}"),
                ));
            };

            let (key, value) = parse_variable_line(trimmed, source_path)?;
            let entry = ConfigEntry {
                section: section.clone(),
                subsection: subsection.clone(),
                key: key.clone(),
                value: value.clone(),
            };
            self.entries.push(entry);

            if depth < MAX_INCLUDE_DEPTH && key == "path" && let Some(inc_raw) = value.as_deref() {
                let should_include = if section == "include" && subsection.is_none() {
                    true
                } else if section == "includeif"
                    && let Some(cond) = subsection.as_deref()
                {
                    self.evaluate_include_if(cond, base_dir, ctx)
                } else {
                    false
                };

                if should_include && !inc_raw.is_empty() {
                    let inc_path = resolve_include_path(inc_raw, base_dir, self.home_dir.as_deref());
                    self.load_file_if_exists(&inc_path, ctx, depth + 1)?;
                }
            }
        }

        Ok(())
    }

    fn evaluate_include_if(
        &self,
        condition: &str,
        base_dir: Option<&Path>,
        ctx: &GitConfigContext,
    ) -> bool {
        if let Some(pat) = condition.strip_prefix("gitdir:") {
            return match_gitdir_condition(pat, base_dir, ctx, self.home_dir.as_deref(), false);
        }
        if let Some(pat) = condition.strip_prefix("gitdir/i:") {
            return match_gitdir_condition(pat, base_dir, ctx, self.home_dir.as_deref(), true);
        }
        if let Some(pat) = condition.strip_prefix("onbranch:") {
            return match_onbranch_condition(pat, ctx.branch.as_deref());
        }
        if let Some(pat) = condition.strip_prefix("hasconfig:remote.*.url:") {
            return self.match_hasconfig_remote_url(pat);
        }
        false
    }

    fn match_hasconfig_remote_url(&self, pat: &str) -> bool {
        let Ok(glob) = GlobBuilder::new(pat).literal_separator(false).build() else {
            return false;
        };
        let matcher = glob.compile_matcher();
        self.entries.iter().any(|e| {
            e.section == "remote"
                && e.subsection.is_some()
                && e.key == "url"
                && e.value.as_deref().is_some_and(|u| matcher.is_match(u))
        })
    }

    fn apply_env_overrides(&mut self) -> Result<()> {
        if let Ok(params) = std::env::var("GIT_CONFIG_PARAMETERS")
            && !params.trim().is_empty()
        {
            for spec in parse_git_config_parameters(&params) {
                let (k, v) = Self::parse_cli_override(&spec)?;
                self.apply_override(&k, &v)?;
            }
        }

        if let Ok(count_str) = std::env::var("GIT_CONFIG_COUNT")
            && !count_str.trim().is_empty()
        {
            let count: usize = count_str
                .trim()
                .parse()
                .map_err(|_| invalid_cfg(None, format!("invalid GIT_CONFIG_COUNT '{count_str}'")))?;
            for idx in 0..count {
                let key_var = format!("GIT_CONFIG_KEY_{idx}");
                let val_var = format!("GIT_CONFIG_VALUE_{idx}");
                let key = std::env::var(&key_var).map_err(|_| {
                    invalid_cfg(None, format!("missing {key_var} while GIT_CONFIG_COUNT={count}"))
                })?;
                let val = std::env::var(&val_var).unwrap_or_default();
                self.apply_override(&key, &val)?;
            }
        }

        Ok(())
    }
}

fn invalid_cfg(source: Option<&Path>, message: impl Into<String>) -> OzError {
    OzError::InvalidConfig {
        path: source
            .unwrap_or_else(|| Path::new("<git-config>"))
            .to_path_buf(),
        message: message.into(),
    }
}

fn parse_git_bool(raw: &str) -> Option<bool> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("true")
        || trimmed.eq_ignore_ascii_case("yes")
        || trimmed.eq_ignore_ascii_case("on")
        || trimmed == "1"
    {
        Some(true)
    } else if trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no")
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed == "0"
        || trimmed.is_empty()
    {
        Some(false)
    } else {
        None
    }
}

fn validate_canonical_key(raw: &str) -> Result<()> {
    let _ = split_canonical_key(raw)?;
    Ok(())
}

fn split_canonical_key(raw: &str) -> Result<(String, Option<String>, String)> {
    let first_dot = raw.find('.').ok_or_else(|| {
        invalid_cfg(
            None,
            format!("invalid git config key '{raw}': missing section separator"),
        )
    })?;
    let last_dot = raw.rfind('.').ok_or_else(|| {
        invalid_cfg(
            None,
            format!("invalid git config key '{raw}': missing key separator"),
        )
    })?;
    let section = &raw[..first_dot];
    let key = &raw[last_dot + 1..];
    if section.is_empty()
        || key.is_empty()
        || !is_valid_config_ident(section)
        || !is_valid_config_ident(key)
    {
        return Err(invalid_cfg(None, format!("invalid git config key '{raw}'")));
    }
    let subsection = if first_dot == last_dot {
        None
    } else {
        Some(raw[first_dot + 1..last_dot].to_string())
    };
    Ok((
        section.to_ascii_lowercase(),
        subsection,
        key.to_ascii_lowercase(),
    ))
}

fn is_valid_config_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Joins physical lines ending with an unescaped trailing `\` outside comments.
fn join_logical_lines(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_comment = false;

    for raw_line in input.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if current.is_empty() {
            in_quotes = false;
            in_comment = false;
        }

        let chars: Vec<char> = line.chars().collect();
        let mut idx = 0;
        let mut continued = false;

        while idx < chars.len() {
            let ch = chars[idx];
            if in_comment {
                current.push(ch);
                idx += 1;
                continue;
            }
            if ch == '\\' {
                if idx + 1 == chars.len() {
                    continued = true;
                    break;
                }
                current.push(ch);
                current.push(chars[idx + 1]);
                idx += 2;
                continue;
            }
            if ch == '"' {
                in_quotes = !in_quotes;
                current.push(ch);
                idx += 1;
                continue;
            }
            if !in_quotes && (ch == '#' || ch == ';') {
                in_comment = true;
                current.push(ch);
                idx += 1;
                continue;
            }
            current.push(ch);
            idx += 1;
        }

        if continued {
            continue;
        }
        out.push(std::mem::take(&mut current));
    }

    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn parse_section_header(line: &str, source_path: Option<&Path>) -> Result<(String, Option<String>)> {
    let after_open = line
        .strip_prefix('[')
        .ok_or_else(|| invalid_cfg(source_path, format!("invalid git config section header: {line}")))?;

    let mut chars = after_open.chars().peekable();
    let mut section = String::new();
    let mut saw_space = false;
    let mut legacy_sub = false;

    while let Some(&ch) = chars.peek() {
        if ch == ']' {
            chars.next();
            let rest: String = chars.collect();
            let rest = rest.trim_start();
            if !rest.is_empty() && !rest.starts_with('#') && !rest.starts_with(';') {
                return Err(invalid_cfg(
                    source_path,
                    format!("trailing characters after git config header: {line}"),
                ));
            }
            if section.is_empty() {
                return Err(invalid_cfg(
                    source_path,
                    format!("empty git config section name: {line}"),
                ));
            }
            return Ok((section.to_ascii_lowercase(), None));
        }
        if ch.is_ascii_whitespace() {
            saw_space = true;
            break;
        }
        if ch == '.' {
            legacy_sub = true;
            chars.next();
            break;
        }
        section.push(ch);
        chars.next();
    }

    if section.is_empty() {
        return Err(invalid_cfg(
            source_path,
            format!("empty git config section name: {line}"),
        ));
    }

    if legacy_sub {
        let mut sub = String::new();
        let mut closed = false;
        for ch in chars.by_ref() {
            if ch == ']' {
                closed = true;
                break;
            }
            sub.push(ch);
        }
        if !closed {
            return Err(invalid_cfg(
                source_path,
                format!("unclosed git config section header: {line}"),
            ));
        }
        return Ok((section.to_ascii_lowercase(), Some(sub.to_ascii_lowercase())));
    }

    if !saw_space {
        return Err(invalid_cfg(
            source_path,
            format!("unclosed git config section header: {line}"),
        ));
    }

    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_whitespace() {
            chars.next();
        } else {
            break;
        }
    }

    if chars.peek() == Some(&'"') {
        chars.next();
        let mut sub = String::new();
        let mut closed_quote = false;
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                let esc = chars.next().ok_or_else(|| {
                    invalid_cfg(source_path, format!("dangling escape in git config header: {line}"))
                })?;
                sub.push(esc);
            } else if ch == '"' {
                closed_quote = true;
                break;
            } else {
                sub.push(ch);
            }
        }
        if !closed_quote {
            return Err(invalid_cfg(
                source_path,
                format!("unclosed subsection quote in git config header: {line}"),
            ));
        }
        while let Some(&ch) = chars.peek() {
            if ch.is_ascii_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        if chars.next() != Some(']') {
            return Err(invalid_cfg(
                source_path,
                format!("missing ']' in git config section header: {line}"),
            ));
        }
        let rest: String = chars.collect();
        let rest = rest.trim_start();
        if !rest.is_empty() && !rest.starts_with('#') && !rest.starts_with(';') {
            return Err(invalid_cfg(
                source_path,
                format!("trailing characters after git config header: {line}"),
            ));
        }
        return Ok((section.to_ascii_lowercase(), Some(sub)));
    }

    Err(invalid_cfg(
        source_path,
        format!("invalid git config section header: {line}"),
    ))
}

fn parse_variable_line(line: &str, source_path: Option<&Path>) -> Result<(String, Option<String>)> {
    let mut eq_pos = None;
    let mut comment_pos = None;

    for (idx, ch) in line.char_indices() {
        if ch == '=' {
            eq_pos = Some(idx);
            break;
        }
        if ch == '#' || ch == ';' {
            comment_pos = Some(idx);
            break;
        }
    }

    let Some(eq_idx) = eq_pos else {
        let key_part = match comment_pos {
            Some(c) => line[..c].trim(),
            None => line.trim(),
        };
        if !is_valid_config_ident(key_part) {
            return Err(invalid_cfg(
                source_path,
                format!("invalid git config variable name '{key_part}'"),
            ));
        }
        return Ok((key_part.to_ascii_lowercase(), None));
    };

    let key_part = line[..eq_idx].trim();
    if !is_valid_config_ident(key_part) {
        return Err(invalid_cfg(
            source_path,
            format!("invalid git config variable name '{key_part}'"),
        ));
    }
    let val_raw = &line[eq_idx + 1..];
    let val = parse_config_value(val_raw, source_path)?;
    Ok((key_part.to_ascii_lowercase(), Some(val)))
}

fn parse_config_value(raw: &str, source_path: Option<&Path>) -> Result<String> {
    let trimmed = raw.trim_start();
    let mut out = String::new();
    let mut trailing_ws = String::new();
    let mut in_quotes = false;
    let mut chars = trimmed.chars().peekable();

    while let Some(ch) = chars.next() {
        if !in_quotes && (ch == '#' || ch == ';') {
            break;
        }
        if ch == '"' {
            if !in_quotes {
                out.push_str(&trailing_ws);
                trailing_ws.clear();
            }
            in_quotes = !in_quotes;
            continue;
        }
        if ch == '\\' {
            let esc = chars.next().ok_or_else(|| {
                invalid_cfg(source_path, format!("dangling escape in git config value: {raw}"))
            })?;
            let decoded = match esc {
                'n' => '\n',
                't' => '\t',
                'b' => '\x08',
                '"' => '"',
                '\\' => '\\',
                other => other,
            };
            if !in_quotes {
                out.push_str(&trailing_ws);
                trailing_ws.clear();
            }
            out.push(decoded);
            continue;
        }
        if !in_quotes && ch.is_ascii_whitespace() {
            trailing_ws.push(ch);
        } else {
            if !in_quotes && !trailing_ws.is_empty() {
                out.push_str(&trailing_ws);
                trailing_ws.clear();
            }
            out.push(ch);
        }
    }

    if in_quotes {
        return Err(invalid_cfg(
            source_path,
            format!("unclosed quote in git config value: {raw}"),
        ));
    }

    Ok(out)
}

fn expand_tilde_with_home(raw: &str, home_dir: Option<&Path>) -> PathBuf {
    if raw == "~"
        && let Some(home) = home_dir
    {
        return home.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home_dir
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

fn resolve_include_path(raw: &str, base_dir: Option<&Path>, home_dir: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde_with_home(raw, home_dir);
    if expanded.is_absolute() {
        expanded
    } else if let Some(base) = base_dir {
        base.join(expanded)
    } else {
        expanded
    }
}

fn canonicalize_with_tail(path: &Path) -> Option<PathBuf> {
    let mut cur = Some(path);
    let mut tail_parts: Vec<&std::ffi::OsStr> = Vec::new();
    while let Some(p) = cur {
        if p.exists() {
            let mut canon = fs::canonicalize(p).ok()?;
            for part in tail_parts.iter().rev() {
                canon.push(part);
            }
            return Some(canon);
        }
        if let Some(name) = p.file_name() {
            tail_parts.push(name);
            cur = p.parent();
        } else {
            break;
        }
    }
    None
}

fn match_gitdir_condition(
    pattern: &str,
    base_dir: Option<&Path>,
    ctx: &GitConfigContext,
    home_dir: Option<&Path>,
    case_insensitive: bool,
) -> bool {
    let raw_pat = normalize_slashes(pattern);
    let mut patterns = Vec::new();

    if raw_pat == "~" {
        if let Some(home) = home_dir {
            patterns.push(normalize_slashes(&home.to_string_lossy()));
            if let Ok(canon) = fs::canonicalize(home) {
                patterns.push(normalize_slashes(&canon.to_string_lossy()));
            }
        }
    } else if let Some(rest) = raw_pat.strip_prefix("~/") {
        if let Some(home) = home_dir {
            let home_norm = normalize_slashes(&home.to_string_lossy());
            patterns.push(format!("{}/{}", home_norm.trim_end_matches('/'), rest));
            if let Ok(canon) = fs::canonicalize(home) {
                let canon_norm = normalize_slashes(&canon.to_string_lossy());
                patterns.push(format!("{}/{}", canon_norm.trim_end_matches('/'), rest));
            }
        }
    } else if let Some(rest) = raw_pat.strip_prefix("./") {
        if let Some(base) = base_dir {
            let base_norm = normalize_slashes(&base.to_string_lossy());
            patterns.push(format!("{}/{}", base_norm.trim_end_matches('/'), rest));
            if let Ok(canon) = fs::canonicalize(base) {
                let canon_norm = normalize_slashes(&canon.to_string_lossy());
                patterns.push(format!("{}/{}", canon_norm.trim_end_matches('/'), rest));
            }
        }
    } else if !raw_pat.starts_with('/')
        && !(raw_pat.len() >= 2
            && raw_pat.as_bytes()[0].is_ascii_alphabetic()
            && raw_pat.as_bytes()[1] == b':')
    {
        patterns.push(format!("**/{raw_pat}"));
    } else {
        patterns.push(raw_pat.clone());
        if !raw_pat.contains(['*', '?', '['])
            && let Some(canon) = canonicalize_with_tail(Path::new(raw_pat.trim_end_matches('/')))
        {
            let mut c = normalize_slashes(&canon.to_string_lossy());
            if raw_pat.ends_with('/') && !c.ends_with('/') {
                c.push('/');
            }
            patterns.push(c);
        }
    }

    if patterns.is_empty() {
        patterns.push(raw_pat);
    }

    let matchers: Vec<globset::GlobMatcher> = patterns
        .into_iter()
        .filter_map(|mut pat| {
            if pat.ends_with('/') {
                pat.push_str("**");
            }
            GlobBuilder::new(&pat)
                .literal_separator(true)
                .case_insensitive(case_insensitive)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        })
        .collect();
    if matchers.is_empty() {
        return false;
    }

    let mut candidates = Vec::new();
    for maybe_dir in [
        ctx.git_dir.as_deref(),
        ctx.git_common_dir.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        candidates.push(normalize_slashes(&maybe_dir.to_string_lossy()));
        if let Ok(canon) = fs::canonicalize(maybe_dir) {
            candidates.push(normalize_slashes(&canon.to_string_lossy()));
        }
    }
    if let Some(wd) = ctx.workdir_path.as_deref() {
        let dot_git = wd.join(".git");
        candidates.push(normalize_slashes(&dot_git.to_string_lossy()));
        if let Ok(canon) = fs::canonicalize(wd) {
            candidates.push(normalize_slashes(&canon.join(".git").to_string_lossy()));
        }
    }

    candidates
        .iter()
        .any(|c| matchers.iter().any(|m| m.is_match(c)))
}

fn match_onbranch_condition(pattern: &str, branch: Option<&str>) -> bool {
    let Some(raw_branch) = branch else {
        return false;
    };
    let short = raw_branch.strip_prefix("refs/heads/").unwrap_or(raw_branch);
    let mut pat = pattern.to_string();
    if pat.ends_with('/') {
        pat.push_str("**");
    }
    let Ok(glob) = GlobBuilder::new(&pat).literal_separator(true).build() else {
        return false;
    };
    glob.compile_matcher().is_match(short)
}

fn normalize_slashes(s: &str) -> String {
    let norm = s.replace('\\', "/");
    norm.strip_prefix("//?/").unwrap_or(&norm).to_string()
}

fn parse_git_config_parameters(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        while idx < chars.len() && chars[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= chars.len() {
            break;
        }
        let mut token = String::new();
        if chars[idx] == '\'' {
            idx += 1;
            while idx < chars.len() {
                if chars[idx] == '\'' {
                    if idx + 3 < chars.len()
                        && chars[idx + 1] == '\\'
                        && chars[idx + 2] == '\''
                        && chars[idx + 3] == '\''
                    {
                        token.push('\'');
                        idx += 4;
                        continue;
                    }
                    idx += 1;
                    break;
                }
                token.push(chars[idx]);
                idx += 1;
            }
            out.push(token);
        } else {
            while idx < chars.len() && !chars[idx].is_ascii_whitespace() {
                token.push(chars[idx]);
                idx += 1;
            }
            out.push(token);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_sections_quotes_escapes_and_implicit_booleans() {
        let raw = r#"
# Top comment
[core]
    autocrlf = input ; inline comment
    filemode
[filter "lfs"]
    clean = "git-lfs clean -- %f"
    smudge = git-lfs\ smudge\ --\ %f
    required = true
[user]
    name = "Ada \"Lovelace\""
    email = ada@example.com
"#;
        let cfg = GitConfig::parse_str(raw, None, &GitConfigContext::default()).unwrap();
        assert_eq!(cfg.autocrlf(), AutoCrlf::Input);
        assert_eq!(cfg.get_bool("core", None, "filemode"), Some(true));
        let lfs = cfg.filter_driver("lfs").unwrap();
        assert_eq!(lfs.clean.as_deref(), Some("git-lfs clean -- %f"));
        assert_eq!(lfs.smudge.as_deref(), Some("git-lfs smudge -- %f"));
        assert!(lfs.required);
        assert_eq!(cfg.user_name(), Some(r#"Ada "Lovelace""#));
        assert_eq!(cfg.user_email(), Some("ada@example.com"));
    }

    #[test]
    fn include_if_gitdir_case_sensitivity_and_onbranch() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let work_repo = home.join("Work").join("service");
        let git_dir = work_repo.join(".git");
        fs::create_dir_all(&git_dir).unwrap();

        let inc_work = tmp.path().join("work.gitconfig");
        fs::write(&inc_work, "[user]\n\temail = work@corp.example\n").unwrap();
        let inc_ci = tmp.path().join("work_ci.gitconfig");
        fs::write(&inc_ci, "[user]\n\tname = Corp Bot\n").unwrap();
        let inc_main = tmp.path().join("branch_release.gitconfig");
        fs::write(&inc_main, "[commit]\n\tgpgsign = true\n").unwrap();

        let main_cfg = format!(
            "[user]\n\temail = personal@example.com\n\tname = Personal\n\
             [includeIf \"gitdir:~/work/\"]\n\tpath = {}\n\
             [includeIf \"gitdir/i:~/work/\"]\n\tpath = {}\n\
             [includeIf \"onbranch:release/\"]\n\tpath = {}\n",
            inc_work.display(),
            inc_ci.display(),
            inc_main.display(),
        );
        let global_cfg = tmp.path().join(".gitconfig");
        fs::write(&global_cfg, main_cfg).unwrap();

        let ctx = GitConfigContext {
            git_dir: Some(git_dir),
            git_common_dir: Some(work_repo.join(".git")),
            workdir_path: Some(work_repo),
            branch: Some("release/v1".to_string()),
            home_dir: Some(home),
            global_config: Some(global_cfg),
            no_system: Some(true),
            ignore_env_configs: true,
            ..Default::default()
        };

        let cfg = GitConfig::load_with_context(&ctx).unwrap();
        // Case-sensitive gitdir:~/work/ should NOT match ~/Work/service/.git
        assert_eq!(cfg.user_email(), Some("personal@example.com"));
        // Case-insensitive gitdir/i:~/work/ MUST match ~/Work/service/.git
        assert_eq!(cfg.user_name(), Some("Corp Bot"));
        // onbranch:release/ MUST match release/v1
        assert_eq!(cfg.commit_gpgsign(), Some(true));
    }

    #[test]
    fn url_insteadof_and_pushinsteadof_longest_match_precedence() {
        let raw = r#"
[url "https://git.internal/"]
    insteadOf = "gh:"
[url "https://git.internal/mirror/"]
    insteadOf = "gh:org/"
[url "ssh://git@git.internal/"]
    pushInsteadOf = "gh:"
[remote "origin"]
    url = "gh:org/app.git"
[remote "custom"]
    url = "gh:org/app.git"
    pushurl = "gh:custom/repo.git"
"#;
        let cfg = GitConfig::parse_str(raw, None, &GitConfigContext::default()).unwrap();

        // Longest matching insteadOf wins on fetch
        let origin = cfg.resolve_remote_urls("origin");
        assert_eq!(
            origin.fetch_url.as_deref(),
            Some("https://git.internal/mirror/app.git")
        );
        // Without pushurl, pushInsteadOf applies on push
        assert_eq!(
            origin.push_url.as_deref(),
            Some("ssh://git@git.internal/org/app.git")
        );

        // With explicit pushurl, pushInsteadOf does NOT apply, insteadOf DOES apply
        let custom = cfg.resolve_remote_urls("custom");
        assert_eq!(
            custom.push_url.as_deref(),
            Some("https://git.internal/custom/repo.git")
        );
    }
}
