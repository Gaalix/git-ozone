// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `git-ozone` alias binary entrypoint enabling `git ozone <cmd>` discovery.

fn main() -> std::process::ExitCode {
    oz_cli::run_cli()
}
