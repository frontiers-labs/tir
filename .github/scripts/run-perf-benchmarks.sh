#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."
export LC_ALL=C

# Results shift when the toolchain changes, with no change in TIR.
mkdir -p bench-results
echo "$(uname -m): $(rustc --version); $(ldd --version | sed -n 1p)" > bench-results/environment

run() {
  local program=$1 package=$2 benchmark=$3
  local bundle="bench-results/$program/$package-$benchmark"
  rm -rf "$bundle"
  export CRITERION_HOME="$PWD/$bundle/criterion"
  cargo xtask bench -- -p "$package" --bench "$benchmark"
  if [ "$program" = functions ]; then
    cargo bench --locked -p "$package" --bench "$benchmark" -- --list |
      sed -n 's/: benchmark$//p' | sort > "$bundle/listed"
    find "$CRITERION_HOME" -path '*/new/benchmark.json' -exec jq -r '.full_id' {} \; |
      sort > "$bundle/recorded"
    test -s "$bundle/listed"
    diff "$bundle/listed" "$bundle/recorded"
  fi
  {
    if [ "$program" = functions ]; then
      while IFS= read -r sample; do
        jq -n --slurpfile sample "$sample" --slurpfile benchmark "${sample%/*}/benchmark.json" '
          {($benchmark[0].full_id): {latency: {samples:
            [$sample[0] | range(0; .times | length) as $i | .times[$i] / .iters[$i]]}}}'
      done < <(find "$CRITERION_HOME" -path '*/new/sample.json')
    else
      find "$CRITERION_HOME" -path '*/new/bencher.json' -exec cat {} \;
    fi
  } | jq -s --arg program "$program" --arg architecture "$(uname -m)" \
    --arg environment "$(uname -m) $(lscpu | sed -n 's/^Model name:[[:space:]]*//p')" \
    -f .github/scripts/criterion-to-benchboard.jq > "$bundle/summary.tmp"
  mv "$bundle/summary.tmp" "$bundle/summary.json"
}

case "${1:-}" in
  coremark|dhrystone)
    run "$1" fcc "$1"
    ;;
  functions)
    targets=$(cargo metadata --no-deps --format-version 1 | jq -r '
      .packages[] as $p | $p.targets[] | select(.kind | index("bench")) |
      select($p.name != "fcc" or (.name != "coremark" and .name != "dhrystone" and .name != "whetstone")) |
      [$p.name, .name] | @tsv')
    while IFS=$'\t' read -r package benchmark; do
      run functions "$package" "$benchmark"
    done <<< "$targets"
    ;;
  *)
    echo "Usage: $0 coremark|dhrystone|functions" >&2
    exit 2
    ;;
esac
