// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Automatic TTY pager resolution and safe output emission.
//!
//! Precedence when stdout is an interactive TTY (`!json && !agent && !ci && stdout_is_tty`):
//! 1. `$OZ_PAGER`
//! 2. `$GIT_PAGER`
//! 3. `delta` (when present on `$PATH`)
//! 4. `less -FRX`
//!
//! Strictly disabled when `--json`, `--agent`, `OZ_AGENT=1`, `CI=1`, non-TTY stdout,
//! or when the configured pager is empty or `"cat"`.

use oz_core::InteractionMode;
use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Inputs controlling deterministic pager selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagerSelectionInput<'a> {
    pub json_flag: bool,
    pub agent_flag: bool,
    pub oz_agent_env: Option<&'a str>,
    pub ci_env: Option<&'a str>,
    pub stdout_is_tty: bool,
    pub oz_pager_env: Option<&'a str>,
    pub git_pager_env: Option<&'a str>,
    pub path_env: Option<&'a OsStr>,
}

/// Resolved pager target for terminal output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PagerChoice {
    /// Paging is suppressed or explicitly disabled (`cat` / empty).
    Disabled,
    /// Direct executable invocation without shell wrapping.
    Command { program: String, args: Vec<String> },
    /// Shell-evaluated pager pipeline (when `$OZ_PAGER` / `$GIT_PAGER` contains shell syntax).
    Shell(String),
}

fn is_truthy_env(val: Option<&str>) -> bool {
    let Some(raw) = val else {
        return false;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Returns `true` when paging is permitted for the given [`InteractionMode`].
///
/// Strictly disabled under `--json`, `--agent`, `OZ_AGENT=1`, `CI=1`, or non-TTY stdout.
#[must_use]
pub(crate) const fn pager_allowed_for_mode(mode: InteractionMode) -> bool {
    !mode.json && !mode.agent && !mode.ci && mode.stdout_is_tty
}

/// Resolves the active pager purely from explicit inputs without reading ambient process state.
#[must_use]
pub fn resolve_pager_spec(input: PagerSelectionInput<'_>) -> PagerChoice {
    if input.json_flag
        || input.agent_flag
        || is_truthy_env(input.oz_agent_env)
        || is_truthy_env(input.ci_env)
        || !input.stdout_is_tty
    {
        return PagerChoice::Disabled;
    }

    if let Some(oz_pager) = input.oz_pager_env {
        return parse_pager_env(oz_pager);
    }
    if let Some(git_pager) = input.git_pager_env {
        return parse_pager_env(git_pager);
    }

    if let Some(path_var) = input.path_env {
        if let Some(delta_path) = find_binary_in_path("delta", path_var) {
            return PagerChoice::Command {
                program: delta_path.to_string_lossy().into_owned(),
                args: Vec::new(),
            };
        }
        if let Some(less_path) = find_binary_in_path("less", path_var) {
            return PagerChoice::Command {
                program: less_path.to_string_lossy().into_owned(),
                args: vec!["-FRX".to_string()],
            };
        }
    }

    PagerChoice::Command {
        program: "less".to_string(),
        args: vec!["-FRX".to_string()],
    }
}

/// Resolves the pager choice for the current process environment and [`InteractionMode`].
#[must_use]
pub(crate) fn resolve_current_pager(mode: InteractionMode) -> PagerChoice {
    if !pager_allowed_for_mode(mode) {
        return PagerChoice::Disabled;
    }
    let oz_pager = std::env::var("OZ_PAGER").ok();
    let git_pager = std::env::var("GIT_PAGER").ok();
    let path_env: Option<OsString> = std::env::var_os("PATH");
    let oz_agent_env = std::env::var("OZ_AGENT").ok();
    let ci_env = std::env::var("CI").ok();

    resolve_pager_spec(PagerSelectionInput {
        json_flag: mode.json,
        agent_flag: mode.agent,
        oz_agent_env: oz_agent_env.as_deref(),
        ci_env: ci_env.as_deref(),
        stdout_is_tty: mode.stdout_is_tty,
        oz_pager_env: oz_pager.as_deref(),
        git_pager_env: git_pager.as_deref(),
        path_env: path_env.as_deref(),
    })
}

fn parse_pager_env(raw: &str) -> PagerChoice {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "cat" {
        return PagerChoice::Disabled;
    }
    if contains_shell_metachars(trimmed) {
        return PagerChoice::Shell(trimmed.to_string());
    }
    let tokens = split_simple_command(trimmed);
    if tokens.is_empty() {
        return PagerChoice::Disabled;
    }
    if tokens.len() == 1 && tokens[0] == "cat" {
        return PagerChoice::Disabled;
    }
    PagerChoice::Command {
        program: tokens[0].clone(),
        args: tokens[1..].to_vec(),
    }
}

fn contains_shell_metachars(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '|' | ';' | '&' | '>' | '<' | '$' | '`'))
}

fn split_simple_command(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
            } else if c == '\\' {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '\'' => in_single = true,
            '"' => in_double = true,
            '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            ch if ch.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            other => cur.push(other),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn find_binary_in_path(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidate_names = [format!("{name}.exe"), name.to_string()];
    #[cfg(not(windows))]
    let candidate_names = [name.to_string()];

    for dir in std::env::split_paths(path_var) {
        for cand in &candidate_names {
            let full = dir.join(cand);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

/// Emits `content` through the active TTY pager if permitted, or directly to stdout.
pub(crate) fn emit_paged_output(mode: InteractionMode, content: &str) -> std::io::Result<()> {
    if content.is_empty() {
        return Ok(());
    }

    let choice = resolve_current_pager(mode);
    match choice {
        PagerChoice::Disabled => write_stdout_ignoring_broken_pipe(content),
        PagerChoice::Command { program, args } => {
            let mut cmd = Command::new(&program);
            cmd.args(&args);
            spawn_and_feed_pager(cmd, content)
        }
        PagerChoice::Shell(shell_cmd) => {
            #[cfg(windows)]
            let cmd = {
                let mut c = Command::new("cmd");
                c.args(["/C", &shell_cmd]);
                c
            };
            #[cfg(not(windows))]
            let cmd = {
                let mut c = Command::new("sh");
                c.args(["-c", &shell_cmd]);
                c
            };
            spawn_and_feed_pager(cmd, content)
        }
    }
}

fn spawn_and_feed_pager(mut cmd: Command, content: &str) -> std::io::Result<()> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if std::env::var_os("LESS").is_none() {
        cmd.env("LESS", "FRX");
    }

    let Ok(mut child) = cmd.spawn() else {
        return write_stdout_ignoring_broken_pipe(content);
    };

    if let Some(mut stdin) = child.stdin.take()
        && let Err(err) = stdin.write_all(content.as_bytes())
        && err.kind() != ErrorKind::BrokenPipe
    {
        let _ = child.wait();
        return Err(err);
    }
    let _ = child.wait();
    Ok(())
}

fn write_stdout_ignoring_broken_pipe(content: &str) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    if let Err(err) = stdout.write_all(content.as_bytes())
        && err.kind() != ErrorKind::BrokenPipe
    {
        return Err(err);
    }
    let _ = stdout.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn pager_is_strictly_disabled_in_json_agent_ci_and_non_tty() {
        let tmp = TempDir::new().expect("tempdir");
        let fake_delta = tmp.path().join("delta");
        std::fs::write(&fake_delta, "#!/bin/sh\ncat\n").expect("write fake delta");

        let base = PagerSelectionInput {
            json_flag: false,
            agent_flag: false,
            oz_agent_env: None,
            ci_env: None,
            stdout_is_tty: true,
            oz_pager_env: None,
            git_pager_env: None,
            path_env: Some(tmp.path().as_os_str()),
        };
        assert!(matches!(
            resolve_pager_spec(base),
            PagerChoice::Command { .. }
        ));

        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                json_flag: true,
                oz_pager_env: Some("less -FRX"),
                ..base
            }),
            PagerChoice::Disabled
        );
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                agent_flag: true,
                oz_pager_env: Some("delta"),
                ..base
            }),
            PagerChoice::Disabled
        );
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                oz_agent_env: Some("1"),
                ..base
            }),
            PagerChoice::Disabled
        );
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                ci_env: Some("true"),
                ..base
            }),
            PagerChoice::Disabled
        );
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                stdout_is_tty: false,
                oz_pager_env: Some("delta"),
                ..base
            }),
            PagerChoice::Disabled
        );
    }

    #[test]
    fn pager_selection_precedence_respects_oz_pager_git_pager_delta_and_less_frx() {
        let dir_with_both = TempDir::new().expect("tempdir");
        std::fs::write(dir_with_both.path().join("delta"), "").expect("write delta");
        std::fs::write(dir_with_both.path().join("less"), "").expect("write less");

        let dir_less_only = TempDir::new().expect("tempdir");
        std::fs::write(dir_less_only.path().join("less"), "").expect("write less");

        // 1. $OZ_PAGER wins over $GIT_PAGER, delta, and less
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                json_flag: false,
                agent_flag: false,
                oz_agent_env: None,
                ci_env: None,
                stdout_is_tty: true,
                oz_pager_env: Some("more -s"),
                git_pager_env: Some("less -R"),
                path_env: Some(dir_with_both.path().as_os_str()),
            }),
            PagerChoice::Command {
                program: "more".to_string(),
                args: vec!["-s".to_string()],
            }
        );

        // 2. $GIT_PAGER wins over delta and less
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                json_flag: false,
                agent_flag: false,
                oz_agent_env: None,
                ci_env: None,
                stdout_is_tty: true,
                oz_pager_env: None,
                git_pager_env: Some("bat --paging=always"),
                path_env: Some(dir_with_both.path().as_os_str()),
            }),
            PagerChoice::Command {
                program: "bat".to_string(),
                args: vec!["--paging=always".to_string()],
            }
        );

        // 3. delta preferred when on PATH and no explicit pager env set
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                json_flag: false,
                agent_flag: false,
                oz_agent_env: None,
                ci_env: None,
                stdout_is_tty: true,
                oz_pager_env: None,
                git_pager_env: None,
                path_env: Some(dir_with_both.path().as_os_str()),
            }),
            PagerChoice::Command {
                program: dir_with_both
                    .path()
                    .join("delta")
                    .to_string_lossy()
                    .into_owned(),
                args: Vec::new(),
            }
        );

        // 4. less -FRX fallback when delta is absent
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                json_flag: false,
                agent_flag: false,
                oz_agent_env: None,
                ci_env: None,
                stdout_is_tty: true,
                oz_pager_env: None,
                git_pager_env: None,
                path_env: Some(dir_less_only.path().as_os_str()),
            }),
            PagerChoice::Command {
                program: dir_less_only
                    .path()
                    .join("less")
                    .to_string_lossy()
                    .into_owned(),
                args: vec!["-FRX".to_string()],
            }
        );

        // 5. $OZ_PAGER="cat" or "" explicitly disables paging
        assert_eq!(
            resolve_pager_spec(PagerSelectionInput {
                json_flag: false,
                agent_flag: false,
                oz_agent_env: None,
                ci_env: None,
                stdout_is_tty: true,
                oz_pager_env: Some("cat"),
                git_pager_env: Some("less -FRX"),
                path_env: Some(dir_with_both.path().as_os_str()),
            }),
            PagerChoice::Disabled
        );
    }
}
