// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ecosystem warm build/dependency cache detection, baseline capture, and `mtime` preservation.

use chrono::{DateTime, Utc};
use oz_core::OzoneConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

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
}

/// Detects ecosystem cache paths (`target/`, `node_modules/`, `.venv/`, and `[warm].paths`).
#[must_use]
pub fn detect_warm_targets(repo_root: &Path, config: &OzoneConfig) -> Vec<WarmCacheTarget> {
    let mut targets = Vec::new();
    if config.warm.auto_detect {
        if repo_root.join("Cargo.toml").is_file() {
            targets.push(WarmCacheTarget {
                relative_path: "target".to_string(),
                ecosystem: EcosystemKind::RustCargo,
                lock_indicators: vec!["target/.cargo-lock".to_string()],
            });
        }
        if repo_root.join("package.json").is_file() {
            for dir in ["node_modules", ".next", ".turbo", ".vite"] {
                if repo_root.join(dir).is_dir() || dir == "node_modules" {
                    targets.push(WarmCacheTarget {
                        relative_path: dir.to_string(),
                        ecosystem: EcosystemKind::NodeJs,
                        lock_indicators: vec!["package-lock.json".to_string()],
                    });
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
