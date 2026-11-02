#!/usr/bin/env bash
# Regenerates the 7z fixtures (7zip-*.7z) next to this script, using real 7-Zip.
# Made with 7-Zip 26.03 (x64) : Copyright (c) 1999-2026 Igor Pavlov (the version `7z` prints).
# Source tree (built in a temp dir, outside the repo):
#   src/hello.txt        "hello 7z\n" x 200
#   src/sub/data.bin     4096 bytes, byte i = (i*31) % 256
#   src/empty/           empty folder
#   src/zero.txt         empty file
#   src/Проект.txt       "unicode name\n"
# Usage: bash make-7z-fixtures.sh   (env SEVENZ=path/to/7z.exe and PY=python to override)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SEVENZ="${SEVENZ:-/c/Program Files/7-Zip/7z.exe}"
PY="${PY:-python}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

"$SEVENZ" | head -1

"$PY" - "$WORK/src" <<'PYEOF'
import os, sys
root = sys.argv[1]
for d in ("sub", "empty"):
    os.makedirs(os.path.join(root, d), exist_ok=True)
with open(os.path.join(root, "hello.txt"), "wb") as f:
    f.write(b"hello 7z\n" * 200)
with open(os.path.join(root, "sub", "data.bin"), "wb") as f:
    f.write(bytes((i * 31) % 256 for i in range(4096)))
open(os.path.join(root, "zero.txt"), "wb").close()
with open(os.path.join(root, "Проект.txt"), "wb") as f:
    f.write("unicode name\n".encode("utf-8"))
PYEOF

# make NAME SWITCHES...: archive $WORK/src into $HERE/NAME, then test it.
make() {
    local name="$1"; shift
    local out="$HERE/$name"
    rm -f "$out"
    (cd "$WORK" && "$SEVENZ" a -bd -y "$out" src "$@" > "$WORK/a.log") \
        || { cat "$WORK/a.log"; exit 1; }
    local pw=()
    case "$name" in *aes*) pw=(-psecret) ;; esac
    (cd "$WORK" && "$SEVENZ" t "$out" "${pw[@]}" -bd > "$WORK/t.log") \
        || { cat "$WORK/t.log"; echo "7z t failed for $name" >&2; exit 1; }
    echo "made $name ($(wc -c < "$out") bytes)"
}

make 7zip-lzma2-solid.7z -m0=lzma2 -mx=5 -ms=on
make 7zip-ppmd.7z -m0=ppmd
make 7zip-bzip2-nonsolid.7z -m0=bzip2 -ms=off
make 7zip-bcj2.7z -m0=bcj2 -m1=lzma -m2=lzma -m3=lzma -mb0:1 -mb0s1:2 -mb0s2:3
make 7zip-aes.7z -m0=lzma2 -psecret
make 7zip-aes-header.7z -m0=lzma2 -psecret -mhe=on

# Copy of fixture 1 with 16 bytes of packed data (offset 64..80, after the 32-byte signature
# header) XOR-ed with 0x55, so the data CRC fails.
"$PY" - "$HERE/7zip-lzma2-solid.7z" "$HERE/7zip-corrupt.7z" <<'PYEOF'
import sys
data = bytearray(open(sys.argv[1], "rb").read())
for i in range(64, 80):
    data[i] ^= 0x55
open(sys.argv[2], "wb").write(bytes(data))
PYEOF
echo "made 7zip-corrupt.7z ($(wc -c < "$HERE/7zip-corrupt.7z") bytes)"
