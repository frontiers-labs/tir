#!/usr/bin/env bash
# Feeds every crash this target is already tracking back into the fuzzer and
# lists the ones that no longer crash. Without this a crash issue could only be
# closed by hand, which is how the last tracker ended up full of stale bugs.
#
# The C-program defects are replayed by `xtask fcc-fuzz --replay`; this covers
# the raw fuzzer inputs, which only the fuzzer knows how to run.
set -euo pipefail

target=$1
defects=$2
xtask=${XTASK:-target/ci/xtask}

mkdir -p "$defects"
fixed="$defects/fixed-$target.txt"
: >"$fixed"

gh issue list --state open --label fuzz --limit 500 --json body >tracked.json ||
  echo '[]' >tracked.json
"$xtask" fcc-fuzz --extract tracked <tracked.json

for record in tracked/*.json; do
  [ -e "$record" ] || continue
  jq -e --arg prefix "libfuzzer:$target:" '.identity | startswith($prefix)' \
    "$record" >/dev/null || continue

  if [ "$(jq -r .language "$record")" = base64 ]; then
    jq -r .artifact "$record" | base64 -d >crash.bin
  else
    jq -rj .artifact "$record" >crash.bin
  fi

  signature=$(basename "$record" .json)
  if cargo +nightly fuzz run --fuzz-dir utils/fuzz "$target" crash.bin >/dev/null 2>&1; then
    echo "FIXED $signature"
    echo "$signature" >>"$fixed"
  else
    echo "STILL $signature"
  fi
done
