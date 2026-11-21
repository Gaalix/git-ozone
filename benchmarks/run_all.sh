#!/usr/bin/env bash
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Reproducible Public Benchmark Suite orchestrator for Ozone (`oz`).
#
# Presets:
#   ./benchmarks/run_all.sh --quick   # Fast smoke run (5 agents, 50 MiB cache, 5k files)
#   ./benchmarks/run_all.sh           # Standard run (20 agents, 512 MiB cache, 250k files)
#   ./benchmarks/run_all.sh --full    # Full v1.0 ROADMAP gate (20 agents, 5000 MiB cache, 250k files)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

AGENTS=20
CACHE_MB=512
SOURCE_FILES=200
CACHE_FILES=2000
STATUS_FILES=250000
STATUS_RUNS=7
CONCURRENCY_AGENTS=10
CONCURRENCY_ROUNDS=3
OZ_BIN=""
VFSD_BIN=""
OZ_BACKEND="auto"
WORK_DIR=""
OUTPUT_DIR="${SCRIPT_DIR}/results"
SKIP_GIT_WARM_COPY=0
WITH_WARM_BUILDS=0
QUICK_MODE=0
ONLY_SUITE=""
VERIFY_CARGO_GATE=0

usage() {
  cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Presets:
  --quick, --smoke         Quick verification (5 agents, 50 MiB cache, 5,000 files, 3 runs)
  --full                   Full ROADMAP v1.0 gate (20 agents, 5000 MiB cache, 250,000 files)

Overrides:
  --agents N               Worktree agent count (default: ${AGENTS})
  --cache-mb M             Warm cache size in MiB (default: ${CACHE_MB})
  --source-files F         Tracked source file count for worktrees suite (default: ${SOURCE_FILES})
  --cache-files C          Warm cache file count for worktrees suite (default: ${CACHE_FILES})
  --files N                Monorepo file count for status suite (default: ${STATUS_FILES})
  --runs R                 Warm status iterations (default: ${STATUS_RUNS})
  --concurrency-agents N   Concurrent commit agent count (default: ${CONCURRENCY_AGENTS})
  --rounds R               Concurrent commit rounds (default: ${CONCURRENCY_ROUNDS})
  --skip-git-warm-copy     Skip copying warm caches for git worktrees
  --with-warm-builds       Also run Phase 5A warm incremental builds suite (bench_warm_builds.py)
  --only SUITE             Run only one suite: worktrees | status | vcbench | concurrency | warm_builds
  --verify-cargo-gate      Also cross-verify VCBench against Rust m3_exit_gates test
  --oz-bin PATH            Explicit path to \`oz\` binary
  --vfsd-bin PATH          Explicit path to \`oz-vfsd\` binary
  --oz-backend NAME        Backend override (default: auto; e.g. cas_hardlink in NoNewPrivs containers)
  --work-dir PATH          Base directory for temporary benchmark sandboxes
  --output-dir PATH        Output directory for latest.json & REPORT.md (default: benchmarks/results)
  -h, --help               Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --quick|--smoke)
      QUICK_MODE=1
      AGENTS=5
      CACHE_MB=50
      SOURCE_FILES=100
      CACHE_FILES=400
      STATUS_FILES=5000
      STATUS_RUNS=3
      CONCURRENCY_AGENTS=5
      CONCURRENCY_ROUNDS=2
      shift
      ;;
    --full)
      QUICK_MODE=0
      AGENTS=20
      CACHE_MB=5000
      SOURCE_FILES=200
      CACHE_FILES=4000
      STATUS_FILES=250000
      STATUS_RUNS=7
      CONCURRENCY_AGENTS=10
      CONCURRENCY_ROUNDS=3
      shift
      ;;
    --agents) AGENTS="$2"; shift 2 ;;
    --cache-mb) CACHE_MB="$2"; shift 2 ;;
    --source-files) SOURCE_FILES="$2"; shift 2 ;;
    --cache-files) CACHE_FILES="$2"; shift 2 ;;
    --files) STATUS_FILES="$2"; shift 2 ;;
    --runs) STATUS_RUNS="$2"; shift 2 ;;
    --concurrency-agents) CONCURRENCY_AGENTS="$2"; shift 2 ;;
    --rounds) CONCURRENCY_ROUNDS="$2"; shift 2 ;;
    --skip-git-warm-copy) SKIP_GIT_WARM_COPY=1; shift ;;
    --with-warm-builds) WITH_WARM_BUILDS=1; shift ;;
    --only) ONLY_SUITE="$2"; shift 2 ;;
    --verify-cargo-gate) VERIFY_CARGO_GATE=1; shift ;;
    --oz-bin) OZ_BIN="$2"; shift 2 ;;
    --vfsd-bin) VFSD_BIN="$2"; shift 2 ;;
    --oz-backend) OZ_BACKEND="$2"; shift 2 ;;
    --work-dir) WORK_DIR="$2"; shift 2 ;;
    --output-dir) OUTPUT_DIR="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

PYTHON="${PYTHON:-python3}"
if ! command -v "${PYTHON}" >/dev/null 2>&1; then
  echo "ERROR: python3 is required to run the Ozone benchmark suite." >&2
  exit 1
fi

mkdir -p "${OUTPUT_DIR}"

COMMON_ARGS=()
[[ -n "${OZ_BIN}" ]] && COMMON_ARGS+=(--oz-bin "${OZ_BIN}")
[[ -n "${VFSD_BIN}" ]] && COMMON_ARGS+=(--vfsd-bin "${VFSD_BIN}")
[[ -n "${WORK_DIR}" ]] && COMMON_ARGS+=(--work-dir "${WORK_DIR}")

TMP_JSON_DIR="$(mktemp -d "${OUTPUT_DIR}/.run-XXXXXX")"
trap 'rm -rf "${TMP_JSON_DIR}"' EXIT

WT_JSON="${TMP_JSON_DIR}/worktrees.json"
ST_JSON="${TMP_JSON_DIR}/status.json"
VC_JSON="${TMP_JSON_DIR}/vcbench.json"
CC_JSON="${TMP_JSON_DIR}/concurrency.json"

should_run() {
  [[ -z "${ONLY_SUITE}" || "${ONLY_SUITE}" == "$1" ]]
}

REPORT_ARGS=()

if should_run "worktrees"; then
  echo "==> [1/4] Running Worktree Spawn & Disk Footprint Benchmark (agents=${AGENTS}, cache=${CACHE_MB} MiB)..." >&2
  WT_EXTRA=()
  [[ "${SKIP_GIT_WARM_COPY}" -eq 1 ]] && WT_EXTRA+=(--skip-git-warm-copy)
  "${PYTHON}" "${SCRIPT_DIR}/bench_worktrees.py" \
    --agents "${AGENTS}" \
    --cache-mb "${CACHE_MB}" \
    --source-files "${SOURCE_FILES}" \
    --cache-files "${CACHE_FILES}" \
    --oz-backend "${OZ_BACKEND}" \
    --output "${WT_JSON}" \
    "${WT_EXTRA[@]}" \
    "${COMMON_ARGS[@]}"
  REPORT_ARGS+=(--worktrees "${WT_JSON}")
fi

if should_run "status"; then
  echo "==> [2/4] Running Monorepo Status Latency Benchmark (files=${STATUS_FILES}, runs=${STATUS_RUNS})..." >&2
  "${PYTHON}" "${SCRIPT_DIR}/bench_status_250k.py" \
    --files "${STATUS_FILES}" \
    --runs "${STATUS_RUNS}" \
    --oz-backend "${OZ_BACKEND}" \
    --output "${ST_JSON}" \
    "${COMMON_ARGS[@]}"
  REPORT_ARGS+=(--status "${ST_JSON}")
fi

if should_run "vcbench"; then
  echo "==> [3/4] Running Agent VCBench (Tokens & Commands)..." >&2
  VC_EXTRA=()
  [[ "${VERIFY_CARGO_GATE}" -eq 1 ]] && VC_EXTRA+=(--verify-cargo-gate)
  "${PYTHON}" "${SCRIPT_DIR}/bench_agent_vcbench.py" \
    --output "${VC_JSON}" \
    "${VC_EXTRA[@]}" \
    "${COMMON_ARGS[@]}"
  REPORT_ARGS+=(--vcbench "${VC_JSON}")
fi

if should_run "concurrency"; then
  echo "==> [4/4] Running Concurrent Commit Contention Benchmark (agents=${CONCURRENCY_AGENTS}, rounds=${CONCURRENCY_ROUNDS})..." >&2
  "${PYTHON}" "${SCRIPT_DIR}/bench_concurrency.py" \
    --agents "${CONCURRENCY_AGENTS}" \
    --rounds "${CONCURRENCY_ROUNDS}" \
    --oz-backend "${OZ_BACKEND}" \
    --output "${CC_JSON}" \
    "${COMMON_ARGS[@]}"
  REPORT_ARGS+=(--concurrency "${CC_JSON}")
fi

WARM_BUILDS_RAN=0
if [[ "${WITH_WARM_BUILDS}" -eq 1 || "${ONLY_SUITE}" == "warm_builds" ]]; then
  WARM_BUILDS_JSON="${OUTPUT_DIR}/warm_builds.json"
  WARM_BUILDS_MD="${OUTPUT_DIR}/WARM_BUILDS.md"
  WARM_ARGS=(
    --oz-backend "${OZ_BACKEND}"
    --output "${WARM_BUILDS_JSON}"
    --markdown-out "${WARM_BUILDS_MD}"
  )
  if [[ "${QUICK_MODE}" -eq 1 ]]; then
    WARM_ARGS+=(--quick)
  fi
  if [[ ${#COMMON_ARGS[@]} -gt 0 ]]; then
    WARM_ARGS+=("${COMMON_ARGS[@]}")
  fi
  echo "==> [opt] Running Phase 5A Warm Builds Benchmark..." >&2
  "${PYTHON}" "${SCRIPT_DIR}/bench_warm_builds.py" "${WARM_ARGS[@]}"
  WARM_BUILDS_RAN=1
fi

if [[ "${ONLY_SUITE}" == "warm_builds" && "${WARM_BUILDS_RAN}" -eq 1 ]]; then
  exit 0
fi

if [[ ${#REPORT_ARGS[@]} -eq 0 ]]; then
  echo "ERROR: Invalid --only value '${ONLY_SUITE}'. Expected: worktrees | status | vcbench | concurrency | warm_builds" >&2
  exit 2
fi

LATEST_JSON="${OUTPUT_DIR}/latest.json"
REPORT_MD="${OUTPUT_DIR}/REPORT.md"

echo "==> Generating ${LATEST_JSON} and ${REPORT_MD}..." >&2
"${PYTHON}" "${SCRIPT_DIR}/generate_report.py" \
  "${REPORT_ARGS[@]}" \
  --merge-out "${LATEST_JSON}" \
  --report-out "${REPORT_MD}"

echo ""
cat "${REPORT_MD}"
