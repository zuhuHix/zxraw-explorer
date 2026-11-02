#!/usr/bin/env bash
# Regenerates the RAR fixtures (rar*-*.rar, rar*-vol.*) next to this script, using rar.exe.
# ZXraw only reads RAR (the UnRAR licence forbids writing it); these were made with the
# command-line RAR of WinRAR (trial), taken out of the official installer from rarlab.com
# without installing it:  RAR 7.21 x64  Copyright (c) 1993-2026 Alexander Roshal  27 Apr 2026
# RAR 7 no longer writes RAR4 archives, so the -ma4 ones were made with
#                          RAR 6.24 x64  Copyright (c) 1993-2023 Alexander Roshal  3 Oct 2023
# Source tree (built in a temp dir, outside the repo):
#   src/hello.txt        "hello rar\n" x 200
#   src/sub/data.bin     4096 bytes, byte i = (i*31) % 256
#   src/empty/           empty folder
#   src/zero.txt         empty file
#   src/Проект.txt       "unicode name\n"
# Usage: bash make-rar-fixtures.sh   (env RAR=path/to/rar.exe (7.x), RAR4=path/to/rar.exe (6.x)
#        and PY=python to override)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
RAR="${RAR:-rar}"
RAR4="${RAR4:-rar6}"
PY="${PY:-python}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

"$RAR" | sed -n 2p
"$RAR4" | sed -n 2p

"$PY" - "$WORK/src" <<'PYEOF'
import os, sys
root = sys.argv[1]
for d in ("sub", "empty"):
    os.makedirs(os.path.join(root, d), exist_ok=True)
with open(os.path.join(root, "hello.txt"), "wb") as f:
    f.write(b"hello rar\n" * 200)
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
    local rar="$RAR"
    case " $* " in *" -ma4 "*) rar="$RAR4" ;; esac
    rm -f "$out"
    (cd "$WORK" && "$rar" a -r -idq -y "$@" "$out" src > "$WORK/a.log") \
        || { cat "$WORK/a.log"; exit 1; }
    local pw=(-p-)
    case "$name" in *enc*) pw=(-psecret) ;; esac
    (cd "$WORK" && "$rar" t -idq "${pw[@]}" "$out" > "$WORK/t.log") \
        || { cat "$WORK/t.log"; echo "rar t failed for $name" >&2; exit 1; }
    echo "made $name ($(wc -c < "$out") bytes)"
}

make rar5.rar -ma5
make rar4.rar -ma4
make rar5-solid.rar -ma5 -s
make rar4-solid.rar -ma4 -s
make rar5-enc.rar -ma5 -psecret
make rar4-enc.rar -ma4 -psecret
make rar5-hdrenc.rar -ma5 -hpsecret
make rar4-hdrenc.rar -ma4 -hpsecret

# Multi-volume sets: 24 KiB of incompressible bytes split into 10 KiB volumes.
"$PY" -c "import random,sys; random.seed(7); open(sys.argv[1],'wb').write(bytes(random.randrange(256) for _ in range(24576)))" "$WORK/src/big.bin"
rm -f "$HERE"/rar5-vol.part*.rar "$HERE"/rar4-vol.r?? "$HERE"/rar4-vol.rar
(cd "$WORK" && "$RAR" a -r -idq -y -ma5 -m0 -v10k "$HERE/rar5-vol.rar" src > /dev/null)
(cd "$WORK" && "$RAR4" a -r -idq -y -ma4 -m0 -v10k -vn "$HERE/rar4-vol.rar" src > /dev/null)
(cd "$WORK" && "$RAR" t -idq "$HERE/rar5-vol.part1.rar" && "$RAR" t -idq "$HERE/rar4-vol.rar")
ls "$HERE"/rar5-vol.* "$HERE"/rar4-vol.* | sed 's/^/made /'
rm "$WORK/src/big.bin"

# One stored file whose data is damaged: its CRC no longer matches.
(cd "$WORK/src" && "$RAR" a -idq -y -ma5 -m0 "$WORK/store.rar" hello.txt > /dev/null)
"$PY" - "$WORK/store.rar" "$HERE/rar5-badcrc.rar" <<'PYEOF'
import sys
data = bytearray(open(sys.argv[1], "rb").read())
at = data.index(b"hello rar\n") + 500
data[at] ^= 0x55
open(sys.argv[2], "wb").write(bytes(data))
PYEOF
echo "made rar5-badcrc.rar"

# Unsafe names: "../evil.txt" (patched into the header, its CRC recomputed) next to ok.txt.
mkdir -p "$WORK/u"
printf 'evil\n' > "$WORK/u/abcevil.txt"
printf 'ok\n' > "$WORK/u/ok.txt"
(cd "$WORK/u" && "$RAR" a -idq -y -ma5 -m0 "$WORK/u.rar" abcevil.txt ok.txt > /dev/null)
"$PY" "$HERE/rar5patch.py" "$WORK/u.rar" "$HERE/rar5-unsafe.rar" abcevil.txt --name ../evil.txt
echo "made rar5-unsafe.rar"

# A 2 GiB dictionary declared in the header of a compressed file (128 KiB << 14).
(cd "$WORK/src" && "$RAR" a -idq -y -ma5 -m3 "$WORK/d.rar" hello.txt > /dev/null)
"$PY" "$HERE/rar5patch.py" "$WORK/d.rar" "$HERE/rar5-bigdict.rar" hello.txt --dict-log 14
echo "made rar5-bigdict.rar"

# A hard link stored as a link (-oh): link.txt points at target.txt.
mkdir -p "$WORK/h"
printf 'target\n' > "$WORK/h/target.txt"
"$PY" -c "import os,sys; os.link(sys.argv[1], sys.argv[2])" "$WORK/h/target.txt" "$WORK/h/link.txt"
(cd "$WORK/h" && "$RAR" a -idq -y -ma5 -oh "$HERE/rar5-hardlink.rar" target.txt link.txt > /dev/null)
echo "made rar5-hardlink.rar"
