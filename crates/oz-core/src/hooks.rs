// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Standard Git hook discovery and execution (`core.hooksPath` and `.git/hooks/`).
//!
//! Supports `pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`, and
//! `pre-push`, compatible with `husky`, `lefthook`, `pre-commit`, and custom scripts.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::error::{OzError, Result};
use crate::git_config::GitConfig;
use crate::repo::OzoneRepo;

/// Outcome of attempting to run a Git hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookRunOutcome {
    /// Hook file did not exist or was not executable.
    Skipped,
    /// Hook executed and exited with status `0`.
    Passed {
        hook: String,
        hook_path: PathBuf,
        output: String,
    },
}

/// Author environment metadata exported to commit hooks (matching Git).
#[derive(Debug, Clone, Default)]
pub struct CommitHookEnv {
    pub author_name: Option<String>,
    pub author_email: Option<String>,
    pub author_date: Option<String>,
}

/// One ref update line fed to `pre-push` on stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrePushRefLine {
    pub local_ref: String,
    pub local_oid: String,
    pub remote_ref: String,
    pub remote_oid: String,
}

impl PrePushRefLine {
    /// Formats this entry in Git's `<local ref> <local oid> <remote ref> <remote oid>` layout.
    #[must_use]
    pub fn format_line(&self) -> String {
        format!(
            "{} {} {} {}\n",
            self.local_ref, self.local_oid, self.remote_ref, self.remote_oid
        )
    }
}

/// Resolves the active Git hooks directory (`core.hooksPath` if configured, else
/// `<git_common_dir>/hooks`).
#[must_use]
pub fn resolve_hooks_dir(repo: &OzoneRepo, cfg: &GitConfig) -> PathBuf {
    if let Some(raw_hooks_path) = cfg.hooks_path() {
        let expanded = expand_tilde_env(raw_hooks_path);
        if expanded.is_absolute() {
            return expanded;
        }
        return repo.workdir_path.join(expanded);
    }
    repo.git_common_dir.join("hooks")
}

/// Resolves the executable hook script path for `hook_name`, returning `None` if the
/// hook does not exist or does not have an executable permission bit set.
#[must_use]
pub fn find_executable_hook(hooks_dir: &Path, hook_name: &str) -> Option<PathBuf> {
    let candidate = hooks_dir.join(hook_name);
    let meta = fs::metadata(&candidate).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        if meta.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(candidate)
}

/// Runs `pre-commit` unless `no_verify` is `true`.
pub fn run_pre_commit(
    repo: &OzoneRepo,
    cfg: &GitConfig,
    env_ctx: &CommitHookEnv,
    no_verify: bool,
) -> Result<HookRunOutcome> {
    if no_verify {
        return Ok(HookRunOutcome::Skipped);
    }
    run_hook_command(repo, cfg, "pre-commit", &[], None, Some(env_ctx))
}

/// Runs `prepare-commit-msg <msg_file> <source> [<sha>]` (NOT skipped by `--no-verify`,
/// matching Git).
pub fn run_prepare_commit_msg(
    repo: &OzoneRepo,
    cfg: &GitConfig,
    env_ctx: &CommitHookEnv,
    msg_file: &Path,
    source: &str,
    sha: Option<&str>,
) -> Result<HookRunOutcome> {
    let msg_str = msg_file.to_string_lossy().into_owned();
    let mut args = vec![msg_str, source.to_string()];
    if let Some(commit_sha) = sha {
        args.push(commit_sha.to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_hook_command(
        repo,
        cfg,
        "prepare-commit-msg",
        &arg_refs,
        None,
        Some(env_ctx),
    )
}

/// Runs `commit-msg <msg_file>` unless `no_verify` is `true`.
pub fn run_commit_msg(
    repo: &OzoneRepo,
    cfg: &GitConfig,
    env_ctx: &CommitHookEnv,
    msg_file: &Path,
    no_verify: bool,
) -> Result<HookRunOutcome> {
    if no_verify {
        return Ok(HookRunOutcome::Skipped);
    }
    let msg_str = msg_file.to_string_lossy().into_owned();
    run_hook_command(repo, cfg, "commit-msg", &[&msg_str], None, Some(env_ctx))
}

/// Runs `post-commit` after a commit is recorded. Following Git semantics, a non-zero exit
/// from `post-commit` does not fail the already-recorded commit, returning `Ok` with any
/// captured output.
pub fn run_post_commit(
    repo: &OzoneRepo,
    cfg: &GitConfig,
    env_ctx: &CommitHookEnv,
) -> HookRunOutcome {
    match run_hook_command(repo, cfg, "post-commit", &[], None, Some(env_ctx)) {
        Ok(outcome) => outcome,
        Err(_) => HookRunOutcome::Skipped,
    }
}

/// Runs `pre-push <remote_name> <remote_url>` with stdin lines formatted per Git, unless
/// `no_verify` is `true`.
pub fn run_pre_push(
    repo: &OzoneRepo,
    cfg: &GitConfig,
    remote_name: &str,
    remote_url: &str,
    ref_lines: &[PrePushRefLine],
    no_verify: bool,
) -> Result<HookRunOutcome> {
    if no_verify {
        return Ok(HookRunOutcome::Skipped);
    }
    let mut stdin_payload = String::new();
    for line in ref_lines {
        stdin_payload.push_str(&line.format_line());
    }
    run_hook_command(
        repo,
        cfg,
        "pre-push",
        &[remote_name, remote_url],
        Some(stdin_payload.as_bytes()),
        None,
    )
}

fn run_hook_command(
    repo: &OzoneRepo,
    cfg: &GitConfig,
    hook_name: &str,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
    commit_env: Option<&CommitHookEnv>,
) -> Result<HookRunOutcome> {
    let hooks_dir = resolve_hooks_dir(repo, cfg);
    let Some(hook_path) = find_executable_hook(&hooks_dir, hook_name) else {
        return Ok(HookRunOutcome::Skipped);
    };

    let output = execute_hook_script(repo, &hook_path, args, stdin_bytes, commit_env)?;
    let combined = combine_hook_output(&output.stdout, &output.stderr);

    if output.status.success() {
        return Ok(HookRunOutcome::Passed {
            hook: hook_name.to_string(),
            hook_path,
            output: combined,
        });
    }

    let exit_code = output.status.code();
    let code_label = exit_code.map_or_else(|| "signal".to_string(), |c| c.to_string());
    let detail = if combined.is_empty() {
        format!(
            "{hook_name} hook ({}) failed with exit code {code_label}",
            hook_path.display()
        )
    } else {
        format!(
            "{hook_name} hook ({}) failed with exit code {code_label}:\n{combined}",
            hook_path.display()
        )
    };

    let bypass_cmd = match hook_name {
        "pre-push" => "oz push --no-verify".to_string(),
        _ => "oz commit --no-verify -m \"...\"".to_string(),
    };

    Err(OzError::HookFailed {
        hook: hook_name.to_string(),
        hook_exit_code: exit_code,
        message: detail,
        recovery: vec![
            format!("Fix the {hook_name} hook failure and retry."),
            format!("Or bypass verification hooks with `{bypass_cmd}`."),
        ],
    })
}

fn execute_hook_script(
    repo: &OzoneRepo,
    hook_path: &Path,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
    commit_env: Option<&CommitHookEnv>,
) -> Result<std::process::Output> {
    let run_with_program = |program: &Path, extra_first_arg: Option<&Path>| {
        let mut cmd = Command::new(program);
        if let Some(script) = extra_first_arg {
            cmd.arg(script);
        }
        cmd.args(args);
        cmd.current_dir(&repo.workdir_path);
        cmd.env("GIT_DIR", &repo.git_dir);
        cmd.env("GIT_INDEX_FILE", repo.git_dir.join("index"));
        cmd.env("GIT_PREFIX", "");
        if let Some(cenv) = commit_env {
            if let Some(ref name) = cenv.author_name {
                cmd.env("GIT_AUTHOR_NAME", name);
            }
            if let Some(ref email) = cenv.author_email {
                cmd.env("GIT_AUTHOR_EMAIL", email);
            }
            if let Some(ref date) = cenv.author_date {
                cmd.env("GIT_AUTHOR_DATE", date);
            }
        }
        if stdin_bytes.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd
    };

    let mut child = match run_with_program(hook_path, None).spawn() {
        Ok(child) => child,
        #[cfg(unix)]
        Err(err) if err.raw_os_error() == Some(libc::ENOEXEC) => {
            run_with_program(Path::new("/bin/sh"), Some(hook_path)).spawn()?
        }
        Err(err) => return Err(OzError::Io(err)),
    };

    if let Some(input) = stdin_bytes
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(input);
    }

    child.wait_with_output().map_err(OzError::Io)
}

fn combine_hook_output(stdout: &[u8], stderr: &[u8]) -> String {
    let out_s = String::from_utf8_lossy(stdout).trim().to_string();
    let err_s = String::from_utf8_lossy(stderr).trim().to_string();
    match (out_s.is_empty(), err_s.is_empty()) {
        (true, true) => String::new(),
        (false, true) => out_s,
        (true, false) => err_s,
        (false, false) => format!("{out_s}\n{err_s}"),
    }
}

fn expand_tilde_env(raw: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    if raw == "~"
        && let Some(h) = home
    {
        return h;
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(h) = home
    {
        return h.join(rest);
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_config::GitConfigContext;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, OzoneRepo) {
        let tmp = TempDir::new().unwrap();
        let status = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(tmp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let repo = OzoneRepo::discover(Some(tmp.path())).unwrap();
        (tmp, repo)
    }

    #[cfg(unix)]
    fn write_exec(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn resolves_core_hooks_path_relative_and_skips_non_executable() {
        let (tmp, repo) = init_repo();
        let custom_dir = tmp.path().join(".husky").join("_");
        let hook = custom_dir.join("pre-commit");
        fs::create_dir_all(&custom_dir).unwrap();
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();

        let cfg = GitConfig::parse_str(
            "[core]\n\thooksPath = .husky/_\n",
            None,
            &GitConfigContext::default(),
        )
        .unwrap();

        // Not executable yet -> skipped cleanly
        let skipped = run_pre_commit(&repo, &cfg, &CommitHookEnv::default(), false).unwrap();
        assert_eq!(skipped, HookRunOutcome::Skipped);

        // Now mark executable -> fails with HookFailed (exit code 1)
        write_exec(&hook, "#!/bin/sh\necho 'lint error' >&2\nexit 3\n");
        let err = run_pre_commit(&repo, &cfg, &CommitHookEnv::default(), false).unwrap_err();
        assert_eq!(err.error_code(), "check_failed");
        assert_eq!(err.exit_code().as_i32(), 1);
        assert!(err.to_string().contains("lint error"));

        // Bypassed with no_verify = true
        let bypassed = run_pre_commit(&repo, &cfg, &CommitHookEnv::default(), true).unwrap();
        assert_eq!(bypassed, HookRunOutcome::Skipped);
    }

    #[cfg(unix)]
    #[test]
    fn prepare_and_commit_msg_receive_args_and_env() {
        let (_tmp, repo) = init_repo();
        let hooks = repo.git_common_dir.join("hooks");
        write_exec(
            &hooks.join("prepare-commit-msg"),
            "#!/bin/sh\nprintf '\\nPrepared-By: %s\\n' \"$2\" >> \"$1\"\n",
        );
        write_exec(
            &hooks.join("commit-msg"),
            "#!/bin/sh\nprintf 'Author: %s <%s>\\n' \"$GIT_AUTHOR_NAME\" \"$GIT_AUTHOR_EMAIL\" >> \"$1\"\n",
        );
        let cfg = GitConfig::default();
        let msg_path = repo.git_dir.join("COMMIT_EDITMSG");
        fs::write(&msg_path, "feat: test\n").unwrap();
        let env_ctx = CommitHookEnv {
            author_name: Some("Ada".to_string()),
            author_email: Some("ada@example.com".to_string()),
            author_date: Some("@1700000000 +0000".to_string()),
        };

        run_prepare_commit_msg(&repo, &cfg, &env_ctx, &msg_path, "message", None).unwrap();
        run_commit_msg(&repo, &cfg, &env_ctx, &msg_path, false).unwrap();

        let body = fs::read_to_string(&msg_path).unwrap();
        assert!(body.contains("Prepared-By: message"), "{body}");
        assert!(body.contains("Author: Ada <ada@example.com>"), "{body}");
    }
}
