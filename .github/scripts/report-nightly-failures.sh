#!/usr/bin/env bash
# Files one issue per defect and closes it when that defect stops reproducing.
#
# Grouping by job is what made the old tracker useless: a fuzzer finds many
# distinct bugs, and one issue per job meant one issue for all of them, holding
# no evidence and impossible to close because the next night found something
# else. Defects are grouped by signature instead — a hash of the reduced
# program and the passes that miscompile it, which is stable across the seeds
# that found it — and the issue carries the minimal case and a command that
# reproduces just that one failure.
#
# A red job that recorded no defect is a build or harness problem rather than a
# miscompile, and still gets one issue per job, because "the build is broken"
# genuinely is a single bug.
set -euo pipefail

defects=${1:-defects}
xtask=${XTASK:-target/ci/xtask}

gh label create fuzz --description "Found by the nightly fuzzers" --color d93f0b 2>/dev/null || true

open_issues=$(gh issue list --state open --limit 500 --json number,title)

# The issue tracking `$1`, matched on the signature or job name in its title.
issue_for() {
  jq -r --arg needle "$1" '.[] | select(.title | contains($needle)) | .number' \
    <<<"$open_issues" | head -1
}

reproduce() {
  case "$1" in
    corpus) echo 'cargo xtask fcc-fuzz --corpus' ;;
    fcc-bench) echo 'cargo xtask fcc-bench' ;;
    differential-fuzz) echo 'cargo xtask fcc-fuzz --self-test' ;;
    libfuzzer) echo 'cargo +nightly fuzz run --fuzz-dir utils/fuzz <target>' ;;
    lints) echo 'cargo clippy --workspace --all-targets --no-deps -- -D warnings' ;;
    *) echo 'cargo nextest run --workspace --locked' ;;
  esac
}

filed=""
for record in "$defects"/*.json; do
  [ -e "$record" ] || continue
  rendered=$("$xtask" fcc-fuzz --render "$record")
  title=$(head -1 <<<"$rendered")
  signature=${title##*\[}
  signature=${signature%\]}
  filed+="$signature"$'\n'
  if [ -n "$(issue_for "[$signature]")" ]; then
    echo "already tracked: $title"
    continue
  fi
  gh issue create --title "$title" --body "$(tail -n +2 <<<"$rendered")" \
    --label bug --label fuzz
done

# Replaying every tracked defect is what makes closing honest: the issue goes
# away when the bug does, whoever fixed it and whether or not they said so.
for list in "$defects"/fixed*.txt; do
  [ -e "$list" ] || continue
  while read -r signature; do
    [ -n "$signature" ] || continue
    # Refiled this very run: nondeterministic, so it is not fixed.
    grep -qxF "$signature" <<<"$filed" && continue
    number=$(issue_for "[$signature]")
    [ -n "$number" ] || continue
    gh issue close "$number" --comment "No longer reproduces as of $RUN_URL."
  done <"$list"
done

jobs_with_defects=$(jq -rs '[.[].job] | unique | .[]' "$defects"/*.json 2>/dev/null || true)

while read -r job result; do
  title="Nightly infrastructure failure: $job"
  number=$(issue_for "$title")
  case "$result" in
    failure)
      # The defects it found are the report; a generic issue on top would be
      # the very duplicate this script exists to avoid.
      if grep -qxF "$job" <<<"$jobs_with_defects"; then
        continue
      fi
      body="\`$job\` failed without recording a defect, so this is a build or
harness problem rather than a miscompile.

- Run: $RUN_URL
- Reproduce: \`$(reproduce "$job")\`

Closes automatically once \`$job\` completes again."
      if [ -n "$number" ]; then
        gh issue comment "$number" --body "Still failing: $RUN_URL"
      else
        gh issue create --title "$title" --body "$body" --label bug
      fi
      ;;
    success)
      if [ -n "$number" ]; then
        gh issue close "$number" --comment "Passing again: $RUN_URL"
      fi
      ;;
  esac
done < <(jq -r 'to_entries[] | "\(.key) \(.value.result)"' <<<"$RESULTS")
