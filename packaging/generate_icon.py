#!/usr/bin/env python3
"""Generate the ZXraw Explorer icons from the logo masters written by `draw_logo.py`.

Inputs (transparent, 1024 px, one pair per colour in `draw_logo.COLOURS`):
    assets/logo/zxraw-<colour>.png          full mark
    assets/logo/zxraw-<colour>-small.png    simplified mark for the smallest frames

Outputs:
    assets/icons/zxraw-<colour>.ico   every size in SIZES, PNG-compressed frames
    assets/icons/zxraw-<colour>.png   the 256 px frame, embedded by `app_icon.rs`
    assets/icons/icons.txt            the .ico files in exe resource order (read by build.rs):
                                      the default colour first, then the rest in `COLOURS` order
    assets/zxraw.png, assets/zxraw.ico   the default (Graphite) icon, for the installer and README

Frames up to SMALL_MAX px come from the simplified master (no letters, no blades).
"""

import io
import struct
import sys
from pathlib import Path

from PIL import Image

sys.path.insert(0, str(Path(__file__).resolve().parent))
from draw_logo import COLOURS  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
LOGO = ASSETS / "logo"
ICONS = ASSETS / "icons"
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
SMALL_MAX = 24  # frames up to this size use the simplified master


def frames_for(name: str) -> dict[int, Image.Image]:
    full = Image.open(LOGO / f"zxraw-{name}.png").convert("RGBA")
    small = Image.open(LOGO / f"zxraw-{name}-small.png").convert("RGBA")
    return {
        s: (small if s <= SMALL_MAX else full).resize((s, s), Image.Resampling.LANCZOS)
        for s in SIZES
    }


def write_ico(path: Path, frames: dict[int, Image.Image]) -> None:
    """An .ico with one PNG-compressed frame per size (Pillow's ICO writer cannot store a
    different image per size, so the container is written here)."""
    blobs = []
    for s in SIZES:
        buf = io.BytesIO()
        frames[s].save(buf, format="PNG", optimize=True)
        blobs.append(buf.getvalue())
    out = bytearray(struct.pack("<HHH", 0, 1, len(SIZES)))
    offset = 6 + 16 * len(SIZES)
    for s, blob in zip(SIZES, blobs):
        dim = 0 if s >= 256 else s
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    for blob in blobs:
        out += blob
    path.write_bytes(bytes(out))


def main() -> None:
    ICONS.mkdir(exist_ok=True)
    manifest = []
    for name, _ in COLOURS:
        frames = frames_for(name)
        write_ico(ICONS / f"zxraw-{name}.ico", frames)
        frames[256].save(ICONS / f"zxraw-{name}.png", format="PNG", optimize=True)
        manifest.append(f"zxraw-{name}.ico")
        if name == COLOURS[0][0]:
            frames[256].save(ASSETS / "zxraw.png", format="PNG", optimize=True)
            (ASSETS / "zxraw.ico").write_bytes((ICONS / f"zxraw-{name}.ico").read_bytes())
    (ICONS / "icons.txt").write_text("\n".join(manifest) + "\n", encoding="utf-8", newline="\n")
    print("wrote", len(manifest), "colours")


if __name__ == "__main__":
    main()
