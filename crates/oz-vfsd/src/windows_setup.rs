// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Windows ProjFS feature enablement (`Client-ProjFS`) and startup script helpers for `oz setup`.

use oz_core::{OzError, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Non-restart PowerShell command required to enable Windows Projected File System (`Client-ProjFS`).
pub const PROJFS_ENABLE_POWERSHELL_CMD: &str =
    "Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart";

/// Resolves the expected path to `ProjectedFSLib.dll` under `%SystemRoot%\System32\`.
#[must_use]
pub fn projected_fs_dll_path() -> PathBuf {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    projected_fs_dll_path_in(Path::new(&system_root))
}

/// Resolves `<system_root>/System32/ProjectedFSLib.dll`.
#[must_use]
pub fn projected_fs_dll_path_in(system_root: &Path) -> PathBuf {
    system_root.join("System32").join("ProjectedFSLib.dll")
}

/// Returns `true` when `ProjectedFSLib.dll` is present under `<system_root>/System32/`.
#[must_use]
pub fn is_windows_projfs_enabled_in(system_root: &Path) -> bool {
    projected_fs_dll_path_in(system_root).is_file()
}

/// Returns `true` when `ProjectedFSLib.dll` is present on the host Windows system.
#[must_use]
pub fn is_windows_projfs_enabled() -> bool {
    projected_fs_dll_path().is_file()
}

/// Generates a Windows batch (`.cmd`) startup script that launches `oz-vfsd.exe` for `git_dir`.
#[must_use]
pub fn generate_windows_startup_cmd(vfsd_bin: &Path, git_dir: &Path) -> String {
    let sock_path = git_dir.join("ozone").join("vfsd.sock");
    let bin_s = sanitize_cmd_arg(vfsd_bin.as_os_str());
    let git_s = sanitize_cmd_arg(git_dir.as_os_str());
    let sock_s = sanitize_cmd_arg(sock_path.as_os_str());
    format!("@echo off\r\n\"{bin_s}\" --git-dir \"{git_s}\" --socket \"{sock_s}\" --foreground\r\n")
}

fn sanitize_cmd_arg(raw: &OsStr) -> String {
    raw.to_string_lossy().replace('"', "")
}

/// Evaluates whether `ProjectedFSLib.dll` exists under `system_root`; if missing, invokes the
/// supplied PowerShell runner with [`PROJFS_ENABLE_POWERSHELL_CMD`].
///
/// Returns:
/// - `Ok(false)` when ProjFS is already enabled (`ProjectedFSLib.dll` exists)
/// - `Ok(true)` when the command enabled `Client-ProjFS`
/// - `Err(OzError::CheckFailed)` with an Administrator recovery command on failure
pub fn enable_windows_projfs_with<F>(system_root: &Path, run_powershell: F) -> Result<bool>
where
    F: FnOnce(&str) -> std::io::Result<std::process::Output>,
{
    if is_windows_projfs_enabled_in(system_root) {
        return Ok(false);
    }

    let recovery = vec![format!(
        "Run in an elevated (Administrator) PowerShell: {PROJFS_ENABLE_POWERSHELL_CMD}"
    )];

    let output =
        run_powershell(PROJFS_ENABLE_POWERSHELL_CMD).map_err(|err| OzError::CheckFailed {
            message: format!("failed to execute PowerShell to enable Client-ProjFS: {err}"),
            recovery: recovery.clone(),
        })?;

    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exit status {}", output.status)
    };

    Err(OzError::CheckFailed {
        message: format!("failed to enable Windows Client-ProjFS optional feature: {detail}"),
        recovery,
    })
}

/// Enables Windows Projected File System (`Client-ProjFS`) non-interactively on Windows.
///
/// - On Windows: returns `Ok(false)` if `ProjectedFSLib.dll` already exists, otherwise runs
///   non-interactive `powershell.exe` returning `Ok(true)` or `OzError::CheckFailed` with an
///   Administrator recovery instruction.
/// - On non-Windows hosts: returns `OzError::InvalidUsage`.
pub fn enable_windows_projfs() -> Result<bool> {
    #[cfg(target_os = "windows")]
    {
        use std::process::{Command, Stdio};

        let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        enable_windows_projfs_with(Path::new(&system_root), |cmd| {
            Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    cmd,
                ])
                .stdin(Stdio::null())
                .output()
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(OzError::InvalidUsage {
            message: "Windows ProjFS setup (`Client-ProjFS`) is only supported on Windows"
                .to_string(),
            recovery: vec![PROJFS_ENABLE_POWERSHELL_CMD.to_string()],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{ExitStatus, Output};
    use tempfile::TempDir;

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }

    #[test]
    fn generates_windows_startup_cmd_and_detects_dll() {
        let dir = TempDir::new().expect("tempdir");
        assert!(!is_windows_projfs_enabled_in(dir.path()));

        let dll = projected_fs_dll_path_in(dir.path());
        std::fs::create_dir_all(dll.parent().expect("parent")).expect("mkdir System32");
        std::fs::write(&dll, b"stub-dll").expect("write stub dll");
        assert!(is_windows_projfs_enabled_in(dir.path()));

        let already = enable_windows_projfs_with(dir.path(), |_| {
            panic!("powershell must not run when ProjectedFSLib.dll already exists")
        })
        .expect("already enabled");
        assert!(!already);

        let cmd = generate_windows_startup_cmd(
            Path::new("C:\\Program Files\\Ozone\\oz-vfsd.exe"),
            Path::new("C:\\repo\\.git"),
        );
        assert!(cmd.contains("oz-vfsd.exe"));
        assert!(cmd.contains("--git-dir"));
        assert!(cmd.contains("--socket"));
        assert!(cmd.contains("vfsd.sock"));
        assert!(cmd.contains("--foreground"));
    }

    #[test]
    fn enable_projfs_helper_handles_success_and_admin_recovery_error() {
        let dir = TempDir::new().expect("tempdir");
        let enabled = enable_windows_projfs_with(dir.path(), |ps_cmd| {
            assert_eq!(ps_cmd, PROJFS_ENABLE_POWERSHELL_CMD);
            Ok(Output {
                status: exit_status(0),
                stdout: b"Online\n".to_vec(),
                stderr: Vec::new(),
            })
        })
        .expect("enable succeeded");
        assert!(enabled);

        let err = enable_windows_projfs_with(dir.path(), |_| {
            Ok(Output {
                status: exit_status(1),
                stdout: Vec::new(),
                stderr: b"The requested operation requires elevation.".to_vec(),
            })
        })
        .expect_err("non-elevated must return CheckFailed");
        match err {
            OzError::CheckFailed { message, recovery } => {
                assert!(message.contains("requires elevation"));
                assert!(
                    recovery
                        .iter()
                        .any(|r| r.contains(PROJFS_ENABLE_POWERSHELL_CMD))
                );
            }
            other => panic!("expected CheckFailed, got {other:?}"),
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn enable_windows_projfs_returns_invalid_usage_on_non_windows() {
        let err = enable_windows_projfs().expect_err("non-windows must fail");
        assert!(matches!(err, OzError::InvalidUsage { .. }));
    }
}
