#!/usr/bin/env bash
# Compare the Rust each backend's TMDL generates now against a snapshot dir
# (default .audit/gen-master), reporting only real differences: the SymSource
# rows rustgen/behavior.rs emits vary run to run, and prettyplease indents by
# nesting depth, so both files are sorted and left-trimmed before diffing.
# Usage: .audit/gen-diff.sh [snapshot-dir]   (run after `cargo build`)
set -uo pipefail
snap=${1:-.audit/gen-master}
status=0
for b in x86_64 riscv arm64 ptx; do
  now=$(ls -t target/debug/build/*/out/$b.rs 2>/dev/null | head -1)
  [ -f "$snap/$b.rs" ] && [ -n "$now" ] || { echo "$b: no snapshot"; continue; }
  n=$(diff <(sed 's/^ *//' "$snap/$b.rs" | sort) <(sed 's/^ *//' "$now" | sort) \
      | grep -c '^[<>]')
  echo "$b: $n differing lines"
  [ "$n" = 0 ] || status=1
done
exit $status
