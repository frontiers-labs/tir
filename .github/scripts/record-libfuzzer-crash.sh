#!/usr/bin/env bash
# Turns a cargo-fuzz crash into a defect record, so the issue carries the input
# that crashed and the diagnosis rather than the name of a red job. The crash is
# identified by libFuzzer's own content hash of the input, which is stable: the
# same crash found again tomorrow lands on the same issue.
set -euo pipefail

target=$1
log=$2
defects=$3

mkdir -p "$defects"

crash=$(ls -t "utils/fuzz/artifacts/$target"/crash-* 2>/dev/null | head -1 || true)
if [ -z "$crash" ]; then
  echo "$target failed without leaving a crash input" >&2
  exit 0
fi

sha=$(basename "$crash")
sha=${sha#crash-}
# These targets parse text, so a crash input is usually readable. Keep it
# verbatim when it prints — an issue you can read beats one you have to decode —
# and fall back to base64 otherwise. Either way the bytes are exact, because
# replay feeds them straight back in.
if LC_ALL=C grep -qP '[^\x09\x0a\x0d\x20-\x7e]' "$crash"; then
  language=base64
  hint="# base64 -d the case above into crash.bin, then:"
  base64 -w0 "$crash" >input.txt
else
  language=text
  hint="# save the case above verbatim as crash.bin, then:"
  cp "$crash" input.txt
fi

# libFuzzer prints its diagnosis after this banner; everything before it is
# progress output that does not belong in an issue.
sed -n '/==ERROR\|panicked at\|SUMMARY:/,$p' "$log" | head -40 >trace.txt

jq -n \
  --arg target "$target" \
  --arg sha "$sha" \
  --arg language "$language" \
  --arg hint "$hint" \
  --arg run "${RUN_URL:-}" \
  --rawfile input input.txt \
  --rawfile trace trace.txt \
  '{
    job: "libfuzzer",
    summary: "Crash in \($target)",
    identity: "libfuzzer:\($target):\($sha)",
    reproduce: "\($hint)\ncargo +nightly fuzz run --fuzz-dir utils/fuzz \($target) crash.bin",
    details: "`\($target)` crashed on the input below.\n\nRun: \($run)\n\n```\n\($trace)```",
    artifact: $input,
    language: $language
  }' >"$defects/$target-$sha.json"

echo "recorded crash $sha in $target"
