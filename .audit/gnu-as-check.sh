#!/usr/bin/env bash
# Compare the bytes tir emits for an assembly check against GNU as, instruction
# by instruction. The checks in backends/x86_64/checks/obj record GNU as output
# as their reference; this is that comparison, rerunnable.
#
# Usage: .audit/gnu-as-check.sh backends/x86_64/checks/obj/memory-escapes.S
set -uo pipefail
src=$1
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The assembly itself: lit directives and check lines are comments.
grep -v '^[[:space:]]*[#/]' "$src" > "$work/in.s"
as --64 -msyntax=intel -mnaked-reg -o "$work/in.o" "$work/in.s" 2>"$work/as.err" || {
  echo "GNU as rejected the input:"; cat "$work/as.err"; exit 1; }
objdump -d --insn-width=16 -M intel "$work/in.o" \
  | sed -n 's/^ *[0-9a-f]*:\t\([0-9a-f ]*\)\t.*/\1/p' \
  | tr -s ' ' | sed 's/ *$//' > "$work/gas.txt"

target/debug/tir mc --march=x86_64 --filetype=obj-ascii "$src" \
  | sed -n 's/^ *\[\(.*\)\]$/\1/p' \
  | tr -d ',' | sed 's/0x//g' | tr 'A-F' 'a-f' > "$work/tir.txt"

if diff -u "$work/gas.txt" "$work/tir.txt" > "$work/diff.txt"; then
  echo "$src: $(wc -l < "$work/gas.txt") instructions, bytes identical to GNU as"
else
  echo "$src: differs from GNU as (- gas, + tir)"
  cat "$work/diff.txt"
  exit 1
fi
