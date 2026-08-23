#!/usr/bin/env bash
# Files one issue per failing nightly job and closes it when the job goes green
# again. Grouping is by job, not by run or fuzz seed: the seed rotates daily, so
# keying on it would guarantee a fresh duplicate every night. The seed and the
# failing run are recorded in a comment on the one issue instead.
set -euo pipefail

open_issues=$(gh issue list --state open --limit 200 --json number,title)

reproduce() {
  case "$1" in
    corpus) echo 'cargo xtask fcc-fuzz --corpus' ;;
    differential-fuzz) echo "cargo xtask fcc-fuzz --seed $SEED --iterations 500" ;;
    libfuzzer) echo 'cargo +nightly fuzz run --fuzz-dir utils/fuzz <target>' ;;
    lints) echo 'cargo clippy --workspace --all-targets --no-deps -- -D warnings' ;;
    *) echo 'cargo nextest run --workspace --locked' ;;
  esac
}

while read -r job result; do
  title="Nightly failure: $job"
  number=$(jq -r --arg t "$title" '.[] | select(.title == $t) | .number' <<<"$open_issues" | head -1)

  case "$result" in
    failure)
      body="\`$job\` failed in the nightly run.

- Run: $RUN_URL
- Reproduce: \`$(reproduce "$job")\`

This issue tracks every nightly failure of \`$job\`. It is reused rather than
reopened per run, and closes automatically once the job passes again."
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
