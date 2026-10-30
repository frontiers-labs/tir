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
  nisyros/tests/open.c -L target/debug -lnisyros -ldl \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/open"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/plugin.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/plugin"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/kernel.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/kernel"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/batch.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/batch"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/ownership.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/ownership"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/matrix.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/matrix"

export VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation
"$temp/open" strict
"$temp/plugin" strict
"$temp/batch"
if "$temp/ownership"; then
  :
else
  status=$?
  test "$status" -eq 77
fi
target/debug/nisyros-pack "$temp/add_one.spv" "$temp/add_one.nsy" \
  main 24 8 64 1 1 addr:0 addr:8 value:16:4
"$temp/kernel" "$temp/add_one.nsy"
if "$temp/matrix" --probe; then
  glslangValidator -V --target-env vulkan1.3 -S comp \
    nisyros/tests/matrix.comp -o "$temp/matrix.spv"
  spirv-val --target-env vulkan1.3 "$temp/matrix.spv"
  spirv-dis "$temp/matrix.spv" -o "$temp/matrix.spvasm"
  rg -q 'OpCooperativeMatrixMulAddKHR' "$temp/matrix.spvasm"
  target/debug/nisyros-pack "$temp/matrix.spv" "$temp/matrix.nsy" \
    main 8 8 64 1 1 addr:0 matrix:16:16:16:0:0:1:1:0:3 float16 memory-model
  "$temp/matrix" "$temp/matrix.nsy"
else
  status=$?
  test "$status" -eq 77
fi
