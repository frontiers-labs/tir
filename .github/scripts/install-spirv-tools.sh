#!/usr/bin/env bash
set -euxo pipefail

if [[ -f /etc/apt/blacksmith-ubuntu-mirrors.txt ]]; then
  printf '%s\n' 'https://archive.ubuntu.com/ubuntu' \
    | sudo tee /etc/apt/blacksmith-ubuntu-mirrors.txt >/dev/null
fi

sudo find /etc/apt -type f \( -name '*.list' -o -name '*.sources' \) \
  -exec sed -i \
    -e 's|http://archive.ubuntu.com/ubuntu|https://archive.ubuntu.com/ubuntu|g' \
    -e 's|http://security.ubuntu.com/ubuntu|https://security.ubuntu.com/ubuntu|g' {} +
sudo apt-get update
sudo apt-get install -y --no-install-recommends spirv-tools
