#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

# Results shift when the toolchain changes, with no change in TIR.
mkdir -p bench-results
echo "$(uname -m): $(rustc --version); $(ldd --version | sed -n 1p)" > bench-results/environment

case "${1:-}" in
  # Wall time and peak memory of fcc, gcc and clang, sampled in rotated order
  # until every median is stable. Validation and workload arguments match local
  # runs. The case count catches a filter or a compiler that measured nothing.
  coremark)
    cargo bench --locked -p fcc --bench coremark -- \
      --level O2 --warmups 1 --min-cases 21 --output "$PWD/bench-results/coremark"
    ;;
  dhrystone)
    cargo bench --locked -p fcc --bench dhrystone -- \
      --level O2 --warmups 1 --min-cases 9 --output "$PWD/bench-results/dhrystone"
    ;;
  functions)
    # Every function benchmark in the workspace. A new one needs no edit here.
    cargo bench --locked --workspace -- \
      --skip-programs --output "$PWD/bench-results/functions"
    # A target that records nothing must fail the job, so compare what ran
    # with what the workspace lists.
    listed=$(cargo bench --locked --workspace -- --list --skip-programs | sed -n 's/: benchmark$//p' | sort)
    recorded=$(jq -r '.cases[].id' bench-results/functions/*/results.json | sort)
    if [ -z "$listed" ] || [ "$listed" != "$recorded" ]; then
      echo "Recorded function benchmarks differ from the listed ones:" >&2
      diff <(echo "$listed") <(echo "$recorded") >&2 || true
      exit 1
    fi
    ;;
  *)
    echo "Usage: $0 coremark|dhrystone|functions" >&2
    exit 2
    ;;
esac
