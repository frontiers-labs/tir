#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT

arch=${NISYROS_ROCM_ARCH:-$(rocminfo | awk '/^[[:space:]]*Name:[[:space:]]+gfx[[:digit:]]/ && !arch { arch = $2 } END { print arch }')}
test -n "$arch"
cargo build --locked -p nisyros
hipcc --genco --offload-arch="$arch" -O2 \
  nisyros/tests/add_one.hip.cpp -o "$temp/add_one.hsaco"
target/debug/nisyros-pack --rocm "$temp/add_one.hsaco" "$temp/add_one.nsy" \
  nisyros_add_one 24 8 64 1 1 addr:0 addr:8 value:16:4
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/rocm.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/rocm"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/batch.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/batch"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/backend_filter.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/backend_filter"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/rocm_peer.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/rocm_peer"
NISYROS_DISABLE_BACKENDS=xdna "$temp/rocm" "$temp/add_one.nsy"
NISYROS_DISABLE_BACKENDS=vulkan,xdna "$temp/batch"
if NISYROS_DISABLE_BACKENDS=vulkan,xdna "$temp/rocm_peer"; then
  :
else
  status=$?
  test "$status" -eq 77
fi
NISYROS_DISABLE_BACKENDS=rocm,vulkan,xdna "$temp/backend_filter"
