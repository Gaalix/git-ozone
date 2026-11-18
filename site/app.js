(() => {
  "use strict";

  const INSTALL_SNIPPETS = {
    homebrew: {
      command: "brew install ozone-vcs/tap/oz",
      note: "Installs <code>oz</code>, <code>git-ozone</code>, and <code>oz-vfsd</code> on macOS &amp; Linux. Verify with <code>oz doctor</code>."
    },
    shell: {
      command: "curl -fsSL https://raw.githubusercontent.com/ozone-vcs/ozone/main/packaging/install.sh | bash",
      note: "Detects OS/arch, verifies <code>SHA256SUMS.txt</code>, and installs static binaries. Supports <code>--version</code>, <code>--prefix</code>, and <code>--dry-run</code>."
    },
    deb: {
      command: "curl -fsSLO https://github.com/ozone-vcs/ozone/releases/download/v1.0.0/ozone_1.0.0_amd64.deb\nsudo apt install ./ozone_1.0.0_amd64.deb",
      note: "Debian/Ubuntu <code>.deb</code> (<code>amd64</code> / <code>arm64</code>) with <code>Recommends: fuse3</code>. Installs to <code>/usr/bin/</code>."
    },
    winget: {
      command: "winget install OzoneVCS.Ozone",
      note: "Installs <code>oz.exe</code>, <code>git-ozone.exe</code>, and <code>oz-vfsd.exe</code>. Run <code>oz setup --enable-projfs</code> on NTFS (no reboot)."
    },
    powershell: {
      command: "irm https://raw.githubusercontent.com/ozone-vcs/ozone/main/packaging/install.ps1 | iex",
      note: "Verifies SHA256 checksum, installs binaries, and updates user <code>PATH</code>. Supports <code>-Version</code>, <code>-Prefix</code>, and <code>-DryRun</code>."
    },
    cargo: {
      command: "cargo binstall oz-cli",
      note: "Downloads pre-built GitHub Release binaries via <code>[package.metadata.binstall]</code>, or build from source with <code>cargo install --path crates/oz-cli --locked</code>."
    }
  };

  const COMPARISONS = {
    worktrees: {
      gitMeta: "Cold caches · gigabytes duplicated",
      gitCode: `# Spawn 2 agent worktrees with warm Rust/Node caches
git worktree add -b feat/auth ../repo-auth main
cp -r target node_modules ../repo-auth/   # Minutes + 5 GB copied

git worktree add -b feat/billing ../repo-billing main
cp -r target node_modules ../repo-billing/ # Another 5 GB copied

# No path scope guardrails; shared .git/index.lock races`,
      ozMeta: "Zero-byte CoW · warm caches · scoped",
      ozCode: `# Spawn isolated CoW workspaces in ../<repo>.oz/<ws>
oz ws spawn agent-auth \\
  --branch feat/auth --from main \\
  --scope "crates/auth/**" --deny "Cargo.lock"

oz ws spawn agent-billing \\
  --branch feat/billing --from main \\
  --scope "crates/billing/**" --deny "migrations/**"

# Warm caches cloned via APFS / FICLONE / FUSE3 / ProjFS
# Deterministic mtime preservation keeps Cargo & tsc warm`,
      takeaway: "Ozone clones multi-gigabyte build caches via native reflinks or persistent-upperdir VFS without duplicating data blocks or triggering cold rebuilds."
    },
    hunk: {
      gitMeta: "Interactive TTY trap · index.lock",
      gitCode: `# Interactive staging breaks autonomous agents
git status
git add -p crates/auth/src/token.rs   # Blocks on TTY prompt!
git add crates/auth/src/pkce.rs
git commit -m "feat(auth): add PKCE verifier"
# Requires stash/index gymnastics to keep debug edits dirty`,
      ozMeta: "1 non-interactive command · Change-Id",
      ozCode: `# Deterministic file [f1] and hunk (h1a) IDs
oz status --json

# Stage-free partial commit built in RAM via gix
oz commit -m "feat(auth): add PKCE verifier" --pick f2 h1a

# Guardrails check secrets, conflict markers, >50 MB blobs & scope
# Appends Change-Id: I<hex>; leaves h1b and f3 dirty in place`,
      takeaway: "Addressable file and hunk IDs replace interactive `git add -p` and `git stash` with a single deterministic command."
    },
    integrate: {
      gitMeta: "Mutates main · modal MERGE_HEAD",
      gitCode: `# Merges directly on live working tree; fails mid-way
git checkout main
git merge feat/auth feat/billing
# Leaves conflict markers on main if anything collides
# No automated test gate before advancing refs/heads/main`,
      ozMeta: "In-memory merge · --verify gate · sandbox",
      ozCode: `# Detect overlapping hunks before merge
oz radar --json

# 100% in-memory 3-way merge + ephemeral _integrate/ test gate
oz integrate agent-auth agent-billing \\
  --into main --verify "cargo test"

# If conflicts occur, main stays untouched; resolve deterministically:
oz resolve c1 --take ours --continue`,
      takeaway: "`main` and active agent workspaces are never left half-merged; conflicts materialize strictly inside ephemeral `../<repo>.oz/_integrate/`."
    },
    undo: {
      gitMeta: "Reflog archaeology · lost dirty edits",
      gitCode: `# Uncommitted edits wiped by reset --hard cannot be recovered
git reflog
git reset --hard HEAD@{1}

# AI attribution pollutes commit trailers or is lost completely
git blame crates/auth/src/token.rs`,
      ozMeta: "< 10 ms MVCC undo · refs/notes/ai",
      ozCode: `# Lightweight hook checkpoint + instant MVCC rewind
oz checkpoint -m "before refactor"
oz undo                # Rewinds refs + dirty tree in < 10 ms
oz redo                # Even undo is undoable

# Line-level human vs. AI attribution (clean commit messages)
oz blame crates/auth/src/token.rs --json`,
      takeaway: "Every mutation and agent PostToolUse hook writes an immutable OpLog entry, while AI session provenance lives cleanly in standard `refs/notes/ai`."
    }
  };

  // Structured for live v1.0 release benchmark results from `benchmarks/results/latest.json`.
  // `measured: false` renders honest Milestone 4 targets until the release benchmark run populates values.
  const BENCHMARK_DATA = window.OZONE_BENCHMARKS || {
    measured: false,
    suites: [
      {
        title: "20-Agent Worktree Spawn & Disk",
        target: "< 500 ms/ws · < 50 MB",
        metrics: [
          { label: "Per-workspace spawn latency (p50)", value: "Pending v1.0 run" },
          { label: "Total extra disk (5 GB cache)", value: "Pending v1.0 run" },
          { label: "Script", value: "benchmarks/bench_worktrees.py" }
        ]
      },
      {
        title: "250k-File Monorepo Status",
        target: "O(dirty_files)",
        metrics: [
          { label: "Clean / dirty warm status (oz vs git)", value: "Pending v1.0 run" },
          { label: "Dirty-file parity check", value: "Verified per run" },
          { label: "Script", value: "benchmarks/bench_status_250k.py" }
        ]
      },
      {
        title: "Agent VCBench (4 Canonical Tasks)",
        target: ">= 60% fewer cmds · >= 50% fewer tokens",
        metrics: [
          { label: "Command count reduction", value: "Pending v1.0 run" },
          { label: "Token overhead reduction", value: "Pending v1.0 run" },
          { label: "Script", value: "benchmarks/bench_agent_vcbench.py" }
        ]
      },
      {
        title: "10-Agent Concurrent Commits",
        target: "0% lock errors",
        metrics: [
          { label: "oz commit lock contention", value: "Pending v1.0 run" },
          { label: "C git lock contention baseline", value: "Pending v1.0 run" },
          { label: "Script", value: "benchmarks/bench_concurrency.py" }
        ]
      }
    ]
  };

  function initTheme() {
    const root = document.documentElement;
    const toggle = document.getElementById("theme-toggle");
    const saved = localStorage.getItem("oz-site-theme");
    const preferred =
      saved ||
      (window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches
        ? "light"
        : "dark");
    root.setAttribute("data-theme", preferred);

    if (toggle) {
      toggle.addEventListener("click", () => {
        const next = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
        root.setAttribute("data-theme", next);
        try {
          localStorage.setItem("oz-site-theme", next);
        } catch (_) {
          // Ignore storage quota / private-mode errors
        }
      });
    }
  }

  function initInstallTabs() {
    const buttons = document.querySelectorAll("[data-install-tab]");
    const codeEl = document.getElementById("install-command");
    const noteEl = document.getElementById("install-note");
    const copyBtn = document.getElementById("copy-install-btn");
    const copyStatus = document.getElementById("copy-status");

    function selectTab(key) {
      const item = INSTALL_SNIPPETS[key];
      if (!item || !codeEl || !noteEl) return;

      buttons.forEach((btn) => {
        const active = btn.getAttribute("data-install-tab") === key;
        btn.classList.toggle("is-active", active);
        btn.setAttribute("aria-selected", String(active));
      });
      codeEl.textContent = item.command;
      noteEl.innerHTML = item.note;
    }

    buttons.forEach((btn) => {
      btn.addEventListener("click", () => selectTab(btn.getAttribute("data-install-tab")));
    });

    if (copyBtn && codeEl) {
      copyBtn.addEventListener("click", async () => {
        const text = codeEl.textContent || "";
        try {
          await navigator.clipboard.writeText(text);
          copyBtn.textContent = "Copied!";
          if (copyStatus) copyStatus.textContent = "Install command copied to clipboard.";
        } catch (_) {
          copyBtn.textContent = "Select & Copy";
        }
        window.setTimeout(() => {
          copyBtn.textContent = "Copy";
        }, 1600);
      });
    }

    selectTab("homebrew");
  }

  function initComparisonTabs() {
    const buttons = document.querySelectorAll("[data-compare-tab]");
    const gitMeta = document.getElementById("compare-git-meta");
    const gitCode = document.getElementById("compare-git-code");
    const ozMeta = document.getElementById("compare-oz-meta");
    const ozCode = document.getElementById("compare-oz-code");
    const takeaway = document.getElementById("compare-takeaway");

    function selectCompare(key) {
      const item = COMPARISONS[key];
      if (!item || !gitCode || !ozCode) return;

      buttons.forEach((btn) => {
        const active = btn.getAttribute("data-compare-tab") === key;
        btn.classList.toggle("is-active", active);
        btn.setAttribute("aria-selected", String(active));
      });

      if (gitMeta) gitMeta.textContent = item.gitMeta;
      gitCode.textContent = item.gitCode;
      if (ozMeta) ozMeta.textContent = item.ozMeta;
      ozCode.textContent = item.ozCode;
      if (takeaway) takeaway.textContent = item.takeaway;
    }

    buttons.forEach((btn) => {
      btn.addEventListener("click", () => selectCompare(btn.getAttribute("data-compare-tab")));
    });

    selectCompare("worktrees");
  }

  function renderBenchmarks() {
    const container = document.getElementById("bench-cards");
    const banner = document.getElementById("bench-status-banner");
    if (!container) return;

    if (banner && BENCHMARK_DATA.measured) {
      banner.classList.add("is-live");
      const title = document.getElementById("bench-status-title");
      const detail = document.getElementById("bench-status-detail");
      if (title) title.textContent = "Live v1.0.0 Measured Benchmark Results";
      if (detail && BENCHMARK_DATA.environment) {
        detail.textContent = BENCHMARK_DATA.environment;
      }
    }

    container.textContent = "";
    BENCHMARK_DATA.suites.forEach((suite) => {
      const card = document.createElement("article");
      card.className = "bench-card";

      const head = document.createElement("div");
      head.className = "bench-card-head";

      const h3 = document.createElement("h3");
      h3.textContent = suite.title;

      const badge = document.createElement("span");
      badge.className = "bench-target";
      badge.textContent = suite.target;

      head.appendChild(h3);
      head.appendChild(badge);
      card.appendChild(head);

      suite.metrics.forEach((metric) => {
        const row = document.createElement("div");
        row.className = "bench-metric-row";

        const lbl = document.createElement("span");
        lbl.className = "bench-metric-label";
        lbl.textContent = metric.label;

        const val = document.createElement("span");
        val.className = "bench-metric-value";
        val.textContent = metric.value;

        row.appendChild(lbl);
        row.appendChild(val);
        card.appendChild(row);
      });

      container.appendChild(card);
    });
  }

  initTheme();
  initInstallTabs();
  initComparisonTabs();
  renderBenchmarks();
})();
