#!/usr/bin/env bash
# Regenerates the hicolor icon set from assets/zxraw.png.
#
# Output: packaging/linux/icons/hicolor/<N>x<N>/apps/zxraw.png for each N in SIZES that does
# not exceed the source. The source is 256 px, so nothing is upscaled and there is no 512 px
# icon; a larger master would add 512 (and 1024) to SIZES.
#
# Tool order: ImageMagick (magick or convert), then python3 -I with Pillow, then ffmpeg. The
# tools are only used if already installed; nothing is installed. The output is committed,
# so the package builds do not need any of them.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
source_png="$repo/assets/zxraw.png"
out_root="$here/icons/hicolor"
SIZES=(16 24 32 48 64 128 256 512)

[[ -f "$source_png" ]] || { echo "missing $source_png" >&2; exit 1; }

# Source size from the PNG IHDR chunk (bytes 16-23, big-endian), no image tool needed.
read -r src_w src_h < <(python3 -I -c '
import struct, sys
data = open(sys.argv[1], "rb").read(24)
assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
w, h = struct.unpack(">II", data[16:24])
print(w, h)
' "$source_png")
if [[ "$src_w" != "$src_h" ]]; then
    echo "source is not square (${src_w}x${src_h})" >&2
    exit 1
fi
src=$src_w

wanted=()
for n in "${SIZES[@]}"; do
    if (( n <= src )); then
        wanted+=("$n")
    else
        echo "skip ${n}x${n}: source is only ${src}px, not upscaling" >&2
    fi
done

if command -v magick >/dev/null 2>&1; then
    tool="magick"
elif command -v convert >/dev/null 2>&1 && convert -version 2>/dev/null | grep -q ImageMagick; then
    tool="convert"
elif python3 -I -c 'import PIL' >/dev/null 2>&1; then
    tool="pillow"
elif command -v ffmpeg >/dev/null 2>&1; then
    tool="ffmpeg"
else
    echo "need ImageMagick, python3 with Pillow, or ffmpeg" >&2
    exit 1
fi
echo "resizing $source_png (${src}px) with $tool"

for n in "${wanted[@]}"; do
    dir="$out_root/${n}x${n}/apps"
    mkdir -p "$dir"
    out="$dir/zxraw.png"
    case "$tool" in
        magick)
            magick "$source_png" -filter Lanczos -resize "${n}x${n}" -strip "$out" ;;
        convert)
            convert "$source_png" -filter Lanczos -resize "${n}x${n}" -strip "$out" ;;
        pillow)
            python3 -I -c '
import sys
from PIL import Image
src, n, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
img = Image.open(src).convert("RGBA")
if img.size != (n, n):
    img = img.resize((n, n), Image.LANCZOS)
img.save(out, format="PNG", optimize=True)
' "$source_png" "$n" "$out" ;;
        ffmpeg)
            ffmpeg -v error -y -i "$source_png" -vf "scale=${n}:${n}:flags=lanczos" -frames:v 1 "$out" ;;
    esac
done

echo "wrote:"
for n in "${wanted[@]}"; do
    echo "  icons/hicolor/${n}x${n}/apps/zxraw.png"
done
