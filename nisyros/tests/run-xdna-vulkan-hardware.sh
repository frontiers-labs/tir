#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT

cargo build -q -p nisyros
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/xdna_vulkan.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/xdna_vulkan"
NISYROS_DISABLE_BACKENDS=rocm "$temp/xdna_vulkan"
