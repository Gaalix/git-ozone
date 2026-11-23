// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Executable resolution rules for locating the `oz-vfsd` daemon binary.

use oz_core::{OzError, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Environment variable allowing explicit override of the `oz-vfsd` binary path.
pub const OZ_VFSD_BIN_ENV: &str = "OZ_VFSD_BIN";

/// Returns the platform-specific executable filename (`oz-vfsd` or `oz-vfsd.exe`).
#[must_use]
pub const fn vfsd_binary_name() -> &'static str {
    if cfg!(windows) {
        "oz-vfsd.exe"
    } else {
        "oz-vfsd"
    }
}

/// Locates the `oz-vfsd` executable using standard Ozone precedence:
/// 1. `OZ_VFSD_BIN` environment variable
/// 2. Sibling of `std::env::current_exe()`
/// 3. Ancestors of `exe_dir` (handles `target/debug/deps` and `target/debug/build/<pkg>/<hash>/out`)
/// 4. Entries in `PATH`
pub fn find_oz_vfsd_bin() -> Result<PathBuf> {
    let env_override = std::env::var_os(OZ_VFSD_BIN_ENV);
    let current_exe = std::env::current_exe().ok();
    let path_env = std::env::var_os("PATH");
    resolve_oz_vfsd_bin_with(
        env_override.as_deref(),
        current_exe.as_deref(),
        path_env.as_deref(),
    )
}

/// Deterministic helper implementing the 4-stage `oz-vfsd` lookup order.
pub fn resolve_oz_vfsd_bin_with(
    env_override: Option<&OsStr>,
    current_exe: Option<&Path>,
    path_env: Option<&OsStr>,
) -> Result<PathBuf> {
    // 1. Explicit OZ_VFSD_BIN override.
    if let Some(raw_override) = env_override
        && !raw_override.is_empty()
    {
        let p = PathBuf::from(raw_override);
        if p.is_file() {
            return Ok(p);
        }
        return Err(OzError::BinaryNotFound {
            name: "oz-vfsd".to_string(),
            message: format!(
                "{OZ_VFSD_BIN_ENV} is set to '{}', which does not exist or is not a file",
                p.display()
            ),
        });
    }

    let bin_name = vfsd_binary_name();

    if let Some(exe) = current_exe
        && let Some(exe_dir) = exe.parent()
    {
        // 2 & 3. Sibling or recent ancestor of current_exe() (covers target/debug/deps and
        // Cargo's build/<crate>/<hash>/out test layout).
        let mut cursor = Some(exe_dir);
        for _ in 0..=5 {
            let Some(dir) = cursor else {
                break;
            };
            let cand = dir.join(bin_name);
            if cand.is_file() {
                return Ok(cand);
            }
            cursor = dir.parent();
        }
    }

    // 4. Search PATH.
    if let Some(path_var) = path_env {
        for dir in std::env::split_paths(path_var) {
            let cand = dir.join(bin_name);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }

    Err(OzError::BinaryNotFound {
        name: "oz-vfsd".to_string(),
        message: format!(
            "could not locate '{bin_name}' via {OZ_VFSD_BIN_ENV}, current_exe sibling/parent, or PATH"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolves_oz_vfsd_according_to_precedence_rules() {
        let dir = TempDir::new().expect("tempdir");
        let bin_name = vfsd_binary_name();

        // Setup directories:
        // - override_dir/oz-vfsd
        // - target/debug/oz-vfsd
        // - target/debug/deps/test_runner
        // - path_dir/oz-vfsd
        let override_bin = dir.path().join("override-vfsd");
        std::fs::write(&override_bin, b"").expect("write override");

        let debug_dir = dir.path().join("target/debug");
        let deps_dir = debug_dir.join("deps");
        let path_dir = dir.path().join("bin");
        std::fs::create_dir_all(&deps_dir).expect("mkdir deps");
        std::fs::create_dir_all(&path_dir).expect("mkdir path");

        let debug_bin = debug_dir.join(bin_name);
        let deps_exe = deps_dir.join("unit_test_bin");
        let path_bin = path_dir.join(bin_name);
        std::fs::write(&debug_bin, b"").expect("write debug bin");
        std::fs::write(&deps_exe, b"").expect("write deps exe");
        std::fs::write(&path_bin, b"").expect("write path bin");

        // 1. OZ_VFSD_BIN wins over everything else.
        let found1 = resolve_oz_vfsd_bin_with(
            Some(override_bin.as_os_str()),
            Some(&debug_dir.join("oz")),
            Some(path_dir.as_os_str()),
        )
        .expect("rule 1");
        assert_eq!(found1, override_bin);

        // 2. Sibling of current_exe wins over PATH.
        let found2 = resolve_oz_vfsd_bin_with(
            None,
            Some(&debug_dir.join("oz")),
            Some(path_dir.as_os_str()),
        )
        .expect("rule 2");
        assert_eq!(found2, debug_bin);

        // 3. Parent of exe_dir (target/debug/deps -> target/debug/oz-vfsd) wins over PATH.
        let found3 = resolve_oz_vfsd_bin_with(None, Some(&deps_exe), Some(path_dir.as_os_str()))
            .expect("rule 3");
        assert_eq!(found3, debug_bin);

        // 3b. Cargo build-dir ancestor layout (target/debug/build/<crate>/<hash>/out/test_runner).
        let build_out = debug_dir.join("build/oz-vfsd/abc12345/out");
        std::fs::create_dir_all(&build_out).expect("mkdir build out");
        let nested_test = build_out.join("fuse_live_bin");
        std::fs::write(&nested_test, b"").expect("write nested test");
        let found3b =
            resolve_oz_vfsd_bin_with(None, Some(&nested_test), Some(path_dir.as_os_str()))
                .expect("rule 3b");
        assert_eq!(found3b, debug_bin);

        // 4. PATH fallback when current_exe has no sibling or ancestor binary.
        let nowhere = dir.path().join("nowhere/oz");
        let found4 = resolve_oz_vfsd_bin_with(None, Some(&nowhere), Some(path_dir.as_os_str()))
            .expect("rule 4");
        assert_eq!(found4, path_bin);

        // Explicit missing OZ_VFSD_BIN errors immediately.
        let missing = dir.path().join("does-not-exist");
        assert!(resolve_oz_vfsd_bin_with(Some(missing.as_os_str()), Some(&nowhere), None).is_err());
    }
}
