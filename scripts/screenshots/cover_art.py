#!/usr/bin/env python3
"""Draws the album cover the README screenshots show in the Now Playing card.

Soft glows of pink, coral, violet and teal on a dusk field, with a few thin
wave lines: only the mood and colours of a late-evening seaside, no figure,
logo or lettering. Deterministic, so a rerun gives the same file.

Usage: python scripts/screenshots/cover_art.py [output.png]
       (default docs/images/now-playing-cover.png)
"""

import math
import random
import sys
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

SIZE = 1200
DUSK_TOP = (30, 8, 58)
DUSK_BOTTOM = (6, 18, 44)
GLOWS = [
    # centre x, centre y, radius (fractions of SIZE), colour
    (0.24, 0.30, 0.30, (230, 40, 140)),
    (0.72, 0.44, 0.24, (240, 110, 50)),
    (0.44, 0.86, 0.26, (10, 150, 170)),
    (0.88, 0.10, 0.20, (110, 40, 210)),
]


def field() -> Image.Image:
    image = Image.new("RGB", (SIZE, SIZE))
    draw = ImageDraw.Draw(image)
    for y in range(SIZE):
        t = y / (SIZE - 1)
        draw.line(
            [(0, y), (SIZE, y)],
            fill=tuple(round(a + (b - a) * t) for a, b in zip(DUSK_TOP, DUSK_BOTTOM)),
        )
    return image


def glow(cx: float, cy: float, radius: float, colour: tuple) -> Image.Image:
    layer = Image.new("RGB", (SIZE, SIZE))
    r = radius * SIZE
    ImageDraw.Draw(layer).ellipse(
        [cx * SIZE - r, cy * SIZE - r, cx * SIZE + r, cy * SIZE + r], fill=colour
    )
    return layer.filter(ImageFilter.GaussianBlur(r * 0.6))


def waves() -> Image.Image:
    layer = Image.new("RGB", (SIZE, SIZE))
    draw = ImageDraw.Draw(layer)
    for i in range(7):
        base = SIZE * (0.56 + i * 0.05)
        amplitude = SIZE * (0.012 + i * 0.004)
        phase = i * 0.9
        points = [
            (x, base + amplitude * math.sin(x / SIZE * 2 * math.pi * 1.5 + phase))
            for x in range(0, SIZE + 8, 8)
        ]
        shade = 54 - i * 6
        draw.line(points, fill=(shade, shade, shade), width=2)
    return layer.filter(ImageFilter.GaussianBlur(1.2))


def grain(seed: int = 7) -> Image.Image:
    rng = random.Random(seed)
    small = Image.new("L", (SIZE // 2, SIZE // 2))
    small.putdata([rng.randint(0, 10) for _ in range((SIZE // 2) ** 2)])
    return small.resize((SIZE, SIZE), Image.BILINEAR).convert("RGB")


def main() -> None:
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "docs/images/now-playing-cover.png")
    image = field()
    for cx, cy, radius, colour in GLOWS:
        image = ImageChops.screen(image, glow(cx, cy, radius, colour))
    image = ImageChops.add(image, waves())
    image = ImageChops.add(image, grain())
    image.save(out, optimize=True)


if __name__ == "__main__":
    main()
