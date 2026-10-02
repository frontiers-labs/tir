#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

case "${1:-}" in
  programs)
    # Keep validation and fixed workload arguments identical to local runs.
    cargo bench --locked -p fcc --bench coremark -- \
      --engine cachegrind --phase all --compiler fcc --level O2 \
      --min-cases 7 --timeout 18000 --output "$PWD/bench-results/coremark"
    cargo bench --locked -p fcc --bench dhrystone -- \
      --engine cachegrind --phase all --compiler fcc --level O2 \
      --min-cases 3 --timeout 18000 --output "$PWD/bench-results/dhrystone"
    ;;
  native)
    # Wall time and peak memory of fcc, gcc and clang, sampled in rotated order
    # on one runner. Shared runners are too noisy to gate on these, so they are
    # recorded for the comparison between compilers. One CoreMark round takes
    # over a minute, which is why the sample count is below the default.
    cargo bench --locked -p fcc --bench coremark -- \
      --level O2 --sample-size 5 --warmups 1 --min-cases 21 \
      --output "$PWD/bench-results/native/coremark"
    cargo bench --locked -p fcc --bench dhrystone -- \
      --level O2 --sample-size 5 --warmups 1 --min-cases 9 \
      --output "$PWD/bench-results/native/dhrystone"
    ;;
  functions)
    # Every function benchmark in the workspace. A new one needs no edit here.
    cargo bench --locked --workspace -- \
      --engine cachegrind --skip-programs --output "$PWD/bench-results/functions"
    # A target that records nothing must fail the job, so compare what ran
    # with what the workspace lists.
    listed=$(cargo bench --locked --workspace -- --list --skip-programs | sed -n 's/: benchmark$//p' | sort)
    recorded=$(jq -r '.results | keys[]' bench-results/functions/*/summary.json | sort)
    if [ -z "$listed" ] || [ "$listed" != "$recorded" ]; then
      echo "Recorded function benchmarks differ from the listed ones:" >&2
      diff <(echo "$listed") <(echo "$recorded") >&2 || true
      exit 1
    fi
    ;;
  environment)
    # Counts shift when any of these change, with no change in TIR.
    rust=$(rustc --version)
    valgrind=$(valgrind --version)
    libc=$(ldd --version | sed -n 1p)
    echo "$rust; $valgrind; $libc"
    ;;
  *)
    echo "Usage: $0 programs|native|functions|environment" >&2
    exit 2
    ;;
esac
