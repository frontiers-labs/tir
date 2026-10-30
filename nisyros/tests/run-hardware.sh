#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT

cargo build --locked -p nisyros
glslangValidator -V --target-env vulkan1.3 -S comp \
  nisyros/tests/add_one.comp -o "$temp/add_one.spv"
spirv-val --target-env vulkan1.3 "$temp/add_one.spv"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/open.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/open"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/kernel.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/kernel"

export VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation
"$temp/open" strict
target/debug/nisyros-pack "$temp/add_one.spv" "$temp/add_one.nsy" \
  main 24 8 64 1 1 addr:0 addr:8 value:16:4
"$temp/kernel" "$temp/add_one.nsy"
