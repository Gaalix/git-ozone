// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `oz-vfsd` background VFS daemon entrypoint.

use oz_core::OzoneRepo;
use oz_vfsd::client::{global_daemon_socket_path, socket_path_for_git_dir};
use oz_vfsd::server::{run_daemon, run_global_daemon};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP_TEXT: &str = "\
Ozone CoW VFS background daemon

Usage: oz-vfsd [OPTIONS]

Options:
      --global             Run a single hardened user-wide daemon serving all repositories owned by this user
      --git-dir <GIT_DIR>  Path to the repository's `.git` common directory
      --socket <SOCKET>    Path to the local IPC socket (defaults to `<git-dir>/ozone/vfsd.sock`, or `$XDG_RUNTIME_DIR/oz-vfsd.sock` with `--global`)
      --foreground         Run in foreground (logging directly to stderr)
  -h, --help               Print help
  -V, --version            Print version
";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Args {
    /// Run a single hardened user-wide daemon serving all repositories owned by this user.
    global: bool,
    /// Path to the repository's `.git` common directory.
    git_dir: Option<PathBuf>,
    /// Path to the local IPC socket.
    socket: Option<PathBuf>,
    /// Run in foreground (logging directly to stderr).
    foreground: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParseOutcome {
    Run(Args),
    Help,
    Version,
}

fn version_text() -> String {
    format!("oz-vfsd {}\n", env!("CARGO_PKG_VERSION"))
}

fn strip_ascii_prefix_os(arg: &OsStr, prefix: &str) -> Option<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        arg.as_bytes()
            .strip_prefix(prefix.as_bytes())
            .map(|rest| OsStr::from_bytes(rest).to_os_string())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let wide: Vec<u16> = arg.encode_wide().collect();
        let pref_wide: Vec<u16> = prefix.encode_utf16().collect();
        wide.strip_prefix(pref_wide.as_slice())
            .map(OsString::from_wide)
    }
    #[cfg(not(any(unix, windows)))]
    {
        arg.to_str()?.strip_prefix(prefix).map(OsString::from)
    }
}

fn parse_args_from<I, T>(iter: I) -> Result<ParseOutcome, String>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut raw_iter = iter.into_iter().map(Into::into);
    // Skip argv[0] binary name.
    let _ = raw_iter.next();

    let mut args = Args::default();
    while let Some(arg) = raw_iter.next() {
        if arg == "-h" || arg == "--help" {
            return Ok(ParseOutcome::Help);
        }
        if arg == "-V" || arg == "--version" {
            return Ok(ParseOutcome::Version);
        }
        if arg == "--global" {
            args.global = true;
            continue;
        }
        if arg == "--foreground" {
            args.foreground = true;
            continue;
        }
        if arg == "--git-dir" {
            let val = raw_iter.next().ok_or_else(|| {
                "a value is required for '--git-dir <GIT_DIR>' but none was supplied".to_string()
            })?;
            if val.is_empty() {
                return Err(
                    "a value is required for '--git-dir <GIT_DIR>' but none was supplied"
                        .to_string(),
                );
            }
            args.git_dir = Some(PathBuf::from(val));
            continue;
        }
        if let Some(val) = strip_ascii_prefix_os(&arg, "--git-dir=") {
            if val.is_empty() {
                return Err(
                    "a value is required for '--git-dir <GIT_DIR>' but none was supplied"
                        .to_string(),
                );
            }
            args.git_dir = Some(PathBuf::from(val));
            continue;
        }
        if arg == "--socket" {
            let val = raw_iter.next().ok_or_else(|| {
                "a value is required for '--socket <SOCKET>' but none was supplied".to_string()
            })?;
            if val.is_empty() {
                return Err(
                    "a value is required for '--socket <SOCKET>' but none was supplied".to_string(),
                );
            }
            args.socket = Some(PathBuf::from(val));
            continue;
        }
        if let Some(val) = strip_ascii_prefix_os(&arg, "--socket=") {
            if val.is_empty() {
                return Err(
                    "a value is required for '--socket <SOCKET>' but none was supplied".to_string(),
                );
            }
            args.socket = Some(PathBuf::from(val));
            continue;
        }

        let rendered = arg.to_string_lossy();
        if rendered.starts_with('-') {
            return Err(format!("unexpected argument '{rendered}' found"));
        }
        return Err(format!("unexpected positional argument '{rendered}'"));
    }

    if args.global && args.git_dir.is_some() {
        return Err("the argument '--global' cannot be used with '--git-dir <GIT_DIR>'".to_string());
    }

    Ok(ParseOutcome::Run(args))
}

fn infer_git_dir_from_socket(sock: &Path) -> Option<PathBuf> {
    let parent = sock.parent()?;
    if parent.file_name().and_then(|n| n.to_str()) == Some("ozone") {
        return parent.parent().map(Path::to_path_buf);
    }
    Some(parent.to_path_buf())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args_from(std::env::args_os()) {
        Ok(ParseOutcome::Help) => {
            print!("{HELP_TEXT}");
            return ExitCode::SUCCESS;
        }
        Ok(ParseOutcome::Version) => {
            print!("{}", version_text());
            return ExitCode::SUCCESS;
        }
        Ok(ParseOutcome::Run(parsed)) => parsed,
        Err(msg) => {
            eprintln!("error: {msg}\n\nFor more information, try '--help'.");
            return ExitCode::from(2);
        }
    };

    if args.global {
        let Some(socket_path) = args.socket.or_else(global_daemon_socket_path) else {
            eprintln!("oz-vfsd: cannot determine global socket path (pass --socket <PATH>)");
            return ExitCode::from(2);
        };
        if args.foreground {
            eprintln!(
                "oz-vfsd {} listening globally on {}",
                env!("CARGO_PKG_VERSION"),
                socket_path.display()
            );
        }
        if let Err(err) = run_global_daemon(&socket_path).await {
            eprintln!("oz-vfsd error: {err}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    let (git_dir, socket_path) = match (args.git_dir, args.socket) {
        (Some(git_dir), Some(socket)) => (git_dir, socket),
        (Some(git_dir), None) => {
            let sock = socket_path_for_git_dir(&git_dir);
            (git_dir, sock)
        }
        (None, Some(socket)) => {
            let git_dir = infer_git_dir_from_socket(&socket).unwrap_or_else(|| PathBuf::from("."));
            (git_dir, socket)
        }
        (None, None) => match OzoneRepo::discover(None) {
            Ok(repo) => {
                let sock = repo.layout.vfsd_socket_path();
                (repo.git_common_dir, sock)
            }
            Err(err) => {
                eprintln!("oz-vfsd: missing --git-dir and no repository discovered: {err}");
                return ExitCode::from(2);
            }
        },
    };

    if args.foreground {
        eprintln!(
            "oz-vfsd {} listening on {} (git-dir: {})",
            env!("CARGO_PKG_VERSION"),
            socket_path.display(),
            git_dir.display()
        );
    }

    if let Err(err) = run_daemon(&git_dir, &socket_path).await {
        eprintln!("oz-vfsd error: {err}");
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_and_per_repo_flags() {
        assert_eq!(
            parse_args_from(["oz-vfsd"]),
            Ok(ParseOutcome::Run(Args::default()))
        );

        assert_eq!(
            parse_args_from([
                "oz-vfsd",
                "--git-dir",
                "/repo/.git",
                "--socket",
                "/repo/.git/ozone/vfsd.sock",
                "--foreground",
            ]),
            Ok(ParseOutcome::Run(Args {
                global: false,
                git_dir: Some(PathBuf::from("/repo/.git")),
                socket: Some(PathBuf::from("/repo/.git/ozone/vfsd.sock")),
                foreground: true,
            }))
        );

        assert_eq!(
            parse_args_from([
                "oz-vfsd",
                "--git-dir=/repo/.git",
                "--socket=/tmp/vfsd.sock",
            ]),
            Ok(ParseOutcome::Run(Args {
                global: false,
                git_dir: Some(PathBuf::from("/repo/.git")),
                socket: Some(PathBuf::from("/tmp/vfsd.sock")),
                foreground: false,
            }))
        );
    }

    #[test]
    fn parses_global_help_and_version_flags() {
        assert_eq!(
            parse_args_from(["oz-vfsd", "--global", "--foreground"]),
            Ok(ParseOutcome::Run(Args {
                global: true,
                git_dir: None,
                socket: None,
                foreground: true,
            }))
        );

        for flag in ["-h", "--help"] {
            assert_eq!(parse_args_from(["oz-vfsd", flag]), Ok(ParseOutcome::Help));
        }
        assert!(HELP_TEXT.contains("Usage: oz-vfsd [OPTIONS]"));
        assert!(HELP_TEXT.contains("--global"));
        assert!(HELP_TEXT.contains("--git-dir <GIT_DIR>"));
        assert!(HELP_TEXT.contains("--socket <SOCKET>"));
        assert!(HELP_TEXT.contains("--foreground"));

        for flag in ["-V", "--version"] {
            assert_eq!(
                parse_args_from(["oz-vfsd", flag]),
                Ok(ParseOutcome::Version)
            );
        }
        assert_eq!(
            version_text(),
            format!("oz-vfsd {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn rejects_conflicting_missing_and_unknown_arguments() {
        let conflict = parse_args_from(["oz-vfsd", "--global", "--git-dir", "/repo/.git"])
            .expect_err("global + git-dir must conflict");
        assert!(conflict.contains("--global") && conflict.contains("--git-dir"));

        assert!(parse_args_from(["oz-vfsd", "--git-dir"]).is_err());
        assert!(parse_args_from(["oz-vfsd", "--git-dir="]).is_err());
        assert!(parse_args_from(["oz-vfsd", "--socket"]).is_err());
        assert!(parse_args_from(["oz-vfsd", "--socket="]).is_err());
        assert!(parse_args_from(["oz-vfsd", "--unknown"]).is_err());
        assert!(parse_args_from(["oz-vfsd", "-x"]).is_err());
        assert!(parse_args_from(["oz-vfsd", "positional"]).is_err());
    }

    #[test]
    fn infers_git_dir_from_standard_and_custom_socket_paths() {
        assert_eq!(
            infer_git_dir_from_socket(Path::new("/repo/.git/ozone/vfsd.sock")),
            Some(PathBuf::from("/repo/.git"))
        );
        assert_eq!(
            infer_git_dir_from_socket(Path::new("/tmp/oz-vfsd-1000/vfsd.sock")),
            Some(PathBuf::from("/tmp/oz-vfsd-1000"))
        );
    }
}
