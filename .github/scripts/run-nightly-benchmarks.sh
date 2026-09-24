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
  micro)
    python3 .github/scripts/discover-nightly-benchmarks.py
    ;;
  *)
    echo "Usage: $0 programs|micro" >&2
    exit 2
    ;;
esac
