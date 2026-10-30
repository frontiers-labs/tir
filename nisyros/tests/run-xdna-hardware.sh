#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
if test "$#" -lt 2 || test "$#" -gt 3; then
  echo "usage: $0 <xclbin> <cu-name> [tct-1col.elf]" >&2
  exit 2
fi
xclbin=$1
cu_name=$2
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT

cargo build -q -p nisyros
cargo run -q -p nisyros --bin nisyros-pack -- --xdna "$xclbin" \
  "$temp/validate.nsy" "$cu_name" 16 8 1 1 1 addr:0 value:8:4 value:12:4
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include nisyros/tests/xdna.c \
  -L target/debug -lnisyros -Wl,-rpath,"$repo/target/debug" -o "$temp/xdna"
"$temp/xdna" "$temp/validate.nsy"

if test "$#" -eq 3; then
  llvm-objcopy --dump-section .ctrltext="$temp/control.bin" "$3"
  cargo run -q -p nisyros --bin nisyros-pack -- --xdna "$xclbin" \
    "$temp/dma.nsy" "$cu_name" 32 8 1 1 1 \
    addr:0 value:8:4 value:12:4 addr:16 addr:24
  cc -std=c11 -Wall -Wextra -Werror -I nisyros/include nisyros/tests/xdna_dma.c \
    -L target/debug -lnisyros -Wl,-rpath,"$repo/target/debug" -o "$temp/xdna_dma"
  "$temp/xdna_dma" "$temp/dma.nsy" "$temp/control.bin"
fi
