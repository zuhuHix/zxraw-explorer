#!/usr/bin/env bash
# Regenerates the tar fixtures (`fixtures/tar-*`) with real tools: GNU tar 1.35 (Git for
# Windows' /usr/bin/tar), bsdtar 3.8 (Windows' tar.exe), gzip, and Python's tarfile for
# entries Windows can't make on disk (symbolic links, devices, FIFOs).
# Run from this folder in Git Bash. The tests in `tests_tar.rs` describe the expected contents.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

long="deep/$(printf 'd%.0s' {1..60})/$(printf 'f%.0s' {1..70}).txt"
mkdir -p src/docs "src/$(dirname "$long")"
printf 'hello tar\n' > src/docs/readme.txt
printf 'caf\xc3\xa9 au lait\n' > "src/docs/café.txt"
printf 'long name\n' > "src/$long"
head -c 9000 /dev/zero | tr '\0' 'z' > src/zeds.bin
ln src/docs/readme.txt src/docs/hardlink.txt
touch -d '2021-06-01 12:00:00 UTC' src/docs/readme.txt "src/docs/café.txt" src/zeds.bin

# GNU format: 'L' long names, 'K'/'1' hard link. Fixed owners and times.
/usr/bin/tar --format=gnu --owner=0 --group=0 --numeric-owner -C src -cf "$here/tar-gnu.tar" docs deep zeds.bin
# POSIX (PAX) through gzip, as two gzip members (the tar split in the middle).
/usr/bin/tar --format=posix --owner=0 --group=0 --numeric-owner -C src -cf pax.tar docs deep zeds.bin
head -c 5120 pax.tar | gzip -9n > "$here/tar-pax-2members.tar.gz"
tail -c +5121 pax.tar | gzip -9n >> "$here/tar-pax-2members.tar.gz"
# bsdtar: ustar through xz, PAX through zstd (with its frame checksum), V7 plain.
tarexe=/c/Windows/System32/tar.exe
(cd src && "$tarexe" --format ustar -J -cf "$here/tar-bsd-ustar.tar.xz" docs zeds.bin)
(cd src && "$tarexe" --format pax --zstd -cf "$here/tar-bsd-pax.tar.zst" docs deep zeds.bin)
(cd src && "$tarexe" --format v7 -cf "$here/tar-bsd-v7.tar" docs zeds.bin)

# Links, devices and FIFOs (never extracted), and a name given twice (the last one wins).
python - "$here/tar-links.tar" <<'PY'
import io, sys, tarfile
with tarfile.open(sys.argv[1], "w", format=tarfile.GNU_FORMAT) as t:
    def add(name, data=b"", **kw):
        i = tarfile.TarInfo(name)
        for k, v in kw.items():
            setattr(i, k, v)
        i.size = len(data)
        i.mtime = 1600000000
        t.addfile(i, io.BytesIO(data))
    add("a/real.txt", b"first version\n")
    add("a/link-out", type=tarfile.SYMTYPE, linkname="/etc/passwd")
    add("a/link-in", type=tarfile.SYMTYPE, linkname="real.txt")
    add("a/hard-out", type=tarfile.LNKTYPE, linkname="../../outside.txt")
    add("a/tty", type=tarfile.CHRTYPE, devmajor=4, devminor=1)
    add("a/disk", type=tarfile.BLKTYPE, devmajor=8, devminor=0)
    add("a/pipe", type=tarfile.FIFOTYPE)
    add("a/real.txt", b"second version wins\n")
PY

# A single file in two gzip members, with its name in the first header.
printf 'part one\n' > single.txt
gzip -c single.txt > "$here/tar-single-2members.txt.gz"
printf 'part two\n' | gzip -c >> "$here/tar-single-2members.txt.gz"
ls -l "$here"/tar-*
