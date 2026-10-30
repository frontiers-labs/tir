#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT

cargo build --locked -p nisyros
cc -std=c11 -Wall -Wextra -Werror -fPIC -shared -I nisyros/include \
  nisyros/tests/external_provider.c -o "$temp/libnisyros-test-provider.so"
cc -std=c11 -Wall -Wextra -Werror -I nisyros/include \
  nisyros/tests/external_provider_client.c -L target/debug -lnisyros \
  -Wl,-rpath,"$repo/target/debug" -o "$temp/external-provider-client"
NISYROS_PLUGIN_LIBRARY="$temp/libnisyros-test-provider.so" \
  VK_ICD_FILENAMES=/nonexistent "$temp/external-provider-client"
