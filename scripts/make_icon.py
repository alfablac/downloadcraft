#!/usr/bin/env python3
"""Generate downloadcraft app icon assets (original download-arrow mark).

Run: python scripts/make_icon.py
Outputs assets/downloadcraft.ico, .png, .icns.
"""
from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "assets"
GREEN = (95, 158, 110, 255)
DARK = (28, 28, 28, 255)
LIGHT = (232, 232, 232, 255)


def render(size: int) -> Image.Image:
    # Render at 4x and downsample for clean edges.
    s = size * 4
    im = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.rectangle([0, 0, s - 1, s - 1], fill=DARK)
    d.rectangle([0, 0, s - 1, s - 1], outline=GREEN, width=max(2, s // 32))
    # Down arrow: shaft + head, then receiving tray bar.
    cx = s // 2
    shaft_w = s // 10
    top = s // 7
    mid = s * 9 // 16
    d.rectangle([cx - shaft_w // 2, top, cx + shaft_w // 2, mid], fill=LIGHT)
    d.polygon([(s // 4, mid - s // 16), (s * 3 // 4, mid - s // 16), (cx, s * 3 // 4)], fill=LIGHT)
    bar_y = s * 13 // 16
    bar_h = max(3, s // 24)
    d.rectangle([s // 5, bar_y, s * 4 // 5, bar_y + bar_h], fill=GREEN)
    return im.resize((size, size), Image.LANCZOS)


def main() -> None:
    OUT.mkdir(exist_ok=True)
    base = render(512)
    base.save(OUT / "downloadcraft.png")
    sizes = [16, 20, 24, 32, 48, 64, 128, 256]
    base.save(OUT / "downloadcraft.ico", sizes=[(x, x) for x in sizes])
    base.save(OUT / "downloadcraft.icns", sizes=[(x, x) for x in [16, 32, 64, 128, 256, 512]])
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
