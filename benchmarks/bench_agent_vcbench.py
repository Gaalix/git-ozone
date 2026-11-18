#!/usr/bin/env python3
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Benchmark Suite 3: VCBench-Style Autonomous Agent Evaluation (`git` vs `oz`).

Runs the 4 canonical deterministic non-interactive VCS workflows directly against
the real `git` and `oz` binaries on paired identical repositories:
  1. Selective Hunk Commits (4 dirty files, 3-hunk file -> commit `f2` + `h1a`)
  2. Multi-Amends (3 iterative refinement rounds preserving `Change-Id:` & local debug edits)
  3. Split Commits (entangled 5-file / 3-hunk tree -> 3 clean atomic commits)
  4. Conflict Resolution (integrate 2 conflicting branches into `main`, resolve `c1` ours & `c2` theirs)

Meters exact command counts, UTF-8 bytes, `ceil(utf8_bytes / 4)` approx LLM tokens,
lexical tokens, and wall-clock latency, while verifying bit-for-bit identical Git
commit trees, identical remaining working trees, preserved `Change-Id:`, and clean
`git fsck --full`.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

from bench_common import (
    REPO_ROOT,
    Sandbox,
    build_base_env,
    choose_work_dir_base,
    collect_system_metadata,
    discover_binaries,
    init_git_repo,
    log,
    run_cmd,
    write_json,
)

ROADMAP_CMD_REDUCTION_TARGET_PCT = 60.0
ROADMAP_TOKEN_REDUCTION_TARGET_PCT = 50.0


def count_lexical_tokens(text: str) -> int:
    """Matches `crates/oz-cli/tests/m3_exit_gates.rs::count_lexical_tokens` exactly."""
    count = 0
    in_ident = False
    for ch in text:
        if ch.isspace():
            in_ident = False
        elif ch.isalnum() or ch == "_":
            if not in_ident:
                count += 1
                in_ident = True
        else:
            in_ident = False
            count += 1
    return count


def approx_llm_tokens(utf8_bytes: int) -> int:
    return (utf8_bytes + 3) // 4


def render_shell_cmd(prog: str, args: Sequence[str]) -> str:
    parts = [prog]
    for arg in args:
        if any(c.isspace() for c in arg) or '"' in arg or arg == "":
            escaped = arg.replace('"', '\\"')
            parts.append(f'"{escaped}"')
        else:
            parts.append(arg)
    return " ".join(parts)


def extract_single_hunk_patch(full_file_diff: str, hunk_zero_idx: int) -> str:
    """Extract a single valid unified diff hunk for non-interactive `git apply --cached -`."""
    header_lines: List[str] = []
    hunks: List[List[str]] = []
    current_hunk: Optional[List[str]] = None

    for line in full_file_diff.splitlines(keepends=True):
        if line.startswith("@@ "):
            if current_hunk is not None:
                hunks.append(current_hunk)
            current_hunk = [line]
        elif current_hunk is not None:
            current_hunk.append(line)
        else:
            header_lines.append(line)
    if current_hunk is not None:
        hunks.append(current_hunk)

    if hunk_zero_idx >= len(hunks):
        raise RuntimeError(
            f"Requested hunk {hunk_zero_idx} out of {len(hunks)} hunks in diff:\n{full_file_diff}"
        )
    patch = "".join(header_lines) + "".join(hunks[hunk_zero_idx])
    if not patch.endswith("\n"):
        patch += "\n"
    return patch


def render_three_section_rust_module(
    module_title: str,
    sec_a_body: str,
    sec_b_body: str,
    sec_c_body: str,
) -> str:
    """Identical ~115-line Rust module fixture from `m3_exit_gates.rs` with 3 separated hunk sections."""
    lines: List[str] = [
        f"//! {module_title} — production service module.",
        "//!",
        "//! Maintains strict invariants across request validation, execution,",
        "//! and telemetry reporting within the multi-tenant workspace engine.",
        "",
        "#[derive(Debug, Clone, PartialEq, Eq)]",
        "pub struct ServiceContext {",
        "    pub tenant_id: String,",
        "    pub request_id: String,",
        "    pub now_epoch_secs: u64,",
        "    pub max_skew_secs: u64,",
        "    pub region: String,",
        "}",
        "",
        "/// Primary section A entrypoint.",
        *sec_a_body.splitlines(),
        "",
    ]

    for idx in range(1, 8):
        lines.extend(
            [
                f"pub fn invariant_guard_{idx}(ctx: &ServiceContext) -> Result<&str, &'static str> {{",
                "    if ctx.tenant_id.trim().is_empty() {",
                '        return Err("tenant_id must not be empty");',
                "    }",
                "    Ok(ctx.region.as_str())",
                "}",
            ]
        )
    lines.extend(["", "/// Secondary section B handler.", *sec_b_body.splitlines(), ""])

    for idx in range(8, 15):
        lines.extend(
            [
                f"pub fn normalize_header_slot_{idx}(raw: &str) -> Option<String> {{",
                "    let trimmed = raw.trim();",
                "    if trimmed.is_empty() {",
                "        return None;",
                "    }",
                "    Some(trimmed.to_ascii_lowercase())",
                "}",
            ]
        )
    lines.extend(["", "/// Tertiary section C finalize routine.", *sec_c_body.splitlines(), ""])
    return "\n".join(lines)


def write_rel(repo_dir: Path, rel: str, content: str) -> None:
    p = repo_dir / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content, encoding="utf-8")


def snapshot_worktree_files(root: Path) -> Dict[str, bytes]:
    out: Dict[str, bytes] = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d != ".git"]
        for fn in filenames:
            full = Path(dirpath) / fn
            rel = full.relative_to(root).as_posix()
            out[rel] = full.read_bytes()
    return out


def assert_git_fsck_clean(repo_dir: Path, git_bin: str, env: Dict[str, str]) -> None:
    run_cmd([git_bin, "fsck", "--full"], cwd=repo_dir, env=env, check=True)


@dataclass
class InvocationRecord:
    cmd: str
    exit_code: int
    utf8_bytes: int
    approx_tokens: int
    lexical_tokens: int
    elapsed_ms: float
    stdout: str
    stderr: str


@dataclass
class AgentHarness:
    label: str
    git_bin: str
    oz_bin: str
    env: Dict[str, str]
    invocations: List[InvocationRecord] = field(default_factory=list)

    def exec_git(
        self,
        cwd: Path,
        args: Sequence[str],
        stdin_payload: Optional[str] = None,
        expect_success: bool = True,
    ) -> InvocationRecord:
        rendered = render_shell_cmd("git", args)
        code, stdout, stderr, elapsed_ms = run_cmd(
            [self.git_bin, *args],
            cwd=cwd,
            env=self.env,
            stdin_text=stdin_payload,
            check=False,
        )
        if expect_success and code != 0:
            raise RuntimeError(f"[{self.label}] `{rendered}` failed (exit {code}):\nstdout: {stdout}\nstderr: {stderr}")
        return self._record(rendered, stdin_payload or "", code, stdout, stderr, elapsed_ms)

    def exec_oz(
        self,
        cwd: Path,
        args: Sequence[str],
        expect_success: bool = True,
    ) -> InvocationRecord:
        rendered = render_shell_cmd("oz", args)
        code, stdout, stderr, elapsed_ms = run_cmd(
            [self.oz_bin, *args],
            cwd=cwd,
            env=self.env,
            check=False,
        )
        if expect_success and code != 0:
            raise RuntimeError(f"[{self.label}] `{rendered}` failed (exit {code}):\nstdout: {stdout}\nstderr: {stderr}")
        return self._record(rendered, "", code, stdout, stderr, elapsed_ms)

    def _record(
        self,
        cmd_line: str,
        stdin_str: str,
        code: int,
        stdout: str,
        stderr: str,
        elapsed_ms: float,
    ) -> InvocationRecord:
        utf8_bytes = (
            len(cmd_line.encode("utf-8"))
            + len(stdin_str.encode("utf-8"))
            + len(stdout.encode("utf-8"))
            + len(stderr.encode("utf-8"))
        )
        rec = InvocationRecord(
            cmd=cmd_line,
            exit_code=code,
            utf8_bytes=utf8_bytes,
            approx_tokens=approx_llm_tokens(utf8_bytes),
            lexical_tokens=(
                count_lexical_tokens(cmd_line)
                + count_lexical_tokens(stdin_str)
                + count_lexical_tokens(stdout)
                + count_lexical_tokens(stderr)
            ),
            elapsed_ms=elapsed_ms,
            stdout=stdout,
            stderr=stderr,
        )
        self.invocations.append(rec)
        return rec

    @property
    def command_count(self) -> int:
        return len(self.invocations)

    @property
    def total_utf8_bytes(self) -> int:
        return sum(i.utf8_bytes for i in self.invocations)

    @property
    def total_approx_tokens(self) -> int:
        return sum(i.approx_tokens for i in self.invocations)

    @property
    def total_lexical_tokens(self) -> int:
        return sum(i.lexical_tokens for i in self.invocations)

    @property
    def total_wall_ms(self) -> float:
        return sum(i.elapsed_ms for i in self.invocations)


def init_paired_repos(
    sandbox: Sandbox,
    prefix: str,
    git_bin: str,
    oz_bin: str,
    env: Dict[str, str],
) -> Tuple[Path, Path]:
    git_dir = sandbox.root / f"{prefix}-git"
    oz_dir = sandbox.root / f"{prefix}-oz"
    for d in (git_dir, oz_dir):
        init_git_repo(d, git_bin=git_bin, env=env, user_name="VCBench Agent", user_email="agent@vcbench.dev")
    # Ensure `.git/ozone/` metadata is initialized cleanly via `oz setup --json`
    run_cmd([oz_bin, "setup", "--json"], cwd=oz_dir, env=env)
    sandbox.register_oz_repo(oz_dir)
    return git_dir, oz_dir


def summarize_task(task_id: str, name: str, git_h: AgentHarness, oz_h: AgentHarness, verified: bool) -> Dict[str, Any]:
    cmd_red = 100.0 * (1.0 - (oz_h.command_count / git_h.command_count))
    approx_red = 100.0 * (1.0 - (oz_h.total_approx_tokens / git_h.total_approx_tokens))
    lex_red = 100.0 * (1.0 - (oz_h.total_lexical_tokens / git_h.total_lexical_tokens))
    return {
        "id": task_id,
        "name": name,
        "git_commands": git_h.command_count,
        "oz_commands": oz_h.command_count,
        "command_reduction_pct": round(cmd_red, 2),
        "git_utf8_bytes": git_h.total_utf8_bytes,
        "oz_utf8_bytes": oz_h.total_utf8_bytes,
        "git_approx_tokens": git_h.total_approx_tokens,
        "oz_approx_tokens": oz_h.total_approx_tokens,
        "approx_token_reduction_pct": round(approx_red, 2),
        "git_lexical_tokens": git_h.total_lexical_tokens,
        "oz_lexical_tokens": oz_h.total_lexical_tokens,
        "lexical_token_reduction_pct": round(lex_red, 2),
        "git_wall_ms": round(git_h.total_wall_ms, 2),
        "oz_wall_ms": round(oz_h.total_wall_ms, 2),
        "parity_and_fsck_verified": verified,
    }


def run_task1_selective_hunk_commits(
    sb: Sandbox, git_bin: str, oz_bin: str, env: Dict[str, str]
) -> Dict[str, Any]:
    git_dir, oz_dir = init_paired_repos(sb, "t1", git_bin, oz_bin, env)

    base_session = render_three_section_rust_module(
        "Session Token Validator",
        "pub fn validate_session_expiry(ctx: &ServiceContext, expires_at: u64) -> bool {\n    expires_at > ctx.now_epoch_secs\n}",
        "pub fn trace_session_lookup(ctx: &ServiceContext) -> usize {\n    ctx.request_id.len()\n}",
        "pub fn compute_retry_budget(_attempts: u32) -> u32 {\n    3\n}",
    )
    base_invoice = render_three_section_rust_module(
        "Billing Invoice Ledger",
        "pub fn subtotal_cents(units: u64, unit_price: u64) -> u64 {\n    units.saturating_mul(unit_price)\n}",
        "pub fn apply_tax_bps(amount: u64, bps: u64) -> u64 {\n    amount + (amount * bps) / 10_000\n}",
        'pub fn invoice_currency() -> &\'static str {\n    "USD"\n}',
    )
    base_metrics = render_three_section_rust_module(
        "Telemetry Metrics Exporter",
        'pub fn metric_namespace() -> &\'static str {\n    "ozone.vcs"\n}',
        "pub fn sample_rate_pct() -> u8 {\n    10\n}",
        "pub fn flush_interval_ms() -> u64 {\n    5_000\n}",
    )

    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/auth/src/session.rs", base_session)
        write_rel(repo, "crates/billing/src/invoice.rs", base_invoice)
        write_rel(repo, "crates/telemetry/src/metrics.rs", base_metrics)
        run_cmd([git_bin, "add", "-A"], cwd=repo, env=env)
        run_cmd([git_bin, "commit", "-q", "-m", "chore: seed baseline service modules"], cwd=repo, env=env)

    dirty_session = render_three_section_rust_module(
        "Session Token Validator",
        "pub fn validate_session_expiry(ctx: &ServiceContext, expires_at: u64) -> bool {\n    let effective_deadline = expires_at.saturating_add(ctx.max_skew_secs);\n    effective_deadline > ctx.now_epoch_secs\n}",
        'pub fn trace_session_lookup(ctx: &ServiceContext) -> usize {\n    eprintln!("DEBUG lookup tenant={} req={}", ctx.tenant_id, ctx.request_id);\n    ctx.request_id.len()\n}',
        "pub fn compute_retry_budget(attempts: u32) -> u32 {\n    // WIP experimental exponential backoff cap\n    (3u32.saturating_add(attempts)).min(12)\n}",
    )
    new_pkce_verifier = (
        "//! RFC 7636 PKCE S256 challenge verifier for OAuth2 native & agent flows.\n\n"
        "#[derive(Debug, Clone, PartialEq, Eq)]\n"
        "pub struct PkceVerifier {\n"
        "    verifier: String,\n"
        "}\n\n"
        "impl PkceVerifier {\n"
        "    pub fn new(raw: impl Into<String>) -> Result<Self, &'static str> {\n"
        "        let s = raw.into();\n"
        "        if !(43..=128).contains(&s.len()) {\n"
        '            return Err("PKCE code_verifier length must be between 43 and 128 bytes");\n'
        "        }\n"
        "        if !s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')) {\n"
        '            return Err("PKCE code_verifier contains invalid RFC 7636 characters");\n'
        "        }\n"
        "        Ok(Self { verifier: s })\n"
        "    }\n\n"
        "    pub fn as_str(&self) -> &str {\n"
        "        &self.verifier\n"
        "    }\n"
        "}\n"
    )
    dirty_invoice = base_invoice.replace(
        "units.saturating_mul(unit_price)",
        "units.saturating_mul(unit_price).saturating_sub(500) // WIP promo coupon",
    )
    dirty_metrics = base_metrics.replace("    10", "    100 // local debug full sampling")

    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/auth/src/session.rs", dirty_session)
        write_rel(repo, "crates/auth/src/verifier.rs", new_pkce_verifier)
        write_rel(repo, "crates/billing/src/invoice.rs", dirty_invoice)
        write_rel(repo, "crates/telemetry/src/metrics.rs", dirty_metrics)

    commit_msg = "feat(auth): honor max clock skew and add PKCE verifier"

    # Ozone agent
    oz_h = AgentHarness("oz/task1", git_bin, oz_bin, env)
    st = oz_h.exec_oz(oz_dir, ["status"])
    assert "[f1] M  crates/auth/src/session.rs" in st.stdout
    assert "hunks: h1a, h1b, h1c" in st.stdout
    cm = oz_h.exec_oz(oz_dir, ["commit", "-m", commit_msg, "--pick", "f2", "h1a"])
    assert "2 file(s) committed (3 dirty remaining" in cm.stdout

    # Git agent
    git_h = AgentHarness("git/task1", git_bin, oz_bin, env)
    git_h.exec_git(git_dir, ["status"])
    diff_out = git_h.exec_git(git_dir, ["diff", "crates/auth/src/session.rs"])
    h1a_patch = extract_single_hunk_patch(diff_out.stdout, 0)
    git_h.exec_git(git_dir, ["add", "crates/auth/src/verifier.rs"])
    git_h.exec_git(git_dir, ["apply", "--cached", "-"], stdin_payload=h1a_patch)
    git_h.exec_git(git_dir, ["diff", "--cached", "--stat"])
    git_h.exec_git(git_dir, ["commit", "-m", commit_msg])
    git_h.exec_git(git_dir, ["status"])

    # Verify bit-for-bit tree OIDs, remaining working trees, and git fsck
    _, oz_tree, _, _ = run_cmd([git_bin, "rev-parse", "HEAD^{tree}"], cwd=oz_dir, env=env)
    _, git_tree, _, _ = run_cmd([git_bin, "rev-parse", "HEAD^{tree}"], cwd=git_dir, env=env)
    assert oz_tree.strip() == git_tree.strip(), "Task 1 committed tree OID mismatch"
    assert snapshot_worktree_files(oz_dir) == snapshot_worktree_files(git_dir), "Task 1 worktree mismatch"
    assert_git_fsck_clean(oz_dir, git_bin, env)
    assert_git_fsck_clean(git_dir, git_bin, env)

    return summarize_task("task1_selective_hunk", "1. Selective Hunk Commits", git_h, oz_h, True)


def run_task2_multi_amends(
    sb: Sandbox, git_bin: str, oz_bin: str, env: Dict[str, str]
) -> Dict[str, Any]:
    git_dir, oz_dir = init_paired_repos(sb, "t2", git_bin, oz_bin, env)

    base_limiter = render_three_section_rust_module(
        "Token Bucket Rate Limiter",
        "pub fn allow_request(tokens: u64, cost: u64) -> bool {\n    tokens >= cost\n}",
        "pub fn debug_bucket_state(tokens: u64) -> u64 {\n    tokens\n}",
        "pub fn refill_rate_per_sec() -> u64 {\n    50\n}",
    )
    base_middleware = render_three_section_rust_module(
        "HTTP Rate-Limit Middleware",
        "pub fn enforce_limit(allowed: bool) -> u16 {\n    if allowed { 200 } else { 429 }\n}",
        "pub fn default_burst_capacity() -> u64 {\n    100\n}",
        'pub fn middleware_name() -> &\'static str {\n    "rate_limit_v1"\n}',
    )
    base_probe = render_three_section_rust_module(
        "Local Diagnostic Probe",
        "pub fn local_probe_enabled() -> bool {\n    false\n}",
        "pub fn probe_port() -> u16 {\n    9099\n}",
        'pub fn probe_tag() -> &\'static str {\n    "prod"\n}',
    )

    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/api/src/limiter.rs", base_limiter)
        write_rel(repo, "crates/api/src/middleware.rs", base_middleware)
        write_rel(repo, "crates/api/src/probe.rs", base_probe)
        run_cmd([git_bin, "add", "-A"], cwd=repo, env=env)
        run_cmd([git_bin, "commit", "-q", "-m", "chore: seed rate limiter baseline"], cwd=repo, env=env)
        run_cmd([git_bin, "checkout", "-q", "-b", "feat/rate-limiter"], cwd=repo, env=env)

    _, root_main_oz, _, _ = run_cmd([git_bin, "rev-parse", "main"], cwd=oz_dir, env=env)
    _, root_main_git, _, _ = run_cmd([git_bin, "rev-parse", "main"], cwd=git_dir, env=env)

    v1_limiter = render_three_section_rust_module(
        "Token Bucket Rate Limiter",
        "pub fn allow_request(tokens: u64, cost: u64) -> bool {\n    cost > 0 && tokens >= cost\n}",
        "pub fn debug_bucket_state(tokens: u64) -> u64 {\n    tokens\n}",
        "pub fn refill_rate_per_sec() -> u64 {\n    50\n}",
    )
    initial_change_id = "I7f8e9d0c1b2a3f4e5d6c7b8a9f0e1d2c3b4a5968"
    initial_msg = f"feat(api): add non-zero cost check to token bucket\n\nChange-Id: {initial_change_id}"
    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/api/src/limiter.rs", v1_limiter)
        run_cmd([git_bin, "add", "crates/api/src/limiter.rs"], cwd=repo, env=env)
        run_cmd([git_bin, "commit", "-q", "-m", initial_msg], cwd=repo, env=env)

    debug_sec_b = (
        "pub fn debug_bucket_state(tokens: u64) -> u64 {\n"
        '    eprintln!("LOCAL_DEBUG_TRACE remaining_tokens={tokens}");\n'
        "    tokens\n"
        "}"
    )
    dirty_probe = base_probe.replace("    false", "    true // local bench harness")

    oz_h = AgentHarness("oz/task2", git_bin, oz_bin, env)
    git_h = AgentHarness("git/task2", git_bin, oz_bin, env)

    # Round 1
    r1_limiter = render_three_section_rust_module(
        "Token Bucket Rate Limiter",
        "pub fn allow_request(tokens: u64, cost: u64) -> bool {\n    let capped_cost = cost.max(1);\n    tokens >= capped_cost\n}",
        debug_sec_b,
        "pub fn refill_rate_per_sec() -> u64 {\n    50\n}",
    )
    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/api/src/limiter.rs", r1_limiter)
        write_rel(repo, "crates/api/src/probe.rs", dirty_probe)

    oz_h.exec_oz(oz_dir, ["status"])
    oz_h.exec_oz(oz_dir, ["commit", "--amend", "-m", "feat(api): clamp minimum request cost in token bucket", "--pick", "h1a"])

    git_h.exec_git(git_dir, ["status"])
    git_h.exec_git(git_dir, ["log", "-1", "--format=%B"])
    g_diff1 = git_h.exec_git(git_dir, ["diff", "crates/api/src/limiter.rs"])
    g_patch1 = extract_single_hunk_patch(g_diff1.stdout, 0)
    git_h.exec_git(git_dir, ["apply", "--cached", "-"], stdin_payload=g_patch1)
    git_h.exec_git(git_dir, ["commit", "--amend", "--no-edit"])
    git_h.exec_git(git_dir, ["status"])

    # Round 2
    r2_headers = (
        "//! Standard RFC 6585 `Retry-After` & `X-RateLimit-Remaining` header builder.\n\n"
        "pub fn format_retry_after_secs(deficit_tokens: u64, refill_per_sec: u64) -> String {\n"
        "    let secs = deficit_tokens.div_ceil(refill_per_sec.max(1)).max(1);\n"
        "    secs.to_string()\n"
        "}\n"
    )
    r2_middleware = base_middleware.replace(
        "if allowed { 200 } else { 429 }",
        "if allowed { 200 } else { 429 /* Retry-After attached */ }",
    )
    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/api/src/headers.rs", r2_headers)
        write_rel(repo, "crates/api/src/middleware.rs", r2_middleware)

    oz_h.exec_oz(oz_dir, ["status"])
    oz_h.exec_oz(
        oz_dir,
        ["commit", "--amend", "-m", "feat(api): add Retry-After formatter and middleware wiring", "--pick", "f1", "f3"],
    )

    git_h.exec_git(git_dir, ["status"])
    git_h.exec_git(git_dir, ["add", "crates/api/src/headers.rs", "crates/api/src/middleware.rs"])
    git_h.exec_git(git_dir, ["diff", "--cached", "--stat"])
    git_h.exec_git(git_dir, ["commit", "--amend", "--no-edit"])
    git_h.exec_git(git_dir, ["status"])

    # Round 3
    r3_limiter = render_three_section_rust_module(
        "Token Bucket Rate Limiter",
        "pub fn allow_request(tokens: u64, cost: u64) -> bool {\n    let capped_cost = cost.max(1).min(10_000);\n    tokens >= capped_cost\n}",
        debug_sec_b,
        "pub fn refill_rate_per_sec() -> u64 {\n    50\n}",
    )
    r3_middleware = r2_middleware.replace("    100", "    250")
    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/api/src/limiter.rs", r3_limiter)
        write_rel(repo, "crates/api/src/middleware.rs", r3_middleware)

    st3 = oz_h.exec_oz(oz_dir, ["status"])
    assert "[f1] M  crates/api/src/limiter.rs" in st3.stdout
    assert "hunks: h1a, h1b" in st3.stdout
    oz_h.exec_oz(
        oz_dir,
        ["commit", "--amend", "-m", "feat(api): finalize bounded token bucket and middleware headers", "--pick", "h1a", "f2"],
    )

    git_h.exec_git(git_dir, ["status"])
    g_diff3 = git_h.exec_git(git_dir, ["diff", "crates/api/src/limiter.rs"])
    g_patch3 = extract_single_hunk_patch(g_diff3.stdout, 0)
    git_h.exec_git(git_dir, ["apply", "--cached", "-"], stdin_payload=g_patch3)
    git_h.exec_git(git_dir, ["add", "crates/api/src/middleware.rs"])
    git_h.exec_git(git_dir, ["commit", "--amend", "--no-edit"])
    git_h.exec_git(git_dir, ["status"])

    # Verify parent, Change-Id preservation, identical trees, identical dirty working trees, and git fsck
    _, oz_parent, _, _ = run_cmd([git_bin, "rev-parse", "HEAD^"], cwd=oz_dir, env=env)
    _, git_parent, _, _ = run_cmd([git_bin, "rev-parse", "HEAD^"], cwd=git_dir, env=env)
    assert oz_parent.strip() == root_main_oz.strip()
    assert git_parent.strip() == root_main_git.strip()

    _, oz_msg, _, _ = run_cmd([git_bin, "log", "-1", "--format=%B"], cwd=oz_dir, env=env)
    _, git_msg, _, _ = run_cmd([git_bin, "log", "-1", "--format=%B"], cwd=git_dir, env=env)
    assert f"Change-Id: {initial_change_id}" in oz_msg
    assert f"Change-Id: {initial_change_id}" in git_msg

    _, oz_tree, _, _ = run_cmd([git_bin, "rev-parse", "HEAD^{tree}"], cwd=oz_dir, env=env)
    _, git_tree, _, _ = run_cmd([git_bin, "rev-parse", "HEAD^{tree}"], cwd=git_dir, env=env)
    assert oz_tree.strip() == git_tree.strip(), "Task 2 final tree OID mismatch"
    assert snapshot_worktree_files(oz_dir) == snapshot_worktree_files(git_dir), "Task 2 worktree mismatch"
    assert_git_fsck_clean(oz_dir, git_bin, env)
    assert_git_fsck_clean(git_dir, git_bin, env)

    return summarize_task("task2_multi_amends", "2. Multi-Amends (3x)", git_h, oz_h, True)


def run_task3_split_commits(
    sb: Sandbox, git_bin: str, oz_bin: str, env: Dict[str, str]
) -> Dict[str, Any]:
    git_dir, oz_dir = init_paired_repos(sb, "t3", git_bin, oz_bin, env)

    base_settings = render_three_section_rust_module(
        "Global Application Settings",
        "pub fn db_pool_timeout_ms() -> u64 {\n    3_000\n}",
        "pub fn webhook_signing_enabled() -> bool {\n    false\n}",
        "pub fn audit_export_batch_size() -> usize {\n    100\n}",
    )
    base_audit = render_three_section_rust_module(
        "Audit Log Exporter",
        "pub fn next_batch_limit() -> usize {\n    100\n}",
        'pub fn export_format() -> &\'static str {\n    "jsonl"\n}',
        "pub fn compression_level() -> u8 {\n    3\n}",
    )
    base_db = render_three_section_rust_module(
        "Database Connection Pool",
        "pub fn acquire_timeout_ms() -> u64 {\n    3_000\n}",
        "pub fn max_connections() -> u32 {\n    32\n}",
        "pub fn idle_reap_secs() -> u64 {\n    120\n}",
    )
    base_dispatcher = render_three_section_rust_module(
        "Webhook Event Dispatcher",
        'pub fn build_delivery_headers(event_id: &str) -> Vec<(&\'static str, String)> {\n    vec![("X-Ozone-Event", event_id.to_string())]\n}',
        "pub fn max_delivery_attempts() -> u8 {\n    5\n}",
        'pub fn delivery_user_agent() -> &\'static str {\n    "Ozone-Webhooks/1.0"\n}',
    )

    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/app_config/src/settings.rs", base_settings)
        write_rel(repo, "crates/audit/src/exporter.rs", base_audit)
        write_rel(repo, "crates/db/src/pool.rs", base_db)
        write_rel(repo, "crates/webhooks/src/dispatcher.rs", base_dispatcher)
        run_cmd([git_bin, "add", "-A"], cwd=repo, env=env)
        run_cmd([git_bin, "commit", "-q", "-m", "chore: initial platform crates"], cwd=repo, env=env)

    entangled_settings = render_three_section_rust_module(
        "Global Application Settings",
        "pub fn db_pool_timeout_ms() -> u64 {\n    7_500\n}",
        "pub fn webhook_signing_enabled() -> bool {\n    true\n}",
        "pub fn audit_export_batch_size() -> usize {\n    500\n}",
    )
    dirty_db = base_db.replace("    3_000", "    7_500 // aligned with app_config")
    dirty_dispatcher = base_dispatcher.replace(
        'vec![("X-Ozone-Event", event_id.to_string())]',
        'vec![("X-Ozone-Event", event_id.to_string()), ("X-Ozone-Signature-256", format!("sha256={event_id}"))]',
    )
    new_signature = (
        "//! Deterministic webhook payload signature header helper.\n\n"
        "pub fn compute_signature_tag(secret_id: &str, payload_len: usize) -> String {\n"
        '    format!("v1:{secret_id}:{payload_len}")\n'
        "}\n"
    )
    dirty_audit = base_audit.replace("    100", "    500 // high-throughput SOC2 export batch")

    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/app_config/src/settings.rs", entangled_settings)
        write_rel(repo, "crates/audit/src/exporter.rs", dirty_audit)
        write_rel(repo, "crates/db/src/pool.rs", dirty_db)
        write_rel(repo, "crates/webhooks/src/dispatcher.rs", dirty_dispatcher)
        write_rel(repo, "crates/webhooks/src/signature.rs", new_signature)

    msg_a = "feat(db): raise connection pool acquire timeout to 7.5s"
    msg_b = "feat(webhooks): enable HMAC signature headers on dispatch"
    msg_c = "feat(audit): increase SOC2 JSONL export batch size to 500"

    # Ozone agent (4 commands total)
    oz_h = AgentHarness("oz/task3", git_bin, oz_bin, env)
    st = oz_h.exec_oz(oz_dir, ["status"])
    assert "[f1] M  crates/app_config/src/settings.rs" in st.stdout
    assert "hunks: h1a, h1b, h1c" in st.stdout
    oz_h.exec_oz(oz_dir, ["commit", "-m", msg_a, "--pick", "f3", "h1a"])
    oz_h.exec_oz(oz_dir, ["commit", "-m", msg_b, "--pick", "f3", "f4", "h1a"])
    oz_h.exec_oz(oz_dir, ["commit", "-m", msg_c, "--pick", "f1", "f2"])

    # Git agent
    git_h = AgentHarness("git/task3", git_bin, oz_bin, env)
    git_h.exec_git(git_dir, ["status"])
    diff_s1 = git_h.exec_git(git_dir, ["diff", "crates/app_config/src/settings.rs"])
    patch_s1 = extract_single_hunk_patch(diff_s1.stdout, 0)
    git_h.exec_git(git_dir, ["apply", "--cached", "-"], stdin_payload=patch_s1)
    git_h.exec_git(git_dir, ["add", "crates/db/src/pool.rs"])
    git_h.exec_git(git_dir, ["diff", "--cached", "--stat"])
    git_h.exec_git(git_dir, ["commit", "-m", msg_a])

    diff_s2 = git_h.exec_git(git_dir, ["diff", "crates/app_config/src/settings.rs"])
    patch_s2 = extract_single_hunk_patch(diff_s2.stdout, 0)
    git_h.exec_git(git_dir, ["apply", "--cached", "-"], stdin_payload=patch_s2)
    git_h.exec_git(git_dir, ["add", "crates/webhooks/src/dispatcher.rs", "crates/webhooks/src/signature.rs"])
    git_h.exec_git(git_dir, ["diff", "--cached", "--stat"])
    git_h.exec_git(git_dir, ["commit", "-m", msg_b])

    git_h.exec_git(git_dir, ["add", "crates/app_config/src/settings.rs", "crates/audit/src/exporter.rs"])
    git_h.exec_git(git_dir, ["commit", "-m", msg_c])
    final_st = git_h.exec_git(git_dir, ["status"])
    assert "working tree clean" in final_st.stdout

    for rev in ("HEAD~2^{tree}", "HEAD~1^{tree}", "HEAD^{tree}"):
        _, oz_t, _, _ = run_cmd([git_bin, "rev-parse", rev], cwd=oz_dir, env=env)
        _, git_t, _, _ = run_cmd([git_bin, "rev-parse", rev], cwd=git_dir, env=env)
        assert oz_t.strip() == git_t.strip(), f"Task 3 tree OID mismatch at {rev}"
    assert snapshot_worktree_files(oz_dir) == snapshot_worktree_files(git_dir), "Task 3 worktree mismatch"
    assert_git_fsck_clean(oz_dir, git_bin, env)
    assert_git_fsck_clean(git_dir, git_bin, env)

    return summarize_task("task3_split_commits", "3. Split Commits (3-way)", git_h, oz_h, True)


def run_task4_conflict_resolution(
    sb: Sandbox, git_bin: str, oz_bin: str, env: Dict[str, str]
) -> Dict[str, Any]:
    git_dir, oz_dir = init_paired_repos(sb, "t4", git_bin, oz_bin, env)

    base_executor = render_three_section_rust_module(
        "Task Executor Engine",
        "pub fn max_parallel_workers() -> usize {\n    8\n}",
        "pub fn worker_stack_bytes() -> usize {\n    2 * 1024 * 1024\n}",
        "pub fn steal_batch_limit() -> usize {\n    16\n}",
    )
    base_policy = render_three_section_rust_module(
        "Runtime Isolation Policy",
        'pub fn isolation_tier() -> &\'static str {\n    "shared_default"\n}',
        "pub fn memory_ceiling_mb() -> u64 {\n    2_048\n}",
        "pub fn allow_network_egress() -> bool {\n    false\n}",
    )

    for repo in (git_dir, oz_dir):
        write_rel(repo, "crates/runtime/src/executor.rs", base_executor)
        write_rel(repo, "crates/runtime/src/policy.rs", base_policy)
        run_cmd([git_bin, "add", "-A"], cwd=repo, env=env)
        run_cmd([git_bin, "commit", "-q", "-m", "chore: seed runtime modules"], cwd=repo, env=env)

        run_cmd([git_bin, "checkout", "-q", "-b", "feat/cache-policy"], cwd=repo, env=env)
        write_rel(repo, "crates/runtime/src/executor.rs", base_executor.replace("    8", "    16 // cache-aware worker pool"))
        write_rel(repo, "crates/runtime/src/policy.rs", base_policy.replace('"shared_default"', '"warm_cache_shared"'))
        write_rel(repo, "crates/runtime/src/cache.rs", "pub fn lru_shards() -> usize {\n    64\n}\n")
        run_cmd([git_bin, "add", "-A"], cwd=repo, env=env)
        run_cmd([git_bin, "commit", "-q", "-m", "feat(runtime): tune cache worker pool"], cwd=repo, env=env)

        run_cmd([git_bin, "checkout", "-q", "-b", "feat/tenant-isolation", "main"], cwd=repo, env=env)
        write_rel(repo, "crates/runtime/src/executor.rs", base_executor.replace("    8", "    4 // strict per-tenant worker cap"))
        write_rel(repo, "crates/runtime/src/policy.rs", base_policy.replace('"shared_default"', '"strict_cow_sandbox"'))
        write_rel(repo, "crates/runtime/src/tenant.rs", "pub fn enforce_tenant_quota() -> bool {\n    true\n}\n")
        run_cmd([git_bin, "add", "-A"], cwd=repo, env=env)
        run_cmd([git_bin, "commit", "-q", "-m", "feat(runtime): strict tenant sandbox policy"], cwd=repo, env=env)

    # Ozone agent
    oz_h = AgentHarness("oz/task4", git_bin, oz_bin, env)
    int_out = oz_h.exec_oz(
        oz_dir,
        ["integrate", "feat/cache-policy", "feat/tenant-isolation", "--into", "main"],
        expect_success=False,
    )
    assert int_out.exit_code == 1, f"expected exit code 1 for conflict, got {int_out.exit_code}"
    assert "hint: oz resolve c1 --take ours" in int_out.stderr
    assert "hint: oz resolve c2 --take theirs" in int_out.stderr

    oz_sandbox_dir = sb.root / "t4-oz.oz" / "_integrate"
    sb.register_mount(oz_sandbox_dir)
    sandbox_diff = oz_h.exec_oz(oz_dir, ["-C", "../t4-oz.oz/_integrate", "diff"])
    assert "cache-aware worker pool" in sandbox_diff.stdout
    assert "strict_cow_sandbox" in sandbox_diff.stdout

    oz_h.exec_oz(oz_dir, ["resolve", "c1", "--take", "ours"])
    cont_out = oz_h.exec_oz(oz_dir, ["resolve", "c2", "--take", "theirs", "--continue"])
    assert "Integrated [feat/cache-policy, feat/tenant-isolation] into main" in cont_out.stdout

    # Git agent
    git_h = AgentHarness("git/task4", git_bin, oz_bin, env)
    git_h.exec_git(git_dir, ["status"])
    git_h.exec_git(git_dir, ["checkout", "main"])
    git_h.exec_git(git_dir, ["merge", "--no-edit", "feat/cache-policy"])
    merge2 = git_h.exec_git(git_dir, ["merge", "--no-edit", "feat/tenant-isolation"], expect_success=False)
    assert merge2.exit_code != 0
    git_h.exec_git(git_dir, ["status"])
    git_h.exec_git(
        git_dir,
        ["log", "--merge", "-p", "--", "crates/runtime/src/executor.rs", "crates/runtime/src/policy.rs"],
    )
    git_h.exec_git(git_dir, ["checkout", "--ours", "--", "crates/runtime/src/executor.rs"])
    git_h.exec_git(git_dir, ["checkout", "--theirs", "--", "crates/runtime/src/policy.rs"])
    git_h.exec_git(git_dir, ["add", "crates/runtime/src/executor.rs", "crates/runtime/src/policy.rs"])
    git_h.exec_git(git_dir, ["diff", "--cached", "--stat"])
    git_h.exec_git(git_dir, ["commit", "--no-edit"])
    git_h.exec_git(git_dir, ["status"])

    _, oz_main_tree, _, _ = run_cmd([git_bin, "rev-parse", "main^{tree}"], cwd=oz_dir, env=env)
    _, git_main_tree, _, _ = run_cmd([git_bin, "rev-parse", "main^{tree}"], cwd=git_dir, env=env)
    assert oz_main_tree.strip() == git_main_tree.strip(), "Task 4 integrated main tree mismatch"
    assert not oz_sandbox_dir.exists(), "_integrate sandbox must be cleaned up after --continue"
    assert_git_fsck_clean(oz_dir, git_bin, env)
    assert_git_fsck_clean(git_dir, git_bin, env)

    return summarize_task("task4_conflict_resolution", "4. Conflict Resolution", git_h, oz_h, True)


def run_benchmark(args: argparse.Namespace) -> Dict[str, Any]:
    bins = discover_binaries(args.oz_bin, args.vfsd_bin)
    if not bins["oz"]:
        raise SystemExit("ERROR: `oz` binary not found. Build with `cargo build --release` or pass --oz-bin.")
    if not bins["git"]:
        raise SystemExit("ERROR: `git` binary not found on PATH.")

    work_base = choose_work_dir_base(args.work_dir)
    env = build_base_env(bins)
    metadata = collect_system_metadata(work_base, bins)

    log(f"[vcbench] Running 4 canonical non-interactive agent tasks ({bins['oz']} vs {bins['git']})...")
    with Sandbox(prefix="ozb-vc-", base_dir=work_base, bins=bins, keep=args.keep_sandbox) as sb:
        tasks = [
            run_task1_selective_hunk_commits(sb, str(bins["git"]), str(bins["oz"]), env),
            run_task2_multi_amends(sb, str(bins["git"]), str(bins["oz"]), env),
            run_task3_split_commits(sb, str(bins["git"]), str(bins["oz"]), env),
            run_task4_conflict_resolution(sb, str(bins["git"]), str(bins["oz"]), env),
        ]

    tot_git_cmds = sum(t["git_commands"] for t in tasks)
    tot_oz_cmds = sum(t["oz_commands"] for t in tasks)
    tot_git_bytes = sum(t["git_utf8_bytes"] for t in tasks)
    tot_oz_bytes = sum(t["oz_utf8_bytes"] for t in tasks)
    tot_git_approx = sum(t["git_approx_tokens"] for t in tasks)
    tot_oz_approx = sum(t["oz_approx_tokens"] for t in tasks)
    tot_git_lex = sum(t["git_lexical_tokens"] for t in tasks)
    tot_oz_lex = sum(t["oz_lexical_tokens"] for t in tasks)
    tot_git_ms = sum(t["git_wall_ms"] for t in tasks)
    tot_oz_ms = sum(t["oz_wall_ms"] for t in tasks)

    agg_cmd_red = 100.0 * (1.0 - (tot_oz_cmds / tot_git_cmds))
    agg_approx_red = 100.0 * (1.0 - (tot_oz_approx / tot_git_approx))
    agg_lex_red = 100.0 * (1.0 - (tot_oz_lex / tot_git_lex))

    cargo_gate_check: Optional[Dict[str, Any]] = None
    if args.verify_cargo_gate:
        log("[vcbench] Cross-verifying against `cargo test --release -p oz-cli --test m3_exit_gates`...")
        rc, _, stderr, elapsed_ms = run_cmd(
            [
                "cargo",
                "test",
                "--release",
                "--locked",
                "-p",
                "oz-cli",
                "--test",
                "m3_exit_gates",
                "vcbench_agent_eval_exit_gate_commands_and_tokens",
                "--",
                "--nocapture",
            ],
            cwd=REPO_ROOT,
            env=env,
            check=False,
        )
        cargo_gate_check = {
            "passed": rc == 0,
            "exit_code": rc,
            "elapsed_ms": round(elapsed_ms, 2),
            "stderr_tail": stderr.strip().splitlines()[-5:] if rc != 0 else [],
        }

    return {
        "suite": "agent_vcbench",
        "system": metadata,
        "tasks": tasks,
        "aggregate": {
            "git_commands": tot_git_cmds,
            "oz_commands": tot_oz_cmds,
            "command_reduction_pct": round(agg_cmd_red, 2),
            "git_utf8_bytes": tot_git_bytes,
            "oz_utf8_bytes": tot_oz_bytes,
            "git_approx_tokens": tot_git_approx,
            "oz_approx_tokens": tot_oz_approx,
            "approx_token_reduction_pct": round(agg_approx_red, 2),
            "git_lexical_tokens": tot_git_lex,
            "oz_lexical_tokens": tot_oz_lex,
            "lexical_token_reduction_pct": round(agg_lex_red, 2),
            "git_wall_ms": round(tot_git_ms, 2),
            "oz_wall_ms": round(tot_oz_ms, 2),
            "all_parity_verified": all(t["parity_and_fsck_verified"] for t in tasks),
        },
        "roadmap_gates": {
            "command_reduction_target_pct": ROADMAP_CMD_REDUCTION_TARGET_PCT,
            "token_reduction_target_pct": ROADMAP_TOKEN_REDUCTION_TARGET_PCT,
            "command_reduction_pass": agg_cmd_red >= ROADMAP_CMD_REDUCTION_TARGET_PCT,
            "approx_token_reduction_pass": agg_approx_red >= ROADMAP_TOKEN_REDUCTION_TARGET_PCT,
            "lexical_token_reduction_pass": agg_lex_red >= ROADMAP_TOKEN_REDUCTION_TARGET_PCT,
        },
        "cargo_gate_cross_check": cargo_gate_check,
    }


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Run deterministic VCBench agent evaluation (git vs oz) on 4 canonical multi-commit & conflict tasks."
    )
    p.add_argument("--oz-bin", default=None, help="Path to `oz` binary (auto-detected if omitted)")
    p.add_argument("--vfsd-bin", default=None, help="Path to `oz-vfsd` binary (auto-detected if omitted)")
    p.add_argument("--work-dir", default=None, help="Base directory for temporary benchmark sandboxes")
    p.add_argument("--output", default=None, help="Optional JSON output file path")
    p.add_argument("--verify-cargo-gate", action="store_true", help="Also cross-run Rust m3_exit_gates VCBench test")
    p.add_argument("--keep-sandbox", action="store_true", help="Keep sandbox directory after run")
    return p.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    payload = run_benchmark(args)
    if args.output:
        write_json(Path(args.output), payload)
        log(f"[vcbench] Wrote JSON results to {args.output}")
    else:
        print(json.dumps(payload, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
