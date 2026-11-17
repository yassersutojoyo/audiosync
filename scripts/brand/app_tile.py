"""The Windows app icon: the white mark on a teal tile, drawn with Pillow.

  python -I scripts/brand/app_tile.py
      Draws brand/audiosync-mark.svg in white on the tile at each size and writes
        desktop/res/audiosync.ico            the exe's icon (Explorer, the installer)
        desktop/res/audiosync-tile-256.png   the windows' icon (title bar, taskbar, Alt+Tab)
        desktop/msix/Assets/*.png            the MSIX package logos, 44, 150 and 50 px
      The tile follows the Windows icon grid: inset 3 on a 48 px canvas, with the
      mark 30 px wide in its middle. Its corners are superellipse arcs of exponent
      5 that cross the 45 degree line where a radius of 22.5% of the side would,
      the Mac icon's corner. It shades from LIGHT_TEAL at
      the top left to TEAL at the bottom right, as CSS's 120 degree gradient does.

  python -I scripts/brand/app_tile.py --check
      Checks the committed desktop/res/audiosync.ico: the sizes Windows asks for,
      the tile's gradient ending in TEAL, the white mark in the middle and the clear
      corners, at 32 px. Needs no Pillow.

Run from the repository root.
"""
import importlib.util
import math
import re
import struct
import sys
import tempfile
import zlib

MARK = 'brand/audiosync-mark.svg'
ICO = 'desktop/res/audiosync.ico'
WINDOW_PNG = 'desktop/res/audiosync-tile-256.png'
MSIX = {'desktop/msix/Assets/Square44x44Logo.png': 44,
        'desktop/msix/Assets/Square150x150Logo.png': 150,
        'desktop/msix/Assets/StoreLogo.png': 50}
SIZES = (16, 20, 24, 32, 40, 48, 64, 256)
TEAL = (0x17, 0x8A, 0x7E)
LIGHT_TEAL = (0x22, 0xAE, 0x9F)
ANGLE = 120
GRID, INSET, MARK_WIDTH = 48, 3, 30
CORNER, EXPONENT = 0.225, 5
# Pillow draws shapes without anti-aliasing, so each pixel is drawn 16 x 16
# times larger and averaged down.
SUPERSAMPLE = 16


def mark_shapes():
    """The mark's outlines as polygons in its viewBox, and the viewBox's size."""
    with open(MARK, encoding='utf-8') as f:
        svg = f.read()
    width, height = map(float, re.search(r'viewBox="0 0 ([\d.]+) ([\d.]+)"', svg).groups())
    shapes = []
    for d in re.findall(r'<path [^>]*\bd="([^"]+)"', svg):
        tokens = re.findall(r'[MLCZ]|-?[\d.]+', d)
        at = 0
        while at < len(tokens):
            command = tokens[at]
            at += 1
            if command == 'Z':
                continue
            count = 6 if command == 'C' else 2
            numbers = [float(n) for n in tokens[at:at + count]]
            at += count
            if command == 'M':
                shapes.append([tuple(numbers)])
            elif command == 'L':
                shapes[-1].append(tuple(numbers))
            else:
                (x0, y0), (x1, y1, x2, y2, x3, y3) = shapes[-1][-1], numbers
                for step in range(1, 33):
                    t = step / 32
                    u = 1 - t
                    shapes[-1].append((u ** 3 * x0 + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t ** 3 * x3,
                                       u ** 3 * y0 + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t ** 3 * y3))
    return shapes, width, height


def shade(u, v):
    """The tile's colour at (u, v), fractions of its width and height, for CSS's
    linear-gradient(120deg, LIGHT_TEAL, TEAL): the gradient line runs through the
    middle and ends where the perpendiculars through two corners cross it."""
    dx, dy = math.sin(math.radians(ANGLE)), -math.cos(math.radians(ANGLE))
    t = ((u - 0.5) * dx + (v - 0.5) * dy) / (abs(dx) + abs(dy)) + 0.5
    return tuple(round(a + (b - a) * min(max(t, 0), 1)) for a, b in zip(LIGHT_TEAL, TEAL))


def gradient():
    """The tile's gradient on a 256 px canvas, opaque."""
    from PIL import Image
    side, span = 256, GRID - 2 * INSET
    pixels = []
    for row in range(side):
        v = ((row + 0.5) / side * GRID - INSET) / span
        for column in range(side):
            u = ((column + 0.5) / side * GRID - INSET) / span
            pixels.append(shade(u, v) + (255,))
    image = Image.new('RGBA', (side, side))
    image.putdata(pixels)
    return image


def squircle(left, top, side):
    """The tile's outline as a polygon, each corner a superellipse arc."""
    reach = min(0.5, CORNER * (1 - math.sqrt(0.5)) / (1 - 2 ** (-1 / EXPONENT))) * side
    points = []
    for cx, cy, sx, sy, reverse in ((left + side - reach, top + reach, 1, -1, True),
                                    (left + side - reach, top + side - reach, 1, 1, False),
                                    (left + reach, top + side - reach, -1, 1, True),
                                    (left + reach, top + reach, -1, -1, False)):
        for step in range(65):
            t = (64 - step if reverse else step) / 64 * math.pi / 2
            points.append((cx + sx * math.cos(t) ** (2 / EXPONENT) * reach,
                           cy + sy * math.sin(t) ** (2 / EXPONENT) * reach))
    return points


def tile(size):
    from PIL import Image, ImageDraw
    shapes, width, height = mark_shapes()
    side = size * SUPERSAMPLE
    unit = side / GRID
    # The gradient is smooth, so it is worked out at 256 px and scaled up.
    image = gradient().resize((side, side), Image.Resampling.BILINEAR)
    mask = Image.new('L', (side, side), 0)
    ImageDraw.Draw(mask).polygon(squircle(INSET * unit, INSET * unit, side - 2 * INSET * unit), fill=255)
    image.putalpha(mask)
    draw = ImageDraw.Draw(image)
    scale = MARK_WIDTH * unit / width
    x, y = (side - width * scale) / 2, (side - height * scale) / 2
    for shape in shapes:
        draw.polygon([(x + px * scale, y + py * scale) for px, py in shape], fill=(255, 255, 255, 255))
    return image.resize((size, size), Image.Resampling.BOX)


def render():
    spec = importlib.util.spec_from_file_location('ico', 'scripts/brand/ico.py')
    ico = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(ico)
    with tempfile.TemporaryDirectory() as work:
        pngs = []
        for size in SIZES:
            pngs.append(f'{work}/{size}.png')
            tile(size).save(pngs[-1])
        ico.pack(ICO, pngs)
    tile(256).save(WINDOW_PNG)
    for path, size in MSIX.items():
        tile(size).save(path)


def png_pixels(data):
    """Decodes an 8-bit RGBA, non-interlaced PNG into rows of (r, g, b, a)."""
    width, height, depth, color, _, _, interlace = struct.unpack('>IIBBBBB', data[16:29])
    if (depth, color, interlace) != (8, 6, 0):
        sys.exit(f'{ICO}: a {width} px image is not 8-bit RGBA without interlacing')
    idat, at = b'', 8
    while at < len(data):
        length, kind = struct.unpack('>I4s', data[at:at + 8])
        if kind == b'IDAT':
            idat += data[at + 8:at + 8 + length]
        at += 12 + length
    raw = zlib.decompress(idat)
    stride = width * 4
    rows, previous = [], bytearray(stride)
    for y in range(height):
        start = y * (stride + 1)
        kind, line = raw[start], bytearray(raw[start + 1:start + 1 + stride])
        for x in range(stride):
            a = line[x - 4] if x >= 4 else 0
            b = previous[x]
            c = previous[x - 4] if x >= 4 else 0
            if kind == 1:
                line[x] = (line[x] + a) & 255
            elif kind == 2:
                line[x] = (line[x] + b) & 255
            elif kind == 3:
                line[x] = (line[x] + (a + b) // 2) & 255
            elif kind == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append([tuple(line[i:i + 4]) for i in range(0, stride, 4)])
        previous = line
    return rows


def check():
    with open(ICO, 'rb') as f:
        data = f.read()
    count = struct.unpack('<HHH', data[:6])[2]
    images = {}
    for index in range(count):
        size, _, _, _, _, _, length, offset = struct.unpack('<BBBBHHII', data[6 + 16 * index:22 + 16 * index])
        images[size or 256] = data[offset:offset + length]
    if tuple(sorted(images)) != SIZES:
        sys.exit(f'{ICO}: sizes {sorted(images)}, expected {list(SIZES)}')
    # At 256 px the tile spans 16 to 240 and its corner crosses the diagonal
    # near 30.5, so (34, 34) is on the tile while (27, 27) and (16, 60) are clear.
    big = png_pixels(images[256])
    if big[34][34][3] != 255 or big[27][27][3] != 0 or big[60][16][3] != 0:
        sys.exit(f'{ICO}: the 256 px corner is not the 22.5% superellipse')
    pixels = png_pixels(images[32])
    # Pixels clear of the mark; a linear gradient averages to its value at a pixel's centre.
    for x, y in ((16, 4), (6, 28), (25, 28)):
        u, v = (((n + 0.5) * GRID / 32 - INSET) / (GRID - 2 * INSET) for n in (x, y))
        expected = shade(u, v)
        if any(abs(a - b) > 1 for a, b in zip(pixels[y][x], expected)) or pixels[y][x][3] != 255:
            sys.exit(f'{ICO}: the 32 px tile at ({x}, {y}) is {pixels[y][x]}, expected {expected} of the 120 degree gradient')
    # Row 28 lies on the tile below the mark; the gradient makes it darker than row 4.
    if pixels[28][16] == pixels[4][16]:
        sys.exit(f'{ICO}: the 32 px tile is {pixels[4][16]} above and below the mark, expected a gradient')
    # The middle falls on the wave.
    if pixels[16][16] != (255, 255, 255, 255):
        sys.exit(f'{ICO}: the 32 px middle is {pixels[16][16]}, expected the white mark')
    if pixels[0][0][3] != 0:
        sys.exit(f'{ICO}: the 32 px corner is not clear')
    print(f'{ICO}: the teal tile at {", ".join(map(str, SIZES))} px')


def main():
    if sys.argv[1:] == ['--check']:
        check()
    elif not sys.argv[1:]:
        render()
    else:
        sys.exit('usage: app_tile.py [--check]')


if __name__ == '__main__':
    main()
