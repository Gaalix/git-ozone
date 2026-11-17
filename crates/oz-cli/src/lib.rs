// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ozone CLI (`oz` and `git-ozone`) argument parsing, `oz.v1` JSON rendering, and `doctor`.

use clap::{Parser, Subcommand};
use oz_core::{
    BackendKind, CliInvocation, Envelope, ExitCode, InteractionMode, MountHealth, OzError,
    OzoneRepo, find_git_ozone_on_path,
};
use oz_cow::{FilesystemProbe, LinuxFuseReadiness, detect_filesystem, select_backend};
use oz_vfsd::find_oz_vfsd_bin;
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Top-level CLI options shared across `oz` and `git-ozone`.
#[derive(Debug, Parser)]
#[command(
    name = "oz",
    disable_version_flag = true,
    about = "Pure-Rust CoW Git client and multi-agent workspace engine"
)]
pub struct Cli {
    /// Emit canonical `oz.v1` JSON output envelope.
    #[arg(long, global = true)]
    pub json: bool,

    /// Force strict non-interactive agent mode (also enabled via `OZ_AGENT=1`).
    #[arg(long, global = true)]
    pub agent: bool,

    /// Run as if `oz` was started in `<DIR>` instead of the current working directory.
    #[arg(short = 'C', value_name = "DIR", global = true)]
    pub directory: Option<PathBuf>,

    /// Print version information (supports `--json`).
    #[arg(short = 'V', long = "version", global = true)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Supported Ozone subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Inspect OS, filesystem (`statfs`), FUSE3/reflink readiness, and colocated repo status.
    Doctor,
    /// Print version information (equivalent to `--version`).
    Version,
}

#[derive(Debug, Serialize)]
struct VersionData {
    name: &'static str,
    invoked_as: &'static str,
    version: &'static str,
    os: &'static str,
    arch: &'static str,
    pure_rust: bool,
    non_interactive: bool,
}

#[derive(Debug, Serialize)]
pub struct DoctorFilesystem {
    #[serde(rename = "type")]
    pub fs_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magic: Option<String>,
    pub reflink_ready: bool,
}

#[derive(Debug, Serialize)]
pub struct DoctorVfsd {
    pub found: bool,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct DoctorGitOzoneAlias {
    pub invoked_as: &'static str,
    pub found_on_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct DoctorRepoStatus {
    pub in_repo: bool,
    pub colocated: bool,
    pub repo_root: Option<PathBuf>,
    pub git_dir: Option<PathBuf>,
    pub git_common_dir: Option<PathBuf>,
    pub ozone_dir: Option<PathBuf>,
    pub ozone_initialized: bool,
    pub is_linked_worktree: bool,
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub mount_health: Option<&'static str>,
    pub repo_config_present: bool,
    pub local_config_present: bool,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub invoked_as: &'static str,
    pub version: &'static str,
    pub non_interactive: bool,
    pub os: String,
    pub arch: String,
    pub target_path: PathBuf,
    pub filesystem: DoctorFilesystem,
    pub fuse: Option<LinuxFuseReadiness>,
    pub projfs_ready: Option<bool>,
    pub recommended_backend: BackendKind,
    pub oz_vfsd: DoctorVfsd,
    pub git_ozone_alias: DoctorGitOzoneAlias,
    pub repo: DoctorRepoStatus,
}

/// Entrypoint shared by `oz` and `git-ozone` binaries.
pub fn run_cli() -> std::process::ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    run_with_args(args).to_process_exit_code()
}

/// Executes the CLI against explicit argument vector and returns canonical [`ExitCode`].
pub fn run_with_args(args: Vec<OsString>) -> ExitCode {
    let invocation = CliInvocation::from_arg0(args.first().map(OsString::as_os_str));
    let raw_json_hint = args.iter().any(|a| a == "--json");
    let raw_agent_hint = args.iter().any(|a| a == "--agent");

    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(err) => {
            use clap::error::ErrorKind;
            match err.kind() {
                ErrorKind::DisplayHelp => {
                    let _ = err.print();
                    return ExitCode::Success;
                }
                _ => {
                    if raw_json_hint {
                        let mode = InteractionMode::detect(true, raw_agent_hint);
                        let _ = mode;
                        let env = Envelope::failure(
                            "invalid_usage",
                            ExitCode::UsageOrGuardrail,
                            err.to_string().trim().to_string(),
                        )
                        .with_recovery_command("oz --help");
                        println!(
                            "{}",
                            env.to_json_pretty()
                                .unwrap_or_else(|_| "{\"schema\":\"oz.v1\",\"ok\":false}".into())
                        );
                    } else {
                        let _ = err.print();
                    }
                    return ExitCode::UsageOrGuardrail;
                }
            }
        }
    };

    let mode = InteractionMode::detect(cli.json, cli.agent);

    if cli.version || matches!(cli.command, Some(Commands::Version)) {
        return handle_version(invocation, mode);
    }

    match cli.command {
        Some(Commands::Doctor) => handle_doctor(invocation, &cli, mode),
        Some(Commands::Version) => handle_version(invocation, mode),
        None => {
            let msg = "no subcommand provided; run `oz --help` or `oz doctor --json`";
            if mode.json {
                let env = Envelope::failure("invalid_usage", ExitCode::UsageOrGuardrail, msg)
                    .with_recovery_command("oz --help")
                    .with_recovery_command("oz doctor --json");
                print_json_envelope(&env);
            } else {
                eprintln!("error: {msg}");
            }
            ExitCode::UsageOrGuardrail
        }
    }
}

fn handle_version(invocation: CliInvocation, mode: InteractionMode) -> ExitCode {
    let data = VersionData {
        name: "oz",
        invoked_as: invocation.binary_name(),
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        pure_rust: true,
        non_interactive: mode.non_interactive,
    };

    if mode.json {
        let env = Envelope::success(data);
        print_json_envelope(&env);
    } else {
        println!("{} {}", invocation.binary_name(), env!("CARGO_PKG_VERSION"));
    }
    ExitCode::Success
}

fn handle_doctor(invocation: CliInvocation, cli: &Cli, mode: InteractionMode) -> ExitCode {
    match build_doctor_envelope(invocation, cli.directory.as_deref(), mode) {
        Ok(env) => {
            if mode.json {
                print_json_envelope(&env);
            } else {
                print_doctor_human(&env);
            }
            ExitCode::Success
        }
        Err(err) => {
            if mode.json {
                let env = Envelope::from_error(&err);
                print_json_envelope(&env);
            } else {
                eprintln!("error: {err}");
            }
            err.exit_code()
        }
    }
}

/// Computes diagnostics and builds the `oz.v1` [`Envelope`] for `oz doctor`.
pub fn build_doctor_envelope(
    invocation: CliInvocation,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> oz_core::Result<Envelope<DoctorReport>> {
    let repo_result = OzoneRepo::discover(directory);

    let (repo_opt, repo_warnings) = match repo_result {
        Ok(repo) => (Some(repo), Vec::new()),
        Err(OzError::NotARepository { .. }) => (None, Vec::new()),
        Err(OzError::InvalidConfig { path, message }) => {
            return Err(OzError::InvalidConfig { path, message });
        }
        Err(err) => {
            let msg = format!("repository discovery warning: {err}");
            (None, vec![msg])
        }
    };

    let target_path = if let Some(dir) = directory {
        dir.to_path_buf()
    } else if let Some(ref repo) = repo_opt {
        repo.workdir_path.clone()
    } else {
        std::env::current_dir()
            .or_else(|_| {
                std::env::var_os("PWD")
                    .map(PathBuf::from)
                    .ok_or_else(|| std::io::Error::other("cwd unavailable"))
            })
            .unwrap_or_else(|_| PathBuf::from("."))
    };

    let probe_path = repo_opt
        .as_ref()
        .map(|r| r.repo_root.as_path())
        .unwrap_or(target_path.as_path());
    let probe: FilesystemProbe = detect_filesystem(probe_path)?;

    let configured_backend = repo_opt.as_ref().and_then(|r| r.config.backend);
    let recommended_backend = select_backend(&probe, configured_backend);

    let vfsd_bin = find_oz_vfsd_bin().ok();
    let git_ozone_alias = find_git_ozone_on_path();

    let mut warnings = repo_warnings;
    let mut recovery_commands = Vec::new();

    if let Some(ref fuse) = probe.fuse
        && !fuse.ready
        && !probe.reflink_supported
    {
        warnings.push(
            "Linux FUSE3 readiness incomplete (/dev/fuse rw or fusermount3 missing); will fall back to cas_hardlink"
                .to_string(),
        );
    }

    if let Some(ref repo) = repo_opt
        && repo.mount_health == MountHealth::DisconnectedEnotconn
    {
        warnings.push(format!(
            "Workspace '{}' mountpoint returned ENOTCONN; real host disk .git/ozone state remains intact",
            repo.workspace.as_deref().unwrap_or("unknown")
        ));
        recovery_commands.push("oz ws list --json".to_string());
    }

    let (workspace, branch, head_oid, repo_status) = match repo_opt {
        Some(repo) => {
            let status = DoctorRepoStatus {
                in_repo: true,
                colocated: true,
                repo_root: Some(repo.repo_root.clone()),
                git_dir: Some(repo.git_dir.clone()),
                git_common_dir: Some(repo.git_common_dir.clone()),
                ozone_dir: Some(repo.layout.ozone_dir.clone()),
                ozone_initialized: repo.layout.is_initialized(),
                is_linked_worktree: repo.is_linked_worktree,
                workspace: repo.workspace.clone(),
                branch: repo.branch.clone(),
                head_oid: repo.head_oid.clone(),
                mount_health: Some(repo.mount_health.as_str()),
                repo_config_present: repo.repo_root.join(".ozone.toml").is_file(),
                local_config_present: repo.layout.local_config_path().is_file(),
            };
            (repo.workspace, repo.branch, repo.head_oid, status)
        }
        None => (
            None,
            None,
            None,
            DoctorRepoStatus {
                in_repo: false,
                colocated: false,
                repo_root: None,
                git_dir: None,
                git_common_dir: None,
                ozone_dir: None,
                ozone_initialized: false,
                is_linked_worktree: false,
                workspace: None,
                branch: None,
                head_oid: None,
                mount_health: None,
                repo_config_present: false,
                local_config_present: false,
            },
        ),
    };

    let report = DoctorReport {
        invoked_as: invocation.binary_name(),
        version: env!("CARGO_PKG_VERSION"),
        non_interactive: mode.non_interactive,
        os: probe.os,
        arch: probe.arch,
        target_path,
        filesystem: DoctorFilesystem {
            fs_type: probe.fs_kind.as_str().to_string(),
            magic: probe.fs_magic,
            reflink_ready: probe.reflink_supported,
        },
        fuse: probe.fuse,
        projfs_ready: probe.projfs_ready,
        recommended_backend,
        oz_vfsd: DoctorVfsd {
            found: vfsd_bin.is_some(),
            path: vfsd_bin,
        },
        git_ozone_alias: DoctorGitOzoneAlias {
            invoked_as: invocation.binary_name(),
            found_on_path: git_ozone_alias,
        },
        repo: repo_status,
    };

    let mut env = Envelope::success(report)
        .with_workspace(workspace)
        .with_branch(branch)
        .with_head_oid(head_oid)
        .with_recovery_commands(recovery_commands);
    for w in warnings {
        env = env.with_warning(w);
    }
    Ok(env)
}

fn print_json_envelope<T: Serialize>(env: &Envelope<T>) {
    match env.to_json_pretty() {
        Ok(s) => println!("{s}"),
        Err(err) => {
            eprintln!("error: failed to serialize oz.v1 envelope: {err}");
        }
    }
}

fn print_doctor_human(env: &Envelope<DoctorReport>) {
    let d = &env.data;
    println!("Ozone Doctor ({} {})", d.invoked_as, d.version);
    println!("  OS / Arch:           {} / {}", d.os, d.arch);
    println!(
        "  Filesystem:          {} (reflink_ready: {})",
        d.filesystem.fs_type, d.filesystem.reflink_ready
    );
    if let Some(ref fuse) = d.fuse {
        println!(
            "  Linux FUSE3:         ready={} (/dev/fuse rw={}, fusermount3={})",
            fuse.ready,
            fuse.dev_fuse_rw,
            fuse.fusermount3_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "missing".to_string())
        );
    }
    if let Some(projfs) = d.projfs_ready {
        println!("  Windows ProjFS:      ready={projfs}");
    }
    println!("  Recommended backend: {}", d.recommended_backend);
    println!(
        "  oz-vfsd binary:      {}",
        d.oz_vfsd
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not found".to_string())
    );
    if d.repo.in_repo {
        println!(
            "  Repository:          {} (branch: {}, ozone_initialized: {})",
            d.repo
                .repo_root
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            d.repo.branch.as_deref().unwrap_or("detached"),
            d.repo.ozone_initialized
        );
    } else {
        println!("  Repository:          not inside a git repository");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oz_core::OZ_SCHEMA_V1;
    use tempfile::TempDir;

    #[test]
    fn version_json_and_doctor_json_produce_valid_oz_v1_envelope() {
        let mode = InteractionMode::from_input(oz_core::InteractionInput {
            json_flag: true,
            agent_flag: true,
            oz_agent_env: Some("1"),
            ci_env: None,
            stdout_is_tty: false,
            stdin_is_tty: false,
        });

        let dir = TempDir::new().expect("tempdir");
        let init = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .output()
            .expect("git init");
        assert!(init.status.success());

        let env = build_doctor_envelope(CliInvocation::GitOzone, Some(dir.path()), mode)
            .expect("doctor envelope");
        assert_eq!(env.schema, OZ_SCHEMA_V1);
        assert!(env.ok);
        assert_eq!(env.branch.as_deref(), Some("main"));
        assert!(env.data.repo.in_repo);
        assert!(env.data.repo.colocated);
        assert_eq!(env.data.invoked_as, "git-ozone");

        let json = env.to_json_pretty().expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema"], "oz.v1");
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["repo"]["in_repo"], true);
    }

    #[test]
    fn cli_parses_version_and_doctor_flags_and_returns_expected_exit_codes() {
        let dir = TempDir::new().expect("tempdir");
        assert_eq!(
            run_with_args(vec!["oz".into(), "--version".into(), "--json".into()]),
            ExitCode::Success
        );
        assert_eq!(
            run_with_args(vec![
                "git-ozone".into(),
                "-C".into(),
                dir.path().as_os_str().to_os_string(),
                "--agent".into(),
                "doctor".into(),
                "--json".into(),
            ]),
            ExitCode::Success
        );
        assert_eq!(
            run_with_args(vec!["oz".into(), "--unknown-flag".into(), "--json".into()]),
            ExitCode::UsageOrGuardrail
        );
    }
}
