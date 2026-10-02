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
  functions)
    # Every function benchmark in the workspace. A new one needs no edit here.
    cargo bench --locked --workspace -- \
      --engine cachegrind --skip-programs --output "$PWD/bench-results/functions"
    ls bench-results/functions/*/summary.bmf.json >/dev/null
    ;;
  *)
    echo "Usage: $0 programs|functions" >&2
    exit 2
    ;;
esac
