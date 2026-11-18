# Migrating from `git worktree` to Ozone

Ozone is designed for **zero-friction adoption and zero lock-in**. Because Ozone stores all Git objects, refs, and worktree metadata inside standard `.git/` directories, you can adopt `oz` incrementally alongside existing `git worktree` checkouts, wrap legacy scripts with `oz git`, or revert completely back to plain `git` at any time without losing a single commit or branch.

---

## 1. Command Mapping: `git` vs. `oz` vs. `oz git`

| Task | Traditional `git` | Native `oz` | `oz git` Shim (`alias git="oz git"`) |
| :--- | :--- | :--- | :--- |
| Create isolated worktree on new branch | `git worktree add -b feat/x ../ws-x main` | `oz ws spawn ws-x --branch feat/x --from main` | `oz git worktree add -b feat/x ws-x main` |
| List worktrees | `git worktree list` | `oz ws list` | `oz git worktree list` |
| Remove worktree | `git worktree remove ../ws-x` | `oz ws rm ws-x` | `oz git worktree remove ws-x` |
| Force-remove dirty worktree | `git worktree remove --force ../ws-x` | `oz ws rm ws-x --force` | `oz git worktree remove -f ws-x` |
| Prune stale / merged worktrees | `git worktree prune` | `oz ws gc` | `oz git worktree prune` |
| Check working-tree status | `git status` | `oz status` | `oz git status` |
| Inspect diff | `git diff [path]` | `oz diff [f1 \| h1a \| path]` | `oz git diff [path]` |
| Stage & commit all changes | `git add -A && git commit -m "msg"` | `oz commit -m "msg"` | `oz git add -A && oz git commit -m "msg"` |
| Partial / hunk commit | `git add -p && git commit -m "msg"` | `oz commit -m "msg" --pick f1 h2a` | `oz git add <file> && oz git commit -m "msg"` |
| Amend last commit | `git commit --amend --no-edit` | `oz commit --amend --no-edit` | `oz git commit --amend --no-edit` |
| Undo recent commit / reset | `git reflog` + `git reset --hard HEAD@{1}` | `oz undo` (`< 10 ms`, works on dirty edits too) | `oz undo` (covers delegated `oz git` mutations) |
| Merge feature branches | `git checkout main && git merge feat/a feat/b` | `oz integrate feat/a feat/b --into main --verify "cargo test"` | `oz git merge feat/a` |
| Rebase stacked branches | `git rebase --update-refs main` | `oz stack rebase --onto main` | `oz git rebase main` |
| Clone with partial blobs | `git clone --filter=blob:none <url>` | `oz clone --filter=blob:none <url>` | `oz git clone --filter=blob:none <url>` |
| Fetch / push safely | `git fetch --prune` / `git push --force-with-lease` | `oz fetch --prune` / `oz push --atomic --force-with-lease` | `oz git fetch --prune` / `oz git push --force-with-lease` |
| Open GitHub PR / stack | `gh pr create` (manual base wiring) | `oz pr create` / `oz pr stack` | — |
| Line authorship (human + AI) | `git blame <file>` | `oz blame <file>` | `oz git blame <file>` (delegated) |

---

## 2. Colocated `.git/` Safety Guarantee

Unlike VCS tools that replace `.git/` or require a separate repository format, Ozone keeps standard Git ownership intact:

```text
my-repo/
├── .git/
│   ├── objects/              # Shared Git ODB (blobs, trees, commits, packfiles)
│   ├── refs/
│   │   ├── heads/*           # Real named local branches (never detached by default)
│   │   ├── remotes/*         # Standard remote-tracking refs
│   │   ├── notes/ai          # Standard Git notes for AI provenance (git notes --ref=ai)
│   │   └── ozone/            # Internal Ozone checkpoints (ignored by standard git workflows)
│   ├── worktrees/<ws>/       # Standard Git linked worktree metadata
│   └── ozone/                # Isolated Ozone state (config, oplog, baselines, VFS upperdirs)
├── .ozone.toml               # Optional repo-committed config
└── ...
my-repo.oz/                   # Default grouped sibling workspace container
├── agent-auth/               # Real filesystem path, real branch checkout
└── agent-billing/
```

**What this means in practice:**

* Every `oz ws spawn` workspace checks out a **real named branch** (`HEAD -> refs/heads/<branch>`) and registers standard `.git/worktrees/<ws>/` metadata so VS Code, Cursor, JetBrains, `cargo`, `gh`, and plain `git log` / `git show` work seamlessly inside the workspace.
* Existing `git worktree add` checkouts continue to work untouched and appear alongside Ozone workspaces in `oz git worktree list`.
* Placing workspaces in `../<repo>.oz/<ws>` keeps them on the **same filesystem volume** as `<repo>` (preventing cross-device `EXDEV` errors that disable reflinks) while avoiding parent-directory config bleed (`Cargo.toml`, `tsconfig.json`, `pyproject.toml`) that breaks nested worktrees.

---

## 3. Step-by-Step Migration Path

### Step 1: Run `oz doctor` in Your Existing Repository

```bash
cd /path/to/my-repo
oz doctor
```

Confirm that `repo.in_repo: true`, `repo.colocated: true`, and check `recommended_backend` (`apfs_clonefile`, `linux_ficlone`, `linux_fuse3`, `windows_refs_clone`, `windows_proj_fs`, or `cas_hardlink`).

### Step 2: Build Once to Prime Warm Caches (Optional)

If your repo has large build/dependency caches (`target/`, `node_modules/`, `.venv/`), ensure your main worktree has a clean build before spawning workspaces so Ozone captures a quiescent warm baseline:

```bash
# Example: build Rust or install Node deps once in main repo
cargo build
# or: npm ci
```

The first `oz ws spawn` automatically captures a quiescent baseline into `.git/ozone/baselines/<commit>` (skipping if `.cargo-lock` or another build lock is actively held) and preserves `mtime` on untouched files so subsequent builds inside every workspace hit warm caches immediately.

### Step 3: Spawn New Workspaces with `oz ws spawn`

Replace:

```bash
git worktree add -b feat/auth ../my-repo-auth main
cp -r target ../my-repo-auth/target   # Slow, duplicates gigabytes of disk
```

With:

```bash
oz ws spawn auth --branch feat/auth --from main
```

Your workspace is ready at `../my-repo.oz/auth` with warm caches cloned via CoW/VFS in milliseconds.

### Step 4: Clean Up Legacy `git worktree` Directories (At Your Own Pace)

Once branches from older `git worktree add` folders are merged, remove them with either plain `git` or `oz git`:

```bash
git worktree remove ../my-repo-old
# or:
oz git worktree prune
```

---

## 4. Using the `oz git` Compatibility Shim

If you have shell muscle memory, Makefiles, or agent prompts hardcoded to `git`, you can route `git` through Ozone:

```bash
# Temporary for current shell (or add to ~/.bashrc / ~/.zshrc)
alias git="oz git"
```

Or invoke Git's native subcommand alias without replacing `git`:

```bash
git ozone status
git ozone ws spawn agent-1 --from main
```

### What `oz git` Intercepts Natively

`oz git` intercepts **13 high-impact commands** directly on Ozone's CoW + `gix` + MVCC OpLog engine:

`worktree`, `status`, `diff`, `add`, `reset` (path unpinning), `commit`, `switch`, `checkout`, `merge`, `rebase`, `clone`, `fetch`, `push`.

Key compatibility details:

* **Virtual `pinned` set (`oz git add` / `oz git reset <path>`):** `oz git add <file>` records paths into `.git/ozone/state/<ws>.json` (`pinned_paths`) without locking or rewriting `.git/index`. The next `oz git commit` commits only pinned paths and clears them on success.
* **Delegated legacy commands wrapped in OpLog:** Commands not intercepted natively (e.g., `git log`, `git show`, `git tag`, `git stash`, `git reset --hard`, `git clean`, `git cherry-pick`, `git revert`) delegate to the real system `git` binary. Mutating delegated commands are wrapped in an Ozone `MutationTransaction` so **`oz undo` reverses even destructive legacy commands like `oz git reset --hard`**.
* **Explicit real-git binary override:** Ozone automatically skips `oz` and `git-ozone` on `PATH` to avoid recursion, and honors `OZ_REAL_GIT=/path/to/git` if you want to pin a specific system `git` executable.

---

## 5. Zero Lock-In: How to Uninstall & Revert to Plain `git`

Every commit, branch, tag, and object created by Ozone is standard Git data inside `.git/objects/` and `.git/refs/heads/*`. To revert completely back to plain C `git`:

### 1. Commit or Save Any Uncommitted Work in Ozone Workspaces

```bash
oz ws list
# Inside any workspace with dirty edits you want to keep:
oz -C ../my-repo.oz/<ws> commit -m "wip: save work"
```

### 2. Remove Ozone Workspaces Cleanly (Unmounts FUSE3 / ProjFS Safely)

```bash
# Remove each workspace (branches remain in refs/heads/* unless --delete-branch is passed)
oz ws rm <ws>

# Verify no active Ozone workspaces remain
oz ws list
```

> [!NOTE]
> Always use `oz ws rm <ws>` before deleting `.git/ozone/` if you use Linux FUSE3 (`linux_fuse3`) or Windows ProjFS (`windows_proj_fs`), so Ozone unmounts the virtual filesystem cleanly first. On Linux, if a stale FUSE mount remains after manual deletion, unmount it with `fusermount3 -u ../<repo>.oz/<ws>`.

### 3. Remove Optional Background Services & Shell Alias

```bash
# If you enabled alias git="oz git"
unalias git   # and remove from ~/.bashrc / ~/.zshrc

# Linux: if you ran `oz setup --systemd-user`, disable the user service
systemctl --user disable --now 'oz-vfsd@*.service' 2>/dev/null || true
```

### 4. Remove Ozone Metadata from the Repository

```bash
# Remove local Ozone state, OpLog, baselines, and VFS upperdirs
rm -rf .git/ozone

# Remove internal Ozone checkpoint refs (optional — harmless if left)
git for-each-ref --format='%(refname)' refs/ozone/ | xargs -r -n1 git update-ref -d

# Optional: remove AI provenance notes (or keep them — they are standard git notes)
# git update-ref -d refs/notes/ai

# Remove empty sibling container directory if present
rmdir ../$(basename "$PWD").oz 2>/dev/null || true

# Optional: remove repo-committed config and generated agent harness files
# rm -f .ozone.toml
```

### 5. Uninstall Ozone Binaries

* **Homebrew:** `brew uninstall oz`
* **APT / `.deb`:** `sudo apt remove ozone`
* **`winget`:** `winget uninstall OzoneVCS.Ozone`
* **Shell / PowerShell / Cargo:** Remove `oz`, `git-ozone`, and `oz-vfsd` from your install prefix (e.g. `~/.local/bin` or `~/.cargo/bin`).

Run `git status` and `git log --oneline -n 5` — your repository is 100% standard Git with every commit and branch intact.
