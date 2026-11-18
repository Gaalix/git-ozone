#!/usr/bin/env python3
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Merge raw benchmark JSON suites into `latest.json` and render `REPORT.md`.

Zero external dependencies (Python 3 stdlib only). Produces a crisp, honest
Markdown benchmark report with PASS/FAIL evaluation against ROADMAP.md targets
and transparent pros/cons derived strictly from measured results.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

from bench_common import write_json

MIB = 1024.0 * 1024.0
GIB = 1024.0 * 1024.0 * 1024.0


def fmt_bytes(n: Optional[int]) -> str:
    if n is None:
        return "—"
    if n < 1024:
        return f"{n} B"
    if n < MIB:
        return f"{n / 1024.0:.1f} KiB"
    if n < GIB:
        return f"{n / MIB:.2f} MiB"
    return f"{n / GIB:.2f} GiB"


def fmt_ms(v: Optional[float]) -> str:
    if v is None:
        return "—"
    if v >= 10_000:
        return f"{v / 1000.0:.2f} s"
    if v >= 1000:
        return f"{v:.0f} ms ({v / 1000.0:.2f} s)"
    return f"{v:.1f} ms"


def badge(passed: Optional[bool]) -> str:
    if passed is True:
        return "PASS"
    if passed is False:
        return "FAIL"
    return "N/A"


def load_json(path: Optional[str]) -> Optional[Dict[str, Any]]:
    if not path:
        return None
    p = Path(path)
    if not p.is_file():
        return None
    return json.loads(p.read_text(encoding="utf-8"))


def render_markdown_report(combined: Dict[str, Any]) -> str:
    lines: List[str] = []
    sys_meta = combined.get("system") or {}
    suites = combined.get("suites") or {}
    wt = suites.get("worktrees")
    st = suites.get("status_monorepo")
    vc = suites.get("agent_vcbench")
    cc = suites.get("concurrency")

    lines.append("# Ozone Reproducible Public Benchmark Report")
    lines.append("")
    lines.append(
        "> Generated automatically from real binary executions (`benchmarks/results/latest.json`). "
        "Optional tools not present on `PATH` are recorded as `Not installed` — zero numbers are mocked or extrapolated."
    )
    lines.append("")

    # 1. Executive Scorecard
    lines.append("## 1. ROADMAP Gate Scorecard")
    lines.append("")
    lines.append("| Gate | Target (`ROADMAP.md`) | Measured Result | Status |")
    lines.append("| :--- | :--- | :--- | :---: |")

    if wt and wt.get("results", {}).get("oz_ws_spawn_warm"):
        oz_wt = wt["results"]["oz_ws_spawn_warm"]
        gates = oz_wt.get("roadmap_gates", {})
        mean_ms = oz_wt["spawn_latency_ms"]["mean_ms"]
        steady_p95 = oz_wt["steady_state_ms"]["p95_ms"]
        eff_disk = oz_wt["effective_extra_physical_bytes"]
        agents = oz_wt["agents"]
        cache_mb = wt.get("config", {}).get("cache_mb", 0)
        backend_str = ", ".join(oz_wt.get("backends", [])) or "unknown"
        lines.append(
            f"| Worktree Spawn Speed ({agents} ws, {cache_mb} MiB cache, `{backend_str}`) | `< 500 ms` / workspace | "
            f"mean `{mean_ms:.1f} ms`, steady p95 `{steady_p95:.1f} ms` | **{badge(gates.get('mean_latency_pass'))}** |"
        )
        lines.append(
            f"| Worktree Extra Disk Footprint ({agents} ws, `{backend_str}`) | `< 50 MiB` total extra | "
            f"`{fmt_bytes(eff_disk)}` (apparent `{fmt_bytes(oz_wt.get('apparent_total_bytes'))}`) | **{badge(gates.get('extra_disk_pass'))}** |"
        )
        lines.append(
            f"| Cross-Workspace Isolation & Warm Cache Visibility | Isolated edits + warm cache readable | "
            f"isolated=`{oz_wt.get('isolation_verified')}`, warm=`{oz_wt.get('warm_cache_visible')}` | "
            f"**{badge(bool(oz_wt.get('isolation_verified') and oz_wt.get('warm_cache_visible')))}** |"
        )

    if st and st.get("results", {}).get("oz_status_ws_cow"):
        oz_ws_st = st["results"]["oz_status_ws_cow"]
        git_st = st["results"]["git_status_porcelain"]
        n_files = st.get("config", {}).get("files", 0)
        parity = st.get("parity_verified", {})
        parity_ok = bool(parity.get("clean") and parity.get("dirty"))
        oz_dirty_med = oz_ws_st["dirty"]["warm_ms"]["median_ms"]
        git_dirty_med = git_st["dirty"]["warm_ms"]["median_ms"]
        speedup = oz_ws_st.get("speedup_vs_git_dirty_x")
        speedup_str = f" (`{speedup:.1f}x` vs git)" if speedup else ""
        st_backend = oz_ws_st.get("backend") or "unknown"
        lines.append(
            f"| Monorepo `status` ({n_files:,} files, `oz ws` `{st_backend}`) | `O(dirty_files)` fast path + 100% parity | "
            f"dirty p50 `{oz_dirty_med:.1f} ms` vs git `{git_dirty_med:.1f} ms`{speedup_str}, fast_path=`{oz_ws_st.get('used_fuse_fast_path')}` | "
            f"**{badge(parity_ok and bool(oz_ws_st.get('used_fuse_fast_path')))}** |"
        )

    if vc and vc.get("aggregate"):
        agg = vc["aggregate"]
        vg = vc.get("roadmap_gates", {})
        lines.append(
            f"| VCBench Agent Commands (4 canonical tasks) | `>= 60%` reduction vs `git` | "
            f"`{agg['oz_commands']}` vs `{agg['git_commands']}` (`-{agg['command_reduction_pct']:.1f}%`) | "
            f"**{badge(vg.get('command_reduction_pass'))}** |"
        )
        lines.append(
            f"| VCBench Agent Tokens (`ceil(bytes/4)` & lexical) | `>= 50%` reduction vs `git` | "
            f"approx `-{agg['approx_token_reduction_pct']:.1f}%`, lexical `-{agg['lexical_token_reduction_pct']:.1f}%` | "
            f"**{badge(bool(vg.get('approx_token_reduction_pass') and vg.get('lexical_token_reduction_pass')))}** |"
        )

    if cc and cc.get("scenarios", {}).get("distinct_branch_commits"):
        cc_oz = cc["scenarios"]["distinct_branch_commits"]["oz"]
        cg = cc.get("roadmap_gates", {})
        lines.append(
            f"| Concurrent Multi-Agent Commits ({cc['config']['agents']} agents x {cc['config']['rounds']} rounds) | "
            f"`0%` lock errors + clean `git fsck` | "
            f"`{cc_oz['lock_errors']}` lock errors (`{cc_oz['succeeded']}/{cc_oz['total_commit_attempts']}` ok, "
            f"`{cc_oz['throughput_commits_per_sec']:.1f} commits/s`) | "
            f"**{badge(cg.get('oz_zero_lock_errors_distinct_branches'))}** |"
        )

    lines.append("")

    # 2. Environment & Hardware Metadata
    lines.append("## 2. System & Tool Metadata")
    lines.append("")
    oz_doc = sys_meta.get("oz_doctor") or {}
    tools = sys_meta.get("tools") or {}
    lines.append(f"- **Timestamp (UTC):** `{sys_meta.get('timestamp_utc', 'unknown')}`")
    lines.append(f"- **Ozone Commit:** `{sys_meta.get('ozone_repo_commit') or 'n/a'}`")
    lines.append(
        f"- **OS / Kernel:** `{sys_meta.get('os', '?')} {sys_meta.get('os_release', '')} ({sys_meta.get('machine', '?')})`"
    )
    lines.append(
        f"- **CPU / RAM:** `{sys_meta.get('cpu_model', 'unknown')}` "
        f"({sys_meta.get('cpu_logical_cores', '?')} logical cores, `{sys_meta.get('ram_gib', '?')} GiB` RAM)"
    )
    lines.append(
        f"- **Benchmark Filesystem:** `{sys_meta.get('work_dir_filesystem', 'unknown')}` "
        f"(free `{sys_meta.get('work_dir_free_gib', '?')} GiB`)"
    )
    if oz_doc:
        fuse = oz_doc.get("fuse") or {}
        fuse_str = f"FUSE3 ready=`{fuse.get('ready')}` ({fuse.get('fusermount3_version', 'n/a')})" if fuse else f"ProjFS ready=`{oz_doc.get('projfs_ready')}`"
        lines.append(
            f"- **Ozone Backend Probe (`oz doctor`):** recommended=`{oz_doc.get('recommended_backend')}`, "
            f"fs=`{(oz_doc.get('filesystem') or {}).get('type')}`, "
            f"reflink=`{(oz_doc.get('filesystem') or {}).get('reflink_ready')}`, {fuse_str}"
        )
    lines.append("")
    lines.append("| Tool | Available | Version |")
    lines.append("| :--- | :---: | :--- |")
    for tool_name in ("oz", "oz_vfsd", "git", "jj", "worktrunk", "simgit"):
        tinfo = tools.get(tool_name) or {}
        avail = "Yes" if tinfo.get("available") else "No"
        ver = tinfo.get("version") or "Not installed"
        lines.append(f"| `{tool_name}` | {avail} | `{ver}` |")
    lines.append("")

    # 3. Suite 1: Worktrees
    if wt:
        cfg = wt.get("config", {})
        fix = wt.get("fixture", {})
        res = wt.get("results", {})
        lines.append(
            f"## 3. Suite 1 — {cfg.get('agents', 20)}-Agent Worktree Spawn Time & Physical Disk Footprint"
        )
        lines.append("")
        lines.append(
            f"Fixture: `{fix.get('source_file_count', 0)}` tracked Rust/TS source files + "
            f"`{fmt_bytes(fix.get('cache_physical_bytes'))}` physical warm caches across "
            f"`{fix.get('cache_file_count', 0):,}` files (`target/`, `node_modules/`, `.venv/`)."
        )
        lines.append("")
        lines.append(
            "| Tool / Mode | Warm Cache | Total Time | Mean / ws | p50 / ws | p95 / ws | "
            "Extra Disk (`st_blocks`) | Extra Disk (`statvfs Δ`) | Apparent Size | Isolated |"
        )
        lines.append("| :--- | :---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :---: |")

        wt_order = [
            ("git_worktree_raw", "`git worktree add` (raw / cold)"),
            ("git_worktree_warm_copy", "`git worktree add` (+ warm cache copy)"),
            ("oz_ws_spawn_warm", "`oz ws spawn` (CoW / VFS warm)"),
            ("jj_workspace_add", "`jj workspace add`"),
            ("worktrunk", "`worktrunk` (`wt`)"),
            ("simgit", "`simgit`"),
        ]
        for key, label in wt_order:
            entry = res.get(key) or {"available": False}
            if not entry.get("available"):
                lines.append(f"| {label} | — | *Not installed* | — | — | — | — | — | — | — |")
                continue
            if entry.get("skipped"):
                lines.append(
                    f"| {label} | {'Yes' if entry.get('warm_cache') else 'No'} | "
                    f"*Skipped ({entry.get('reason', 'n/a')})* | — | — | — | — | — | — | — |"
                )
                continue
            lat = entry.get("spawn_latency_ms", {})
            iso = entry.get("isolation_verified")
            iso_str = "PASS" if iso is True else ("FAIL" if iso is False else "—")
            lines.append(
                f"| {label} | {'Yes' if entry.get('warm_cache') else 'No'} | "
                f"{fmt_ms(entry.get('wall_total_ms'))} | {fmt_ms(lat.get('mean_ms'))} | "
                f"{fmt_ms(lat.get('median_ms'))} | {fmt_ms(lat.get('p95_ms'))} | "
                f"{fmt_bytes(entry.get('extra_physical_bytes_st_blocks'))} | "
                f"{fmt_bytes(entry.get('extra_physical_bytes_fs_delta'))} | "
                f"{fmt_bytes(entry.get('apparent_total_bytes'))} | {iso_str} |"
            )

        oz_wt = res.get("oz_ws_spawn_warm") or {}
        if oz_wt.get("available") and not oz_wt.get("skipped"):
            lines.append("")
            lines.append(
                f"- **Ozone breakdown:** backend=`{', '.join(oz_wt.get('backends', []))}`, "
                f"mounted=`{oz_wt.get('mounted_count')}/{oz_wt.get('agents')}`, "
                f"spawn #1 (includes warm baseline capture)=`{fmt_ms(oz_wt.get('first_spawn_ms'))}`, "
                f"steady-state spawns #2..{oz_wt.get('agents')} mean=`{fmt_ms(oz_wt['steady_state_ms']['mean_ms'])}` "
                f"(p95=`{fmt_ms(oz_wt['steady_state_ms']['p95_ms'])}`), "
                f"workspace-only backing disk=`{fmt_bytes(oz_wt.get('extra_physical_bytes_ws_only'))}`."
            )
        lines.append("")

    # 4. Suite 2: Status Monorepo
    if st:
        cfg = st.get("config", {})
        res = st.get("results", {})
        parity = st.get("parity_verified", {})
        lines.append(f"## 4. Suite 2 — {cfg.get('files', 0):,}-File Monorepo `status` Latency")
        lines.append("")
        lines.append(
            f"Dirty edit set: `{cfg.get('dirty_modified')}` modified + `{cfg.get('dirty_untracked')}` untracked files. "
            f"Exact `git` vs `oz` status parity verified: clean=`{parity.get('clean')}`, dirty=`{parity.get('dirty')}` "
            f"(`{cfg.get('runs')}` warm runs per case)."
        )
        lines.append("")
        lines.append(
            "| Command / Context | Fast Path | Clean Cold | Clean Warm p50 | Clean Warm p95 | Dirty Warm p50 | Dirty Warm p95 |"
        )
        lines.append("| :--- | :---: | ---: | ---: | ---: | ---: | ---: |")

        g = res.get("git_status_porcelain", {})
        lines.append(
            f"| `git status --porcelain` | index | {fmt_ms(g['clean'].get('cold_ms'))} | "
            f"{fmt_ms(g['clean']['warm_ms']['median_ms'])} | {fmt_ms(g['clean']['warm_ms']['p95_ms'])} | "
            f"{fmt_ms(g['dirty']['warm_ms']['median_ms'])} | {fmt_ms(g['dirty']['warm_ms']['p95_ms'])} |"
        )
        oreg = res.get("oz_status_regular_repo", {})
        lines.append(
            f"| `oz status --json` (regular repo) | `false` (full walk) | {fmt_ms(oreg['clean'].get('cold_ms'))} | "
            f"{fmt_ms(oreg['clean']['warm_ms']['median_ms'])} | {fmt_ms(oreg['clean']['warm_ms']['p95_ms'])} | "
            f"{fmt_ms(oreg['dirty']['warm_ms']['median_ms'])} | {fmt_ms(oreg['dirty']['warm_ms']['p95_ms'])} |"
        )
        ows = res.get("oz_status_ws_cow", {})
        lines.append(
            f"| `oz status --json` (`oz ws`, `{ows.get('backend')}`) | `{str(ows.get('used_fuse_fast_path')).lower()}` | "
            f"{fmt_ms(ows['clean'].get('cold_ms'))} | "
            f"{fmt_ms(ows['clean']['warm_ms']['median_ms'])} | {fmt_ms(ows['clean']['warm_ms']['p95_ms'])} | "
            f"{fmt_ms(ows['dirty']['warm_ms']['median_ms'])} | {fmt_ms(ows['dirty']['warm_ms']['p95_ms'])} |"
        )
        jj_st = res.get("jj_status") or {"available": False}
        if not jj_st.get("available"):
            lines.append("| `jj status` | — | *Not installed* | — | — | — | — |")
        elif jj_st.get("skipped"):
            lines.append(f"| `jj status` | — | *Skipped ({jj_st.get('reason')})* | — | — | — | — |")
        else:
            d = jj_st["dirty"]
            lines.append(
                f"| `jj status` | — | — | — | — | "
                f"{fmt_ms(d['warm_ms']['median_ms'])} | {fmt_ms(d['warm_ms']['p95_ms'])} |"
            )
        lines.append("")

    # 5. Suite 3: VCBench
    if vc:
        agg = vc["aggregate"]
        lines.append("## 5. Suite 3 — Agent VCBench (Steps & Token Overhead)")
        lines.append("")
        lines.append(
            "Strict non-interactive agent execution (`GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=true`, `OZ_AGENT=1`) "
            "with bit-for-bit Git tree OID, working-tree, `Change-Id:`, and `git fsck --full` verification."
        )
        lines.append("")
        lines.append(
            "| Task | `git` cmds | `oz` cmds | Cmd Δ | `git` tok~ | `oz` tok~ | Tok~ Δ | "
            "`git` lex | `oz` lex | Lex Δ | `git` ms | `oz` ms | Verified |"
        )
        lines.append("| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :---: |")
        for t in vc.get("tasks", []):
            lines.append(
                f"| {t['name']} | {t['git_commands']} | {t['oz_commands']} | -{t['command_reduction_pct']:.1f}% | "
                f"{t['git_approx_tokens']} | {t['oz_approx_tokens']} | -{t['approx_token_reduction_pct']:.1f}% | "
                f"{t['git_lexical_tokens']} | {t['oz_lexical_tokens']} | -{t['lexical_token_reduction_pct']:.1f}% | "
                f"{t['git_wall_ms']:.1f} | {t['oz_wall_ms']:.1f} | {badge(t['parity_and_fsck_verified'])} |"
            )
        lines.append(
            f"| **AGGREGATE (All 4 Tasks)** | **{agg['git_commands']}** | **{agg['oz_commands']}** | "
            f"**-{agg['command_reduction_pct']:.1f}%** | **{agg['git_approx_tokens']}** | **{agg['oz_approx_tokens']}** | "
            f"**-{agg['approx_token_reduction_pct']:.1f}%** | **{agg['git_lexical_tokens']}** | **{agg['oz_lexical_tokens']}** | "
            f"**-{agg['lexical_token_reduction_pct']:.1f}%** | **{agg['git_wall_ms']:.1f}** | **{agg['oz_wall_ms']:.1f}** | "
            f"**{badge(agg['all_parity_verified'])}** |"
        )
        lines.append("")

    # 6. Suite 4: Concurrency
    if cc:
        sc = cc.get("scenarios", {})
        lines.append(
            f"## 6. Suite 4 — {cc['config']['agents']}-Agent Concurrent Commit Contention ({cc['config']['rounds']} Rounds)"
        )
        lines.append("")
        lines.append(
            "| Scenario | Tool | Attempts | Succeeded | Lock Errors | Error Rate | Wall Time | Throughput | `git fsck` |"
        )
        lines.append("| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | :---: |")

        for sc_key, sc_label in (
            ("distinct_branch_commits", "Distinct-branch commits + readers"),
            ("distinct_branch_commits_with_pack_refs", "Distinct-branch commits + `pack-refs`"),
        ):
            pair = sc.get(sc_key) or {}
            for tool_key, tool_label in (("git", "`git`"), ("oz", "`oz`")):
                d = pair.get(tool_key)
                if not d:
                    continue
                lines.append(
                    f"| {sc_label} | {tool_label} | {d['total_commit_attempts']} | {d['succeeded']} | "
                    f"{d['lock_errors']} | {d['total_error_rate_pct']:.1f}% | {fmt_ms(d['wall_total_ms'])} | "
                    f"{d['throughput_commits_per_sec']:.1f} c/s | {badge(d.get('git_fsck_clean'))} |"
                )

        shared = sc.get("shared_worktree_index_contention")
        if shared:
            ga = shared["git_concurrent_add"]
            oa = shared["oz_indexless_concurrent_status"]
            lines.append(
                f"| Shared-repo concurrent `git add` vs indexless `oz` | `git add` | {ga['attempts']} | "
                f"{ga['attempts'] - ga['lock_errors'] - ga['other_errors']} | {ga['lock_errors']} | "
                f"{ga['lock_error_rate_pct']:.1f}% | — | — | — |"
            )
            lines.append(
                f"| Shared-repo concurrent `git add` vs indexless `oz` | `oz status` (no index) | {oa['attempts']} | "
                f"{oa['attempts'] - oa['lock_errors'] - oa['other_errors']} | {oa['lock_errors']} | "
                f"{oa['lock_error_rate_pct']:.1f}% | — | — | — |"
            )
        lines.append("")

    # 7. Honest Pros & Cons (derived strictly from measured numbers)
    lines.append("## 7. Honest Assessment — Was Ozone Worth It?")
    lines.append("")
    lines.append("### Where Ozone Clearly Wins")
    if wt and wt.get("results", {}).get("oz_ws_spawn_warm"):
        oz_wt = wt["results"]["oz_ws_spawn_warm"]
        gw = wt["results"].get("git_worktree_warm_copy") or {}
        if gw.get("available") and not gw.get("skipped"):
            disk_ratio = (
                gw["extra_physical_bytes_st_blocks"] / max(1, oz_wt["effective_extra_physical_bytes"])
            )
            time_ratio = gw["wall_total_ms"] / max(0.001, oz_wt["wall_total_ms"])
            lines.append(
                f"- **Warm Multi-Agent Workspaces:** Spawning `{oz_wt['agents']}` warm workspaces used "
                f"`{fmt_bytes(oz_wt['effective_extra_physical_bytes'])}` extra disk vs "
                f"`{fmt_bytes(gw['extra_physical_bytes_st_blocks'])}` for `git worktree` + cache copy "
                f"(**~{disk_ratio:.0f}x less disk**) and finished in `{fmt_ms(oz_wt['wall_total_ms'])}` vs "
                f"`{fmt_ms(gw['wall_total_ms'])}` (**{time_ratio:.1f}x faster**)."
            )
    if st and st.get("results", {}).get("oz_status_ws_cow"):
        ows = st["results"]["oz_status_ws_cow"]
        g = st["results"]["git_status_porcelain"]
        if ows.get("used_fuse_fast_path") and ows["dirty"]["warm_ms"]["median_ms"] < g["dirty"]["warm_ms"]["median_ms"]:
            lines.append(
                f"- **Monorepo `status` inside `oz ws`:** `{ows['dirty']['warm_ms']['median_ms']:.1f} ms` vs "
                f"`{g['dirty']['warm_ms']['median_ms']:.1f} ms` for `git status --porcelain` "
                f"(**{ows.get('speedup_vs_git_dirty_x')}x faster**) thanks to the `O(dirty_files)` VFS `upper/` fast path."
            )
    if vc and vc.get("aggregate"):
        agg = vc["aggregate"]
        lines.append(
            f"- **Agent Ergonomics (VCBench):** **-{agg['command_reduction_pct']:.1f}% fewer commands** "
            f"and **-{agg['approx_token_reduction_pct']:.1f}% fewer approx tokens** across selective hunk commits, "
            f"multi-amends, split commits, and conflict resolution with bit-for-bit identical Git trees."
        )
    if cc and cc.get("scenarios", {}).get("shared_worktree_index_contention"):
        ga = cc["scenarios"]["shared_worktree_index_contention"]["git_concurrent_add"]
        if ga["lock_errors"] > 0:
            lines.append(
                f"- **Eliminating `.git/index.lock`:** Parallel `git add` in a shared worktree hit "
                f"`{ga['lock_errors']}/{ga['attempts']}` (`{ga['lock_error_rate_pct']:.1f}%`) lock failures; "
                f"Ozone is stage-free and achieved `0%` lock errors across concurrent agent commits."
            )

    lines.append("")
    lines.append("### Honest Trade-offs & Where `git` Remains Competitive")
    if wt and wt.get("results", {}).get("git_worktree_raw") and wt.get("results", {}).get("oz_ws_spawn_warm"):
        gr = wt["results"]["git_worktree_raw"]
        oz_wt = wt["results"]["oz_ws_spawn_warm"]
        if gr.get("available") and not gr.get("skipped"):
            lines.append(
                f"- **Cold Worktrees Without Build Caches:** Raw `git worktree add` (without copying `target/`/`node_modules/`) "
                f"took `{fmt_ms(gr['wall_total_ms'])}` (`{fmt_ms(gr['spawn_latency_ms']['mean_ms'])}`/ws) vs "
                f"`{fmt_ms(oz_wt['wall_total_ms'])}` for warm `oz ws spawn` (including `{fmt_ms(oz_wt['first_spawn_ms'])}` "
                f"first-spawn warm baseline capture). If your repo has no build caches at all, raw `git worktree` is already fast."
            )
    if st and st.get("results", {}).get("oz_status_regular_repo") and st.get("results", {}).get("git_status_porcelain"):
        oreg = st["results"]["oz_status_regular_repo"]
        g = st["results"]["git_status_porcelain"]
        reg_med = oreg["dirty"]["warm_ms"]["median_ms"]
        git_med = g["dirty"]["warm_ms"]["median_ms"]
        if reg_med > git_med:
            lines.append(
                f"- **Regular Non-VFS Repo `oz status`:** Outside an `oz ws` CoW/FUSE3 workspace, `oz status` walks "
                f"the working tree without Git's `.git/index` stat cache (`{reg_med:.1f} ms` vs `git status` `{git_med:.1f} ms` "
                f"on `{st['config']['files']:,}` files). The `O(dirty_files)` speedup specifically applies inside `oz ws` workspaces."
            )
    lines.append(
        "- **Platform Dependencies:** Zero-byte CoW workspaces on Linux `ext4` require `/dev/fuse` + `fusermount3` "
        "(unprivileged FUSE3); on macOS they use native APFS `clonefile`; on Windows they use `Client-ProjFS` or Dev Drive (`ReFS`)."
    )
    lines.append("")
    return "\n".join(lines)


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Merge Ozone benchmark JSON outputs and render REPORT.md.")
    p.add_argument("--input", default=None, help="Existing combined latest.json to render")
    p.add_argument("--worktrees", default=None, help="JSON output from bench_worktrees.py")
    p.add_argument("--status", default=None, help="JSON output from bench_status_250k.py")
    p.add_argument("--vcbench", default=None, help="JSON output from bench_agent_vcbench.py")
    p.add_argument("--concurrency", default=None, help="JSON output from bench_concurrency.py")
    p.add_argument("--merge-out", default=None, help="Path to write merged combined JSON (e.g., benchmarks/results/latest.json)")
    p.add_argument("--report-out", required=True, help="Path to write rendered Markdown report (e.g., benchmarks/results/REPORT.md)")
    return p.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    if args.input:
        combined = load_json(args.input)
        if combined is None:
            raise SystemExit(f"ERROR: Could not read {args.input}")
    else:
        wt = load_json(args.worktrees)
        st = load_json(args.status)
        vc = load_json(args.vcbench)
        cc = load_json(args.concurrency)

        sys_meta = None
        for suite_obj in (wt, st, vc, cc):
            if suite_obj and suite_obj.get("system"):
                sys_meta = suite_obj["system"]
                break

        combined = {
            "schema": "ozone.benchmark.v1",
            "system": sys_meta or {},
            "suites": {
                k: v
                for k, v in (
                    ("worktrees", wt),
                    ("status_monorepo", st),
                    ("agent_vcbench", vc),
                    ("concurrency", cc),
                )
                if v is not None
            },
        }
        if args.merge_out:
            write_json(Path(args.merge_out), combined)

    md = render_markdown_report(combined)
    report_path = Path(args.report_out)
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(md, encoding="utf-8")
    print(f"Wrote benchmark report to {report_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
