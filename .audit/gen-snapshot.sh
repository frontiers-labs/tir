#!/usr/bin/env bash
# Dump the Rust every backend's TMDL generates, into $1 (default .audit/gen).
# Diff two runs to see exactly what a tmdlc change did to each backend.
# The SymSource lines rustgen/behavior.rs emits vary run to run, so they are
# sorted rather than compared in place.
set -euo pipefail
out=${1:-.audit/gen}
mkdir -p "$out"
cargo build --bin tmdlc >/dev/null 2>&1
tmdlc=target/debug/tmdlc

dump() { # dialect, dir, files...
  local dialect=$1 dir=$2; shift 2
  local args=()
  for f in "$@"; do args+=("$dir/$f"); done
  "$tmdlc" --action=emit-rust --dialect="$dialect" --output=- "${args[@]}" \
    | sort > "$out/$dialect.rs"
  "$tmdlc" --action=emit-operation-list --dialect="$dialect" --output=- "${args[@]}" \
    > "$out/$dialect-ops.rs"
}

dump x86_64 backends/x86_64/defs main.tmdl base.tmdl arith_ext.tmdl conditional.tmdl \
  memory_ext.tmdl atomics.tmdl ordering.tmdl float.tmdl perf.tmdl cpu/intel/tiger_lake.tmdl
dump riscv backends/riscv/defs $(cd backends/riscv/defs && ls *.tmdl)
dump arm64 backends/arm64/defs $(cd backends/arm64/defs && ls *.tmdl)
dump ptx gpu/defs $(cd gpu/defs && ls *.tmdl)

grep -c '^pub struct .*Op' "$out"/*-ops.rs || true
