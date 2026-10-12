#!/usr/bin/env python3
"""Generate the ZXraw Explorer icons from the transparent logo masters.

Inputs (high-resolution artwork on a fully transparent background):
    assets/logo/zxraw-square-transparent.png
    assets/logo/zxraw-round-transparent.png

Outputs:
    assets/icons/<shape>-art.png    256 px cleaned artwork (the app tints it at runtime)
    assets/icons/<shape>-tile.png   256 px mask of the inside of the outline
    assets/icons/zxraw-<shape>-<accent>.ico   the artwork on an accent tile, every size
    assets/icons/icons.txt          the .ico files in exe resource order (read by build.rs)
    assets/zxraw.png, assets/zxraw.ico   the default icon (square on blue), for the
                                         installer and the README

The artwork is a white X and grey aperture inside a grey outline. It is placed on a tile
filled with the accent colour so the icon follows the theme; the tile is clipped to the
inside of the outline. `crates/forge-app/src/app_icon.rs` composites the window icon the
same way, and its tests compare the result against these .ico files, so `ACCENTS` and
`tile_colours` must stay in step with `theme::Accent`.
"""

from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
ICONS = ASSETS / "icons"
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
SHAPES = ("square", "round")
# Accent presets, in `theme::Accent::ALL` order. The first is the default and the exe's
# own file icon.
ACCENTS = (
    ("blue", (77, 144, 254)),
    ("purple", (151, 117, 250)),
    ("pink", (232, 98, 170)),
    ("red", (232, 72, 85)),
    ("orange", (240, 135, 50)),
    ("green", (52, 180, 110)),
    ("teal", (32, 178, 190)),
)
# Pixels at least this opaque count as the shape.
SOLID_ALPHA = 128
# Background removal left a haze of faint pixels; anything fainter than this, or further
# than a few pixels from the solid shape, is cleared so it never shows as specks on a tile.
HAZE_ALPHA = 16
MARGIN = 0.02


def clean_master(path: Path) -> Image.Image:
    image = Image.open(path).convert("RGBA")
    solid = image.getchannel("A").point(lambda a: 255 if a >= SOLID_ALPHA else 0)
    # Opening drops isolated solid specks; the dilation keeps the shape's soft edges.
    solid = solid.filter(ImageFilter.MinFilter(5)).filter(ImageFilter.MaxFilter(5))
    near = np.asarray(solid.filter(ImageFilter.MaxFilter(9))) > 0
    rgba = np.array(image)
    alpha = rgba[..., 3]
    alpha[(alpha < HAZE_ALPHA) | ~near] = 0
    rgba[alpha == 0] = 0
    cleaned = Image.fromarray(rgba, "RGBA")
    left, top, right, bottom = solid.getbbox()
    shape = cleaned.crop((left, top, right, bottom))
    side = round(max(shape.size) * (1 + 2 * MARGIN))
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.alpha_composite(shape, ((side - shape.width) // 2, (side - shape.height) // 2))
    return canvas


def tile_mask(art: Image.Image) -> Image.Image:
    """Everything enclosed by the outline, shrunk a little so it hides under the outline."""
    solid = art.getchannel("A").point(lambda a: 255 if a >= SOLID_ALPHA else 0)
    outside = solid.copy()
    ImageDraw.floodfill(outside, (0, 0), 128)
    mask = outside.point(lambda v: 0 if v == 128 else 255)
    inset = max(3, art.width // 120) | 1
    return mask.filter(ImageFilter.MinFilter(inset))


def tile_colours(accent):
    """Top and bottom of the tile's vertical gradient: a deepened accent, so the white X
    keeps its contrast on every preset."""
    top = tuple(round(c * 0.86) for c in accent)
    bottom = tuple(round(c * 0.56) for c in accent)
    return top, bottom


def compose(art: Image.Image, mask: Image.Image, accent) -> Image.Image:
    size = art.width
    top, bottom = tile_colours(accent)
    rows = np.arange(size, dtype=np.float64)
    t = ((rows + 0.5) / size)[:, None]
    fill = np.round(np.array(top) * (1 - t) + np.array(bottom) * t).astype(np.uint8)
    tile = np.zeros((size, size, 4), np.uint8)
    tile[..., :3] = fill[:, None, :]
    tile[..., 3] = np.asarray(mask)
    out = Image.fromarray(tile, "RGBA")
    out.alpha_composite(art)
    return out


def main() -> None:
    ICONS.mkdir(exist_ok=True)
    manifest = []
    for shape in SHAPES:
        master = clean_master(ASSETS / "logo" / f"zxraw-{shape}-transparent.png")
        mask = tile_mask(master)
        arts = [master.resize((s, s), Image.Resampling.LANCZOS) for s in SIZES]
        masks = [mask.resize((s, s), Image.Resampling.LANCZOS) for s in SIZES]
        arts[-1].save(ICONS / f"{shape}-art.png", format="PNG", optimize=True)
        masks[-1].save(ICONS / f"{shape}-tile.png", format="PNG", optimize=True)
        for name, accent in ACCENTS:
            frames = [compose(a, m, accent) for a, m in zip(arts, masks)]
            ico = f"zxraw-{shape}-{name}.ico"
            frames[-1].save(
                ICONS / ico,
                format="ICO",
                sizes=[(s, s) for s in SIZES],
                append_images=frames[:-1],
            )
            manifest.append(ico)
            if (shape, name) == ("square", ACCENTS[0][0]):
                frames[-1].save(ASSETS / "zxraw.png", format="PNG", optimize=True)
                (ASSETS / "zxraw.ico").write_bytes((ICONS / ico).read_bytes())
    (ICONS / "icons.txt").write_text("\n".join(manifest) + "\n", encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
