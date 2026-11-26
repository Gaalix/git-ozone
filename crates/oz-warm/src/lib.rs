// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ecosystem warm build/dependency cache detection, baseline capture, and `mtime` preservation.

pub mod baseline;
pub mod cargo;
pub mod mtime;
pub mod quiescent;
pub mod trampoline;

pub use baseline::{GcReport, capture_baseline, gc_baselines, touch_baseline_lru};
pub use cargo::{CargoArtifactDisposition, CargoTargetClassifier, is_transient_cargo_lock_path};
pub use mtime::{MtimeApplyStats, apply_baseline_mtimes, compute_changed_paths_between_commits};
pub use quiescent::{
    BaselineResolution, BaselineSource, LoadedBaseline, find_baseline, find_nearest_prior_baseline,
    is_repo_quiescent, resolve_or_capture_baseline,
};
pub use trampoline::{
    is_venv_bin_script, is_venv_pth_file, parse_python_shebang, rewrite_python_pth_bytes,
    rewrite_python_trampoline_bytes, rewrite_pyvenv_cfg_bytes, rewrite_venv_activate_bytes,
};

use chrono::{DateTime, Utc};
use oz_core::{OzoneConfig, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Component, Path};
use walkdir::WalkDir;

/// Ecosystem cache category automatically detected by `oz-warm`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EcosystemKind {
    RustCargo,
    NodeJs,
    Python,
    Custom(String),
}

/// Detected warm cache directory and its lockfile indicators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarmCacheTarget {
    pub relative_path: String,
    pub ecosystem: EcosystemKind,
    pub lock_indicators: Vec<String>,
}

/// Metadata stored per quiescent baseline in `.git/ozone/baselines/<commit>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineMetadata {
    pub commit_oid: String,
    pub created_at: DateTime<Utc>,
    pub targets: Vec<WarmCacheTarget>,
    pub total_bytes: u64,
    #[serde(default)]
    pub file_mtimes_secs: BTreeMap<String, i64>,
    #[serde(default)]
    pub file_mtimes_nanos: BTreeMap<String, u32>,
}

/// Portability report for a Rust Cargo `target/` directory across relocated CoW workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CargoFingerprintReport {
    pub fingerprints_scanned: usize,
    pub non_portable_rerun_paths: Vec<String>,
    pub non_portable_remap_rustflags: Vec<String>,
    pub portable: bool,
}

/// Returns `true` when `file_name` is a transient Cargo advisory lock file (`target/.cargo-lock`,
/// `.cargo-build-lock`, `.cargo-artifact-lock`).
#[must_use]
pub fn is_cargo_lock_filename(file_name: &str) -> bool {
    matches!(
        file_name,
        ".cargo-lock" | ".cargo-build-lock" | ".cargo-artifact-lock"
    )
}

fn rust_cargo_lock_indicators(target_rel: &str) -> Vec<String> {
    let base = target_rel.trim_end_matches('/');
    vec![
        format!("{base}/.cargo-lock"),
        format!("{base}/.cargo-build-lock"),
        format!("{base}/.cargo-artifact-lock"),
    ]
}

fn normalize_repo_relative_dir(repo_root: &Path, raw: &Path) -> Option<String> {
    let rel = if raw.is_absolute() {
        let canon_repo =
            std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        raw.strip_prefix(repo_root)
            .ok()
            .or_else(|| raw.strip_prefix(&canon_repo).ok())?
            .to_path_buf()
    } else {
        raw.to_path_buf()
    };

    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(s) => {
                let s_str = s.to_str()?;
                if !s_str.is_empty() {
                    parts.push(s_str.to_string());
                }
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn parse_cargo_config_target_dir(repo_root: &Path) -> Option<String> {
    for rel_cfg in [".cargo/config.toml", ".cargo/config"] {
        let cfg_path = repo_root.join(rel_cfg);
        let Ok(raw) = std::fs::read_to_string(&cfg_path) else {
            continue;
        };
        let mut in_build = false;
        for line in raw.lines() {
            let no_comment = line.split('#').next().unwrap_or("").trim();
            if no_comment.is_empty() {
                continue;
            }
            if no_comment.starts_with('[') && no_comment.ends_with(']') {
                in_build = no_comment.trim_matches(['[', ']']).trim() == "build";
                continue;
            }
            if in_build
                && let Some((key, val)) = no_comment.split_once('=')
                && key.trim() == "target-dir"
            {
                let trimmed = val.trim().trim_matches('"').trim_matches('\'');
                if let Some(rel) = normalize_repo_relative_dir(repo_root, Path::new(trimmed)) {
                    return Some(rel);
                }
            }
        }
    }
    None
}

/// Detects Cargo `target-dir` from `cargo_target_dir_env` or `.cargo/config.toml` when it
/// resolves inside `repo_root`.
#[must_use]
pub fn resolve_cargo_target_dir_rel(
    repo_root: &Path,
    cargo_target_dir_env: Option<&OsStr>,
) -> Option<String> {
    if let Some(env_val) = cargo_target_dir_env
        && !env_val.is_empty()
        && let Some(rel) = normalize_repo_relative_dir(repo_root, Path::new(env_val))
    {
        return Some(rel);
    }
    parse_cargo_config_target_dir(repo_root)
}

/// Detects ecosystem cache paths (`target/`, custom relative `target-dir`, `node_modules/`,
/// `.venv/`, and `[warm].paths`).
#[must_use]
pub fn detect_warm_targets(repo_root: &Path, config: &OzoneConfig) -> Vec<WarmCacheTarget> {
    let env_target = std::env::var_os("CARGO_TARGET_DIR");
    detect_warm_targets_with_cargo_target_dir(repo_root, config, env_target.as_deref())
}

/// Deterministic variant of [`detect_warm_targets`] accepting an explicit `CARGO_TARGET_DIR`.
#[must_use]
pub fn detect_warm_targets_with_cargo_target_dir(
    repo_root: &Path,
    config: &OzoneConfig,
    cargo_target_dir_env: Option<&OsStr>,
) -> Vec<WarmCacheTarget> {
    let mut targets = Vec::new();
    if config.warm.auto_detect {
        if repo_root.join("Cargo.toml").is_file() {
            let custom_target = resolve_cargo_target_dir_rel(repo_root, cargo_target_dir_env);
            let default_exists = repo_root.join("target").is_dir();
            if custom_target.is_none() || default_exists {
                targets.push(WarmCacheTarget {
                    relative_path: "target".to_string(),
                    ecosystem: EcosystemKind::RustCargo,
                    lock_indicators: rust_cargo_lock_indicators("target"),
                });
            }
            if let Some(custom_rel) = custom_target
                && custom_rel != "target"
            {
                targets.push(WarmCacheTarget {
                    relative_path: custom_rel.clone(),
                    ecosystem: EcosystemKind::RustCargo,
                    lock_indicators: rust_cargo_lock_indicators(&custom_rel),
                });
            }
        }
        if repo_root.join("package.json").is_file() {
            for dir in ["node_modules", ".next", ".turbo", ".vite"] {
                if repo_root.join(dir).is_dir() || dir == "node_modules" {
                    targets.push(WarmCacheTarget {
                        relative_path: dir.to_string(),
                        ecosystem: EcosystemKind::NodeJs,
                        lock_indicators: vec![
                            "package-lock.json".to_string(),
                            "pnpm-lock.yaml".to_string(),
                        ],
                    });
                }
            }
            if is_js_monorepo(repo_root) {
                for nested in detect_nested_workspace_node_modules(repo_root) {
                    if !targets.iter().any(|t| t.relative_path == nested) {
                        targets.push(WarmCacheTarget {
                            relative_path: nested,
                            ecosystem: EcosystemKind::NodeJs,
                            lock_indicators: vec!["pnpm-lock.yaml".to_string()],
                        });
                    }
                }
            }
        }
        if repo_root.join("pyproject.toml").is_file()
            || repo_root.join("requirements.txt").is_file()
        {
            for dir in [".venv", ".mypy_cache", ".ruff_cache", ".pytest_cache"] {
                if repo_root.join(dir).is_dir() || dir == ".venv" {
                    targets.push(WarmCacheTarget {
                        relative_path: dir.to_string(),
                        ecosystem: EcosystemKind::Python,
                        lock_indicators: vec!["uv.lock".to_string()],
                    });
                }
            }
        }
    }

    for custom in &config.warm.paths {
        if !targets.iter().any(|t| t.relative_path == *custom) {
            targets.push(WarmCacheTarget {
                relative_path: custom.clone(),
                ecosystem: EcosystemKind::Custom(custom.clone()),
                lock_indicators: Vec::new(),
            });
        }
    }

    targets.retain(|t| !config.warm.exclude_paths.contains(&t.relative_path));
    targets
}

fn is_js_monorepo(repo_root: &Path) -> bool {
    if repo_root.join("pnpm-workspace.yaml").is_file() {
        return true;
    }
    std::fs::read_to_string(repo_root.join("package.json"))
        .ok()
        .is_some_and(|raw| raw.contains("\"workspaces\""))
}

fn detect_nested_workspace_node_modules(repo_root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let walker = WalkDir::new(repo_root)
        .min_depth(1)
        .max_depth(3)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git"
                    | ".ozone"
                    | "node_modules"
                    | "target"
                    | ".venv"
                    | ".next"
                    | ".turbo"
                    | ".vite"
                    | "dist"
                    | "build"
            )
        });

    for entry in walker.flatten() {
        if !entry.file_type().is_dir() {
            continue;
        }
        let dir = entry.path();
        if dir.join("package.json").is_file()
            && dir.join("node_modules").is_dir()
            && let Ok(rel) = dir.join("node_modules").strip_prefix(repo_root)
            && let Some(norm) = normalize_repo_relative_dir(repo_root, rel)
        {
            found.push(norm);
        }
    }
    found.sort();
    found
}

/// Inspects Cargo `.fingerprint/*/*.json` files under `repo_root/target_rel` for path-coupled
/// settings (absolute `cargo:rerun-if-changed` paths or repo-path-coupled `--remap-path-prefix`
/// flags) that invalidate Cargo fingerprints when spawned at `../<repo>.oz/<ws>`.
pub fn check_cargo_fingerprint_portability(
    repo_root: &Path,
    target_rel: &Path,
) -> Result<CargoFingerprintReport> {
    let target_dir = repo_root.join(target_rel);
    if !target_dir.is_dir() {
        return Ok(CargoFingerprintReport {
            portable: true,
            ..CargoFingerprintReport::default()
        });
    }

    let repo_str = repo_root.to_string_lossy().into_owned();
    let repo_canon = std::fs::canonicalize(repo_root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let matches_repo = |s: &str| -> bool {
        (!repo_str.is_empty() && s.contains(&repo_str))
            || repo_canon
                .as_deref()
                .is_some_and(|c| !c.is_empty() && s.contains(c))
    };

    let mut report = CargoFingerprintReport::default();
    for entry in WalkDir::new(&target_dir)
        .max_depth(7)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let comps: Vec<&str> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();
        let in_fingerprint = comps.contains(&".fingerprint")
            || (comps.contains(&"build") && comps.contains(&"fingerprint"));
        if !in_fingerprint {
            continue;
        }
        report.fingerprints_scanned += 1;
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if let Some(flags) = val.get("rustflags").and_then(|v| v.as_array()) {
            for flag in flags {
                if let Some(flag_str) = flag.as_str()
                    && flag_str.contains("--remap-path-prefix")
                    && matches_repo(flag_str)
                    && !report
                        .non_portable_remap_rustflags
                        .iter()
                        .any(|x| x == flag_str)
                {
                    report
                        .non_portable_remap_rustflags
                        .push(flag_str.to_string());
                }
            }
        }
        if let Some(locals) = val.get("local").and_then(|v| v.as_array()) {
            for item in locals {
                if let Some(paths) = item
                    .get("RerunIfChanged")
                    .and_then(|r| r.get("paths"))
                    .and_then(|p| p.as_array())
                {
                    for p_val in paths {
                        if let Some(p_str) = p_val.as_str()
                            && Path::new(p_str).is_absolute()
                            && matches_repo(p_str)
                            && !report.non_portable_rerun_paths.iter().any(|x| x == p_str)
                        {
                            report.non_portable_rerun_paths.push(p_str.to_string());
                        }
                    }
                }
            }
        }
    }

    report.portable = report.non_portable_rerun_paths.is_empty()
        && report.non_portable_remap_rustflags.is_empty();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detects_cargo_and_custom_warm_targets() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n")
            .expect("write Cargo.toml");
        let mut cfg = OzoneConfig::default();
        cfg.warm.paths.push(".cache/custom".to_string());

        let targets = detect_warm_targets(dir.path(), &cfg);
        assert!(targets.iter().any(|t| t.relative_path == "target"));
        assert!(targets.iter().any(|t| t.relative_path == ".cache/custom"));
    }
}
