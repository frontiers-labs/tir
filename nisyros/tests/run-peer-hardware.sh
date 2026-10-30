#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
export NISYROS_DISABLE_BACKENDS=rocm
icd=${NISYROS_VULKAN_ICD:-/usr/share/vulkan/icd.d/radeon_icd.json}
test -f "$icd"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT
cp "$icd" "$temp/duplicate.json"

cargo build --locked -p nisyros
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/two_devices.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/two_devices"
VK_DRIVER_FILES="$icd:$temp/duplicate.json" \
  VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation \
  "$temp/two_devices"
