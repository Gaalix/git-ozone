# Ozone Quickstart Guide

Get productive with Ozone (`oz` / `git-ozone`) in 60 seconds — whether you are a solo developer speeding up daily Git workflows or orchestrating 5–20 parallel AI coding agents.

---

## 1. Verify Installation & One-Time Setup

After [installing Ozone](../README.md#installation), verify your host filesystem, VFS daemon, SSH signing key, and GitHub authentication:

```bash
oz --version
oz doctor
```

Inside any existing Git repository, initialize colocated `.git/ozone/` metadata (optional — Ozone initializes it lazily on first write as well):

```bash
oz setup
```

### Platform-Specific Persistence (Optional)

* **Linux (`ext4` with FUSE3):** Install a `systemd --user` unit so `oz-vfsd` keeps existing FUSE3 workspaces mounted across reboots:
  ```bash
  oz setup --systemd-user
  ```
* **Windows 11 (`NTFS` without Dev Drive):** Enable Windows Projected File System (`Client-ProjFS`) once without rebooting:
  ```powershell
  oz setup --enable-projfs
  ```
* **macOS (`APFS`), Linux (`Btrfs`/`XFS`/`bcachefs`), Windows (`Dev Drive / ReFS`):** Native kernel reflinks are used automatically — no daemon or extra setup required.

---

## 2. Solo Developer Workflow

Ozone works directly inside any standard `.git/` repository with zero conversion.

### Clone or Open an Existing Repo

```bash
# Fast pure-Rust Wire Protocol v2 partial clone
oz clone --filter=blob:none https://github.com/org/repo.git
cd repo
```

### Indexless Status & Histogram Diffs

`oz status` skips `.git/index` locks and assigns deterministic, session-local IDs to every modified file (`f1`, `f2`, ...) and every diff hunk (`h1a`, `h1b`, ...):

```bash
oz status
```

Example output:

```text
On branch feat/pkce
  [f1] M  crates/auth/src/token.rs (+24/-3) (hunks: h1a, h1b)
  [f2] A  crates/auth/src/pkce.rs (+61/-0) (hunks: h2a)
  [f3] M  crates/auth/src/debug.rs (+4/-0) (hunks: h3a)
```

Inspect specific files or individual hunks with `oz diff`:

```bash
oz diff f1          # Full Histogram diff for file f1
oz diff h1a         # Only hunk h1a
```

### Stage-Free Commits & Surgical Partial Commits

Commit everything in the working directory in one step (built-in guardrails automatically block suspected secrets, unresolved conflict markers, `> 50 MB` binaries, and `--scope`/`--deny` violations):

```bash
oz commit -m "feat(auth): implement PKCE flow"
```

Or commit an exact subset of files and hunks **without `git add -p` or `git stash`**:

```bash
# Commit all of f2 plus only hunk h1a from f1; leave h1b and f3 dirty in place
oz commit -m "feat(auth): add PKCE verifier" --pick f2 h1a

# Or commit everything except debug edits
oz commit -m "feat(auth): clean implementation" --exclude f3 h1b
```

Useful commit flags:

* `--amend` / `--amend --no-edit`: Amend `HEAD` while preserving its `Change-Id:` trailer.
* `-S` / `--sign` (or `--no-sign`, `--signing-key <KEY>`): Pure-Rust OpenSSH Ed25519 `SSHSIG` commit signing (`namespace = "git"`), producing GitHub "Verified" badges without GPG daemons.
* `--force`: Override secret, conflict-marker, or oversized-blob guardrails.
* `--allow-out-of-scope`: Override workspace `--scope` / `--deny` path guardrails.
* `--allow-empty`: Create a commit with no file changes.

### Checkpoints & Sub-10ms Undo / Redo

Every mutating `oz` command (and every `oz checkpoint`) appends an immutable MVCC entry to `.git/ozone/oplog/`:

```bash
oz checkpoint -m "before refactor"   # < 10 ms working-tree snapshot
oz undo                              # Rewind the last mutation or un-checkpointed edits
oz undo --steps 2                    # Rewind the last 2 operations
oz undo op_01J...                    # Rewind a specific OpLog entry ID
oz redo                              # Re-apply the most recent undo
```

### GitHub Stacked PRs, Sync & CI Self-Healing

Ozone reuses `GITHUB_TOKEN`, `GH_TOKEN`, `OZ_GITHUB_TOKEN`, `gh` CLI credentials (`hosts.yml`), or Git credential helpers automatically:

```bash
# Inspect your local branch stack and Change-Id trailers
oz stack status

# Rebase stacked branches in memory onto main (preserving Change-Id trailers)
oz stack rebase --onto main

# Push and create/update a single GitHub PR (title & body inferred from commit + Change-Id)
oz pr create

# Or push all stacked branches atomically and create/update cross-linked stacked PRs
oz pr stack

# Fetch GitHub Check Runs & compact failing Actions job log excerpts (exit code 1 on failure)
oz ci --json

# Watch CI silently until pass or fail-fast (or push & watch in one step)
oz ci --watch --json
oz push --watch --json

# Drill into deeper cleaned logs for a specific failing Actions job
oz ci --job 701 --max-excerpt-lines 150 --json

# After PRs merge: fetch origin, fast-forward main, retarget downstream PR bases,
# rebase local downstream branches, prune merged CoW workspaces, and refresh warm baseline
oz sync
```

---

## 3. Parallel AI Agent Workflow

Ozone is purpose-built for running 5–20 coding agents in parallel on the same machine without disk bloat, lock contention, or silent cross-workspace collisions.

### Step 1: Configure Agent Harnesses (`oz agent setup`)

Install idempotent skills/rules and non-destructive `PostToolUse` `oz checkpoint --quiet` hooks:

```bash
# Auto-detect installed harnesses in the repo, environment, or $HOME
oz agent setup

# Or configure all 5 supported harnesses at once
oz agent setup --all

# Target specific harness(es) or dry-run without writing
oz agent setup --harness claude-code,cursor,antigravity
oz agent setup --all --check
```

| Harness (`--harness`) | Skill / Rule Files Written | Hook Config Merged (`oz checkpoint --quiet`) |
| :--- | :--- | :--- |
| `claude-code` | `.claude/skills/oz/SKILL.md` | `.claude/settings.json` |
| `cursor` | `.cursor/rules/oz.mdc` | `.cursor/hooks.json` |
| `codex-cli` | `.agents/skills/oz/SKILL.md`, `.codex/skills/oz/SKILL.md`, `AGENTS.md` | `.codex/hooks.json` |
| `windsurf` | `.windsurf/rules/oz.md` | `.windsurf/hooks.json` |
| `antigravity` | `.agent/skills/oz/SKILL.md` | `.agent/hooks.json` |

Pass `--no-hooks` to install only the skill/rule files.

### Step 2: Spawn Scoped CoW Workspaces with Warm Caches

Each workspace spawns into a grouped sibling directory `../<repo>.oz/<ws>` on a real named Git branch with its own isolated writable CoW copy of `target/`, `node_modules/`, `.next/`, `.turbo/`, `.vite/`, and `.venv/`:

```bash
oz ws spawn agent-auth \
  --from main \
  --branch feat/auth-pkce \
  --scope "crates/auth/**" \
  --deny "Cargo.lock" "migrations/**"

oz ws spawn agent-billing \
  --from main \
  --branch feat/billing-v2 \
  --scope "crates/billing/**" \
  --deny "Cargo.lock"
```

List active workspaces and mount health at any time:

```bash
oz ws list --json
```

> [!TIP]
> Pass `--no-warm` to skip warm baseline capture for lightweight documentation or scripts-only tasks. Use `oz ws rm <name> [--force] [--delete-branch]` to remove a single workspace or `oz ws gc [--max-gb 10] [--dry-run]` to reclaim merged workspaces and LRU baselines.

### Step 3: Monitor Collisions with `oz radar`

Before agents finish or open PRs, scan all active workspaces (both uncommitted edits and branch-diverged commits) for overlapping hunks, same-file edits, or out-of-scope changes:

```bash
oz radar --json
oz radar --base main --no-committed   # Scan uncommitted working-tree edits only
```

### Step 4: Atomic In-Memory Integration & Conflict Resolution

Merge one or more agent workspaces into `main` completely in memory — `main` and active agent worktrees stay untouched unless the optional `--verify` test gate exits `0`:

```bash
oz integrate agent-auth agent-billing --into main --verify "cargo test"
```

If a merge conflict occurs:
1. `oz integrate` exits with code `1`, leaving `main` and all agent workspaces untouched.
2. Conflicts are materialized exclusively inside the ephemeral CoW sandbox `../<repo>.oz/_integrate/` with deterministic conflict IDs (`c1`, `c2`, ...).
3. Resolve via CLI (or edit files directly in `_integrate/`) and continue:

```bash
# Resolve individual conflicts or all at once
oz resolve c1 --take ours
oz resolve c2 --take theirs --continue

# Or resolve all conflicts with one side and auto-continue
oz resolve --all --take ours --continue

# Or continue manually / abort cleanly
oz integrate --continue --verify "cargo test"
oz integrate --abort
```

### Step 5: Audit AI vs. Human Provenance (`refs/notes/ai`)

When commits run under `--agent`, `OZ_AGENT=1`, `oz mcp`, or detected harness environment variables (`CLAUDECODE`, `CURSOR_TRACE_ID`, `CODEX_*`, `WINDSURF_*`, `ANTIGRAVITY_*`, `OZ_AGENT_SESSION`, `OZ_AGENT_CLIENT`, `OZ_AGENT_MODEL`, `OZ_AGENT_PROMPT`), Ozone writes line-level attribution to standard Git notes (`refs/notes/ai`) while keeping commit messages clean:

```bash
# Line-level human vs. AI attribution
oz blame crates/auth/src/token.rs
oz blame crates/auth/src/token.rs --rev main --json

# 100% interoperable with standard git notes
git notes --ref=ai show HEAD
```

---

## Connecting `oz mcp`

Ozone includes a built-in pure-Rust JSON-RPC 2.0 stdio Model Context Protocol server supporting both newline-delimited JSON-RPC and `Content-Length` framing.

Add `oz mcp` to your MCP client configuration (Claude Code, Cursor, Windsurf, Codex CLI, Antigravity):

```json
{
  "mcpServers": {
    "ozone": {
      "command": "oz",
      "args": ["mcp"]
    }
  }
}
```

To bind a repository directory or explicit provenance session ID:

```json
{
  "mcpServers": {
    "ozone": {
      "command": "oz",
      "args": ["-C", "/path/to/repo", "mcp", "--session-id", "my-agent-session"]
    }
  }
}
```

Exposed MCP tools: `oz_status`, `oz_diff`, `oz_commit`, `oz_ws_spawn`, `oz_radar`, `oz_checkpoint`, `oz_undo`, and `oz_ci_logs`. See the [JSON Schema Reference](json-schema-reference.md#5-oz-mcp-tool-reference) for exact input/output schemas.

---

## Repository Configuration (`.ozone.toml`)

Ozone layers configuration in precedence order: **built-in defaults < `.ozone.toml` (repo-committed) < `.git/ozone/config.toml` (machine-local)**.

```toml
# Maximum disk budget (GiB) retained for warm baselines in .git/ozone/baselines/
max_baseline_gb = 20.0

# Pre-commit oversized binary guardrail limit (MiB)
max_blob_mb = 50

# Sibling workspace path template (must contain "{ws}"; supports "{repo}")
workspace_template = "../{repo}.oz/{ws}"

# Optional local backend override (usually set in .git/ozone/config.toml):
# "apfs_clonefile" | "linux_ficlone" | "linux_fuse3" |
# "windows_refs_clone" | "windows_proj_fs" | "cas_hardlink"
# backend = "linux_fuse3"

[warm]
auto_detect = true                      # Auto-detect Cargo, Node/TS, Python caches
paths = [".custom-cache"]               # Additional relative cache directories
exclude_paths = ["target/package"]      # Relative paths excluded from baseline capture
preserve_mtime = true                   # Preserve baseline mtime on untouched files

[guardrails]
check_secrets = true                    # Block suspected secrets / unignored .env / private keys
check_conflict_markers = true           # Block unresolved <<<<<<< / ======= / >>>>>>> markers
scope = []                              # Default allowed path globs
deny = ["Cargo.lock", "migrations/**"]  # Default denied path globs

[daemon]
auto_start = true                       # Auto-spawn/re-attach oz-vfsd when accessing VFS mounts
allow_other = false                     # Pass FUSE allow_other (requires /etc/fuse.conf)

[signing]
enabled = true                          # Default SSH commit signing (omit to defer to git commit.gpgsign)
key = "~/.ssh/id_ed25519"               # Path or inline "key::ssh-ed25519 ..."
```

---

## Next Steps

* Migrating existing `git worktree` scripts? Read [Migrating from `git worktree`](migration-from-git-worktree.md).
* Building agent tooling on `--json` or `oz mcp`? Read the [JSON Schema Reference](json-schema-reference.md).
* Curious how Ozone measures up against `git`? Read [Benchmarks & Methodology](benchmarks.md).
