#!/usr/bin/env bash
# Rewrite the byte lines of an obj check from what tir emits now, leaving every
# other line (comments, the decoding notes) where it is. Use only after
# .audit/gnu-as-check.sh says the new bytes are the ones GNU as produces.
set -euo pipefail
src=$1
tmp=$(mktemp)
target/debug/tir mc --march=x86_64 --filetype=obj-ascii "$src" \
  | sed -n 's/^ *\(\[.*\]\)$/\1/p' > "$tmp"
python3 - "$src" "$tmp" <<'PY'
import sys
src, bytes_file = sys.argv[1], sys.argv[2]
lines = open(bytes_file).read().split('\n')
lines = [l for l in lines if l]
out, i = [], 0
for line in open(src):
    stripped = line.lstrip('#/ \t').rstrip('\n')
    prefix = line[:len(line) - len(line.lstrip('#/ \t'))]
    if stripped.startswith(('CHECK: [0x', 'CHECK-NEXT: [0x')):
        keyword = stripped.split(':', 1)[0]
        out.append(f"{prefix}{keyword}: {lines[i]}\n")
        i += 1
    else:
        out.append(line)
open(src, 'w').writelines(out)
print(f"{src}: {i} byte lines rewritten")
PY
rm -f "$tmp"
