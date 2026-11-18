#!/usr/bin/env python3
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Shared cross-OS utilities for the Ozone reproducible public benchmark suite.

Zero external Python dependencies (stdlib only) so benchmarks run cleanly on
Linux (ext4/btrfs/xfs + FUSE3), macOS (APFS), and Windows (NTFS/ReFS/ProjFS).
"""

from __future__ import annotations

import atexit
import datetime
import json
import math
import os
import platform
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence, Set, Tuple


BENCH_DIR = Path(__file__).resolve().parent
REPO_ROOT = BENCH_DIR.parent


def is_windows() -> bool:
    return sys.platform.startswith("win")


def is_macos() -> bool:
    return sys.platform == "darwin"


def is_linux() -> bool:
    return sys.platform.startswith("linux")


def exe_suffix() -> str:
    return ".exe" if is_windows() else ""


def _which_executable(candidates: Sequence[str]) -> Optional[str]:
    for name in candidates:
        found = shutil.which(name)
        if found:
            return found
    return None


def _is_worktrunk_binary(path: str) -> bool:
    """Verify `wt` is actually Worktrunk and not Windows Terminal (`wt.exe`)."""
    try:
        proc = subprocess.run(
            [path, "--version"],
            capture_output=True,
            text=True,
            timeout=5,
            stdin=subprocess.DEVNULL,
        )
        combined = f"{proc.stdout}\n{proc.stderr}".lower()
        return proc.returncode == 0 and ("worktrunk" in combined or "wt " in combined)
    except Exception:
        return False


def discover_binaries(
    oz_bin_override: Optional[str] = None,
    vfsd_bin_override: Optional[str] = None,
) -> Dict[str, Optional[str]]:
    """Locate `oz`, `oz-vfsd`, `git`, and optional comparison tools (`jj`, `worktrunk`, `simgit`)."""
    suf = exe_suffix()

    # 1. Resolve `oz`
    oz_path: Optional[str] = None
    oz_candidates: List[Optional[Path]] = [
        Path(oz_bin_override) if oz_bin_override else None,
        Path(os.environ["OZ_BIN"]) if os.environ.get("OZ_BIN") else None,
        REPO_ROOT / "target" / "release" / f"oz{suf}",
        REPO_ROOT / "target" / "debug" / f"oz{suf}",
    ]
    for cand in oz_candidates:
        if cand is not None and cand.is_file():
            oz_path = str(cand.resolve())
            break
    if oz_path is None:
        oz_path = shutil.which("oz")

    # 2. Resolve `oz-vfsd`
    vfsd_path: Optional[str] = None
    vfsd_candidates: List[Optional[Path]] = [
        Path(vfsd_bin_override) if vfsd_bin_override else None,
        Path(os.environ["OZ_VFSD_BIN"]) if os.environ.get("OZ_VFSD_BIN") else None,
        (Path(oz_path).parent / f"oz-vfsd{suf}") if oz_path else None,
        REPO_ROOT / "target" / "release" / f"oz-vfsd{suf}",
        REPO_ROOT / "target" / "debug" / f"oz-vfsd{suf}",
    ]
    for cand in vfsd_candidates:
        if cand is not None and cand.is_file():
            vfsd_path = str(cand.resolve())
            break
    if vfsd_path is None:
        vfsd_path = shutil.which("oz-vfsd")

    # 3. Resolve `git` and optional comparison tools
    git_path = shutil.which("git")
    jj_path = shutil.which("jj")
    simgit_path = shutil.which("simgit")

    worktrunk_path: Optional[str] = None
    for name in ("worktrunk", "wt"):
        found = shutil.which(name)
        if found and _is_worktrunk_binary(found):
            worktrunk_path = found
            break

    return {
        "oz": oz_path,
        "oz_vfsd": vfsd_path,
        "git": git_path,
        "jj": jj_path,
        "worktrunk": worktrunk_path,
        "simgit": simgit_path,
    }


def detect_filesystem_type(path: Path) -> str:
    """Best-effort filesystem type detection across Linux, macOS, and Windows."""
    target = path if path.exists() else path.parent
    try:
        if is_linux():
            proc = subprocess.run(
                ["df", "-T", str(target)],
                capture_output=True,
                text=True,
                timeout=5,
                stdin=subprocess.DEVNULL,
            )
            if proc.returncode == 0:
                lines = [l.strip() for l in proc.stdout.splitlines() if l.strip()]
                if len(lines) >= 2:
                    parts = lines[-1].split()
                    if len(parts) >= 2:
                        return parts[1]
        elif is_macos():
            proc = subprocess.run(
                ["stat", "-f", "%T", str(target)],
                capture_output=True,
                text=True,
                timeout=5,
                stdin=subprocess.DEVNULL,
            )
            if proc.returncode == 0 and proc.stdout.strip():
                return proc.stdout.strip()
        elif is_windows():
            drive = os.path.splitdrive(str(target.resolve()))[0].rstrip(":")
            if drive:
                proc = subprocess.run(
                    [
                        "powershell",
                        "-NoProfile",
                        "-Command",
                        f"(Get-Volume -DriveLetter '{drive}').FileSystemType",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=10,
                    stdin=subprocess.DEVNULL,
                )
                if proc.returncode == 0 and proc.stdout.strip():
                    return proc.stdout.strip()
    except Exception:
        pass
    return "unknown"


def detect_no_new_privs() -> bool:
    """Return True on Linux when the process has `NoNewPrivs: 1` (blocks setuid `fusermount3`)."""
    if not is_linux():
        return False
    try:
        for line in Path("/proc/self/status").read_text(encoding="utf-8").splitlines():
            if line.startswith("NoNewPrivs:"):
                return line.split(":", 1)[1].strip() == "1"
    except OSError:
        pass
    return False


def configure_oz_backend(repo_dir: Path, backend: Optional[str]) -> None:
    """Optionally pin Ozone's local backend in `.git/ozone/config.toml` without dirtying the repo."""
    if not backend or backend == "auto":
        return
    ozone_dir = repo_dir / ".git" / "ozone"
    ozone_dir.mkdir(parents=True, exist_ok=True)
    cfg_path = ozone_dir / "config.toml"
    cfg_path.write_text(f'backend = "{backend}"\n', encoding="utf-8")


def choose_work_dir_base(explicit_dir: Optional[str] = None) -> Path:
    """Select a real-disk base directory with a short path (< 60 bytes on Unix).

    On Linux:
      - `/tmp` is often `tmpfs` (RAM-backed) which skews disk I/O and runs out of space
        on multi-gigabyte worktree benchmarks.
      - `/var/tmp` is denied by Ubuntu's default AppArmor `/etc/apparmor.d/fusermount3`
        profile (`@{HOME}/**/` and `/tmp/**/` are allowed; `/var/tmp` is not).
      - `$HOME/.cache/oz-bench` (when short and on a real persistent filesystem) satisfies
        all three constraints: real disk (`ext4`/`btrfs`/`xfs`), AppArmor `fusermount3`
        allowlist, and short Unix socket path (`< 108` bytes `SUN_LEN`).
    """
    if explicit_dir:
        p = Path(explicit_dir).expanduser().resolve()
        p.mkdir(parents=True, exist_ok=True)
        return p

    if is_linux():
        home = os.environ.get("HOME")
        if home:
            candidate = Path(home) / ".cache" / "oz-bench"
            if len(str(candidate)) <= 55:
                try:
                    candidate.mkdir(parents=True, exist_ok=True)
                    if os.access(candidate, os.W_OK):
                        fs_type = detect_filesystem_type(candidate)
                        if fs_type not in ("tmpfs", "ramfs"):
                            return candidate.resolve()
                except OSError:
                    pass

    base = Path(tempfile.gettempdir()).resolve()
    base.mkdir(parents=True, exist_ok=True)
    return base


def build_base_env(bins: Optional[Dict[str, Optional[str]]] = None) -> Dict[str, str]:
    """Deterministic, non-interactive environment for VCS subprocess execution."""
    env = os.environ.copy()
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GIT_EDITOR"] = "true"
    env["EDITOR"] = "true"
    env["VISUAL"] = "true"
    env["GIT_PAGER"] = "cat"
    env["PAGER"] = "cat"
    env["OZ_AGENT"] = "1"
    env["LC_ALL"] = env.get("LC_ALL", "C")
    if bins and bins.get("oz_vfsd"):
        env["OZ_VFSD_BIN"] = str(bins["oz_vfsd"])
    return env


def run_cmd(
    cmd: Sequence[str],
    cwd: Optional[Path] = None,
    env: Optional[Dict[str, str]] = None,
    stdin_text: Optional[str] = None,
    check: bool = True,
    timeout: Optional[float] = None,
) -> Tuple[int, str, str, float]:
    """Run a subprocess, measuring wall-clock elapsed milliseconds."""
    run_env = env if env is not None else build_base_env()
    t0 = time.perf_counter()
    proc = subprocess.run(
        list(cmd),
        cwd=str(cwd) if cwd is not None else None,
        env=run_env,
        input=stdin_text,
        capture_output=True,
        text=True,
        timeout=timeout,
        stdin=None if stdin_text is not None else subprocess.DEVNULL,
    )
    elapsed_ms = (time.perf_counter() - t0) * 1000.0
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"Command failed (exit {proc.returncode}): {' '.join(cmd)}\n"
            f"cwd: {cwd}\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    return proc.returncode, proc.stdout, proc.stderr, elapsed_ms


def init_git_repo(
    repo_dir: Path,
    git_bin: str = "git",
    env: Optional[Dict[str, str]] = None,
    user_name: str = "Ozone Bench",
    user_email: str = "bench@ozone.dev",
) -> None:
    """Initialize a Git repository on `main` with deterministic settings (`gc.auto=0`)."""
    repo_dir.mkdir(parents=True, exist_ok=True)
    run_cmd([git_bin, "init", "-b", "main"], cwd=repo_dir, env=env)
    run_cmd([git_bin, "config", "user.name", user_name], cwd=repo_dir, env=env)
    run_cmd([git_bin, "config", "user.email", user_email], cwd=repo_dir, env=env)
    run_cmd([git_bin, "config", "gc.auto", "0"], cwd=repo_dir, env=env)
    run_cmd([git_bin, "config", "commit.gpgsign", "false"], cwd=repo_dir, env=env)


def _handle_rmtree_error(func: Callable[..., Any], path: str, _exc_info: Any) -> None:
    """Allow deleting read-only (`0444`) cache/git files cleanly during sandbox teardown."""
    try:
        os.chmod(path, stat.S_IRWXU)
        parent = os.path.dirname(path)
        if parent:
            try:
                os.chmod(parent, stat.S_IRWXU)
            except OSError:
                pass
        func(path)
    except OSError:
        pass


def _rmtree_robust(path: Path) -> None:
    if not path.exists() and not path.is_symlink():
        return
    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=_handle_rmtree_error)
    else:
        shutil.rmtree(path, onerror=_handle_rmtree_error)


def _kill_sandbox_vfsd_pids(root_marker: str, known_pids: Iterable[int]) -> None:
    """Terminate ONLY `oz-vfsd` processes bound to `root_marker` (Linux `/proc`)."""
    if not is_linux():
        return

    candidate_pids: Set[int] = set(known_pids)
    try:
        for entry in os.listdir("/proc"):
            if entry.isdigit():
                candidate_pids.add(int(entry))
    except OSError:
        pass

    for pid in candidate_pids:
        cmdline_path = Path(f"/proc/{pid}/cmdline")
        try:
            raw = cmdline_path.read_bytes()
        except OSError:
            continue
        cmdline = raw.decode("utf-8", errors="ignore")
        if "oz-vfsd" in cmdline and root_marker in cmdline:
            try:
                os.kill(pid, signal.SIGTERM)
            except OSError:
                continue
            deadline = time.monotonic() + 1.5
            while time.monotonic() < deadline and Path(f"/proc/{pid}").exists():
                time.sleep(0.02)
            if Path(f"/proc/{pid}").exists():
                try:
                    os.kill(pid, signal.SIGKILL)
                except OSError:
                    pass
                deadline = time.monotonic() + 1.5
                while time.monotonic() < deadline and Path(f"/proc/{pid}").exists():
                    time.sleep(0.02)


def _unmount_fuse_path(mount_point: Path) -> None:
    if not is_linux():
        return
    for tool in ("fusermount3", "fusermount"):
        bin_path = shutil.which(tool)
        if not bin_path:
            continue
        for args in ([bin_path, "-u", "--", str(mount_point)], [bin_path, "-u", "-z", "--", str(mount_point)]):
            try:
                proc = subprocess.run(
                    args,
                    capture_output=True,
                    timeout=5,
                    stdin=subprocess.DEVNULL,
                )
                if proc.returncode == 0:
                    return
            except Exception:
                pass


_ACTIVE_SANDBOXES: List["Sandbox"] = []
_SIGNALS_REGISTERED = False


def _cleanup_all_sandboxes() -> None:
    for sb in list(_ACTIVE_SANDBOXES):
        try:
            sb.cleanup()
        except Exception:
            pass


def _signal_handler(signum: int, _frame: Any) -> None:
    _cleanup_all_sandboxes()
    sys.exit(128 + signum)


def _register_global_cleanup() -> None:
    global _SIGNALS_REGISTERED
    if _SIGNALS_REGISTERED:
        return
    _SIGNALS_REGISTERED = True
    atexit.register(_cleanup_all_sandboxes)
    for sig_name in ("SIGINT", "SIGTERM"):
        sig = getattr(signal, sig_name, None)
        if sig is not None:
            try:
                signal.signal(sig, _signal_handler)
            except Exception:
                pass


class Sandbox:
    """RAII benchmark sandbox guaranteeing clean FUSE3 teardown & temp dir removal."""

    def __init__(
        self,
        prefix: str = "ozb-",
        base_dir: Optional[Path] = None,
        bins: Optional[Dict[str, Optional[str]]] = None,
        keep: bool = False,
    ) -> None:
        self.base_dir = base_dir if base_dir is not None else choose_work_dir_base()
        self.base_dir.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix=prefix, dir=str(self.base_dir))).resolve()
        self.bins = bins or {}
        self.keep = keep
        self.oz_repos: List[Path] = []
        self.mount_points: List[Path] = []
        self._cleaned = False

        _register_global_cleanup()
        _ACTIVE_SANDBOXES.append(self)

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, _exc_type: Any, _exc: Any, _tb: Any) -> None:
        self.cleanup()

    def register_oz_repo(self, repo_root: Path) -> None:
        resolved = repo_root.resolve()
        if resolved not in self.oz_repos:
            self.oz_repos.append(resolved)

    def register_mount(self, mount_path: Path) -> None:
        resolved = mount_path.resolve()
        if resolved not in self.mount_points:
            self.mount_points.append(resolved)

    def cleanup_oz_repo(self, repo_root: Path) -> None:
        """Cleanly remove workspaces and tear down FUSE3 daemons/mounts for one Ozone repo."""
        oz_bin = self.bins.get("oz")
        env = build_base_env(self.bins)

        # 1. Try graceful `oz ws rm <name> --force` for all listed workspaces.
        if oz_bin and repo_root.is_dir():
            try:
                rc, stdout, _, _ = run_cmd(
                    [oz_bin, "ws", "list", "--json"],
                    cwd=repo_root,
                    env=env,
                    check=False,
                    timeout=15,
                )
                if rc == 0:
                    payload = json.loads(stdout)
                    for ws in payload.get("data", {}).get("workspaces", []):
                        ws_name = ws.get("name")
                        if ws_name:
                            run_cmd(
                                [oz_bin, "ws", "rm", ws_name, "--force", "--json"],
                                cwd=repo_root,
                                env=env,
                                check=False,
                                timeout=15,
                            )
            except Exception:
                pass

        # 2. Inspect persisted `.git/ozone/ws/*/mount.json` for leftover mounts & daemon PIDs.
        mounts: List[Path] = []
        pids: List[int] = []
        ws_meta_root = repo_root / ".git" / "ozone" / "ws"
        if ws_meta_root.is_dir():
            for child in ws_meta_root.iterdir():
                mj = child / "mount.json"
                if mj.is_file():
                    try:
                        rec = json.loads(mj.read_text(encoding="utf-8"))
                        mp = rec.get("mount_point")
                        if mp:
                            mounts.append(Path(mp))
                        pid = rec.get("daemon_pid")
                        if isinstance(pid, int):
                            pids.append(pid)
                    except Exception:
                        pass

        # 3. On Linux, scan `/proc/self/mountinfo` for any mount under this repo's container or sandbox.
        if is_linux():
            prefix = str(self.root)
            try:
                for line in Path("/proc/self/mountinfo").read_text(encoding="utf-8").splitlines():
                    parts = line.split()
                    if len(parts) >= 5 and parts[4].startswith(prefix):
                        mp = Path(parts[4])
                        if mp not in mounts:
                            mounts.append(mp)
            except OSError:
                pass

        for mp in self.mount_points:
            if mp not in mounts:
                mounts.append(mp)

        # 4. Kill any surviving `oz-vfsd` processes scoped to this sandbox root, then unmount.
        _kill_sandbox_vfsd_pids(str(self.root), pids)
        for mp in mounts:
            _unmount_fuse_path(mp)

    def cleanup(self) -> None:
        if self._cleaned:
            return
        self._cleaned = True
        if self in _ACTIVE_SANDBOXES:
            _ACTIVE_SANDBOXES.remove(self)

        for repo_root in list(self.oz_repos):
            try:
                self.cleanup_oz_repo(repo_root)
            except Exception:
                pass

        # Final sweep of `/proc/self/mountinfo` and `oz-vfsd` processes under `self.root`.
        if is_linux():
            _kill_sandbox_vfsd_pids(str(self.root), [])
            prefix = str(self.root)
            try:
                for line in Path("/proc/self/mountinfo").read_text(encoding="utf-8").splitlines():
                    parts = line.split()
                    if len(parts) >= 5 and parts[4].startswith(prefix):
                        _unmount_fuse_path(Path(parts[4]))
            except OSError:
                pass

        if not self.keep:
            _rmtree_robust(self.root)


def collect_inodes(root: Path) -> Set[Tuple[int, int]]:
    """Collect `(st_dev, st_ino)` for all non-directory entries under `root`."""
    inodes: Set[Tuple[int, int]] = set()
    if not root.exists():
        return inodes
    stack = [root]
    while stack:
        cur = stack.pop()
        try:
            with os.scandir(cur) as it:
                for entry in it:
                    try:
                        st = entry.stat(follow_symlinks=False)
                    except OSError:
                        continue
                    if stat.S_ISDIR(st.st_mode):
                        stack.append(Path(entry.path))
                    else:
                        inodes.add((st.st_dev, st.st_ino))
        except OSError:
            continue
    return inodes


def measure_disk_usage(
    paths: Sequence[Path],
    exclude_inodes: Optional[Set[Tuple[int, int]]] = None,
) -> Dict[str, int]:
    """Measure physical block bytes (`st_blocks * 512`), apparent bytes, and file count.

    Deduplicates hardlinks by `(st_dev, st_ino)` and excludes any inode present in
    `exclude_inodes` (e.g., the original base repository's files) so net additional
    physical storage is accurately reported.
    """
    seen: Set[Tuple[int, int]] = set()
    physical_bytes = 0
    apparent_bytes = 0
    file_count = 0

    for root in paths:
        if not root.exists() and not root.is_symlink():
            continue
        if not root.is_dir() or root.is_symlink():
            try:
                st = os.lstat(root)
            except OSError:
                continue
            key = (st.st_dev, st.st_ino)
            if exclude_inodes is not None and key in exclude_inodes:
                continue
            if key in seen:
                continue
            seen.add(key)
            file_count += 1
            apparent_bytes += st.st_size
            blocks = getattr(st, "st_blocks", None)
            physical_bytes += (blocks * 512) if blocks is not None else st.st_size
            continue

        stack = [root]
        while stack:
            cur = stack.pop()
            try:
                with os.scandir(cur) as it:
                    for entry in it:
                        try:
                            st = entry.stat(follow_symlinks=False)
                        except OSError:
                            continue
                        if stat.S_ISDIR(st.st_mode):
                            stack.append(Path(entry.path))
                            continue
                        key = (st.st_dev, st.st_ino)
                        if exclude_inodes is not None and key in exclude_inodes:
                            continue
                        if key in seen:
                            continue
                        seen.add(key)
                        file_count += 1
                        apparent_bytes += st.st_size
                        blocks = getattr(st, "st_blocks", None)
                        physical_bytes += (blocks * 512) if blocks is not None else st.st_size
            except OSError:
                continue

    return {
        "physical_blocks_bytes": physical_bytes,
        "apparent_bytes": apparent_bytes,
        "file_count": file_count,
    }


def sync_fs() -> None:
    if hasattr(os, "sync"):
        try:
            os.sync()
        except OSError:
            pass


def fs_allocated_bytes(path: Path) -> int:
    """Return total allocated bytes on the filesystem containing `path`."""
    target = path if path.exists() else path.parent
    if hasattr(os, "statvfs"):
        st = os.statvfs(target)
        used_blocks = max(0, st.f_blocks - st.f_bfree)
        return int(used_blocks * st.f_frsize)
    usage = shutil.disk_usage(target)
    return int(usage.used)


def fs_free_bytes(path: Path) -> int:
    target = path if path.exists() else path.parent
    return int(shutil.disk_usage(target).free)


def percentile(values: Sequence[float], p: float) -> float:
    """Linear-interpolated percentile (`p` in [0, 100])."""
    if not values:
        return 0.0
    ordered = sorted(values)
    if len(ordered) == 1:
        return float(ordered[0])
    k = (len(ordered) - 1) * (p / 100.0)
    lo = math.floor(k)
    hi = math.ceil(k)
    if lo == hi:
        return float(ordered[int(k)])
    frac = k - lo
    return float(ordered[lo] * (1.0 - frac) + ordered[hi] * frac)


def summarize_ms(samples: Sequence[float]) -> Dict[str, Any]:
    if not samples:
        return {
            "count": 0,
            "min_ms": 0.0,
            "mean_ms": 0.0,
            "median_ms": 0.0,
            "p95_ms": 0.0,
            "max_ms": 0.0,
            "total_ms": 0.0,
        }
    total = float(sum(samples))
    return {
        "count": len(samples),
        "min_ms": round(float(min(samples)), 3),
        "mean_ms": round(total / len(samples), 3),
        "median_ms": round(percentile(samples, 50.0), 3),
        "p95_ms": round(percentile(samples, 95.0), 3),
        "max_ms": round(float(max(samples)), 3),
        "total_ms": round(total, 3),
    }


def _probe_version(binary: Optional[str], args: Sequence[str] = ("--version",)) -> Optional[str]:
    if not binary:
        return None
    try:
        proc = subprocess.run(
            [binary, *args],
            capture_output=True,
            text=True,
            timeout=5,
            stdin=subprocess.DEVNULL,
        )
        out = (proc.stdout or proc.stderr or "").strip()
        if out:
            return out.splitlines()[0].strip()
    except Exception:
        pass
    return None


def _detect_cpu_model() -> str:
    try:
        if is_linux():
            cpuinfo = Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="ignore")
            for line in cpuinfo.splitlines():
                if line.lower().startswith("model name") and ":" in line:
                    return line.split(":", 1)[1].strip()
        elif is_macos():
            proc = subprocess.run(
                ["sysctl", "-n", "machdep.cpu.brand_string"],
                capture_output=True,
                text=True,
                timeout=5,
                stdin=subprocess.DEVNULL,
            )
            if proc.returncode == 0 and proc.stdout.strip():
                return proc.stdout.strip()
    except Exception:
        pass
    return platform.processor() or platform.machine() or "unknown"


def _detect_ram_gib() -> Optional[float]:
    try:
        if is_linux():
            for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
                if line.startswith("MemTotal:"):
                    kb = int(line.split()[1])
                    return round(kb / (1024.0 * 1024.0), 2)
        elif is_macos():
            proc = subprocess.run(
                ["sysctl", "-n", "hw.memsize"],
                capture_output=True,
                text=True,
                timeout=5,
                stdin=subprocess.DEVNULL,
            )
            if proc.returncode == 0:
                return round(int(proc.stdout.strip()) / (1024.0**3), 2)
        elif is_windows():
            import ctypes

            class MEMORYSTATUSEX(ctypes.Structure):
                _fields_ = [
                    ("dwLength", ctypes.c_ulong),
                    ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong),
                    ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong),
                    ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong),
                    ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
                ]

            stat_ex = MEMORYSTATUSEX()
            stat_ex.dwLength = ctypes.sizeof(MEMORYSTATUSEX)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(stat_ex)):
                return round(stat_ex.ullTotalPhys / (1024.0**3), 2)
    except Exception:
        pass
    return None


def _probe_oz_doctor_sanitized(work_dir: Path, bins: Dict[str, Optional[str]]) -> Optional[Dict[str, Any]]:
    """Run `oz doctor --json` and keep only non-sensitive platform capability fields."""
    oz_bin = bins.get("oz")
    if not oz_bin:
        return None
    try:
        rc, stdout, _, _ = run_cmd(
            [oz_bin, "-C", str(work_dir), "doctor", "--json"],
            env=build_base_env(bins),
            check=False,
            timeout=10,
        )
        if rc != 0:
            return None
        parsed = json.loads(stdout)
        data = parsed.get("data", {})
        return {
            "version": data.get("version"),
            "os": data.get("os"),
            "arch": data.get("arch"),
            "filesystem": data.get("filesystem"),
            "fuse": data.get("fuse"),
            "projfs_ready": data.get("projfs_ready"),
            "recommended_backend": data.get("recommended_backend"),
            "oz_vfsd_found": bool((data.get("oz_vfsd") or {}).get("found")),
        }
    except Exception:
        return None


def collect_system_metadata(
    work_dir: Path,
    bins: Dict[str, Optional[str]],
) -> Dict[str, Any]:
    """Collect reproducible hardware, OS, filesystem, and tool version metadata."""
    repo_commit: Optional[str] = None
    if bins.get("git"):
        try:
            rc, out, _, _ = run_cmd(
                [str(bins["git"]), "rev-parse", "--short=12", "HEAD"],
                cwd=REPO_ROOT,
                check=False,
                timeout=5,
            )
            if rc == 0:
                repo_commit = out.strip()
        except Exception:
            pass

    tools: Dict[str, Dict[str, Any]] = {}
    for key in ("oz", "git", "jj", "worktrunk", "simgit"):
        path = bins.get(key)
        tools[key] = {
            "available": path is not None,
            "version": _probe_version(path) if path else None,
        }
    tools["oz_vfsd"] = {
        "available": bins.get("oz_vfsd") is not None,
        "version": _probe_version(bins.get("oz_vfsd")) if bins.get("oz_vfsd") else None,
    }

    return {
        "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "ozone_repo_commit": repo_commit,
        "os": platform.system(),
        "os_release": platform.release(),
        "os_version": platform.version(),
        "machine": platform.machine(),
        "python_version": platform.python_version(),
        "cpu_model": _detect_cpu_model(),
        "cpu_logical_cores": os.cpu_count(),
        "ram_gib": _detect_ram_gib(),
        "work_dir_filesystem": detect_filesystem_type(work_dir),
        "work_dir_free_gib": round(fs_free_bytes(work_dir) / (1024.0**3), 2),
        "no_new_privs": detect_no_new_privs(),
        "tools": tools,
        "oz_doctor": _probe_oz_doctor_sanitized(work_dir, bins),
    }


def write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)
