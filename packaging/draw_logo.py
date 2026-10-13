#!/usr/bin/env python3
"""Draw the ZXraw Explorer mark: a freestanding folder-camera, one per colour.

Writes the transparent 1024 px masters that `generate_icon.py` turns into the icons:
    assets/logo/zxraw-<colour>.png          full mark (frames of 32 px and up)
    assets/logo/zxraw-<colour>-small.png    simplified mark (frames of 16, 20 and 24 px)

The mark is the reference in the header of the design SVG (100-unit grid): a folder tab on
the left, a rounded body, a lens with aperture blades on the right, and an outline Z with a
solid white X cutting into it. There is no tile and no outline ring. Every colour is derived
from the base colour C by the formulas below (mix(a, b, t) = a + (b - a) * t per channel).

Each master is drawn at SS x supersampling and downsampled, so edges are smooth.
"""

import math
import os
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
LOGO = ROOT / "assets" / "logo"
FONT = Path(os.environ.get("WINDIR", r"C:\Windows")) / "Fonts" / "ariblk.ttf"  # Arial Black

# Base colours, in the order of `app_icon::ALL`; the first is the default.
COLOURS = (
    ("graphite", (0x4A, 0x4F, 0x5A)),
    ("blue", (0x48, 0x76, 0xB0)),
    ("steel", (0x4A, 0x6C, 0x98)),
    ("sky", (0x5A, 0x93, 0xD1)),
    ("teal", (0x2F, 0x8C, 0x8C)),
    ("green", (0x3E, 0x8E, 0x5A)),
    ("purple", (0x74, 0x57, 0xB8)),
    ("pink", (0xC4, 0x50, 0x8E)),
    ("red", (0xC8, 0x43, 0x4F)),
    ("orange", (0xD9, 0x77, 0x2E)),
)

SIZE = 1024  # output master size, one SVG unit = SIZE / 100 px
SS = 4  # supersampling factor
WORK = SIZE * SS
K = WORK / 100  # work pixels per SVG unit

WHITE = (255, 255, 255)
BLACK = (0, 0, 0)
LETTER_SIZE = 40 * 0.75  # font-size 40 in a 0.75 group, SVG units
Z_AT = (12.5, 67)  # glyph origin (baseline left), SVG units
X_AT = (22.25, 67)
Z_STROKE = 3.4 * 0.75  # outline width of the Z, centred on the glyph edge
X_HALO = 5 * 0.75  # stroke width under the X; half of it shows outside the glyph
BLADE_WIDTH = 1.6
SMALL_GLASS_R = 13.5  # the simplified mark has a thicker lens ring


def mix(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def palette(base):
    """Every colour of the mark, derived from the base colour C (see the reference SVG)."""
    return {
        "hump": mix(base, WHITE, 0.30),  # the reference SVG literal #80848C (its comment says 0.35)
        "body_top": mix(base, WHITE, 0.10),
        "body_bottom": mix(base, BLACK, 0.18),
        "ring": mix(base, BLACK, 0.45),
        "glass_in": mix(base, WHITE, 0.15),
        "glass_out": mix(base, BLACK, 0.60),
        "blade": mix(base, WHITE, 0.525),  # the reference SVG literal #A9ABB1 (its comment says 0.55)
        "z": mix(base, WHITE, 0.80),
        "halo": base,
    }


def rgb(c, alpha=255):
    return tuple(round(v) for v in c) + (alpha,)


def P(u, v):
    """SVG units to work pixels."""
    return (u * K, v * K)


def _arc(cx, cy, r, a0, a1, n=16):
    return [
        (cx + r * math.cos(math.radians(a0 + (a1 - a0) * i / n)),
         cy + r * math.sin(math.radians(a0 + (a1 - a0) * i / n)))
        for i in range(n + 1)
    ]


def hump_units():
    """The folder tab: M8 34 v-18 a5 5 0 0 1 5-5 h22 a5 5 0 0 1 4 2 l6 9 h42 a5 5 0 0 1 5 5 v7z."""
    return (
        [(8, 34), (8, 16)]
        + _arc(13, 16, 5, 180, 270)
        + [(35, 11)]
        + _arc(35, 16, 5, 270, math.degrees(math.atan2(-3, 4)) + 360)
        + [(45, 22), (87, 22)]
        + _arc(87, 27, 5, 270, 360)
        + [(92, 34)]
    )


def paint(art, mask, colour):
    """Composite a flat colour (RGB) through a work-size L mask onto `art`."""
    layer = Image.new("RGBA", art.size, rgb(colour))
    layer.putalpha(mask)
    art.alpha_composite(layer)


def _mask(draw_fn):
    mask = Image.new("L", (WORK, WORK), 0)
    draw_fn(ImageDraw.Draw(mask))
    return mask


def body_gradient(top, bottom):
    """Vertical gradient over the body's box (y 31..87 units), as a full-size image."""
    rows = np.arange(WORK, dtype=np.float64) + 0.5
    t = np.clip((rows / K - 31) / (87 - 31), 0, 1)[:, None]
    column = np.array(top) * (1 - t) + np.array(bottom) * t
    field = np.broadcast_to(column[:, None, :], (WORK, WORK, 3)).astype(np.uint8)
    return Image.fromarray(np.ascontiguousarray(field), "RGB")


def glass_layer(cx, cy, r, inner, outer):
    """A radial glass disc. The gradient focus sits at (.35, .3) of the disc's box, and its
    radius is .8 of the box width, as in the reference SVG."""
    size = round(2 * r * K)
    x0, y0 = round((cx - r) * K), round((cy - r) * K)
    ys, xs = np.mgrid[0:size, 0:size] + 0.5
    t = np.clip(np.hypot(xs - 0.35 * size, ys - 0.3 * size) / (0.8 * size), 0, 1)[..., None]
    field = np.array(inner) * (1 - t) + np.array(outer) * t
    disc = Image.new("L", (size, size), 0)
    ImageDraw.Draw(disc).ellipse((0, 0, size - 1, size - 1), fill=255)
    crop = Image.fromarray(field.astype(np.uint8), "RGB").convert("RGBA")
    crop.putalpha(disc)
    layer = Image.new("RGBA", (WORK, WORK), (0, 0, 0, 0))
    layer.paste(crop, (x0, y0))
    return layer


def _erode(mask: np.ndarray, r: int) -> np.ndarray:
    """Disc erosion of a boolean mask (pixels outside the array count as background)."""
    h, w = mask.shape
    out = np.ones_like(mask)
    csum = np.zeros((h, w + 1), np.int64)
    csum[:, 1:] = np.cumsum(mask, axis=1)
    idx = np.arange(w)
    for dy in range(-r, r + 1):
        hw = math.isqrt(r * r - dy * dy)
        lo = np.clip(idx - hw, 0, w)
        hi = np.clip(idx + hw + 1, 0, w)
        full = ((csum[:, hi] - csum[:, lo]) == 2 * hw + 1) & (idx - hw >= 0) & (idx + hw < w)
        shifted = np.zeros_like(full)
        if dy >= 0:
            shifted[: h - dy] = full[dy:]
        else:
            shifted[-dy:] = full[: h + dy]
        out &= shifted
    return out


def _letters(art, base):
    """The outline Z and the haloed solid X. Drawn on a crop so the erosion stays small."""
    font = ImageFont.truetype(str(FONT), size=round(LETTER_SIZE * K))
    left, top = P(6, 38)
    right, bottom = P(52, 74)
    ox, oy, w, h = round(left), round(top), round(right) - round(left), round(bottom) - round(top)

    def glyph(text, at, stroke=0.0):
        m = Image.new("L", (w, h), 0)
        x, y = P(*at)
        ImageDraw.Draw(m).text(
            (x - ox, y - oy), text, font=font, fill=255, anchor="ls",
            stroke_width=round(stroke * K), stroke_fill=255,
        )
        return m

    def paint_crop(mask, colour):
        layer = Image.new("RGBA", (w, h), rgb(colour))
        layer.putalpha(mask)
        art.alpha_composite(layer, dest=(ox, oy))

    # Z: an outline centred on the glyph edge = dilated minus eroded.
    half = Z_STROKE / 2
    solid = np.asarray(glyph("Z", Z_AT)) > 127
    dilated = np.asarray(glyph("Z", Z_AT, stroke=half)) > 127
    outline = dilated & ~_erode(solid, round(half * K))
    paint_crop(Image.fromarray((outline * 255).astype(np.uint8), "L"), palette(base)["z"])

    # X: a halo in the base colour (the stroke painted under the fill, so its outer half shows),
    # then the solid white glyph.
    paint_crop(glyph("X", X_AT, stroke=X_HALO / 2), palette(base)["halo"])
    paint_crop(glyph("X", X_AT), WHITE)


def render(base, small=False):
    """The mark at WORK resolution, transparent background."""
    c = palette(base)
    art = Image.new("RGBA", (WORK, WORK), (0, 0, 0, 0))

    # Folder tab, then the body on top of it.
    paint(art, _mask(lambda d: d.polygon([P(*p) for p in hump_units()], fill=255)), c["hump"])
    body = _mask(lambda d: d.rounded_rectangle((*P(8, 31), *P(92, 87)), radius=9 * K, fill=255))
    gradient = body_gradient(c["body_top"], c["body_bottom"]).convert("RGBA")
    gradient.putalpha(body)
    art.alpha_composite(gradient)

    # Lens: ring, glass, aperture blades and a highlight.
    cx, cy = 67, 59
    lens = _mask(lambda d: d.ellipse((*P(cx - 21, cy - 21), *P(cx + 21, cy + 21)), fill=255))
    paint(art, lens, c["ring"])
    glass_r = SMALL_GLASS_R if small else 16
    art.alpha_composite(glass_layer(cx, cy, glass_r, c["glass_in"], c["glass_out"]))
    if not small:
        def blades(d):
            for k in range(6):
                a = math.radians(60 * k)
                pts = []
                for (u, v) in ((0, -16), (4, -4)):
                    x = u * math.cos(a) - v * math.sin(a)
                    y = u * math.sin(a) + v * math.cos(a)
                    pts.append(P(cx + x, cy + y))
                d.line(pts, fill=255, width=round(BLADE_WIDTH * K))
        paint(art, _mask(blades), c["blade"])
    hl_r = 3.6 if small else 2.6
    hl = _mask(lambda d: d.ellipse((*P(61 - hl_r, 53 - hl_r), *P(61 + hl_r, 53 + hl_r)), fill=int(255 * 0.85)))
    paint(art, hl, WHITE)

    if not small:
        _letters(art, base)
    return art


def main() -> None:
    LOGO.mkdir(parents=True, exist_ok=True)
    for name, base in COLOURS:
        render(base).resize((SIZE, SIZE), Image.Resampling.LANCZOS).save(
            LOGO / f"zxraw-{name}.png", format="PNG", optimize=True
        )
        render(base, small=True).resize((SIZE, SIZE), Image.Resampling.LANCZOS).save(
            LOGO / f"zxraw-{name}-small.png", format="PNG", optimize=True
        )
        print("drew", name)


if __name__ == "__main__":
    main()
