"""Helpers for scripts/brand/windows-icons.sh: the Windows icon SVGs and ICO packing.

  python3 -I scripts/brand/ico.py svgs <dir>
      Writes the SVGs the Windows icons are rendered from:
        app.svg             the light app icon (brand/audiosync-icon.svg) cropped to its tile,
                            since Windows icons have no macOS icon-grid margin
        tray-light.svg      the mark for a light taskbar (the readable teal, color.tealText)
        tray-dark.svg       the mark for a dark taskbar (the mint, color.logo dark)
        tray-*-off.svg      the same at the disabled opacity, while streaming is off
      Colors and the opacity come from brand/tokens.json.

  python3 -I scripts/brand/ico.py pack <out.ico> <png>...
      Packs square PNGs (one per size, up to 256 px) into an ICO file. Each image
      is stored as PNG, which Windows reads at every size.

Run from the repository root.
"""
import json
import re
import struct
import sys

MARK = 'brand/audiosync-mark.svg'
ICON = 'brand/audiosync-icon.svg'
TOKENS = 'brand/tokens.json'

# The macOS icon grid: an 824 pt tile centered on a 1024 pt canvas.
TILE_VIEWBOX = '100 100 824 824'
# Room around the mark in the tray icon, as a fraction of its width.
TRAY_PADDING = 0.04


def svgs(directory):
    with open(TOKENS, encoding='utf-8') as f:
        tokens = json.load(f)
    light = tokens['color']['tealText']['light']
    dark = tokens['color']['logo']['dark']
    off = tokens['opacity']['disabled']['value']

    with open(ICON, encoding='utf-8') as f:
        icon = f.read()
    icon, count = re.subn(r'viewBox="0 0 1024 1024"', f'viewBox="{TILE_VIEWBOX}"', icon, count=1)
    if count != 1:
        sys.exit(f'{ICON}: expected a 1024 x 1024 viewBox')
    write(f'{directory}/app.svg', icon)

    with open(MARK, encoding='utf-8') as f:
        mark = f.read()
    match = re.search(r'viewBox="0 0 ([\d.]+) ([\d.]+)"', mark)
    if not match:
        sys.exit(f'{MARK}: expected a viewBox at the origin')
    width, height = float(match.group(1)), float(match.group(2))
    side = max(width, height) * (1 + 2 * TRAY_PADDING)
    x, y = (width - side) / 2, (height - side) / 2
    paths = re.findall(r'<path [^>]*\bd="([^"]+)"', mark)
    if not paths:
        sys.exit(f'{MARK}: no paths')
    for name, color, opacity in (('tray-light', light, 1), ('tray-dark', dark, 1),
                                 ('tray-light-off', light, off), ('tray-dark-off', dark, off)):
        body = ''.join(f'<path fill="{color}" fill-opacity="{opacity:g}" d="{d}"/>' for d in paths)
        write(f'{directory}/{name}.svg',
              f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x:g} {y:g} {side:g} {side:g}">{body}</svg>\n')


def pack(out, pngs):
    images = []
    for path in pngs:
        with open(path, 'rb') as f:
            data = f.read()
        if data[:8] != b'\x89PNG\r\n\x1a\n' or data[12:16] != b'IHDR':
            sys.exit(f'{path}: not a PNG')
        width, height = struct.unpack('>II', data[16:24])
        if width != height or width > 256:
            sys.exit(f'{path}: {width} x {height}; ICO images are square, up to 256 px')
        images.append((width, data))
    images.sort()
    header = struct.pack('<HHH', 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, blobs = b'', b''
    for size, data in images:
        # Width and height 0 mean 256; 1 plane, 32 bits per pixel.
        entries += struct.pack('<BBBBHHII', size % 256, size % 256, 0, 0, 1, 32, len(data), offset)
        blobs += data
        offset += len(data)
    with open(out, 'wb') as f:
        f.write(header + entries + blobs)


def write(path, text):
    with open(path, 'w', encoding='utf-8') as f:
        f.write(text)


def main():
    args = sys.argv[1:]
    if len(args) == 2 and args[0] == 'svgs':
        svgs(args[1])
    elif len(args) >= 3 and args[0] == 'pack':
        pack(args[1], args[2:])
    else:
        sys.exit('usage: ico.py svgs <dir> | ico.py pack <out.ico> <png>...')


if __name__ == '__main__':
    main()
