// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Rust/Cargo `target/` artifact classification for safe warm baseline capture and
//! read-only (`0444`/`0555`) CAS hardlink materialization.
//!
//! Cargo, `rustc`, `rustdoc`, build scripts, and linkers (`ld`/`lld`/`mold`) open many files
//! inside `target/` with `O_WRONLY | O_TRUNC` in place or explicitly assert writability before
//! emitting outputs (`rustc` `check_file_is_writeable`). Meanwhile, third-party registry
//! dependency artifacts under `deps/` (`libserde-*.rlib`, etc.) account for the vast majority
//! of `target/` disk usage and remain read-only across workspace edits.
//!
//! [`CargoTargetClassifier`] separates:
//! - [`CargoArtifactDisposition::SkipTransientLock`]: advisory lock files never copied/linked.
//! - [`CargoArtifactDisposition::MutableWritable`]: Cargo metadata, fingerprints, incremental
//!   caches, dep-info (`.d`), build-script outputs, rustdoc outputs, top-level uplifted targets,
//!   and local workspace / in-repo crate compilation outputs (copied writable with unique inodes).
//! - [`CargoArtifactDisposition::ImmutableCas`]: external third-party dependency artifacts
//!   (safe to hardlink in baselines and `0444`/`0555` CAS-hardlink across workspaces).

use crate::is_cargo_lock_filename;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// How a file inside a Cargo `target/` directory should be handled during warm baseline
/// capture and CAS-hardlink workspace materialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CargoArtifactDisposition {
    /// Transient advisory lock file (`.cargo-lock`, `incremental/**/*.lock`, `doc/.lock`).
    SkipTransientLock,
    /// Mutable Cargo metadata or local workspace crate artifact that MUST be copied writable
    /// with a unique inode (never hardlinked to `repo_root/target` or `0444`/`0555` CAS blobs).
    MutableWritable,
    /// Immutable external dependency artifact safe to share via `0444`/`0555` CAS hardlinks.
    ImmutableCas,
}

/// Classifies files under a Cargo target directory (`target/` or custom `target-dir`) into
/// transient locks, mutable workspace/metadata files, and immutable third-party CAS artifacts.
#[derive(Debug, Clone, Default)]
pub struct CargoTargetClassifier {
    /// Workspace package names (both kebab-case and snake_case, e.g. `"oz-cli"` and `"oz_cli"`).
    workspace_packages: HashSet<String>,
    /// Workspace package + target names (`[lib]`, `[[bin]]`, `[[test]]`, `[[example]]`, `[[bench]]`)
    /// normalized with `-` -> `_` for matching unhashed/hashed `deps/` filenames.
    workspace_targets: HashSet<String>,
    /// 16-hex Cargo unit hashes belonging to workspace packages or in-repo path crates,
    /// discovered from `.fingerprint/<pkg>-<hash>/`, `deps/*.d` / `examples/*.d`, and
    /// Cargo 1.100+ `build/<pkg>/<hash>/out/*.d`.
    local_unit_hashes: HashSet<String>,
    /// Package directory names discovered as repo-local via Cargo 1.100+ `build/<pkg>/<hash>/out/*.d`
    /// (covering all units — lib, build-script compile, and build-script run — of in-repo path
    /// crates not listed in `[workspace].members`).
    local_packages: HashSet<String>,
    /// 16-hex unit hashes in Cargo 1.100+ `build/<pkg>/<hash>/` layout that represent build-script
    /// execution units (identified by a sibling `run/` directory or `fingerprint/run-build-script-*`).
    /// Their `out/` directory is `OUT_DIR` (mutable) rather than compiled `.rlib`/`.rmeta` artifacts.
    build_script_run_hashes: HashSet<String>,
}

impl CargoTargetClassifier {
    /// Builds a classifier for `target_dir` by inspecting workspace manifests under `manifest_roots`
    /// (typically `[baseline_root, repo_root]`) and scanning `.fingerprint/` + `deps/*.d` inside
    /// `target_dir`.
    #[must_use]
    pub fn for_target_dir(manifest_roots: &[&Path], target_dir: &Path, target_rel: &str) -> Self {
        let mut classifier = Self::default();
        for root in manifest_roots {
            classifier.collect_workspace_manifests(root);
        }
        classifier.scan_target_dir(manifest_roots, target_dir, target_rel);
        classifier
    }

    /// Classifies `rel_in_target` (path relative to the Cargo target root, e.g.
    /// `debug/deps/libfoo-0123456789abcdef.rlib` or `.rustc_info.json`).
    #[must_use]
    pub fn classify(&self, rel_in_target: &Path) -> CargoArtifactDisposition {
        let comps: Vec<&str> = rel_in_target
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();
        if comps.is_empty() {
            return CargoArtifactDisposition::ImmutableCas;
        }
        let file_name = comps[comps.len() - 1];

        if is_transient_cargo_lock(&comps, file_name) {
            return CargoArtifactDisposition::SkipTransientLock;
        }

        // Top-level Cargo target metadata files (`.rustc_info.json`, `.rustdoc_fingerprint.json`,
        // `CACHEDIR.TAG`).
        if comps.len() == 1
            && matches!(
                file_name,
                ".rustc_info.json" | ".rustdoc_fingerprint.json" | "CACHEDIR.TAG"
            )
        {
            return CargoArtifactDisposition::MutableWritable;
        }

        // Cargo fingerprints, incremental compilation sessions, rustdoc outputs, examples,
        // `target/tmp` integration-test scratch, and all `.d` dep-info files are opened in-place
        // (`O_WRONLY | O_TRUNC`) or checked for writability by Cargo/rustc/rustdoc.
        if file_name.ends_with(".d")
            || comps.contains(&".fingerprint")
            || comps.contains(&"incremental")
            || comps.contains(&"doc")
            || comps.contains(&"examples")
            || comps.first() == Some(&"tmp")
        {
            return CargoArtifactDisposition::MutableWritable;
        }

        // `build/...`: handles both Cargo 1.100+ `build-dir-new-layout`
        // (`build/<pkg>/<hex16>/{fingerprint,run,out}/...`) and legacy Cargo
        // (`build/<pkg>-<hex16>/{invoked.timestamp,output,root-output,stderr,out,build-script-*}`).
        if let Some(build_pos) = comps.iter().position(|c| *c == "build") {
            let next1 = comps.get(build_pos + 1).copied();
            let next2 = comps.get(build_pos + 2).copied();
            let next3 = comps.get(build_pos + 3).copied();

            // Cargo 1.100+ `build-dir-new-layout`: `build/<pkg>/<hex16>/...`.
            if let (Some(pkg), Some(unit_hash)) = (next1, next2)
                && is_hex_16(unit_hash)
            {
                if matches!(next3, Some("fingerprint" | "run")) {
                    return CargoArtifactDisposition::MutableWritable;
                }
                if self.is_workspace_package(pkg)
                    || self.local_packages.contains(pkg)
                    || self.local_unit_hashes.contains(unit_hash)
                {
                    return CargoArtifactDisposition::MutableWritable;
                }
                if next3 == Some("out") {
                    if self.build_script_run_hashes.contains(unit_hash) {
                        return CargoArtifactDisposition::MutableWritable;
                    }
                    for comp in &comps[build_pos + 4..] {
                        if self.is_local_deps_entry(comp) {
                            return CargoArtifactDisposition::MutableWritable;
                        }
                    }
                    return CargoArtifactDisposition::ImmutableCas;
                }
                return CargoArtifactDisposition::MutableWritable;
            }

            // Legacy layout: `build/<pkg>-<hex16>/...`.
            if matches!(
                next2,
                Some("invoked.timestamp" | "output" | "root-output" | "stderr" | "out")
            ) {
                return CargoArtifactDisposition::MutableWritable;
            }
            if next1.is_some_and(|dir| self.is_local_pkg_hash_dir(dir)) {
                return CargoArtifactDisposition::MutableWritable;
            }
            return CargoArtifactDisposition::ImmutableCas;
        }

        // `deps/<artifact>`: local workspace / in-repo crate outputs must be writable because
        // `rustc` checks `is_writeable` before overwriting `.rlib`/`.rmeta`/`.so`/binaries and
        // linkers (`ld`/`lld`/`mold`) overwrite executables in place. External registry crates
        // stay `0444`/`0555` CAS hardlinks.
        if let Some(deps_pos) = comps.iter().position(|c| *c == "deps") {
            for comp in &comps[deps_pos + 1..] {
                if self.is_local_deps_entry(comp) {
                    return CargoArtifactDisposition::MutableWritable;
                }
            }
            return CargoArtifactDisposition::ImmutableCas;
        }

        // Direct top-level uplifted profile artifacts (`target/<profile>/<bin_or_lib>` or
        // `target/<triple>/<profile>/<bin_or_lib>`) and top-level macOS `.dSYM` bundles: Cargo
        // only uplifts workspace crates here.
        if comps.len() == 2
            || (comps.len() == 3 && comps[0].contains('-'))
            || comps.iter().any(|c| c.ends_with(".dSYM"))
        {
            return CargoArtifactDisposition::MutableWritable;
        }

        CargoArtifactDisposition::ImmutableCas
    }

    fn is_workspace_package(&self, pkg: &str) -> bool {
        self.workspace_packages.contains(pkg)
            || self.workspace_packages.contains(&pkg.replace('-', "_"))
    }

    fn is_local_pkg_hash_dir(&self, dir_name: &str) -> bool {
        if let Some((pkg, hash)) = split_name_and_hex16(dir_name) {
            return self.local_unit_hashes.contains(hash) || self.is_workspace_package(pkg);
        }
        self.is_workspace_package(dir_name)
    }

    fn is_local_deps_entry(&self, entry_name: &str) -> bool {
        if let Some(hash) = extract_cargo_unit_hash(entry_name)
            && self.local_unit_hashes.contains(hash)
        {
            return true;
        }

        let (raw_stem, stripped_stem) = normalize_deps_crate_stems(entry_name);
        (!raw_stem.is_empty() && self.workspace_targets.contains(raw_stem))
            || (!stripped_stem.is_empty() && self.workspace_targets.contains(stripped_stem))
    }

    fn collect_workspace_manifests(&mut self, root: &Path) {
        let root_manifest = root.join("Cargo.toml");
        if !root_manifest.is_file() {
            return;
        }
        let Ok(raw) = std::fs::read_to_string(&root_manifest) else {
            return;
        };
        let parsed = parse_cargo_toml_metadata(&raw);
        self.record_manifest_metadata(&parsed);

        let exclude_patterns = parsed.workspace_exclude;
        let mut member_patterns = parsed.workspace_members;
        if member_patterns.is_empty() {
            member_patterns = parsed.workspace_default_members;
        }
        if member_patterns.is_empty() {
            return;
        }

        for member_dir in expand_workspace_members(root, &member_patterns, &exclude_patterns) {
            let member_manifest = member_dir.join("Cargo.toml");
            if member_manifest == root_manifest || !member_manifest.is_file() {
                continue;
            }
            if let Ok(member_raw) = std::fs::read_to_string(&member_manifest) {
                let member_parsed = parse_cargo_toml_metadata(&member_raw);
                self.record_manifest_metadata(&member_parsed);
            }
        }
    }

    fn record_manifest_metadata(&mut self, parsed: &ParsedCargoManifest) {
        if let Some(ref pkg) = parsed.package_name {
            self.workspace_packages.insert(pkg.clone());
            let snake = pkg.replace('-', "_");
            self.workspace_packages.insert(snake.clone());
            self.workspace_targets.insert(snake);
        }
        for target in &parsed.target_names {
            self.workspace_targets.insert(target.replace('-', "_"));
        }
    }

    fn scan_target_dir(&mut self, manifest_roots: &[&Path], target_dir: &Path, target_rel: &str) {
        if !target_dir.is_dir() {
            return;
        }

        let target_rel_norm = target_rel.replace('\\', "/").trim_matches('/').to_string();
        let mut canonical_roots: Vec<String> = Vec::new();
        let mut target_prefixes: Vec<String> = Vec::new();
        push_normalized_dir_prefix(&mut target_prefixes, target_dir);
        if let Ok(canon) = std::fs::canonicalize(target_dir) {
            push_normalized_dir_prefix(&mut target_prefixes, &canon);
        }
        for root in manifest_roots {
            push_normalized_dir_prefix(&mut canonical_roots, root);
            let canon_root = std::fs::canonicalize(root).ok();
            if let Some(ref canon) = canon_root {
                push_normalized_dir_prefix(&mut canonical_roots, canon);
            }
            if !target_rel_norm.is_empty() {
                let root_target = root.join(&target_rel_norm);
                push_normalized_dir_prefix(&mut target_prefixes, &root_target);
                if let Ok(canon_target) = std::fs::canonicalize(&root_target) {
                    push_normalized_dir_prefix(&mut target_prefixes, &canon_target);
                }
                if let Some(ref canon) = canon_root {
                    push_normalized_dir_prefix(&mut target_prefixes, &canon.join(&target_rel_norm));
                }
            }
        }

        let profile_dirs = discover_cargo_profile_dirs(target_dir);
        for profile_dir in profile_dirs {
            // 1. Collect all unit hashes for known workspace packages from `.fingerprint/<pkg>-<hash>/`.
            let fp_dir = profile_dir.join(".fingerprint");
            if let Ok(entries) = std::fs::read_dir(&fp_dir) {
                for entry in entries.flatten() {
                    let name_os = entry.file_name();
                    let Some(name) = name_os.to_str() else {
                        continue;
                    };
                    if let Some((pkg, hash)) = split_name_and_hex16(name)
                        && (self.workspace_packages.contains(pkg)
                            || self.workspace_packages.contains(&pkg.replace('-', "_")))
                    {
                        self.local_unit_hashes.insert(hash.to_string());
                    }
                }
            }

            // 2. Scan `deps/*.d` and `examples/*.d` for repo-local path crates / custom targets
            // whose source paths are relative to the repository (or reside inside `repo_root`
            // outside `target/`), catching path dependencies not listed in `[workspace].members`.
            for sub in ["deps", "examples"] {
                let sub_dir = profile_dir.join(sub);
                let Ok(entries) = std::fs::read_dir(&sub_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name_os = entry.file_name();
                    let Some(name) = name_os.to_str() else {
                        continue;
                    };
                    let Some(stem) = name.strip_suffix(".d") else {
                        continue;
                    };
                    let Some(hash) = extract_cargo_unit_hash(stem) else {
                        continue;
                    };
                    if self.local_unit_hashes.contains(hash) {
                        continue;
                    }
                    if dep_info_references_local_source(
                        &entry.path(),
                        &canonical_roots,
                        &target_prefixes,
                        &target_rel_norm,
                    ) {
                        self.local_unit_hashes.insert(hash.to_string());
                    }
                }
            }

            // 3. Scan Cargo 1.100+ `build-dir-new-layout` units at `build/<pkg>/<hex16>/`:
            //    - Workspace packages get their unit hashes recorded as local.
            //    - Build-script execution units (`run/` dir or `fingerprint/run-build-script-*`)
            //      have their `out/` directory marked as mutable `OUT_DIR`.
            //    - In-repo path crates not listed in `[workspace].members` are detected from
            //      `out/*.d` referencing local repo sources outside `target/`.
            let build_dir = profile_dir.join("build");
            if let Ok(pkg_entries) = std::fs::read_dir(&build_dir) {
                for pkg_entry in pkg_entries.flatten() {
                    let pkg_path = pkg_entry.path();
                    if !pkg_path.is_dir() {
                        continue;
                    }
                    let pkg_name_os = pkg_entry.file_name();
                    let Some(pkg_name) = pkg_name_os.to_str() else {
                        continue;
                    };
                    // Legacy `build/<pkg>-<hex16>/` dirs are not new-layout package roots.
                    if split_name_and_hex16(pkg_name).is_some() {
                        continue;
                    }
                    let Ok(unit_entries) = std::fs::read_dir(&pkg_path) else {
                        continue;
                    };
                    let is_ws_pkg = self.is_workspace_package(pkg_name);
                    for unit_entry in unit_entries.flatten() {
                        let unit_path = unit_entry.path();
                        if !unit_path.is_dir() {
                            continue;
                        }
                        let unit_hash_os = unit_entry.file_name();
                        let Some(unit_hash) = unit_hash_os.to_str() else {
                            continue;
                        };
                        if !is_hex_16(unit_hash) {
                            continue;
                        }
                        if is_ws_pkg {
                            self.local_unit_hashes.insert(unit_hash.to_string());
                        }
                        let is_bs_run = unit_path.join("run").is_dir()
                            || std::fs::read_dir(unit_path.join("fingerprint"))
                                .ok()
                                .is_some_and(|rd| {
                                    rd.flatten().any(|f| {
                                        f.file_name()
                                            .to_str()
                                            .is_some_and(|n| n.starts_with("run-build-script-"))
                                    })
                                });
                        if is_bs_run {
                            self.build_script_run_hashes.insert(unit_hash.to_string());
                        }
                        if !is_ws_pkg
                            && let Ok(out_entries) = std::fs::read_dir(unit_path.join("out"))
                        {
                            for out_entry in out_entries.flatten() {
                                let out_name_os = out_entry.file_name();
                                let Some(out_name) = out_name_os.to_str() else {
                                    continue;
                                };
                                if !out_name.ends_with(".d") {
                                    continue;
                                }
                                if dep_info_references_local_source(
                                    &out_entry.path(),
                                    &canonical_roots,
                                    &target_prefixes,
                                    &target_rel_norm,
                                ) {
                                    self.local_unit_hashes.insert(unit_hash.to_string());
                                    self.local_packages.insert(pkg_name.to_string());
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Returns `true` when `rel_in_target` is a transient Cargo advisory lock file.
#[must_use]
pub fn is_transient_cargo_lock_path(rel_in_target: &Path) -> bool {
    let comps: Vec<&str> = rel_in_target
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let Some(file_name) = comps.last().copied() else {
        return false;
    };
    is_transient_cargo_lock(&comps, file_name)
}

fn is_transient_cargo_lock(comps: &[&str], file_name: &str) -> bool {
    if is_cargo_lock_filename(file_name) || file_name == ".oz-lock" || file_name.ends_with(".flock")
    {
        return true;
    }
    if comps.contains(&"incremental") && (file_name == "lock" || file_name.ends_with(".lock")) {
        return true;
    }
    comps.contains(&"doc") && file_name == ".lock"
}

fn discover_cargo_profile_dirs(target_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(level1) = std::fs::read_dir(target_dir) else {
        return out;
    };
    for entry in level1.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "doc" || name_str == "tmp" {
            continue;
        }
        if is_profile_dir(&path) {
            out.push(path);
        } else if let Ok(level2) = std::fs::read_dir(&path) {
            // Target triple directory such as `target/x86_64-unknown-linux-gnu/debug/`.
            for sub in level2.flatten() {
                let sub_path = sub.path();
                if sub_path.is_dir() && is_profile_dir(&sub_path) {
                    out.push(sub_path);
                }
            }
        }
    }
    out
}

fn is_profile_dir(dir: &Path) -> bool {
    dir.join(".fingerprint").is_dir()
        || dir.join("deps").is_dir()
        || dir.join("build").is_dir()
        || dir.join("incremental").is_dir()
}

fn push_normalized_dir_prefix(prefixes: &mut Vec<String>, dir: &Path) {
    let mut s = dir.to_string_lossy().replace('\\', "/");
    while s.ends_with('/') && s.len() > 1 {
        s.pop();
    }
    if !s.is_empty() && !prefixes.iter().any(|existing| existing == &s) {
        prefixes.push(s);
    }
}

fn dep_info_references_local_source(
    dep_file: &Path,
    repo_prefixes: &[String],
    target_prefixes: &[String],
    target_rel_norm: &str,
) -> bool {
    let Ok(file) = File::open(dep_file) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).unwrap_or(0) == 0 {
        return false;
    }
    let Some((_, rhs)) = first_line.split_once(": ") else {
        return false;
    };
    for token in split_makefile_dep_tokens(rhs) {
        let norm = token.replace('\\', "/");
        if norm.is_empty() {
            continue;
        }
        if is_under_any_prefix(&norm, target_prefixes) {
            continue;
        }
        if is_absolute_posix_or_windows(&norm) {
            if is_under_any_prefix(&norm, repo_prefixes) {
                return true;
            }
        } else {
            let trimmed = norm.trim_start_matches("./");
            if !target_rel_norm.is_empty()
                && (trimmed == target_rel_norm
                    || trimmed
                        .strip_prefix(target_rel_norm)
                        .is_some_and(|rest| rest.starts_with('/')))
            {
                continue;
            }
            return true;
        }
    }
    false
}

fn split_makefile_dep_tokens(rhs: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut chars = rhs.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&' ') {
            cur.push(' ');
            let _ = chars.next();
        } else if ch.is_ascii_whitespace() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn is_absolute_posix_or_windows(norm: &str) -> bool {
    if norm.starts_with('/') {
        return true;
    }
    let bytes = norm.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn is_under_any_prefix(norm_path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|pref| {
        norm_path == pref
            || norm_path
                .strip_prefix(pref.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn is_hex_16(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn split_name_and_hex16(s: &str) -> Option<(&str, &str)> {
    let (prefix, suffix) = s.rsplit_once('-')?;
    if !prefix.is_empty() && is_hex_16(suffix) {
        Some((prefix, suffix))
    } else {
        None
    }
}

/// Extracts the 16-hex Cargo unit hash from a `deps/` or `examples/` filename or `.dSYM` directory
/// name (e.g. `libapp-46231555f40825ba.rlib` -> `Some("46231555f40825ba")`).
fn extract_cargo_unit_hash(name: &str) -> Option<&str> {
    let bytes = name.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == b'-' && i > 0 && i + 17 <= bytes.len() {
            let cand = &name[i + 1..i + 17];
            let next_ok = i + 17 == bytes.len() || bytes[i + 17] == b'.';
            if next_ok && is_hex_16(cand) {
                return Some(cand);
            }
        }
    }
    None
}

fn normalize_deps_crate_stems(entry_name: &str) -> (&str, &str) {
    let base = entry_name.split('.').next().unwrap_or(entry_name);
    let without_hash = split_name_and_hex16(base).map_or(base, |(prefix, _)| prefix);
    let stripped = without_hash.strip_prefix("lib").unwrap_or(without_hash);
    (without_hash, stripped)
}

#[derive(Debug, Default)]
struct ParsedCargoManifest {
    package_name: Option<String>,
    target_names: Vec<String>,
    workspace_members: Vec<String>,
    workspace_default_members: Vec<String>,
    workspace_exclude: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkspaceArrayField {
    Members,
    DefaultMembers,
    Exclude,
}

fn parse_cargo_toml_metadata(raw: &str) -> ParsedCargoManifest {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        Other,
        Package,
        Target,
        Workspace,
    }

    let mut out = ParsedCargoManifest::default();
    let mut section = Section::Other;
    let mut pending_array: Option<(WorkspaceArrayField, String)> = None;

    for raw_line in raw.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if let Some((field, mut buf)) = pending_array.take() {
            buf.push(' ');
            buf.push_str(line);
            if buf.contains(']') {
                let items = parse_toml_string_array(&buf);
                assign_workspace_array(&mut out, field, items);
            } else {
                pending_array = Some((field, buf));
            }
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            let header = line.trim_matches(['[', ']']).trim();
            section = match header {
                "package" => Section::Package,
                "workspace" => Section::Workspace,
                "lib" | "bin" | "test" | "example" | "bench" => Section::Target,
                _ => Section::Other,
            };
            continue;
        }

        let Some((key_raw, val_raw)) = line.split_once('=') else {
            continue;
        };
        let key = key_raw.trim();
        let val = val_raw.trim();

        match section {
            Section::Package if key == "name" => {
                if let Some(s) = parse_toml_string(val) {
                    out.package_name = Some(s);
                }
            }
            Section::Target if key == "name" => {
                if let Some(s) = parse_toml_string(val) {
                    out.target_names.push(s);
                }
            }
            Section::Workspace => {
                let field = match key {
                    "members" => Some(WorkspaceArrayField::Members),
                    "default-members" => Some(WorkspaceArrayField::DefaultMembers),
                    "exclude" => Some(WorkspaceArrayField::Exclude),
                    _ => None,
                };
                if let Some(f) = field
                    && val.starts_with('[')
                {
                    if val.contains(']') {
                        assign_workspace_array(&mut out, f, parse_toml_string_array(val));
                    } else {
                        pending_array = Some((f, val.to_string()));
                    }
                }
            }
            _ => {}
        }
    }

    out
}

fn assign_workspace_array(
    out: &mut ParsedCargoManifest,
    field: WorkspaceArrayField,
    items: Vec<String>,
) {
    match field {
        WorkspaceArrayField::Members => out.workspace_members = items,
        WorkspaceArrayField::DefaultMembers => out.workspace_default_members = items,
        WorkspaceArrayField::Exclude => out.workspace_exclude = items,
    }
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_double {
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        match ch {
            '"' => in_double = true,
            '\'' => in_single = true,
            '#' => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn parse_toml_string(val: &str) -> Option<String> {
    let trimmed = val.trim();
    if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(inner.to_string());
    }
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return Some(inner.to_string());
    }
    None
}

fn parse_toml_string_array(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(start) = raw.find('[') else {
        return out;
    };
    let Some(end) = raw.rfind(']') else {
        return out;
    };
    let inner = &raw[start + 1..end];
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' || ch == '\'' {
            let quote = ch;
            let mut item = String::new();
            let mut escaped = false;
            for c in chars.by_ref() {
                if escaped {
                    item.push(c);
                    escaped = false;
                    continue;
                }
                if quote == '"' && c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == quote {
                    break;
                }
                item.push(c);
            }
            if !item.is_empty() {
                out.push(item);
            }
        }
    }
    out
}

fn expand_workspace_members(root: &Path, members: &[String], excludes: &[String]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();

    for raw_pat in members {
        let pat = raw_pat
            .replace('\\', "/")
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string();
        if pat.is_empty() {
            continue;
        }
        if !has_glob_meta(&pat) {
            if !matches_any_glob(&pat, excludes) {
                let candidate = root.join(&pat);
                if candidate.is_dir() && seen.insert(candidate.clone()) {
                    dirs.push(candidate);
                }
            }
            continue;
        }

        let max_depth = if pat.contains("**") {
            16
        } else {
            pat.split('/').count()
        };
        let walker = WalkDir::new(root)
            .min_depth(1)
            .max_depth(max_depth)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".git" | ".ozone" | "target" | "node_modules" | ".venv"
                )
            });

        for entry in walker.flatten() {
            if !entry.file_type().is_dir() {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            let rel_posix = rel.to_string_lossy().replace('\\', "/");
            if simple_path_glob_match(&pat, &rel_posix)
                && !matches_any_glob(&rel_posix, excludes)
                && seen.insert(entry.path().to_path_buf())
            {
                dirs.push(entry.into_path());
            }
        }
    }

    dirs
}

fn has_glob_meta(pat: &str) -> bool {
    pat.contains('*') || pat.contains('?') || pat.contains('[')
}

fn matches_any_glob(rel_posix: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|raw| {
        let normalized = raw.replace('\\', "/");
        let pat = normalized.trim_start_matches("./").trim_end_matches('/');
        !pat.is_empty() && simple_path_glob_match(pat, rel_posix)
    })
}

fn simple_path_glob_match(pat: &str, text: &str) -> bool {
    let pat_segs: Vec<&str> = pat.split('/').collect();
    let txt_segs: Vec<&str> = text.split('/').collect();
    match_segments(&pat_segs, &txt_segs)
}

fn match_segments(pat: &[&str], txt: &[&str]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            match_segments(&pat[1..], txt) || (!txt.is_empty() && match_segments(pat, &txt[1..]))
        }
        (Some(p), Some(t)) => {
            match_wildcard(p.as_bytes(), t.as_bytes()) && match_segments(&pat[1..], &txt[1..])
        }
        _ => false,
    }
}

fn match_wildcard(pat: &[u8], txt: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn classifies_cargo_metadata_local_crates_and_external_deps() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("ws");
        std::fs::create_dir_all(repo.join("crates/my-app/src")).expect("mkdir app");
        std::fs::write(
            repo.join("Cargo.toml"),
            r#"
[workspace]
members = [
    "crates/*", # workspace crates
]
"#,
        )
        .expect("write root Cargo.toml");
        std::fs::write(
            repo.join("crates/my-app/Cargo.toml"),
            r#"
[package]
name = "my-app"
version = "0.1.0"

[[bin]]
name = "custom-cli"
"#,
        )
        .expect("write member Cargo.toml");

        let target = repo.join("target");
        std::fs::create_dir_all(target.join("debug/.fingerprint/my-app-0123456789abcdef"))
            .expect("mkdir fp local");
        std::fs::create_dir_all(target.join("debug/.fingerprint/serde-fedcba9876543210"))
            .expect("mkdir fp ext");
        std::fs::create_dir_all(target.join("debug/deps")).expect("mkdir deps");
        std::fs::write(
            target.join("debug/deps/local_path_dep-1111222233334444.d"),
            format!(
                "{}: local_path_dep/src/lib.rs\n",
                target
                    .join("debug/deps/local_path_dep-1111222233334444.d")
                    .display()
            ),
        )
        .expect("write local .d");
        std::fs::write(
            target.join("debug/deps/serde-fedcba9876543210.d"),
            format!(
                "{}: /registry/src/serde-1.0/src/lib.rs {}/debug/build/serde-fedcba9876543210/out/gen.rs\n",
                target.join("debug/deps/serde-fedcba9876543210.d").display(),
                target.display()
            ),
        )
        .expect("write ext .d");

        let cls = CargoTargetClassifier::for_target_dir(&[&repo], &target, "target");

        // Transient locks
        assert_eq!(
            cls.classify(Path::new(".cargo-lock")),
            CargoArtifactDisposition::SkipTransientLock
        );
        assert_eq!(
            cls.classify(Path::new("debug/.cargo-build-lock")),
            CargoArtifactDisposition::SkipTransientLock
        );
        assert_eq!(
            cls.classify(Path::new("debug/incremental/my_app/s-1.lock")),
            CargoArtifactDisposition::SkipTransientLock
        );
        assert_eq!(
            cls.classify(Path::new("doc/.lock")),
            CargoArtifactDisposition::SkipTransientLock
        );

        // Mutable Cargo metadata & local workspace artifacts
        for mutable in [
            ".rustc_info.json",
            ".rustdoc_fingerprint.json",
            "CACHEDIR.TAG",
            "debug/.fingerprint/serde-fedcba9876543210/lib-serde",
            "debug/.fingerprint/my-app-0123456789abcdef/lib-my_app.json",
            "debug/incremental/my_app/s-1/dep-graph.bin",
            "debug/deps/serde-fedcba9876543210.d",
            "debug/deps/libmy_app-0123456789abcdef.rlib",
            "debug/deps/libmy_app-0123456789abcdef.rmeta",
            "debug/deps/my_integ-0123456789abcdef",
            "debug/deps/custom_cli-0123456789abcdef",
            "debug/deps/liblocal_path_dep-1111222233334444.rlib",
            "debug/custom-cli",
            "debug/build/serde-fedcba9876543210/invoked.timestamp",
            "debug/build/serde-fedcba9876543210/output",
            "debug/build/serde-fedcba9876543210/out/gen.rs",
            "debug/build/my-app-0123456789abcdef/build-script-build",
            "debug/examples/demo",
            "doc/crates.js",
        ] {
            assert_eq!(
                cls.classify(Path::new(mutable)),
                CargoArtifactDisposition::MutableWritable,
                "expected MutableWritable for {mutable}"
            );
        }

        // External third-party immutable CAS artifacts
        for immutable in [
            "debug/deps/libserde-fedcba9876543210.rlib",
            "debug/deps/libserde-fedcba9876543210.rmeta",
            "debug/deps/libdep.rlib",
            "debug/build/serde-fedcba9876543210/build-script-build",
        ] {
            assert_eq!(
                cls.classify(Path::new(immutable)),
                CargoArtifactDisposition::ImmutableCas,
                "expected ImmutableCas for {immutable}"
            );
        }
    }

    #[test]
    fn external_crate_with_repo_target_out_dir_stays_immutable_cas_when_scanning_baseline() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("repo");
        let baseline_root = repo.join(".git/ozone/baselines/abc123/root");
        let baseline_target = baseline_root.join("target");

        std::fs::create_dir_all(&repo).expect("mkdir repo");
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .expect("write repo Cargo.toml");
        std::fs::create_dir_all(baseline_target.join("debug/deps")).expect("mkdir baseline deps");
        std::fs::write(
            baseline_root.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .expect("write baseline Cargo.toml");

        // External crate whose `.d` file references `OUT_DIR` under `<repo>/target/...` (note
        // `<repo>/target` does not even need to exist on disk anymore!).
        let repo_target_display = repo.join("target").display().to_string();
        std::fs::write(
            baseline_target.join("debug/deps/ext_with_build-fedcba9876543210.d"),
            format!(
                "{repo_target_display}/debug/deps/ext_with_build-fedcba9876543210.d: /registry/src/ext-with-build-1.0/src/lib.rs {repo_target_display}/debug/build/ext-with-build-fedcba9876543210/out/ext_gen.rs\n"
            ),
        )
        .expect("write ext .d");

        // Local path crate whose `.d` file references an absolute source path inside `<repo>/vendor/...`.
        std::fs::write(
            baseline_target.join("debug/deps/local_vendor-1111222233334444.d"),
            format!(
                "{repo_target_display}/debug/deps/local_vendor-1111222233334444.d: {}/vendor/local/src/lib.rs\n",
                repo.display()
            ),
        )
        .expect("write local .d");

        let cls = CargoTargetClassifier::for_target_dir(
            &[&baseline_root, &repo],
            &baseline_target,
            "target",
        );

        assert_eq!(
            cls.classify(Path::new(
                "debug/deps/libext_with_build-fedcba9876543210.rlib"
            )),
            CargoArtifactDisposition::ImmutableCas,
            "external crate referencing <repo>/target/.../out must stay ImmutableCas"
        );
        assert_eq!(
            cls.classify(Path::new(
                "debug/deps/libext_with_build-fedcba9876543210.rmeta"
            )),
            CargoArtifactDisposition::ImmutableCas
        );
        assert_eq!(
            cls.classify(Path::new(
                "debug/deps/liblocal_vendor-1111222233334444.rlib"
            )),
            CargoArtifactDisposition::MutableWritable,
            "local repo source outside target/ must still classify as MutableWritable"
        );
    }

    #[test]
    fn classifies_cargo_1_100_build_dir_new_layout_units() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("ws-new-layout");
        std::fs::create_dir_all(repo.join("crates/my-app/src")).expect("mkdir app");
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .expect("write root Cargo.toml");
        std::fs::write(
            repo.join("crates/my-app/Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .expect("write member Cargo.toml");

        let target = repo.join("target");
        let app_lib_hash = "0123456789abcdef";
        let ext_lib_hash = "fedcba9876543210";
        let ext_bs_hash = "1122334455667788";
        let ext_run_hash = "8877665544332211";
        let vendor_hash = "aaaabbbbccccdddd";

        for dir in [
            target.join(format!("debug/build/my-app/{app_lib_hash}/fingerprint")),
            target.join(format!("debug/build/my-app/{app_lib_hash}/out")),
            target.join(format!("debug/build/ext-dep/{ext_lib_hash}/fingerprint")),
            target.join(format!("debug/build/ext-dep/{ext_lib_hash}/out")),
            target.join(format!("debug/build/ext-dep/{ext_bs_hash}/fingerprint")),
            target.join(format!("debug/build/ext-dep/{ext_bs_hash}/out")),
            target.join(format!("debug/build/ext-dep/{ext_run_hash}/fingerprint")),
            target.join(format!("debug/build/ext-dep/{ext_run_hash}/out")),
            target.join(format!("debug/build/ext-dep/{ext_run_hash}/run")),
            target.join(format!(
                "debug/build/local-vendor/{vendor_hash}/fingerprint"
            )),
            target.join(format!("debug/build/local-vendor/{vendor_hash}/out")),
        ] {
            std::fs::create_dir_all(dir).expect("mkdir unit dir");
        }

        std::fs::write(
            target.join(format!(
                "debug/build/ext-dep/{ext_lib_hash}/out/ext_dep-{ext_lib_hash}.d"
            )),
            format!(
                "{t}/debug/build/ext-dep/{ext_lib_hash}/out/libext_dep-{ext_lib_hash}.rlib: /registry/src/ext-dep-1.0/src/lib.rs {t}/debug/build/ext-dep/{ext_run_hash}/out/ext_gen.rs\n",
                t = target.display()
            ),
        )
        .expect("write ext .d");
        std::fs::write(
            target.join(format!(
                "debug/build/local-vendor/{vendor_hash}/out/local_vendor-{vendor_hash}.d"
            )),
            format!(
                "{t}/debug/build/local-vendor/{vendor_hash}/out/liblocal_vendor-{vendor_hash}.rlib: {r}/vendor/local/src/lib.rs\n",
                t = target.display(),
                r = repo.display()
            ),
        )
        .expect("write local vendor .d");

        let cls = CargoTargetClassifier::for_target_dir(&[&repo], &target, "target");

        for mutable in [
            format!("debug/build/my-app/{app_lib_hash}/fingerprint/lib-my_app"),
            format!("debug/build/my-app/{app_lib_hash}/out/libmy_app-{app_lib_hash}.rlib"),
            format!("debug/build/ext-dep/{ext_lib_hash}/fingerprint/lib-ext_dep"),
            format!("debug/build/ext-dep/{ext_lib_hash}/out/ext_dep-{ext_lib_hash}.d"),
            format!("debug/build/ext-dep/{ext_run_hash}/fingerprint/run-build-script-build"),
            format!("debug/build/ext-dep/{ext_run_hash}/run/invoked.timestamp"),
            format!("debug/build/ext-dep/{ext_run_hash}/run/stdout"),
            format!("debug/build/ext-dep/{ext_run_hash}/out/ext_gen.rs"),
            format!(
                "debug/build/local-vendor/{vendor_hash}/out/liblocal_vendor-{vendor_hash}.rlib"
            ),
        ] {
            assert_eq!(
                cls.classify(Path::new(&mutable)),
                CargoArtifactDisposition::MutableWritable,
                "expected MutableWritable for {mutable}"
            );
        }

        for immutable in [
            format!("debug/build/ext-dep/{ext_lib_hash}/out/libext_dep-{ext_lib_hash}.rlib"),
            format!("debug/build/ext-dep/{ext_lib_hash}/out/libext_dep-{ext_lib_hash}.rmeta"),
            format!("debug/build/ext-dep/{ext_bs_hash}/out/build_script_build"),
        ] {
            assert_eq!(
                cls.classify(Path::new(&immutable)),
                CargoArtifactDisposition::ImmutableCas,
                "expected ImmutableCas for {immutable}"
            );
        }
    }
}
