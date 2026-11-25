# Ozone JSON Schema Reference (`oz.v1`)

Every Ozone CLI command supports the global `--json` flag, emitting a deterministic, versioned `oz.v1` JSON envelope on `stdout`. Formal Draft 2020-12 JSON Schema definitions live in [`docs/schemas/oz.v1.envelope.schema.json`](schemas/oz.v1.envelope.schema.json).

---

## 1. Canonical `oz.v1` Envelope

```json
{
  "schema": "oz.v1",
  "ok": true,
  "op_id": "op_01J...",
  "workspace": "agent-auth",
  "branch": "feat/pkce",
  "head_oid": "def456...",
  "data": {},
  "warnings": [],
  "conflicts": [],
  "recovery_commands": ["oz undo op_01J..."]
}
```

### Top-Level Fields

| Field | Type | Always Present | Description |
| :--- | :--- | :---: | :--- |
| `schema` | `"oz.v1"` | Yes | Schema version identifier. |
| `ok` | `boolean` | Yes | `true` on success; `false` on error, conflict, guardrail trip, or failing CI check. |
| `op_id` | `string \| null` | Yes | MVCC OpLog entry ID (`op_...`) recorded by mutating commands; `null` for read-only commands. |
| `workspace` | `string \| null` | Yes | Active spawned Ozone workspace name, or `null` in the main repository worktree. |
| `branch` | `string \| null` | Yes | Short active branch name (e.g., `"main"`), or `null` if detached/unborn/non-repo. |
| `head_oid` | `string \| null` | Yes | Full 40-character hex commit OID of `HEAD` (or target commit), or `null` if unborn/non-repo. |
| `data` | `object` | Yes | Command-specific payload. Empty object `{}` on fatal errors (for `oz ci --json` failures, `data` still contains the complete `CiReport`). |
| `warnings` | `WarningEntry[]` | Yes | Plain strings or structured `{ "code": string, "message": string, "path"?: string }` diagnostics (e.g., `out_of_scope`). |
| `conflicts` | `ConflictEntry[]` | Yes | Structured conflict entries with deterministic IDs (`c1`, `c2`, ...), `path`, `message`, optional `base`/`ours`/`theirs` snippets, and `suggested_commands`. |
| `recovery_commands` | `string[]` | Yes | Concrete next-step or undo CLI commands agents/users can run directly. |
| `error` | `EnvelopeError` | Only when `ok == false` | `{ "code": string, "exit_code": 1 \| 2, "message": string }`. Omitted on success. |

---

## 2. Exit Code Taxonomy & Error Codes

Ozone enforces a strict 3-value exit code contract across all commands:

| Exit Code | Meaning | Triggering Conditions |
| :---: | :--- | :--- |
| **`0`** | Success | Command completed cleanly (including passing `oz ci` or clean `oz radar`). |
| **`1`** | Conflict or Check Failure | Merge/rebase conflicts (`conflict`), `--verify` failure or failing CI checks (`check_failed`). |
| **`2`** | Usage, Config, or Guardrail Error | Missing/invalid flags (`invalid_usage`), not a Git repo (`not_a_repository`), bad `.ozone.toml` (`invalid_config`), secret/conflict-marker/size/scope guardrail trip (`guardrail_violation`), disconnected mount (`mount_unavailable`), missing workspace (`workspace_not_found`), missing binary (`binary_not_found`), or Git/I/O/JSON errors. |

### Machine-Readable `error.code` Values

`not_a_repository`, `invalid_config`, `invalid_usage`, `guardrail_violation`, `conflict`, `check_failed`, `mount_unavailable`, `workspace_not_found`, `binary_not_found`, `git_error`, `io_error`, `json_error`, `internal_error`.

### Non-Interactive & Agent Contract

Non-interactive mode activates automatically when **any** of these is true: `--json`, `--agent`, `OZ_AGENT=1` (`1`/`true`/`yes`/`on`), `CI=1`, or non-TTY `stdin`/`stdout`. In non-interactive mode:
* Ozone never spawns `$EDITOR`, `$PAGER`/`less`, or interactive stdin prompts.
* Delegated child processes automatically receive `GIT_TERMINAL_PROMPT=0`, `GIT_PAGER=cat`, `PAGER=cat`, `GCM_INTERACTIVE=never`, and `OZ_AGENT=1`.

---

## 3. Deterministic Identifiers

| Identifier | Pattern | Scope & Rules |
| :--- | :--- | :--- |
| **File ID** | `f1`, `f2`, ... | 1-based deterministic ID assigned in sorted path order by `oz status` and `oz diff`. Accepted by `oz diff`, `oz commit --pick`, and `oz commit --exclude`. |
| **Hunk ID** | `h1a`, `h1b`, ..., `h1z`, `h1aa` | Deterministic `h<file_num><alpha_suffix>` (`0 -> a`, `25 -> z`, `26 -> aa`) assigned to each Histogram diff hunk in file `f<file_num>`. Accepted by `oz diff`, `oz commit --pick`, and `oz commit --exclude`. |
| **Conflict ID** | `c1`, `c2`, ... | 1-based deterministic conflict ID emitted during `oz integrate` / `oz stack rebase` conflicts. Accepted by `oz resolve <id> --take ours\|theirs\|base`. |
| **OpLog ID** | `op_<id>` | Unique monotonic operation ID for every MVCC OpLog entry. Accepted by `oz undo <op_id>`. |
| **Change-Id** | `I<40-hex>` | Standard commit footer trailer (`Change-Id: I...`) appended automatically by `oz commit` and preserved across `--amend`, `oz stack rebase`, and `oz pr stack`. |

---

## 4. Command Response Payloads (`data`)

### `oz status --json`

Returns `StatusReport`. Out-of-scope edits also populate top-level `warnings` with `{ "code": "out_of_scope", "message": "...", "path": "..." }`.

```json
{
  "workspace": "agent-auth",
  "branch": "feat/pkce",
  "head_oid": "a1b2c3d4...",
  "base_tree": "9f8e7d6c...",
  "used_fuse_fast_path": true,
  "changes": [
    {
      "id": "f1",
      "path": "crates/auth/src/token.rs",
      "kind": "modified",
      "additions": 24,
      "deletions": 3,
      "is_binary": false,
      "pinned": false,
      "out_of_scope": false,
      "hunks": [
        {
          "id": "h1a",
          "file_id": "f1",
          "path": "crates/auth/src/token.rs",
          "old_start": 12,
          "old_lines": 5,
          "new_start": 12,
          "new_lines": 18,
          "header": "@@ -12,5 +12,18 @@",
          "lines": [
            { "kind": "context", "content": "pub fn verify() {" },
            { "kind": "add", "content": "    check_pkce();" }
          ]
        }
      ]
    }
  ]
}
```

`kind` values: `"added"`, `"modified"`, `"deleted"`, `"renamed"`, `"copied"`, `"type_changed"`, `"untracked"`, `"conflicted"`.

---

### `oz diff [TARGETS...] --json`

Returns `DiffReport` flattened with `unified_diff`:

```json
{
  "workspace": "agent-auth",
  "branch": "feat/pkce",
  "head_oid": "a1b2c3d4...",
  "files": [ "...FileChange[]..." ],
  "total_additions": 24,
  "total_deletions": 3,
  "unified_diff": "--- a/crates/auth/src/token.rs\n+++ b/crates/auth/src/token.rs\n..."
}
```

---

### `oz commit -m "..." [--pick ...] [--exclude ...] [--amend] [-S] --json`

Returns `CommitOutcome` (with top-level `op_id` and `recovery_commands: ["oz undo <op_id>"]`):

```json
{
  "commit_oid": "def456...",
  "tree_oid": "123abc...",
  "parent_oid": "a1b2c3d4...",
  "branch": "feat/pkce",
  "change_id": "I0123456789abcdef0123456789abcdef01234567",
  "message": "feat(auth): add PKCE verifier\n\nChange-Id: I0123456789abcdef0123456789abcdef01234567\n",
  "committed_paths": ["crates/auth/src/pkce.rs", "crates/auth/src/token.rs"],
  "partial_hunk_paths": ["crates/auth/src/token.rs"],
  "remaining_dirty_count": 2,
  "signed": true,
  "signature_fingerprint": "SHA256:...",
  "amended": false
}
```

---

### `oz ws spawn | list | rm | gc --json`

* **`oz ws spawn <name> [--branch <b...>] [--from <rev>] [--no-warm] [--scope ...] [--deny ...]`:**
  ```json
  {
    "workspace": "agent-auth",
    "branch": "feat/pkce",
    "worktree_path": "/dev/my-repo.oz/agent-auth",
    "git_worktree_dir": "/dev/my-repo/.git/worktrees/agent-auth",
    "backend": "linux_fuse3",
    "base_commit": "a1b2c3d4...",
    "base_tree": "9f8e7d6c...",
    "baseline_id": "a1b2c3d4...",
    "baseline_source": "existing",
    "scope": ["crates/auth/**"],
    "deny": ["Cargo.lock"],
    "mounted": true,
    "spawn_elapsed_ms": 14.2
  }
  ```
  `backend` values: `"apfs_clonefile"`, `"linux_ficlone"`, `"linux_fuse3"`, `"windows_refs_clone"`, `"windows_proj_fs"`, `"cas_hardlink"`.
  `baseline_source` values: `"existing"`, `"captured"`, `"fallback_prior"`, or `null`.

* **`oz ws list`:** `{ "workspaces": [ { "name", "branch", "backend", "worktree_path", "base_commit", "branch_head_oid", "mount_health", "healthy", "mounted", "daemon_pid", "dirty_count", "pinned_count", "created_at", "updated_at" } ] }`
* **`oz ws rm <name> [--force] [--delete-branch]`:** `{ "name", "worktree_path", "branch", "branch_deleted" }`
* **`oz ws gc [--max-gb <GB>] [--target-branch <BRANCH>] [--dry-run]`:** `{ "removed_workspaces", "skipped_dirty_workspaces", "removed_baselines", "reclaimed_baseline_bytes", "remaining_baseline_bytes", "remaining_baseline_count", "max_baseline_gb", "dry_run" }`

---

### `oz radar [--base <REV>] [--no-committed] --json`

Returns `RadarReport`:

```json
{
  "base_branch": "main",
  "base_commit": "a1b2c3d4...",
  "scanned_workspaces": ["agent-auth", "agent-billing"],
  "collisions": [
    {
      "path": "crates/core/src/session.rs",
      "severity": "overlapping_hunks",
      "workspaces": [
        { "ws": "agent-auth", "lines": "42-68" },
        { "ws": "agent-billing", "lines": "55-79" }
      ],
      "suggested_action": "Pause agent-billing or serialize integration after agent-auth"
    }
  ]
}
```

`severity` values: `"overlapping_hunks"`, `"same_file_different_lines"`, `"out_of_scope"`.

---

### `oz integrate` & `oz resolve` & `oz stack` (`--json`)

* **`oz integrate <sources...> [--into <branch>] [--verify "<cmd>"] [--allow-out-of-scope] [--force] [-m "<msg>"]`** (and `--continue`):
  ```json
  {
    "into_branch": "main",
    "previous_oid": "a1b2c3d4...",
    "commit_oid": "f0e1d2c3...",
    "tree_oid": "11223344...",
    "merged_sources": ["agent-auth", "agent-billing"],
    "already_integrated": [],
    "op_id": "op_01J...",
    "main_worktree_synced": true
  }
  ```
  On conflict: exits with code `1`, `ok: false`, `error.code: "conflict"`, and top-level `conflicts: [ { "id": "c1", "path": "...", "message": "...", "base"?: "...", "ours"?: "...", "theirs"?: "...", "suggested_commands": ["oz resolve c1 --take ours", ...] } ]`.
  With `--abort`: `{ "aborted": true, "mode": "integrate", "sandbox_removed": true, "session_removed": true }`.

* **`oz resolve [c1 | --all] --take ours|theirs|base [--continue]`:**
  ```json
  {
    "mode": "integrate",
    "resolved_ids": ["c1"],
    "remaining_unresolved": [],
    "sandbox_path": "/dev/my-repo.oz/_integrate",
    "continued": { "kind": "integrate", "into_branch": "main", "...": "..." }
  }
  ```
  (`continued` is omitted when `--continue` is not passed.)

* **`oz stack status [--onto <branch>]`:** `{ "onto_branch", "onto_oid"?, "active_branch"?, "branches": [ { "branch", "head_oid", "parent_branch"?, "workspace"?, "ahead_count", "behind_onto_count", "needs_rebase", "commits": [ { "oid", "subject", "change_id"? } ] } ] }`
* **`oz stack rebase [branches...] [--onto <branch>] [--continue | --abort]`:** `{ "onto_branch", "onto_oid", "branches": [ { "branch", "previous_oid", "new_oid", "commit_count" } ], "rebased_commits": [ { "branch", "original_oid", "new_oid", "change_id" } ], "op_id"?, "updated_workspaces" }` (or `AbortOutcome` for `--abort`).

---

### `oz checkpoint`, `oz undo`, and `oz redo` (`--json`)

* **`oz checkpoint [-m "..."] [-q]`:** `{ "op_id", "workspace", "base_tree", "dirty_tree", "dirty_file_count", "checkpoint_ref", "op_checkpoint_ref", "entry": { ...OpLogEntry... } }`
* **`oz undo [op_id | --steps N]`:**
  ```json
  {
    "op_id": "op_01J_UNDO...",
    "workspace": "agent-auth",
    "undone_op_ids": ["op_01J_PREV..."],
    "restored_base_tree": "9f8e7d6c...",
    "restored_dirty_tree": "8e7d6c5b...",
    "restored_files": 2,
    "ref_deltas": {
      "refs/heads/feat/pkce": { "old": "def456...", "new": "a1b2c3..." }
    },
    "entry": { "...OpLogEntry..." },
    "engine_elapsed_ms": 2.41
  }
  ```
* **`oz redo`:** Same shape as `oz undo`, with `redone_undo_op_id` instead of `undone_op_ids`.

---

### `oz doctor --json`

Returns `DoctorReport` (safe to run outside a Git repository; never leaks raw credentials — tokens are masked and inline private key PEM payloads are redacted):

```json
{
  "invoked_as": "oz",
  "version": "1.1.0",
  "non_interactive": true,
  "os": "linux",
  "arch": "x86_64",
  "target_path": "/dev/my-repo",
  "filesystem": {
    "type": "ext4",
    "magic": "0xef53",
    "reflink_ready": false,
    "apfs_clonefile_ready": false,
    "dev_drive_refs": false
  },
  "fuse": {
    "dev_fuse_exists": true,
    "dev_fuse_rw": true,
    "fusermount3_path": "/usr/bin/fusermount3",
    "fusermount3_version": "fusermount3 version: 3.14.0",
    "user_allow_other_enabled": false,
    "ready": true
  },
  "projfs_ready": null,
  "windows": null,
  "macos": null,
  "recommended_backend": "linux_fuse3",
  "oz_vfsd": {
    "found": true,
    "path": "/usr/bin/oz-vfsd",
    "daemon_required": true,
    "daemon_running": false,
    "daemon_health": "stopped_idle",
    "socket_path": "/dev/my-repo/.git/ozone/vfsd.sock"
  },
  "git_ozone_alias": {
    "invoked_as": "oz",
    "found_on_path": "/usr/bin/git-ozone",
    "resolved": true,
    "sibling_path": "/usr/bin/git-ozone"
  },
  "repo": {
    "in_repo": true,
    "colocated": true,
    "repo_root": "/dev/my-repo",
    "git_dir": "/dev/my-repo/.git",
    "git_common_dir": "/dev/my-repo/.git",
    "ozone_dir": "/dev/my-repo/.git/ozone",
    "ozone_initialized": true,
    "is_linked_worktree": false,
    "workspace": null,
    "branch": "main",
    "head_oid": "a1b2c3d4...",
    "mount_health": "healthy",
    "repo_config_present": true,
    "local_config_present": false,
    "active_mount_count": 1,
    "unhealthy_mount_count": 0,
    "active_mounts": [
      {
        "workspace": "agent-auth",
        "branch": "feat/pkce",
        "backend": "linux_fuse3",
        "mount_point": "/dev/my-repo.oz/agent-auth",
        "mounted": true,
        "health": "healthy",
        "daemon_pid": 42100,
        "upper_dir": "/dev/my-repo/.git/ozone/ws/agent-auth/upper"
      }
    ]
  },
  "ssh_signing": {
    "enabled_by_default": true,
    "ready": true,
    "key_source": "git_config",
    "key_algorithm": "ssh-ed25519",
    "key_encrypted": false,
    "fingerprint": "SHA256:...",
    "issues": [],
    "recovery_commands": []
  },
  "github_auth": {
    "remote_name": "origin",
    "remote_url": "https://github.com/org/repo.git",
    "host": "github.com",
    "owner": "org",
    "repo": "repo",
    "transport": "https",
    "api_base_url": "https://api.github.com",
    "authenticated": true,
    "token_source": { "kind": "github_token_env" },
    "masked_token": "ghp_****1234"
  },
  "checks": [
    {
      "id": "linux_fuse",
      "category": "platform",
      "status": "ok",
      "summary": "/dev/fuse is read/write and fusermount3 is ready (fusermount3 version: 3.14.0)"
    }
  ],
  "remediation_hints": []
}
```

Key M4 diagnostic fields:
* `windows`: Populated on Windows (`projfs_feature`, `projfs_dll_path`, `projfs_dll_exists`, `projfs_ready`, `dev_drive_detected`, `refs_clone_ready`); `null` on non-Windows.
* `macos`: Populated on macOS (`apfs_detected`, `clonefile_ready`); `null` on non-macOS.
* `oz_vfsd.daemon_health`: One of `"running"`, `"stopped_idle"`, `"stopped_with_active_mounts"`, `"binary_missing"`, or `"not_required"` (with optional `daemon_pid`, `daemon_version`, `socket_path`).
* `checks[]`: Structured diagnostics across all 8 canonical check IDs (`linux_fuse`, `windows_projfs_dev_drive`, `macos_apfs_clonefile`, `oz_vfsd`, `git_ozone_alias`, `ssh_signing`, `github_auth`, `repository_mounts`) with `status` (`"ok" | "warn" | "fail" | "not_applicable"`), `category` (`"platform" | "daemon" | "cli" | "security" | "auth" | "repository"`), `summary`, and optional `remediation[]`.
* `remediation_hints[]`: Deduplicated actionable commands aggregated across failing/warning checks.

---

### `oz pr`, `oz sync`, `oz ci`, `oz clone`, `oz fetch`, `oz push` (`--json`)

* **`oz pr create [-t] [-b] [-B] [-H] [-d] [--no-push] [--remote]`:** `{ "pull_request": { "number", "html_url", "state", "draft", "merged", "title", "body"?, "head": { "ref", "sha" }, "base": { "ref", "sha" } }, "created", "pushed", "head_branch", "base_branch", "change_ids" }`
* **`oz pr stack [branches...] [-B] [-d] [--no-push] [--remote]`:** `{ "root_base", "pushed", "atomic_push_op_id", "items": [ { "number", "html_url", "title", "head_branch", "base_branch", "status", "created", "change_ids" } ] }`
* **`oz sync [--remote] [--base] [--no-fetch] [--no-rebase] [--no-gc] [--no-warm] [--push]`:** `{ "remote", "base_branch", "previous_base_oid"?, "base_oid"?, "fetched", "fast_forwarded", "merged_branches", "retargeted_prs": [ { "number", "head_branch", "old_base", "new_base" } ], "rebased_branches", "removed_workspaces", "deleted_branches", "pruned_checkpoints", "baseline_refreshed", "op_id"?, "warnings"? }`
* **`oz ci [--ref <rev> | --branch <branch>] [--pr <num>] [--max-excerpt-lines <N>] [--check <PAT>] [--ignore-check <PAT>] [--watch [--fail-fast | --wait-all] [--timeout <SECS>] [--startup-timeout <SECS>] [--interval <SECS>] [--progress]] [--job <ID> [--step <NAME>] [--raw]] [--remote]`:**
  ```json
  {
    "ref_name": "feat/pkce",
    "commit_sha": "def456...",
    "pr_number": 42,
    "overall_status": "failing",
    "all_passed": false,
    "total_count": 3,
    "failing_count": 1,
    "pending_count": 1,
    "checks": [
      {
        "name": "cargo test",
        "status": "completed",
        "conclusion": "failure",
        "failing_step": "Run unit tests",
        "log_excerpt": "test auth::pkce_verifier ... FAILED\nassertion `left == right` failed",
        "job_id": 701,
        "run_id": 900,
        "html_url": "https://github.com/acme/rocket/actions/runs/900/job/701"
      }
    ],
    "watch": {
      "outcome": "failed_fast",
      "elapsed_secs": 38,
      "polls": 5,
      "stopped_early": true
    }
  }
  ```
  When `failing_count > 0`, `oz ci --json` exits with code **`1`**, sets top-level `ok: false` and `error.code: "check_failed"`, and still returns the full `CiReport` in `data`. Under `--watch`, `watch.outcome` is one of `"passed"` (exit `0`), `"failed_fast"` / `"failed"` (`"check_failed"`), `"timed_out"` (`"watch_timed_out"`), `"no_checks_timed_out"` (`"no_checks_timed_out"`), or `"superseded"` (`"watch_superseded"`), all non-passing outcomes exiting with code **`1`**. Under `--job <ID>`, returns `CiJobLogReport`: `{ "job_id", "run_id"?, "name", "status", "conclusion"?, "html_url"?, "failing_step"?, "selected_step"?, "raw", "log_excerpt"? }`.
* **`oz clone`:** `{ "destination", "git_dir", "remote_name", "remote_url", "protocol_version", "branch"?, "head_oid"?, "partial_clone", "filter"?, "filter_negotiated", "checked_out_files", "op_id"?, "warnings"? }`
* **`oz fetch`:** `{ "remote", "remote_url", "protocol_version", "pack_received", "filter_negotiated", "updated_refs": [ { "remote_ref", "local_ref", "old_oid"?, "new_oid", "forced" } ], "pruned_refs", "op_id"?, "warnings"? }`
* **`oz push [--watch [--wait-all] [--timeout <SECS>] [--startup-timeout <SECS>] [--check <PAT>] [--ignore-check <PAT>] [--progress]]`:** `{ "remote", "remote_url", "atomic", "pack_objects_sent", "pack_bytes_sent", "pushed_refs": [ { "local_ref"?, "remote_ref", "old_oid"?, "new_oid"?, "deleted", "forced" } ], "upstream_configured", "op_id"?, "ci"? }`

---

### `oz blame <PATH> [--rev <REV>] --json`

Returns `BlameReport` joining commit history with `refs/notes/ai`:

```json
{
  "path": "crates/auth/src/token.rs",
  "revision": "HEAD",
  "resolved_commit_oid": "def456...",
  "total_lines": 14,
  "human_lines": 9,
  "agent_lines": 5,
  "agent_percentage": 35.71,
  "lines": [
    {
      "line_number": 7,
      "origin_line_number": 5,
      "content": "    verify_pkce(verifier)",
      "commit_oid": "def456...",
      "short_oid": "def45678",
      "author_name": "Dev Human",
      "author_email": "dev@example.com",
      "timestamp": "2026-11-18T12:00:00Z",
      "summary": "feat(auth): add PKCE verifier",
      "author_kind": "agent",
      "client": "claude-code",
      "model": "claude-opus-4-6",
      "session_id": "cc-sess-42"
    }
  ]
}
```

### `oz agent setup --json`, `oz setup --json`, `oz --version --json`

* **`oz agent setup`:** `{ "repo_root", "harnesses", "check_only", "created", "updated", "unchanged" }`
* **`oz setup`:** `{ "repo_root", "git_common_dir", "ozone_dir", "recommended_backend", "oz_vfsd_bin", "systemd_unit_path", "projfs_enabled"? }`
* **`oz --version --json`:** `{ "name": "oz", "invoked_as": "oz", "version": "1.1.0", "os": "linux", "arch": "x86_64", "pure_rust": true, "non_interactive": true }`

---

## 5. `oz mcp` Tool Reference

`oz mcp [--session-id <ID>]` runs a pure-Rust JSON-RPC 2.0 stdio Model Context Protocol server negotiating protocol versions `2024-11-05` and `2025-03-26`. Framing is auto-detected per message (newline-delimited JSON-RPC or `Content-Length: N\r\n\r\n`).

Supported JSON-RPC methods: `initialize`, `notifications/initialized`, `ping`, `tools/list`, `tools/call`.

Every `tools/call` returns an MCP `CallToolResult`: `{ "content": [{ "type": "text", "text": "<pretty JSON>" }], "isError": boolean }`. On error (`isError: true`), the JSON text payload contains `{ "ok": false, "code": "...", "message": "...", "recovery_commands": [...] }`.

| Tool Name | Required Args | Optional Args | Description |
| :--- | :--- | :--- | :--- |
| `oz_status` | *(none)* | `include_lines` (default `false`), `workspace`, `directory` | Working-tree status with `f1` / `h1a` IDs and `out_of_scope` warnings. |
| `oz_diff` | *(none)* | `targets[]`, `exclude[]`, `base`, `rev`, `range`, `stat_only`, `line_numbers`, `include_unified` (default `true`), `workspace`, `directory` | Histogram diff + addressable hunks + unified diff text (working tree or revision ranges). |
| `oz_commit` | `message` | `pick[]`, `exclude[]`, `allow_empty`, `force`, `allow_out_of_scope`, `session_id`, `client`, `model`, `prompt_summary`, `workspace`, `directory` | Stage-free whole or partial hunk commit with OpLog & `refs/notes/ai` provenance. |
| `oz_ws_spawn` | `name` | `from`, `branch`, `backend`, `scope[]`, `deny[]`, `warm` (default `true`), `directory` | Spawn isolated CoW workspace on a real branch with warm caches. |
| `oz_radar` | *(none)* | `base`, `include_committed` (default `true`), `workspaces[]`, `directory` | Cross-workspace collision radar. |
| `oz_checkpoint` | *(none)* | `message`, `workspace`, `directory` | Capture `< 10 ms` MVCC checkpoint in OpLog & `refs/ozone/checkpoints/<ws>`. |
| `oz_undo` | *(none)* | `op_id`, `steps` (integer >= 1, default `1`), `workspace`, `directory` | Rewind OpLog mutations/checkpoints (`op_id` and `steps` are mutually exclusive). |
| `oz_ci_logs` | *(none)* | `branch`, `commit_oid`, `pr_number`, `run_id`, `job_id`, `step`, `raw`, `max_excerpt_lines`, `failing_only`, `wait_seconds` (`0..=120`), `fail_fast`, `startup_timeout_seconds`, `check_filter[]`, `ignore_checks[]`, `workspace`, `directory` | Fetch GitHub CI check status, compact failing log excerpts, bounded long-poll watching, or single-job log drill-down. |
| `oz_log` | *(none)* | `rev`, `limit` (default `10`), `author`, `grep`, `pickaxe`, `grep_diff`, `since`, `oneline` (default `true`), `paths[]`, `workspace`, `directory` | Inspect compact commit history with message, diff, date, and path filters. |
| `oz_restore` | `targets[]` | `source`, `remove_untracked` (default `false`), `workspace`, `directory` | Selectively restore working-tree paths or file IDs (`f1`) from `HEAD` or `source` (recorded in OpLog for `oz_undo`). |
| `oz_show` | *(none)* | `rev`, `stat_only`, `include_files`, `start_line`, `end_line`, `workspace`, `directory` | Inspect a commit/tag or read a file blob at any revision (`<rev>:<path>`) without mutating the working tree. |
