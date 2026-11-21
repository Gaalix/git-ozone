// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pre-clap configurable command alias expansion (`st`, `ci`, `co`, `lg`, `br`, and `[aliases]`
//! in `.ozone.toml` / `.git/ozone/config.toml`).

use oz_core::{OzError, OzoneConfig, OzoneRepo, Result, default_aliases};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const MAX_ALIAS_EXPANSION_DEPTH: usize = 8;

/// Built-in top-level `oz` subcommands (never shadowed by aliases).
#[must_use]
pub fn is_builtin_subcommand(name: &str) -> bool {
    matches!(
        name,
        "ws" | "status"
            | "diff"
            | "commit"
            | "checkpoint"
            | "undo"
            | "redo"
            | "radar"
            | "integrate"
            | "resolve"
            | "stack"
            | "clone"
            | "fetch"
            | "push"
            | "pr"
            | "sync"
            | "ci"
            | "agent"
            | "mcp"
            | "blame"
            | "setup"
            | "doctor"
            | "version"
            | "git"
            | "log"
            | "show"
            | "stash"
            | "cherry-pick"
            | "revert"
            | "tag"
            | "branch"
            | "completions"
            | "help"
    )
}

/// Expands configured aliases in `args` before clap parses them, preserving leading/trailing
/// global flags (`--json`, `--agent`, `-C <DIR>`).
pub fn expand_aliases(args: Vec<OsString>) -> Result<Vec<OsString>> {
    let Some((subcmd_idx, dir_opt)) = locate_subcommand_and_dir(&args) else {
        return Ok(args);
    };

    let candidate = args[subcmd_idx].to_string_lossy();
    let tail = &args[subcmd_idx + 1..];

    if candidate == "ci" {
        let aliases = load_aliases_for_dir(dir_opt.as_deref())?;
        if should_expand_ci_alias(tail, aliases.get("ci").map(String::as_str)) {
            return expand_aliases_at_index(args, subcmd_idx, &aliases);
        }
        return Ok(args);
    }

    if is_builtin_subcommand(&candidate) {
        return Ok(args);
    }

    let aliases = load_aliases_for_dir(dir_opt.as_deref())?;
    expand_aliases_at_index(args, subcmd_idx, &aliases)
}

/// Pure testable alias expansion using an explicit alias table.
pub fn expand_aliases_with_map(
    args: Vec<OsString>,
    aliases: &BTreeMap<String, String>,
) -> Result<Vec<OsString>> {
    let Some((subcmd_idx, _)) = locate_subcommand_and_dir(&args) else {
        return Ok(args);
    };
    let candidate = args[subcmd_idx].to_string_lossy();
    let tail = &args[subcmd_idx + 1..];

    if candidate == "ci" {
        if should_expand_ci_alias(tail, aliases.get("ci").map(String::as_str)) {
            return expand_aliases_at_index(args, subcmd_idx, aliases);
        }
        return Ok(args);
    }

    if is_builtin_subcommand(&candidate) {
        return Ok(args);
    }
    expand_aliases_at_index(args, subcmd_idx, aliases)
}

fn should_expand_ci_alias(tail: &[OsString], configured: Option<&str>) -> bool {
    let Some(expansion) = configured else {
        return false;
    };
    let has_commit_flag = tail.iter().any(|arg| {
        let s = arg.to_string_lossy();
        matches!(
            s.as_ref(),
            "-m" | "--message"
                | "--pick"
                | "--exclude"
                | "--force"
                | "--allow-out-of-scope"
                | "--allow-empty"
                | "-S"
                | "--sign"
                | "--no-sign"
                | "--signing-key"
                | "--amend"
                | "--no-edit"
        ) || s.starts_with("-m")
            || s.starts_with("--message=")
            || s.starts_with("--signing-key=")
    });
    if has_commit_flag {
        return true;
    }
    // If `.ozone.toml` explicitly customized `ci` to something other than the built-in `"commit"`,
    // honor the explicit override.
    expansion.trim() != "commit"
}

fn expand_aliases_at_index(
    args: Vec<OsString>,
    subcmd_idx: usize,
    aliases: &BTreeMap<String, String>,
) -> Result<Vec<OsString>> {
    let mut head_token = args[subcmd_idx].to_string_lossy().into_owned();
    let Some(first_expansion) = aliases.get(&head_token) else {
        return Ok(args);
    };

    let mut visited = HashSet::new();
    visited.insert(head_token.clone());

    let mut expanded = split_alias_tokens(first_expansion)?;
    if expanded.is_empty() {
        return Ok(args);
    }

    for _ in 0..MAX_ALIAS_EXPANSION_DEPTH {
        let next_head = expanded[0].as_str();
        if is_builtin_subcommand(next_head) {
            break;
        }
        let Some(next_raw) = aliases.get(next_head) else {
            break;
        };
        if !visited.insert(next_head.to_string()) {
            return Err(OzError::InvalidConfig {
                path: PathBuf::from(".ozone.toml"),
                message: format!("cyclic alias expansion involving '{head_token}' and '{next_head}'"),
            });
        }
        head_token = next_head.to_string();
        let mut next_tokens = split_alias_tokens(next_raw)?;
        next_tokens.extend(expanded.into_iter().skip(1));
        expanded = next_tokens;
        if expanded.is_empty() {
            break;
        }
    }

    let mut out = Vec::with_capacity(args.len() - 1 + expanded.len());
    out.extend_from_slice(&args[..subcmd_idx]);
    out.extend(expanded.into_iter().map(OsString::from));
    out.extend_from_slice(&args[subcmd_idx + 1..]);
    Ok(out)
}

fn locate_subcommand_and_dir(args: &[OsString]) -> Option<(usize, Option<PathBuf>)> {
    let mut dir_before: Option<PathBuf> = None;
    let mut i = 1usize;
    let mut subcmd_idx: Option<usize> = None;

    while i < args.len() {
        let s = args[i].to_string_lossy();
        if matches!(
            s.as_ref(),
            "--json" | "--agent" | "-V" | "--version" | "-h" | "--help"
        ) {
            i += 1;
            continue;
        }
        if s == "-C" {
            if let Some(val) = args.get(i + 1) {
                dir_before = Some(PathBuf::from(val));
            }
            i += 2;
            continue;
        }
        if let Some(rest) = s.strip_prefix("-C")
            && !rest.is_empty()
        {
            dir_before = Some(PathBuf::from(rest));
            i += 1;
            continue;
        }
        if s == "--" || s.starts_with('-') {
            return None;
        }
        subcmd_idx = Some(i);
        break;
    }

    let idx = subcmd_idx?;
    if dir_before.is_some() {
        return Some((idx, dir_before));
    }

    // Also inspect trailing `-C <DIR>` (since `-C` is a clap `global = true` flag).
    let mut trailing_dir: Option<PathBuf> = None;
    let mut j = idx + 1;
    while j < args.len() {
        let s = args[j].to_string_lossy();
        if s == "--" {
            break;
        }
        if s == "-C" {
            if let Some(val) = args.get(j + 1) {
                let cand = PathBuf::from(val);
                if cand.exists() {
                    trailing_dir = Some(cand);
                }
            }
            j += 2;
            continue;
        }
        if let Some(rest) = s.strip_prefix("-C")
            && !rest.is_empty()
        {
            let cand = PathBuf::from(rest);
            if cand.exists() {
                trailing_dir = Some(cand);
            }
        }
        j += 1;
    }

    Some((idx, trailing_dir))
}

fn load_aliases_for_dir(dir: Option<&Path>) -> Result<BTreeMap<String, String>> {
    match OzoneRepo::discover(dir) {
        Ok(repo) => Ok(repo.config.aliases),
        Err(OzError::InvalidConfig { path, message }) => Err(OzError::InvalidConfig { path, message }),
        Err(_) => {
            // If outside a Git repository (or invalid directory handled downstream),
            // still honor standalone `.ozone.toml` if present, otherwise built-in defaults.
            let base = dir.unwrap_or_else(|| Path::new("."));
            let cfg_path = base.join(".ozone.toml");
            if cfg_path.is_file() {
                let cfg = OzoneConfig::load_from_paths(Some(cfg_path.as_path()), None)?;
                return Ok(cfg.aliases);
            }
            Ok(default_aliases())
        }
    }
}

fn split_alias_tokens(raw: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
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
                } else {
                    return Err(OzError::InvalidConfig {
                        path: PathBuf::from(".ozone.toml"),
                        message: format!("unterminated escape in alias '{raw}'"),
                    });
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
                } else {
                    return Err(OzError::InvalidConfig {
                        path: PathBuf::from(".ozone.toml"),
                        message: format!("unterminated escape in alias '{raw}'"),
                    });
                }
            }
            ch if ch.is_whitespace() => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            other => cur.push(other),
        }
    }

    if in_single || in_double {
        return Err(OzError::InvalidConfig {
            path: PathBuf::from(".ozone.toml"),
            message: format!("unclosed quote in alias '{raw}'"),
        });
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn expands_builtin_and_custom_aliases_preserving_global_flags() {
        let mut map = default_aliases();
        map.insert(
            "recent".to_string(),
            "log --oneline -n 5 --grep 'fix bug'".to_string(),
        );

        assert_eq!(
            expand_aliases_with_map(os(&["oz", "st", "--json"]), &map).unwrap(),
            os(&["oz", "status", "--json"])
        );
        assert_eq!(
            expand_aliases_with_map(os(&["oz", "--json", "--agent", "lg", "-n", "3"]), &map)
                .unwrap(),
            os(&["oz", "--json", "--agent", "log", "-n", "3"])
        );
        assert_eq!(
            expand_aliases_with_map(
                os(&["oz", "-C", "/tmp/repo", "co", "main", "--json"]),
                &map
            )
            .unwrap(),
            os(&["oz", "-C", "/tmp/repo", "git", "checkout", "main", "--json"])
        );
        assert_eq!(
            expand_aliases_with_map(os(&["oz", "recent", "--json"]), &map).unwrap(),
            os(&["oz", "log", "--oneline", "-n", "5", "--grep", "fix bug", "--json"])
        );
        // `oz ci -m "..."` expands to `commit`, while `oz ci --json` keeps built-in `ci` command.
        assert_eq!(
            expand_aliases_with_map(os(&["oz", "ci", "-m", "feat: ok", "--json"]), &map).unwrap(),
            os(&["oz", "commit", "-m", "feat: ok", "--json"])
        );
        assert_eq!(
            expand_aliases_with_map(os(&["oz", "ci", "--pr", "2", "--json"]), &map).unwrap(),
            os(&["oz", "ci", "--pr", "2", "--json"])
        );
        assert_eq!(
            expand_aliases_with_map(os(&["oz", "br", "--json"]), &map).unwrap(),
            os(&["oz", "branch", "--json"])
        );
    }
}
