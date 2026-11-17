// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `oz-vfsd` background VFS daemon entrypoint.

use clap::Parser;
use oz_core::OzoneRepo;
use oz_vfsd::client::socket_path_for_git_dir;
use oz_vfsd::server::run_daemon;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "oz-vfsd", version, about = "Ozone CoW VFS background daemon")]
struct Args {
    /// Path to the repository's `.git` common directory.
    #[arg(long)]
    git_dir: Option<PathBuf>,

    /// Path to the local IPC socket (defaults to `<git-dir>/ozone/vfsd.sock`).
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Run in foreground (logging directly to stderr).
    #[arg(long)]
    foreground: bool,
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
    let args = Args::parse();

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
