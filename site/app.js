(() => {
  "use strict";

  const INSTALL_SNIPPETS = {
    shell: {
      command: "curl -fsSL https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/install.sh | bash",
      note: "Detects OS/arch, verifies <code>SHA256SUMS.txt</code>, and installs static binaries. Supports <code>--version</code>, <code>--prefix</code>, and <code>--dry-run</code>."
    },
    homebrew: {
      command: "brew install --formula https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/homebrew/oz.rb",
      note: "Installs <code>oz</code>, <code>git-ozone</code>, and <code>oz-vfsd</code> on macOS &amp; Linux. Verify with <code>oz doctor</code>."
    },
    deb: {
      command: "curl -fsSLO https://github.com/Gaalix/git-ozone/releases/download/v1.0.0/ozone_1.0.0_amd64.deb\nsudo apt install ./ozone_1.0.0_amd64.deb",
      note: "Debian/Ubuntu <code>.deb</code> (<code>amd64</code> / <code>arm64</code>) with <code>Recommends: fuse3</code>. Installs to <code>/usr/bin/</code>."
    },
    winget: {
      command: "winget install OzoneVCS.Ozone",
      note: "Installs <code>oz.exe</code>, <code>git-ozone.exe</code>, and <code>oz-vfsd.exe</code> (or use PowerShell installer before winget community PR merge). Run <code>oz setup --enable-projfs</code> on NTFS."
    },
    powershell: {
      command: "irm https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/install.ps1 | iex",
      note: "Verifies SHA256 checksum, installs binaries, and updates user <code>PATH</code>. Supports <code>-Version</code>, <code>-Prefix</code>, and <code>-DryRun</code>."
    },
    cargo: {
      command: "cargo install --git https://github.com/Gaalix/git-ozone --locked oz-cli oz-vfsd",
      note: "Builds from source directly from GitHub, or install pre-built release binaries with <code>cargo binstall --git https://github.com/Gaalix/git-ozone oz-cli</code>."
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

  // Real v1.0.0 full-gate benchmark results measured from `benchmarks/results/latest.json`
  // (commit 4690cbee08b2, 2026-11-21T21:51:45Z, AMD Ryzen 9 7900X, Ubuntu Linux 7.3.0 ext4,
  // live linux_fuse3 via fusermount3 3.18.3, oz 1.0.0 vs git 2.55.0, jj 0.47.1, worktrunk 0.83.0).
  const BENCHMARK_DATA = window.OZONE_BENCHMARKS || {
    measured: true,
    environment:
      "AMD Ryzen 9 7900X (24 logical cores, 61.39 GiB RAM) · Ubuntu Linux 7.3.0 (ext4, live linux_fuse3) · oz 1.0.0 vs git 2.55.0, jj 0.47.1 & worktrunk 0.83.0 · 7/7 ROADMAP gates PASS (./benchmarks/run_all.sh --full).",
    suites: [
      {
        title: "20-Agent Worktree Spawn & Disk (5 GB Cache)",
        target: "51.2x faster · 54,404x less disk",
        metrics: [
          { label: "oz ws spawn (linux_fuse3, warm 20/20)", value: "444.5 ms total · 19.8 ms p50/ws" },
          { label: "git warm copy · jj (cold) · wt (cold)", value: "22.78 s · 2.61 s · 567.8 ms (cold git: 243.9 ms)" },
          { label: "Extra physical disk (oz vs git warm / cold)", value: "1.84 MiB (97.67 GiB app.) vs 97.96 GiB / 17.03 MiB" }
        ]
      },
      {
        title: "250k-File Monorepo Status (100% Parity)",
        target: "30.2x faster vs git · 15.8x vs jj",
        metrics: [
          { label: "oz status --json (oz ws linux_fuse3)", value: "4.4 ms dirty · 3.3 ms clean p50" },
          { label: "oz status --json (regular repo, dirty/clean)", value: "118.3 ms / 117.4 ms p50" },
          { label: "git status --porcelain · jj status (dirty)", value: "132.1 ms · 69.6 ms p50" }
        ]
      },
      {
        title: "Agent VCBench (4 Canonical Tasks)",
        target: "-68.0% cmds · -72.7% tokens",
        metrics: [
          { label: "Total non-interactive commands", value: "16 oz vs 50 git (-68.0%)" },
          { label: "Approx / lexical token overhead", value: "1,163 vs 4,257 (-72.7%) · lex -69.7%" },
          { label: "Local CLI wall time & tree parity", value: "142.1 ms vs 83.6 ms · 4/4 fsck PASS" }
        ]
      },
      {
        title: "10-Agent Concurrent Commits (30 Ops)",
        target: "0% lock errors · git fsck PASS",
        metrics: [
          { label: "Distinct-branch commits (+ pack-refs)", value: "oz 0% (389–496 c/s) · git 0% (1,043–1,131 c/s)" },
          { label: "Shared-worktree .git/index.lock contention", value: "oz 0/30 (0.0%) vs git add 22/30 (73.3%)" },
          { label: "Integrity verification", value: "30/30 refs + git fsck --full PASS" }
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

    selectTab("shell");
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
