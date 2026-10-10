#!/usr/bin/env python3
"""Generate the ZXraw Explorer window PNG and multi-size Windows ICO."""

from pathlib import Path

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
SIZES = (16, 24, 32, 48, 64, 128, 256)
SUPERSAMPLE = 4


def render(size: int) -> Image.Image:
    scale = SUPERSAMPLE
    canvas = Image.new("RGBA", (size * scale, size * scale), (0, 0, 0, 0))
    draw = ImageDraw.Draw(canvas)

    def box(bounds):
        return tuple(round(value * size * scale / 256) for value in bounds)

    # A deep blue rounded tile holds a bright folder and a simple shutter mark.
    draw.rounded_rectangle(box((8, 8, 248, 248)), radius=box((52, 52, 0, 0))[0], fill="#102033")

    # Folder tab and body are deliberately geometric so the silhouette reads at 16 px.
    draw.polygon(
        [tuple(round(v * size * scale / 256) for v in point) for point in
         ((29, 68), (99, 68), (119, 88), (223, 88), (223, 203), (29, 203))],
        fill="#30C7B1",
    )
    draw.line(
        [tuple(round(v * size * scale / 256) for v in point) for point in
         ((30, 89), (221, 89))],
        fill="#82F0DC",
        width=max(1, round(size * scale / 38)),
    )

    # Six overlapping shutter blades create a recognizable aperture inside the folder.
    cx = cy = 145 * size * scale / 256
    outer = 42 * size * scale / 256
    inner = 11 * size * scale / 256
    for blade in range(6):
        angle = blade * 60
        points = []
        for radius, degrees in ((inner, angle + 8), (outer, angle + 40), (outer, angle + 58), (inner, angle + 68)):
            import math

            radians = math.radians(degrees)
            points.append((round(cx + radius * math.cos(radians)), round(cy + radius * math.sin(radians))))
        draw.polygon(points, fill="#E8FFF8")
    draw.ellipse(box((136, 136, 154, 154)), fill="#102033")
    draw.ellipse(box((141, 141, 149, 149)), fill="#FFB45E")

    return canvas.resize((size, size), Image.Resampling.LANCZOS)


def main() -> None:
    ASSETS.mkdir(parents=True, exist_ok=True)
    render(256).save(ASSETS / "zxraw.png", format="PNG", optimize=True)
    render(256).save(
        ASSETS / "zxraw.ico",
        format="ICO",
        sizes=[(size, size) for size in SIZES],
    )
    print(f"Wrote {ASSETS / 'zxraw.png'} and {ASSETS / 'zxraw.ico'} ({', '.join(map(str, SIZES))} px)")


if __name__ == "__main__":
    main()
