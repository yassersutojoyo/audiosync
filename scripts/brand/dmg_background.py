"""Draws the background of the disk image's Finder window.

Writes:
  brand/dmg/background.png      the window at 1x (660 x 460 pt)
  brand/dmg/background@2x.png   the same at 2x, for Retina screens
  brand/dmg/layout.json         the window size and the icon positions the art is drawn
                                around; make-dmg.sh reads it, so the two can't drift

The art: "Drag AudioSync to Applications" above an arrow that runs between where
Finder puts the app and the Applications link, and below, in a tinted panel, the
three steps past Gatekeeper's first-launch block. README.md sits at the panel's right.
Each icon label gets a gray plate, so it stays readable in Dark Mode too.

Colors come from brand/tokens.json (light values: the window is always drawn light)
and the text is Poppins Medium from brand/fonts, rendered into the pixels, so the
image looks the same whichever host makes it. The script fails when a text color is
below 4.5:1 on what it sits on, or when a line of text runs into an icon.

Needs Pillow. Usage (from the repository root):
  python3 -I scripts/brand/dmg_background.py
"""
import json
import os
import sys

from PIL import Image, ImageDraw, ImageFont

TOKENS = 'brand/tokens.json'
FONT = 'brand/fonts/Poppins-Medium.ttf'
OUT = 'brand/dmg'

WIDTH, HEIGHT = 660, 460
ICON_SIZE, LABEL_SIZE = 96, 13
# The app, the Applications link and README.md. Finder positions an item by its
# icon's center; the label hangs below the icon.
ICONS = {
    'app': (170, 150),
    'applications': (490, 150),
    'readme': (568, 346),
}
TITLE = 'Drag AudioSync to Applications'
HEADING = 'The first time you open it'
STEPS = [
    "If macOS says it can't verify AudioSync, click Done.",
    'Open Privacy & Security in System Settings.',
    'Scroll down, click Open Anyway and confirm with your password. You only do this once.',
]
# In Dark Mode Finder draws the icon labels white, also over a light background, so
# each label sits on a mid-gray plate that keeps 4.5:1 with white and with black text.
PLATE = '#767676'
PLATE_SIZE = (112, 24)       # width, height in points
PLATE_TOP = ICON_SIZE / 2 + 4  # from the icon's center to the plate's top
STEP_GAP, LINE_GAP = 30, 20     # first lines of two steps; wrapped lines of one step
# Rendered at this many times the target size, then scaled down, so the arrow and
# the step badges get smooth edges (Pillow draws shapes without anti-aliasing).
SUPERSAMPLE = 4
MINIMUM = 4.5


def luminance(hex_color):
    def channel(c):
        c /= 255
        return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4
    r, g, b = (int(hex_color[i:i + 2], 16) for i in (1, 3, 5))
    return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)


def ratio(a, b):
    la, lb = sorted((luminance(a), luminance(b)), reverse=True)
    return (la + 0.05) / (lb + 0.05)


def colors():
    with open(TOKENS, encoding='utf-8') as f:
        tokens = json.load(f)['color']
    light = {name: value['light'].upper() for name, value in tokens.items()
             if not name.startswith('$')}
    return {
        'page': light['surface'],
        'panel': light['background'],
        'edge': light['separator'],
        'text': light['text'],
        'accent': light['tealText'],
        'badgeText': light['surface'],
        'plate': PLATE,
        'white': '#FFFFFF',
        'black': '#000000',
    }


def check_contrast(c):
    pairs = [('text', 'page'), ('text', 'panel'), ('accent', 'page'), ('accent', 'panel'),
             ('badgeText', 'accent'), ('white', 'plate'), ('black', 'plate')]
    ok = True
    for fg, bg in pairs:
        r = ratio(c[fg], c[bg])
        print(f'  {fg:9} on {bg:6} {c[fg]} / {c[bg]}  {r:5.2f}:1  {"ok" if r >= MINIMUM else "FAIL"}')
        ok &= r >= MINIMUM
    return ok


class Canvas:
    def __init__(self, scale, c):
        self.k = scale * SUPERSAMPLE
        self.c = c
        self.image = Image.new('RGB', (WIDTH * self.k, HEIGHT * self.k), c['page'])
        self.draw = ImageDraw.Draw(self.image)
        self.fonts = {}

    def font(self, size):
        if size not in self.fonts:
            self.fonts[size] = ImageFont.truetype(FONT, round(size * self.k))
        return self.fonts[size]

    def p(self, *values):
        return [round(v * self.k) for v in values]

    def text(self, x, y, s, size, color, anchor='la'):
        self.draw.text(self.p(x, y), s, font=self.font(size), fill=self.c[color], anchor=anchor)

    def finish(self, scale):
        return self.image.resize((WIDTH * scale, HEIGHT * scale), Image.LANCZOS)


def text_width(s, size):
    return ImageFont.truetype(FONT, size * 10).getlength(s) / 10


def layout():
    """Positions in points, shared by every scale."""
    app_x, row_y = ICONS['app']
    apps_x, _ = ICONS['applications']
    half = ICON_SIZE / 2
    readme_x, _ = ICONS['readme']
    panel = (24, 262, WIDTH - 24, HEIGHT - 24)
    steps_x, first_y = 76, 324
    readme_left = min(readme_x - half, readme_x - PLATE_SIZE[0] / 2) - 12
    steps, y = [], first_y
    for step in STEPS:
        lines = wrap(step, 14, readme_left - steps_x)
        steps.append((y, lines))
        y += STEP_GAP + LINE_GAP * (len(lines) - 1)
    return {
        'title': (WIDTH / 2, 62),
        'arrow': (app_x + half + 22, apps_x - half - 22, row_y),
        'panel': panel,
        'heading': (panel[0] + 24, 286),
        'badge_x': panel[0] + 34,
        'steps_x': steps_x,
        'steps': steps,
        'readme_left': readme_left,
    }


def wrap(text, size, width):
    lines, line = [], ''
    for word in text.split():
        candidate = f'{line} {word}'.strip()
        if line and text_width(candidate, size) > width:
            lines.append(line)
            line = word
        else:
            line = candidate
    return lines + [line]


def plates():
    w, h = PLATE_SIZE
    return [(x - w / 2, y + PLATE_TOP, x + w / 2, y + PLATE_TOP + h) for x, y in ICONS.values()]


def check_fit(spots):
    ok = True
    for y, lines in spots['steps']:
        for line in lines:
            right = spots['steps_x'] + text_width(line, 14)
            if right > spots['readme_left']:
                print(f'  "{line}" ends at {right:.0f} pt, past README.md at {spots["readme_left"]:.0f} pt')
                ok = False
    last_y, last_lines = spots['steps'][-1]
    bottom = last_y + LINE_GAP * (len(last_lines) - 1) + 10
    if bottom > spots['panel'][3] - 10:
        print(f'  the steps end at {bottom:.0f} pt, past the panel at {spots["panel"][3]:.0f} pt')
        ok = False
    if text_width(TITLE, 20) > WIDTH - 48:
        print(f'  "{TITLE}" is wider than the window')
        ok = False
    for x0, y0, x1, y1 in plates():
        if x0 < 0 or x1 > WIDTH or y1 > HEIGHT:
            print(f'  a label plate at {x0:.0f},{y0:.0f} runs off the window')
            ok = False
    return ok


def render(scale, c, spots):
    cv = Canvas(scale, c)
    d = cv.draw

    cv.text(*spots['title'], TITLE, 20, 'text', anchor='mm')

    left, right, y = spots['arrow']
    stroke = 5
    head = 16
    d.line(cv.p(left, y, right - head * 0.6, y), fill=c['accent'], width=round(stroke * cv.k))
    r = stroke / 2
    d.ellipse(cv.p(left - r, y - r, left + r, y + r), fill=c['accent'])
    d.polygon([tuple(cv.p(right, y)), tuple(cv.p(right - head, y - head * 0.75)),
               tuple(cv.p(right - head, y + head * 0.75))], fill=c['accent'])

    x0, y0, x1, y1 = spots['panel']
    d.rounded_rectangle(cv.p(x0, y0, x1, y1), radius=round(14 * cv.k), fill=c['panel'],
                        outline=c['edge'], width=max(1, round(1 * cv.k)))
    cv.text(*spots['heading'], HEADING, 15, 'text', anchor='lt')

    for number, (sy, lines) in enumerate(spots['steps'], start=1):
        bx, r = spots['badge_x'], 11
        d.ellipse(cv.p(bx - r, sy - r, bx + r, sy + r), fill=c['accent'])
        cv.text(bx, sy + 0.5, str(number), 12, 'badgeText', anchor='mm')
        for i, line in enumerate(lines):
            cv.text(spots['steps_x'], sy + i * LINE_GAP, line, 14, 'text', anchor='lm')

    for box in plates():
        d.rounded_rectangle(cv.p(*box), radius=round(6 * cv.k), fill=c['plate'])

    return cv.finish(scale)


def main():
    if len(sys.argv) > 1:
        print(__doc__.strip().splitlines()[-1].strip(), file=sys.stderr)
        return 2
    c = colors()
    spots = layout()
    print('Contrast (light window):')
    if not check_contrast(c):
        print('A text color is below 4.5:1; pick another token.', file=sys.stderr)
        return 1
    if not check_fit(spots):
        print('Text runs into an icon; shorten it or move the icon.', file=sys.stderr)
        return 1

    os.makedirs(OUT, exist_ok=True)
    for scale, name in ((1, 'background.png'), (2, 'background@2x.png')):
        render(scale, c, spots).save(os.path.join(OUT, name), optimize=True)
    with open(os.path.join(OUT, 'layout.json'), 'w', encoding='utf-8', newline='\n') as f:
        json.dump({
            'window': {'width': WIDTH, 'height': HEIGHT},
            'iconSize': ICON_SIZE,
            'labelSize': LABEL_SIZE,
            'icons': {name: {'x': x, 'y': y} for name, (x, y) in ICONS.items()},
        }, f, indent=2)
        f.write('\n')
    print(f'Wrote {OUT}/background.png, background@2x.png and layout.json')
    return 0


if __name__ == '__main__':
    sys.exit(main())
