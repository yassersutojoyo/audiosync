"""Draws the Guide's illustrations.

Writes:
  brand/guide/<name>.svg                 the light variant, drawn below in the five role colors
  brand/guide/<name>-dark.svg            the dark variant: each light color swapped for its role's dark color
  Sources/AudioSync/GuideArtwork.swift   both variants as strings, for the Mac Guide window
The Windows Guide loads the SVGs from brand/guide/ itself (desktop/ui/help.slint).

  source   the signal path: an app's sound into AudioSync, out to several outputs
  delay    two speakers, one held back by its delay, so both reach the listener together
  classes  outputs kept in step (local and AirPlay) next to a Google Cast speaker
           playing after an offset set by ear

Each role color is a color from brand/tokens.json or a mix of two of them. The
script fails on any role pair that would draw a shape below 4.5:1 against what
it sits on (the plan asks the text ratio, above WCAG 1.4.11's 3:1 for shapes),
in either appearance, on either Guide's page. The illustrations carry no text.

Usage (from the repository root):
  python3 -I scripts/brand/guide_art.py           write the files
  python3 -I scripts/brand/guide_art.py --check   write nothing; fail if any file is out of date
"""
import json
import math
import os
import re
import sys

ARTS = ['source', 'delay', 'classes']
GUIDE = 'brand/guide'
TOKENS = 'brand/tokens.json'
MARK = 'brand/audiosync-mark.svg'
SWIFT = 'Sources/AudioSync/GuideArtwork.swift'
USAGE = 'usage: python3 -I scripts/brand/guide_art.py [--check]'

# role: (light, dark); a color is a token name or (token, token, share of the second token).
ROLES = {
    'ground':   (('surface', 'teal', 0.14), ('background', 'teal', 0.16)),  # soft floor under the objects
    'body':     ('muted', 'muted'),                                        # speakers, tiles, the listener
    'shade':    (('muted', 'text', 0.5), ('muted', 'background', 0.22)),   # the side of a body facing away
    'accent':   ('tealText', 'mint'),                                      # the sound, the one focal color
    'knockout': ('surface', 'background'),                                 # cut-outs: grilles, glyphs, the mark
}

# The pages the Guides draw on: macOS's text background (white, and #1E1E1E in dark
# appearance) and the Windows Guide's, the token background.
PAGES = {'light': ['#FFFFFF', ('background', 'light')], 'dark': ['#1E1E1E', ('background', 'dark')]}

# Foreground role, background (a role or 'page'): shapes that must reach 4.5:1.
# Body and shade are two faces of one object: that step is depth, not an edge, so it is not gated.
PAIRS = [
    ('body', 'page'), ('body', 'ground'), ('shade', 'page'), ('shade', 'ground'),
    ('accent', 'page'), ('accent', 'ground'), ('knockout', 'body'), ('knockout', 'shade'),
]
MINIMUM = 4.5

HEAD = ('<svg xmlns="http://www.w3.org/2000/svg" width="360" height="131" viewBox="0 0 440 160" '
        'fill="none" stroke-linecap="round" stroke-linejoin="round">')


def luminance(hex_color):
    def channel(c):
        c /= 255
        return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4
    r, g, b = (int(hex_color[i:i + 2], 16) for i in (1, 3, 5))
    return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)


def ratio(a, b):
    la, lb = sorted((luminance(a), luminance(b)), reverse=True)
    return (la + 0.05) / (lb + 0.05)


def mix(a, b, share):
    channels = (round(int(a[i:i + 2], 16) * (1 - share) + int(b[i:i + 2], 16) * share) for i in (1, 3, 5))
    return '#' + ''.join(f'{c:02X}' for c in channels)


def palette(tokens):
    colors = tokens['color']

    def resolve(spec, appearance):
        if isinstance(spec, str):
            return colors[spec][appearance].upper()
        first, second, share = spec
        return mix(colors[first][appearance], colors[second][appearance], share)

    return {appearance: {role: resolve(specs[i], appearance) for role, specs in ROLES.items()}
            for i, appearance in enumerate(('light', 'dark'))}


def check_contrast(colors, tokens):
    failed = False
    for appearance, roles in colors.items():
        pages = list(dict.fromkeys(p if isinstance(p, str) else tokens['color'][p[0]][p[1]].upper()
                                   for p in PAGES[appearance]))
        for fg, bg in PAIRS:
            for back in (pages if bg == 'page' else [roles[bg]]):
                r = ratio(roles[fg], back)
                mark = 'ok' if r >= MINIMUM else 'FAIL'
                print(f'  {appearance:5} {fg:8} on {bg:8} {roles[fg]} / {back}  {r:5.2f}:1  {mark}')
                failed |= r < MINIMUM
    return not failed


def num(v):
    v = round(v, 1)
    return str(int(v)) if v == int(v) else str(v)


class Drawing:
    """One light SVG, built shape by shape in the light role colors."""

    def __init__(self, colors):
        self.c = colors
        self.lines = [HEAD]

    def svg(self):
        return '\n'.join(self.lines + ['</svg>', ''])

    def add(self, line):
        self.lines.append('  ' + line)

    def ground(self, cx, cy, rx, ry):
        self.add(f'<ellipse cx="{num(cx)}" cy="{num(cy)}" rx="{num(rx)}" ry="{num(ry)}" fill="{self.c["ground"]}"/>')

    def panel(self, x, y, w, h, r):
        self.add(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}" fill="{self.c["ground"]}"/>')

    def solid(self, x, y, w, h, r, lip):
        """A body seen from the front: the shade's full height, the face `lip` shorter on top."""
        self.add(f'<rect x="{num(x)}" y="{num(y)}" width="{num(w)}" height="{num(h)}" rx="{num(r)}" '
                 f'fill="{self.c["shade"]}"/>')
        self.add(f'<rect x="{num(x)}" y="{num(y)}" width="{num(w)}" height="{num(h - lip)}" rx="{num(r)}" '
                 f'fill="{self.c["body"]}"/>')

    def circle(self, cx, cy, r, role):
        self.add(f'<circle cx="{num(cx)}" cy="{num(cy)}" r="{num(r)}" fill="{self.c[role]}"/>')

    def tower(self, cx, top, w, h):
        """A standing speaker: a tweeter over a woofer."""
        self.solid(cx - w / 2, top, w, h, w / 4, h / 9)
        face = h - h / 9
        self.circle(cx, top + face * 0.25, w * 0.11, 'knockout')
        self.circle(cx, top + face * 0.65, w * 0.29, 'knockout')
        self.circle(cx, top + face * 0.65, w * 0.12, 'body')

    def compact(self, cx, top, w, h):
        """A compact speaker with one large driver."""
        self.solid(cx - w / 2, top, w, h, w * 0.3, h / 8)
        face = h - h / 8
        self.circle(cx, top + face / 2, w * 0.32, 'knockout')
        self.circle(cx, top + face / 2, w * 0.13, 'body')

    def headphones(self, cx, cy, span):
        r = span / 2
        self.add(f'<path d="M{num(cx - r)} {num(cy + 4)}V{num(cy)}a{num(r)} {num(r)} 0 0 1 {num(2 * r)} 0'
                 f'V{num(cy + 4)}" stroke="{self.c["body"]}" stroke-width="5"/>')
        self.solid(cx - r - 7, cy - 2, 14, 24, 6, 3)
        self.solid(cx + r - 7, cy - 2, 14, 24, 6, 3)

    def dots(self, x0, x1, y):
        """Silence: a speaker that is waiting."""
        for i in range(int((x1 - x0) / 11) + 1):
            self.circle(x0 + i * 11, y, 2.4, 'body')

    def wave(self, points, width):
        """A smooth stroke through `points` (Catmull-Rom turned into cubic Béziers)."""
        d = f'M{num(points[0][0])} {num(points[0][1])}'
        for i in range(len(points) - 1):
            p0, p1 = points[max(0, i - 1)], points[i]
            p2, p3 = points[i + 1], points[min(len(points) - 1, i + 2)]
            c1 = (p1[0] + (p2[0] - p0[0]) / 6, p1[1] + (p2[1] - p0[1]) / 6)
            c2 = (p2[0] - (p3[0] - p1[0]) / 6, p2[1] - (p3[1] - p1[1]) / 6)
            d += ''.join(f'{"C" if j == 0 else " "}{num(x)} {num(y)}' for j, (x, y) in enumerate((c1, c2, p2)))
        self.add(f'<path d="{d}" stroke="{self.c["accent"]}" stroke-width="{num(width)}"/>')

    def mark(self, cx, cy, width):
        """The brand mark, centered and scaled, as a knockout."""
        with open(MARK, encoding='utf-8') as f:
            source = f.read()
        box = [float(v) for v in re.search(r'viewBox="([^"]+)"', source).group(1).split()]
        d = re.search(r'<path[^>]* d="([^"]+)"', source).group(1)
        scale = width / box[2]

        def move(m):
            return (f'{num(cx + (float(m.group(1)) - box[2] / 2) * scale)} '
                    f'{num(cy + (float(m.group(2)) - box[3] / 2) * scale)}')
        moved = re.sub(r'(-?\d+(?:\.\d+)?) (-?\d+(?:\.\d+)?)', move, d)
        self.add(f'<path fill="{self.c["knockout"]}" d="{moved}"/>')


def sine(path, amp, length, phase=0.0):
    """Points of a sine riding along `path` (a function of arc length s in 0..total that gives
    a point and a unit normal), eased in and out over half a wavelength. Waves with the same
    `length` whose `phase` is their start's distance from a shared origin peak together."""
    total, at = path
    steps = max(8, int(total / (length / 8)))
    points = []
    for i in range(steps + 1):
        s = total * i / steps
        ease = max(0.0, min(1.0, s / (length / 2), (total - s) / (length / 2)))
        ease = 0.5 - 0.5 * math.cos(math.pi * ease)
        (x, y), (nx, ny) = at(s)
        off = amp * ease * math.sin(2 * math.pi * (s + phase) / length)
        points.append((x + nx * off, y + ny * off))
    return points


def straight(x0, y, x1):
    return x1 - x0, lambda s: ((x0 + s, y), (0.0, 1.0))


def s_curve(p0, p3):
    """A cubic that leaves p0 and reaches p3 level, measured by arc length."""
    mid = (p0[0] + p3[0]) / 2
    c1, c2 = (mid, p0[1]), (mid, p3[1])

    def bezier(t):
        u = 1 - t
        return tuple(u ** 3 * a + 3 * u * u * t * b + 3 * u * t * t * c + t ** 3 * d
                     for a, b, c, d in zip(p0, c1, c2, p3))
    dense = [bezier(i / 400) for i in range(401)]
    arc = [0.0]
    for a, b in zip(dense, dense[1:]):
        arc.append(arc[-1] + math.hypot(b[0] - a[0], b[1] - a[1]))

    def at(s):
        j = min(399, max(0, next((k for k in range(400) if arc[k + 1] >= s), 399)))
        a, b = dense[j], dense[j + 1]
        tx, ty = b[0] - a[0], b[1] - a[1]
        n = math.hypot(tx, ty)
        return a, (-ty / n, tx / n)
    return arc[-1], at


def draw_source(colors):
    art = Drawing(colors)
    art.ground(50, 122, 38, 5)
    art.solid(18, 46, 64, 70, 19, 7)
    art.add(f'<path d="M43 66v24l19-12z" fill="{colors["knockout"]}" stroke="{colors["knockout"]}" stroke-width="6"/>')
    art.wave(sine(straight(94, 78, 162), 8, 34), 6)
    art.ground(206, 122, 38, 5)
    art.solid(174, 46, 64, 70, 19, 7)
    art.mark(206, 77.5, 38.4)
    for start, end in ((64, 30), (78, 79), (92, 128)):
        art.wave(sine(s_curve((250, start), (358, end)), 5, 26), 5)
    art.ground(389, 56, 26, 3.5)
    art.tower(389, 8, 30, 46)
    art.ground(389, 106, 26, 3.5)
    art.compact(389, 62, 40, 42)
    art.headphones(389, 128, 34)
    return art.svg()


def draw_delay(colors):
    art = Drawing(colors)
    art.ground(52, 76, 28, 4.5)
    art.tower(52, 20, 34, 54)
    art.ground(52, 148, 28, 4.5)
    art.compact(52, 96, 46, 50)
    art.wave(sine(straight(96, 46, 378), 11, 56), 6)
    art.dots(98, 196, 118)
    art.wave(sine(straight(208, 118, 378), 11, 56, phase=208 - 96), 6)
    art.circle(412, 68, 9.5, 'body')
    art.add(f'<path d="M394 102a18 17 0 0 1 36 0z" fill="{colors["body"]}"/>')
    return art.svg()


def draw_classes(colors):
    art = Drawing(colors)
    art.panel(6, 10, 208, 140, 28)
    art.panel(226, 10, 208, 140, 28)
    art.tower(42, 22, 30, 50)
    art.compact(42, 90, 40, 46)
    art.wave(sine(straight(74, 47, 196), 9, 40), 5)
    art.wave(sine(straight(74, 113, 196), 9, 40), 5)
    art.compact(262, 56, 44, 50)
    art.dots(298, 336, 81)
    # A dimension mark over the silence: the offset is set by ear, not known to AudioSync.
    art.add(f'<path d="M296 67v-7h42v7" stroke="{colors["body"]}" stroke-width="4"/>')
    art.wave(sine(straight(348, 81, 418), 9, 40), 5)
    return art.svg()


DRAW = {'source': draw_source, 'delay': draw_delay, 'classes': draw_classes}


def dark_variant(svg, colors, name):
    swap = {colors['light'][role]: colors['dark'][role] for role in ROLES}
    used = {c.upper() for c in re.findall(r'#[0-9A-Fa-f]{6}\b', svg)}
    unknown = used - set(swap)
    if unknown:
        sys.exit(f'{name} uses colors outside the Guide palette: {sorted(unknown)}')
    return re.sub(r'#[0-9A-Fa-f]{6}\b', lambda m: swap[m.group(0).upper()], svg)


def swift(variants):
    out = ['// Generated by scripts/brand/guide_art.py. Do not edit.', '',
           '/// The Guide\'s illustrations, as in brand/guide/.',
           'enum GuideArt: CaseIterable {']
    out += [f'    case {name}' for name in ARTS]
    for appearance in ('light', 'dark'):
        out += ['', f'    /// The illustration in {appearance} appearance, as SVG.',
                f'    var {appearance}SVG: String {{', '        switch self {']
        for name in ARTS:
            out += [f'        case .{name}:', '            return #"""']
            out += [f'                {line}' if line else '' for line in variants[name][appearance].rstrip('\n').split('\n')]
            out += ['                """#']
        out += ['        }', '    }']
    out += ['}', '']
    return '\n'.join(out)


def outputs():
    """Every file the script writes, as {path: contents}; exits on a failed gate."""
    with open(TOKENS, encoding='utf-8') as f:
        tokens = json.load(f)
    colors = palette(tokens)
    print('Contrast (at least 4.5:1):')
    if not check_contrast(colors, tokens):
        sys.exit('A Guide color pair is below 4.5:1.')
    files, variants = {}, {}
    for name in ARTS:
        light = DRAW[name](colors['light'])
        if re.search(r'<text\b', light):
            sys.exit(f'{name} has text; the Guide illustrations carry none.')
        dark = dark_variant(light, colors, name)
        files[f'{GUIDE}/{name}.svg'] = light
        files[f'{GUIDE}/{name}-dark.svg'] = dark
        variants[name] = {'light': light, 'dark': dark}
    files[SWIFT] = swift(variants)
    return files


def stale(files):
    """The paths that differ from what the script would write, and SVGs in brand/guide/ it doesn't make."""
    out = []
    for path, contents in files.items():
        try:
            with open(path, encoding='utf-8', newline='') as f:
                same = f.read() == contents
        except FileNotFoundError:
            same = False
        if not same:
            out.append(path)
    out += sorted(f'{GUIDE}/{n}' for n in os.listdir(GUIDE)
                  if n.endswith('.svg') and f'{GUIDE}/{n}' not in files)
    return out


def main(args):
    if args not in ([], ['--check']):
        sys.exit(USAGE)
    files = outputs()
    if args == ['--check']:
        out = stale(files)
        if out:
            sys.exit('Out of date (run python3 -I scripts/brand/guide_art.py): ' + ', '.join(out))
        print('The Guide illustrations are up to date.')
        return
    for path, contents in files.items():
        with open(path, 'w', encoding='utf-8', newline='\n') as f:
            f.write(contents)
    print(f'Wrote {len(ARTS) * 2} SVGs in {GUIDE}/ and {SWIFT}.')


if __name__ == '__main__':
    main(sys.argv[1:])
