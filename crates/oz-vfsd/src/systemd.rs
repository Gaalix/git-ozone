// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `systemd --user` service unit generation and installation helpers for `oz setup`.

use oz_core::{OzError, OzoneLayout, Result};
use std::path::{Path, PathBuf};

fn systemd_escape_arg(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn fnv1a_64_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Returns a deterministic `systemd --user` unit filename for `git_dir`,
/// e.g. `oz-vfsd-my-repo-1a2b3c4d.service`.
#[must_use]
pub fn systemd_user_unit_name(git_dir: &Path) -> String {
    let repo_label = git_dir
        .parent()
        .and_then(|p| p.file_name())
        .or_else(|| git_dir.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("repo");

    let sanitized: String = repo_label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let short_label = sanitized.trim_matches('-');
    let label = if short_label.is_empty() {
        "repo"
    } else {
        short_label
    };
    let hash = fnv1a_64_hex(git_dir.to_string_lossy().as_bytes());
    format!("oz-vfsd-{label}-{}.service", &hash[..8])
}

/// Returns the user's systemd unit directory (`$XDG_CONFIG_HOME/systemd/user` or
/// `$HOME/.config/systemd/user`).
pub fn systemd_user_unit_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("systemd").join("user"));
    }
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("systemd")
            .join("user"));
    }
    Err(OzError::Other(
        "cannot resolve systemd --user directory: neither XDG_CONFIG_HOME nor HOME is set"
            .to_string(),
    ))
}

/// Canonical filename for the user-wide `oz-vfsd` systemd user unit serving all repositories.
pub const GLOBAL_SYSTEMD_UNIT_NAME: &str = "oz-vfsd.service";

/// Generates a single user-wide `systemd --user` unit file (`oz-vfsd.service`) that runs
/// `oz-vfsd --global --foreground` for all Git repositories on this machine.
#[must_use]
pub fn generate_global_systemd_user_unit(vfsd_bin: &Path) -> String {
    let bin_arg = systemd_escape_arg(vfsd_bin);
    format!(
        "[Unit]\n\
         Description=Ozone CoW VFS Daemon (User-Wide)\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin_arg} --global --foreground\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Installs the user-wide `oz-vfsd.service` unit file into `unit_dir`, returning the written path.
/// When `try_systemctl_enable` is `true`, runs `systemctl --user daemon-reload` and
/// `systemctl --user enable --now oz-vfsd.service`.
pub fn install_global_systemd_user_unit_in(
    unit_dir: &Path,
    vfsd_bin: &Path,
    try_systemctl_enable: bool,
) -> Result<PathBuf> {
    std::fs::create_dir_all(unit_dir)?;
    let unit_path = unit_dir.join(GLOBAL_SYSTEMD_UNIT_NAME);
    let content = generate_global_systemd_user_unit(vfsd_bin);

    let tmp_path = unit_dir.join(format!(
        ".{GLOBAL_SYSTEMD_UNIT_NAME}.tmp.{}",
        std::process::id()
    ));
    std::fs::write(&tmp_path, content.as_bytes())?;
    std::fs::rename(&tmp_path, &unit_path)?;

    if try_systemctl_enable {
        try_systemctl(&["--user", "daemon-reload"]);
        try_systemctl(&["--user", "enable", "--now", GLOBAL_SYSTEMD_UNIT_NAME]);
    }

    Ok(unit_path)
}

/// Installs and starts the single user-wide `oz-vfsd.service` unit into the user's systemd directory.
pub fn install_global_systemd_user_unit(vfsd_bin: &Path) -> Result<PathBuf> {
    let dir = systemd_user_unit_dir()?;
    install_global_systemd_user_unit_in(&dir, vfsd_bin, cfg!(target_os = "linux"))
}

/// Generates a `systemd --user` service unit file for running `oz-vfsd` on `git_dir`.
#[must_use]
pub fn generate_systemd_user_unit(vfsd_bin: &Path, git_dir: &Path) -> String {
    let bin_arg = systemd_escape_arg(vfsd_bin);
    let git_arg = systemd_escape_arg(git_dir);
    let sock_path = OzoneLayout::from_git_common_dir(git_dir).vfsd_socket_path();
    let sock_arg = systemd_escape_arg(&sock_path);
    let display_dir = git_dir.display();

    format!(
        "[Unit]\n\
         Description=Ozone CoW VFS Daemon ({display_dir})\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin_arg} --git-dir {git_arg} --socket {sock_arg} --foreground\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Installs the generated `systemd --user` unit file into `unit_dir`, returning the
/// written `.service` path. When `try_systemctl_enable` is `true`, attempts a best-effort
/// `systemctl --user daemon-reload` and `systemctl --user enable <unit>`.
pub fn install_systemd_user_unit_in(
    unit_dir: &Path,
    vfsd_bin: &Path,
    git_dir: &Path,
    try_systemctl_enable: bool,
) -> Result<PathBuf> {
    std::fs::create_dir_all(unit_dir)?;
    let unit_name = systemd_user_unit_name(git_dir);
    let unit_path = unit_dir.join(&unit_name);
    let content = generate_systemd_user_unit(vfsd_bin, git_dir);

    let tmp_path = unit_dir.join(format!(".{unit_name}.tmp.{}", std::process::id()));
    std::fs::write(&tmp_path, content.as_bytes())?;
    std::fs::rename(&tmp_path, &unit_path)?;

    if try_systemctl_enable {
        try_systemctl(&["--user", "daemon-reload"]);
        try_systemctl(&["--user", "enable", "--now", &unit_name]);
    }

    Ok(unit_path)
}

/// Installs the `systemd --user` unit for `git_dir` into the user's default unit directory
/// (`$XDG_CONFIG_HOME/systemd/user` or `$HOME/.config/systemd/user`) and attempts a
/// best-effort `systemctl --user daemon-reload && systemctl --user enable`.
pub fn install_systemd_user_unit(vfsd_bin: &Path, git_dir: &Path) -> Result<PathBuf> {
    let dir = systemd_user_unit_dir()?;
    install_systemd_user_unit_in(&dir, vfsd_bin, git_dir, cfg!(target_os = "linux"))
}

fn try_systemctl(args: &[&str]) {
    use std::process::{Command, Stdio};
    let _ = Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generates_and_installs_systemd_user_unit() {
        let dir = TempDir::new().expect("tempdir");
        let unit_dir = dir.path().join("systemd/user");
        let bin = Path::new("/usr/local/bin/oz-vfsd");
        let git_dir = dir.path().join("my repo/.git");

        let content = generate_systemd_user_unit(bin, &git_dir);
        assert!(content.contains("[Unit]"));
        assert!(content.contains("[Service]"));
        assert!(content.contains("[Install]"));
        assert!(content.contains("Type=simple"));
        assert!(content.contains("ExecStart=\"/usr/local/bin/oz-vfsd\" --git-dir \""));
        assert!(content.contains("--foreground"));
        assert!(content.contains("WantedBy=default.target"));

        let installed =
            install_systemd_user_unit_in(&unit_dir, bin, &git_dir, false).expect("install unit");
        assert!(installed.is_file());
        assert!(
            installed
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("oz-vfsd-") && n.ends_with(".service"))
        );
        let on_disk = std::fs::read_to_string(&installed).expect("read unit");
        assert_eq!(on_disk, content);

        let global_content = generate_global_systemd_user_unit(bin);
        assert!(global_content.contains("--global --foreground"));
        let global_installed =
            install_global_systemd_user_unit_in(&unit_dir, bin, false).expect("install global");
        assert_eq!(
            global_installed.file_name().and_then(|n| n.to_str()),
            Some(GLOBAL_SYSTEMD_UNIT_NAME)
        );
        assert_eq!(
            std::fs::read_to_string(&global_installed).unwrap(),
            global_content
        );
    }
}
