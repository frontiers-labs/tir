#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT
output=${1:-/tmp/nisyros-performance-$(date -u +%Y%m%dT%H%M%SZ).csv}

cargo build --locked -p nisyros
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/performance.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/performance"
env -u VK_INSTANCE_LAYERS "$temp/performance" "$output"
printf '%s\n' "$output"
