#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo metadata --locked --format-version 1 | python3 -c '
import json, sys
packages = {package["name"] for package in json.load(sys.stdin)["packages"]}
assert not packages.intersection({"tir-egraph", "tir-relational"}), "legacy engine dependency"
'
test ! -e libs/egraph
test ! -e libs/relational
cargo run --locked -p tir --example ir_storage
LIT_FILTER='^core/checks/IRRoundtrip/' cargo xtask test -- -E 'binary(lit)' --no-fail-fast
