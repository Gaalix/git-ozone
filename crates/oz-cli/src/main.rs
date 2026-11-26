// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `oz` primary CLI binary entrypoint.

use {
    chrono as _, clap as _, clap_complete as _, oz_agent as _, oz_core as _, oz_cow as _,
    oz_github as _, oz_integrate as _, oz_oplog as _, oz_vfsd as _, oz_warm as _, serde as _,
    serde_json as _,
};

fn main() -> std::process::ExitCode {
    oz_cli::run_cli()
}
