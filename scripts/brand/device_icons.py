"""Generates AudioSync's own device icons (circle glyphs, 12 px badges).

Circle glyphs are drawn per grid, near the sheet's 18 in 32: 18 px for the
32 px circle, 16 px for the Windows flyout's 28 px circle and 14 px for the
Mac's 24 and 26 px circles, each on its own whole pixels rather than scaled;
the smaller sets go in a subfolder named by their size.

Every straight horizontal or vertical edge is asserted to land on a whole
pixel; curves and diagonals are left to antialiasing.
"""
import math, os, sys

OUT = sys.argv[1]
ORIGIN = "<!-- AudioSync own drawing, 2026-11-16 -->"


def f(v):
    s = f"{v:.3f}".rstrip("0").rstrip(".")
    return "0" if s == "-0" else s


def whole(*vals):
    for v in vals:
        assert abs(v - round(v)) < 1e-9, f"straight edge off the pixel grid: {v}"


def rr(x0, y0, x1, y1, r=0, tl=None, tr=None, br=None, bl=None):
    whole(x0, y0, x1, y1)
    tl, tr, br, bl = [r if c is None else c for c in (tl, tr, br, bl)]
    p = [f"M{f(x0 + tl)} {f(y0)}H{f(x1 - tr)}"]
    if tr: p.append(f"A{f(tr)} {f(tr)} 0 0 1 {f(x1)} {f(y0 + tr)}")
    p.append(f"V{f(y1 - br)}")
    if br: p.append(f"A{f(br)} {f(br)} 0 0 1 {f(x1 - br)} {f(y1)}")
    p.append(f"H{f(x0 + bl)}")
    if bl: p.append(f"A{f(bl)} {f(bl)} 0 0 1 {f(x0)} {f(y1 - bl)}")
    p.append(f"V{f(y0 + tl)}")
    if tl: p.append(f"A{f(tl)} {f(tl)} 0 0 1 {f(x0 + tl)} {f(y0)}")
    return "".join(p) + "Z"


def circ(cx, cy, r):
    return (f"M{f(cx - r)} {f(cy)}A{f(r)} {f(r)} 0 1 1 {f(cx + r)} {f(cy)}"
            f"A{f(r)} {f(r)} 0 1 1 {f(cx - r)} {f(cy)}Z")


def pt(cx, cy, r, a):
    t = math.radians(a)
    return cx + r * math.cos(t), cy + r * math.sin(t)


def sector(cx, cy, ro, ri, a0, a1):
    large = 1 if (a1 - a0) % 360 > 180 else 0
    ox0, oy0 = pt(cx, cy, ro, a0); ox1, oy1 = pt(cx, cy, ro, a1)
    ix0, iy0 = pt(cx, cy, ri, a0); ix1, iy1 = pt(cx, cy, ri, a1)
    return (f"M{f(ox0)} {f(oy0)}A{f(ro)} {f(ro)} 0 {large} 1 {f(ox1)} {f(oy1)}"
            f"L{f(ix1)} {f(iy1)}A{f(ri)} {f(ri)} 0 {large} 0 {f(ix0)} {f(iy0)}Z")


def poly(points):
    area = sum(points[i][0] * points[(i + 1) % len(points)][1]
               - points[(i + 1) % len(points)][0] * points[i][1] for i in range(len(points)))
    if area < 0:
        points = points[::-1]
    return "M" + "L".join(f"{f(x)} {f(y)}" for x, y in points) + "Z"


def seg(p, q, w):
    dx, dy = q[0] - p[0], q[1] - p[1]
    n = math.hypot(dx, dy)
    nx, ny = -dy / n * w / 2, dx / n * w / 2
    return poly([(p[0] + nx, p[1] + ny), (q[0] + nx, q[1] + ny),
                 (q[0] - nx, q[1] - ny), (p[0] - nx, p[1] - ny)])


def line(points, w, caps=True):
    """A round-joined polyline as filled shapes, all clockwise (nonzero union)."""
    out = [seg(points[i], points[i + 1], w) for i in range(len(points) - 1)]
    joints = points if caps else points[1:-1]
    out += [circ(x, y, w / 2) for x, y in joints]
    return "".join(out)


def rounded_poly(points, r):
    return poly(points) + line(points + [points[0]], 2 * r, caps=True)


def arc_line(cx, cy, rad, w, a0, a1):
    return sector(cx, cy, rad + w / 2, rad - w / 2, a0, a1) + \
        circ(*pt(cx, cy, rad, a0), w / 2) + circ(*pt(cx, cy, rad, a1), w / 2)


# Every grid: walls 2, gaps 2, outer corner radius 2, cut-out radius 1.
GLYPHS = {18: {
    "laptop": ("evenodd",
               rr(3, 3, 15, 13, tl=2, tr=2, br=0, bl=0) + rr(5, 5, 13, 11, 1)
               + rr(1, 13, 17, 15, 1)),
    "speaker": ("evenodd",
                rr(4, 1, 14, 17, 2) + circ(9, 4.5, 1.5) + circ(9, 11.5, 3) + circ(9, 11.5, 1)),
    "smart-speaker": ("evenodd",
                      rr(4, 1, 14, 17, 4) + rr(6, 5, 12, 7, 1) + circ(9, 12, 2)),
    "headphones": ("evenodd",
                   sector(9, 9, 7, 5, 180, 360) + rr(1, 9, 6, 17, 2) + rr(12, 9, 17, 17, 2)),
    "monitor": ("evenodd",
                rr(1, 2, 17, 14, 2) + rr(3, 4, 15, 12, 1) + rr(8, 14, 10, 15)
                + rr(5, 15, 13, 17, 1)),
    "tv": ("evenodd",
           rr(1, 3, 17, 14, 2) + rr(3, 5, 15, 12, 1)
           + rr(3, 14, 6, 16, bl=1, br=1) + rr(12, 14, 15, 16, bl=1, br=1)),
    "bluetooth": ("nonzero",
                  line([(4.5, 5.5), (13.5, 12.5), (9, 16), (9, 2), (13.5, 5.5), (4.5, 12.5)], 2)),
    "usb": ("evenodd",
            rr(6, 1, 8, 5, tl=0.5, tr=0.5) + rr(10, 1, 12, 5, tl=0.5, tr=0.5)
            + rr(4, 5, 14, 13, 2) + rr(8, 13, 10, 17, bl=1, br=1)),
    "zone": ("nonzero",
             rounded_poly([(9, 3), (16, 7), (9, 11), (2, 7)], 1)
             + line([(2, 11), (9, 15), (16, 11)], 2)),
    "device": ("evenodd",
               rr(2, 2, 16, 16, 2) + rr(4, 4, 14, 14, 1) + circ(9, 9, 3)),
}, 14: {
    "laptop": ("evenodd",
               rr(2, 2, 12, 10, tl=2, tr=2, br=0, bl=0) + rr(4, 4, 10, 8, 1) + rr(1, 10, 13, 12, 1)),
    "speaker": ("evenodd",
                rr(3, 1, 11, 13, 2) + circ(7, 4, 1) + circ(7, 9, 2.5) + circ(7, 9, 1)),
    "smart-speaker": ("evenodd",
                      rr(3, 1, 11, 13, 3) + rr(5, 4, 9, 6, 1) + circ(7, 9.5, 1.5)),
    "headphones": ("evenodd",
                   sector(7, 7, 6, 4, 180, 360) + rr(1, 7, 5, 13, 2) + rr(9, 7, 13, 13, 2)),
    "monitor": ("evenodd",
                rr(1, 1, 13, 10, 2) + rr(3, 3, 11, 8, 1) + rr(6, 10, 8, 11) + rr(4, 11, 10, 13, 1)),
    "tv": ("evenodd",
           rr(1, 2, 13, 11, 2) + rr(3, 4, 11, 9, 1)
           + rr(3, 11, 5, 13, bl=1, br=1) + rr(9, 11, 11, 13, bl=1, br=1)),
    "bluetooth": ("nonzero",
                  line([(3.5, 4.5), (10.5, 9.5), (7, 12.5), (7, 1.5), (10.5, 4.5), (3.5, 9.5)], 2)),
    "usb": ("evenodd",
            rr(4, 1, 6, 4, tl=0.5, tr=0.5) + rr(8, 1, 10, 4, tl=0.5, tr=0.5)
            + rr(3, 4, 11, 10, 2) + rr(6, 10, 8, 13, bl=1, br=1)),
    "zone": ("nonzero",
             rounded_poly([(7, 2), (12, 5), (7, 8), (2, 5)], 1)
             + line([(2, 9), (7, 12), (12, 9)], 2)),
    "device": ("evenodd",
               rr(2, 2, 12, 12, 2) + rr(4, 4, 10, 10, 1) + circ(7, 7, 1.5)),
}, 16: {
    "laptop": ("evenodd",
               rr(2, 2, 14, 12, tl=2, tr=2, br=0, bl=0) + rr(4, 4, 12, 10, 1) + rr(1, 12, 15, 14, 1)),
    "speaker": ("evenodd",
                rr(3, 1, 13, 15, 2) + circ(8, 4, 1) + circ(8, 10, 2.75) + circ(8, 10, 1)),
    "smart-speaker": ("evenodd",
                      rr(3, 1, 13, 15, 4) + rr(5, 4, 11, 6, 1) + circ(8, 10.5, 1.75)),
    "headphones": ("evenodd",
                   sector(8, 8, 7, 5, 180, 360) + rr(1, 8, 6, 15, 2) + rr(10, 8, 15, 15, 2)),
    "monitor": ("evenodd",
                rr(1, 2, 15, 12, 2) + rr(3, 4, 13, 10, 1) + rr(7, 12, 9, 13) + rr(4, 13, 12, 15, 1)),
    "tv": ("evenodd",
           rr(1, 2, 15, 12, 2) + rr(3, 4, 13, 10, 1)
           + rr(3, 12, 5, 14, bl=1, br=1) + rr(11, 12, 13, 14, bl=1, br=1)),
    "bluetooth": ("nonzero",
                  line([(4, 5), (12, 11), (8, 14.5), (8, 1.5), (12, 5), (4, 11)], 2)),
    "usb": ("evenodd",
            rr(5, 1, 7, 4, tl=0.5, tr=0.5) + rr(9, 1, 11, 4, tl=0.5, tr=0.5)
            + rr(3, 4, 13, 11, 2) + rr(7, 11, 9, 15, bl=1, br=1)),
    "zone": ("nonzero",
             rounded_poly([(8, 2.5), (14, 6), (8, 9.5), (2, 6)], 1)
             + line([(2, 10), (8, 13.5), (14, 10)], 2)),
    "device": ("evenodd",
               rr(2, 2, 14, 14, 2) + rr(4, 4, 12, 12, 1) + circ(8, 8, 2)),
}}

# 12 px badges: solid shapes, 1 px gaps, corner radius 1.
BADGES = {
    "airplay": ("nonzero",
                poly([(3, 11), (9, 11), (6, 7.5)]) + arc_line(6, 9, 3.25, 1.5, 205, 335)
                + arc_line(6, 9, 6.25, 1.5, 212, 328)),
    "cast": ("nonzero",
             circ(2.5, 6, 1.5) + arc_line(2.5, 6, 4.25, 2, -45, 45) + arc_line(2.5, 6, 7.5, 2, -38, 38)),
    "bluetooth": ("nonzero",
                  rr(5, 1, 7, 11, 1) + line([(3, 3.75), (8.5, 8.25), (6, 10.25)], 1.5, caps=True)
                  + line([(6, 1.75), (8.5, 3.75), (3, 8.25)], 1.5, caps=True)),
    "usb": ("evenodd",
            rr(4, 1, 5, 3) + rr(7, 1, 8, 3) + rr(2, 3, 10, 8, 1) + rr(5, 8, 7, 11)),
    "cable": ("evenodd",
              rr(9, 4, 11, 5) + rr(9, 7, 11, 8) + rr(4, 2, 9, 10, 1) + rr(1, 5, 4, 7)),
    "display": ("evenodd",
                rr(1, 1, 11, 8, 1) + rr(2, 2, 10, 6, 0.5) + rr(5, 8, 7, 10) + rr(3, 10, 9, 11)),
    "network": ("evenodd",
                rr(4, 0, 8, 3, 1) + rr(5, 3, 7, 5) + rr(2, 5, 10, 6) + rr(2, 6, 3, 8) + rr(9, 6, 10, 8)
                + rr(0, 8, 5, 11, 1) + rr(7, 8, 12, 11, 1)),
    "laptop": ("evenodd",
               rr(2, 2, 10, 8, 1) + rr(3, 3, 9, 7, 0.5) + rr(0, 9, 12, 11, 1)),
    "speaker": ("evenodd",
                rr(2, 1, 10, 11, 1.5) + circ(6, 6, 3) + circ(6, 6, 1.25)),
    "headphones": ("evenodd",
                   sector(6, 6, 5, 3.5, 180, 360) + rr(1, 6, 4, 11, 1) + rr(8, 6, 11, 11, 1)),
    "zone": ("nonzero",
             poly([(6, 1), (11, 4), (6, 7), (1, 4)]) + line([(1.5, 7.5), (6, 10.25), (10.5, 7.5)], 1.5)),
}


def svg(size, rule, d):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" width="{size}" height="{size}">'
            f"{ORIGIN}<path fill=\"currentColor\" fill-rule=\"{rule}\" d=\"{d}\"/></svg>\n")


os.makedirs(OUT, exist_ok=True)
for size, glyphs in GLYPHS.items():
    folder = OUT if size == 18 else os.path.join(OUT, str(size))
    os.makedirs(folder, exist_ok=True)
    for k, (rule, d) in glyphs.items():
        open(os.path.join(folder, f"{k}.svg"), "w", newline="\n").write(svg(size, rule, d))
for k, (rule, d) in BADGES.items():
    open(os.path.join(OUT, f"badge-{k}.svg"), "w", newline="\n").write(svg(12, rule, d))
print(len(GLYPHS[18]), "glyphs on", len(GLYPHS), "grids,", len(BADGES), "badges")
