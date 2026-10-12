#!/usr/bin/env python3
"""Generate the ZXraw Explorer window PNGs and multi-size Windows ICOs from the logo masters.

Inputs (high-resolution artwork with a transparent background):
    assets/logo/zxraw-square.png  -> assets/zxraw.png, assets/zxraw.ico (the default icon)
    assets/logo/zxraw-round.png   -> assets/zxraw-round.png, assets/zxraw-round.ico

Each master is trimmed to its solid shape (dropping the faint outer shadow), centred on a
square canvas with a small margin, and downscaled per size so small icons stay sharp.
"""

from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
# Pixels at least this opaque count as the shape; the softer shadow around it is trimmed.
SOLID_ALPHA = 200
MARGIN = 0.02


def square_master(path: Path) -> Image.Image:
    image = Image.open(path).convert("RGBA")
    solid = image.getchannel("A").point(lambda a: 255 if a >= SOLID_ALPHA else 0)
    left, top, right, bottom = solid.getbbox()
    shape = image.crop((left, top, right, bottom))
    side = round(max(shape.size) * (1 + 2 * MARGIN))
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.alpha_composite(shape, ((side - shape.width) // 2, (side - shape.height) // 2))
    return canvas


def build(master_name: str, png_name: str, ico_name: str) -> None:
    master = square_master(ASSETS / "logo" / master_name)
    frames = [master.resize((size, size), Image.Resampling.LANCZOS) for size in SIZES]
    frames[-1].save(ASSETS / png_name, format="PNG", optimize=True)
    frames[-1].save(
        ASSETS / ico_name,
        format="ICO",
        sizes=[(size, size) for size in SIZES],
        append_images=frames[:-1],
    )


def main() -> None:
    build("zxraw-square.png", "zxraw.png", "zxraw.ico")
    build("zxraw-round.png", "zxraw-round.png", "zxraw-round.ico")


if __name__ == "__main__":
    main()
