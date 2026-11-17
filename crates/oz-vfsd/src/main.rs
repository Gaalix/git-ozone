// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `oz-vfsd` background VFS daemon entrypoint (Milestone 0 skeleton).

use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "oz-vfsd", version, about = "Ozone CoW VFS background daemon")]
struct Args {
    /// Path to the local IPC socket (`.git/ozone/vfsd.sock`).
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Run in foreground rather than detaching.
    #[arg(long)]
    foreground: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Some(sock) = args.socket {
        eprintln!(
            "oz-vfsd {} skeleton ready (socket: {}; FUSE3/ProjFS mount server lands in Milestone 1)",
            env!("CARGO_PKG_VERSION"),
            sock.display()
        );
    } else {
        println!(
            "oz-vfsd {} (Milestone 0 daemon skeleton; foreground={})",
            env!("CARGO_PKG_VERSION"),
            args.foreground
        );
    }
}
