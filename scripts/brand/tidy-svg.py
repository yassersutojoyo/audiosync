"""Tidies potrace's SVG output into the brand mark SVG.

potrace writes relative commands inside a flipped, scaled <g>. This script bakes the
transform in, turns curves that are straight within a fraction of a pixel into lines,
snaps nearly horizontal or vertical lines to the axis, moves the mark to the origin,
scales it so its longer side is 1000 units, and writes absolute M/L/C/Z commands.

Usage: python3 tidy-svg.py <potrace.svg> <out.svg> [--fill "#1C9F91"] [--json <outline.json>]
  --json also writes the tidied outline in the input's pixel coordinates (for scoring).
"""
import argparse
import json
import math
import re
import xml.etree.ElementTree as ET

SVG = '{http://www.w3.org/2000/svg}'
STRAIGHT_TOLERANCE = 0.35   # px: control points this close to the chord make a line
AXIS_TOLERANCE_DEG = 1.0    # lines within this angle of an axis are snapped to it
MIN_SNAP_LENGTH = 20        # px: only snap lines at least this long


def parse_transform(text):
    m = (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
    for name, raw in re.findall(r'(\w+)\(([^)]*)\)', text or ''):
        v = [float(x) for x in re.split(r'[ ,]+', raw.strip())]
        if name == 'translate':
            n = (1, 0, 0, 1, v[0], v[1] if len(v) > 1 else 0)
        elif name == 'scale':
            n = (v[0], 0, 0, v[1] if len(v) > 1 else v[0], 0, 0)
        else:
            raise ValueError(f'unsupported transform {name}')
        m = compose(m, n)
    return m


def compose(m, n):
    a, b, c, d, e, f = m
    A, B, C, D, E, F = n
    return (a * A + c * B, b * A + d * B, a * C + c * D, b * C + d * D, a * E + c * F + e, b * E + d * F + f)


def apply(m, p):
    a, b, c, d, e, f = m
    return (a * p[0] + c * p[1] + e, b * p[0] + d * p[1] + f)


def parse_path(d):
    """Returns subpaths as {'start': pt, 'segs': [('L', p) | ('C', c1, c2, p)]} (absolute)."""
    tokens = re.findall(r'[MmLlCcZz]|-?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?', d)
    i, cmd, cur, start, subpaths = 0, None, (0.0, 0.0), (0.0, 0.0), []

    def number():
        nonlocal i
        i += 1
        return float(tokens[i - 1])

    while i < len(tokens):
        if tokens[i].isalpha():
            cmd = tokens[i]
            i += 1
        if cmd in 'Zz':
            cur, cmd = start, None
            continue
        rel = cmd.islower()

        def point():
            x, y = number(), number()
            return (cur[0] + x, cur[1] + y) if rel else (x, y)

        if cmd in 'Mm':
            cur = start = point()
            subpaths.append({'start': cur, 'segs': []})
            cmd = 'l' if rel else 'L'   # further pairs are implicit linetos
        elif cmd in 'Ll':
            p = point()
            subpaths[-1]['segs'].append(('L', p))
            cur = p
        elif cmd in 'Cc':
            c1, c2, p = point(), point(), point()   # all relative to the segment start
            subpaths[-1]['segs'].append(('C', c1, c2, p))
            cur = p
    return subpaths


def load(path):
    outlines = []

    def walk(el, m):
        m = compose(m, parse_transform(el.get('transform')))
        if el.tag == SVG + 'path':
            for sub in parse_path(el.get('d')):
                outlines.append({'start': apply(m, sub['start']),
                                 'segs': [(s[0],) + tuple(apply(m, p) for p in s[1:]) for s in sub['segs']]})
        for child in el:
            walk(child, m)

    walk(ET.parse(path).getroot(), (1, 0, 0, 1, 0, 0))
    return outlines


def distance_to_chord(p, a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    length = math.hypot(dx, dy)
    if length == 0:
        return math.hypot(p[0] - a[0], p[1] - a[1])
    return abs(dy * (p[0] - a[0]) - dx * (p[1] - a[1])) / length


def straighten(sub):
    prev, segs, count = sub['start'], [], 0
    for s in sub['segs']:
        if s[0] == 'C' and max(distance_to_chord(s[1], prev, s[3]), distance_to_chord(s[2], prev, s[3])) <= STRAIGHT_TOLERANCE:
            s = ('L', s[3])
            count += 1
        segs.append(s)
        prev = s[-1]
    sub['segs'] = segs
    return count


def snap_axes(sub):
    """Snaps long, nearly axis-aligned lines exactly onto the axis.

    Consecutive lines along the same axis (including the pair that closes the
    outline) form one run and snap together, to the mean coordinate of their points,
    so snapping one line never tilts its neighbour again. Control points next to a
    moved point move with it, so the curves on either side keep their tangents.
    """
    segs = [list(s) for s in sub['segs']]
    points = [list(sub['start'])] + [list(s[-1]) for s in segs]
    n = len(segs)
    # potrace closes each outline on its start point: points[0] and points[n] are one point.
    closed = n > 0 and math.dist(points[0], points[n]) < 1e-6

    def axis_of(k):
        if segs[k][0] != 'L':
            return None
        a, b = points[k], points[k + 1]
        dx, dy = b[0] - a[0], b[1] - a[1]
        if math.hypot(dx, dy) < MIN_SNAP_LENGTH:
            return None
        angle = math.degrees(math.atan2(abs(dy), abs(dx)))
        return 0 if angle >= 90 - AXIS_TOLERANCE_DEG else 1 if angle <= AXIS_TOLERANCE_DEG else None

    axes = [axis_of(k) for k in range(n)]
    runs, k = [], 0                      # runs of consecutive segments on one axis
    while k < n:
        if axes[k] is None:
            k += 1
            continue
        j = k
        while j + 1 < n and axes[j + 1] == axes[k]:
            j += 1
        runs.append((list(range(k, j + 1)), axes[k]))
        k = j + 1
    if closed and len(runs) > 1 and runs[0][0][0] == 0 and runs[-1][0][-1] == n - 1 and runs[0][1] == runs[-1][1]:
        last = runs.pop()
        runs[0] = (last[0] + runs[0][0], last[1])

    count = 0
    for seg_ids, axis in runs:
        members = set(seg_ids)
        pids = []
        for k in seg_ids:
            for pid in (k, k + 1):
                pid = 0 if closed and pid == n else pid
                if pid not in pids:
                    pids.append(pid)
        target = sum(points[pid][axis] for pid in pids) / len(pids)
        for pid in pids:
            delta = target - points[pid][axis]
            points[pid][axis] = target
            if closed and pid == 0:
                points[n][axis] = target
            before = pid - 1 if pid > 0 else (n - 1 if closed else None)
            after = pid if pid < n else None
            for seg, ctrl in ((before, 2), (after, 1)):
                if seg is not None and seg not in members and segs[seg][0] == 'C':
                    segs[seg][ctrl] = list(segs[seg][ctrl])
                    segs[seg][ctrl][axis] += delta
        count += len(seg_ids)
    sub['start'] = tuple(points[0])
    sub['segs'] = [tuple([s[0]] + [tuple(p) for p in s[1:-1]] + [tuple(points[k + 1])]) for k, s in enumerate(segs)]
    return count


def cubic_extremes(p0, p1, p2, p3, axis):
    """Values of one coordinate of a cubic Bezier at its endpoints and turning points."""
    a0, a1, a2, a3 = p0[axis], p1[axis], p2[axis], p3[axis]
    # B'(t) / 3 = a t^2 + b t + c
    a = -a0 + 3 * a1 - 3 * a2 + a3
    b = 2 * (a0 - 2 * a1 + a2)
    c = a1 - a0
    ts = []
    if abs(a) < 1e-12:
        if abs(b) > 1e-12:
            ts.append(-c / b)
    else:
        disc = b * b - 4 * a * c
        if disc >= 0:
            r = math.sqrt(disc)
            ts += [(-b + r) / (2 * a), (-b - r) / (2 * a)]
    values = [a0, a3]
    for t in ts:
        if 0 < t < 1:
            u = 1 - t
            values.append(u * u * u * a0 + 3 * u * u * t * a1 + 3 * u * t * t * a2 + t * t * t * a3)
    return values


def bbox(outlines):
    """The exact bounding box of the outlines (curves can bulge past their endpoints)."""
    xs, ys = [], []
    for o in outlines:
        prev = o['start']
        for s in o['segs']:
            if s[0] == 'C':
                xs += cubic_extremes(prev, s[1], s[2], s[3], 0)
                ys += cubic_extremes(prev, s[1], s[2], s[3], 1)
            else:
                xs += [prev[0], s[1][0]]
                ys += [prev[1], s[1][1]]
            prev = s[-1]
    return min(xs), min(ys), max(xs), max(ys)


def fmt(v):
    s = f'{v:.2f}'.rstrip('0').rstrip('.')
    return '0' if s in ('-0', '') else s


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('src')
    ap.add_argument('dst')
    ap.add_argument('--fill', default='#1C9F91')
    ap.add_argument('--json')
    a = ap.parse_args()

    outlines = load(a.src)
    straightened = sum(straighten(o) for o in outlines)
    snapped = sum(snap_axes(o) for o in outlines)
    if a.json:
        with open(a.json, 'w') as f:
            json.dump([{'start': o['start'], 'closed': True, 'segs': [[s[0]] + list(s[1:]) for s in o['segs']]}
                       for o in outlines], f)

    x0, y0, x1, y1 = bbox(outlines)
    k = 1000 / max(x1 - x0, y1 - y0)
    w, h = (x1 - x0) * k, (y1 - y0) * k

    def p(pt):
        return f'{fmt((pt[0] - x0) * k)} {fmt((pt[1] - y0) * k)}'

    parts = []
    for o in outlines:
        d = [f'M{p(o["start"])}']
        for s in o['segs']:
            d.append(f'L{p(s[1])}' if s[0] == 'L' else f'C{p(s[1])} {p(s[2])} {p(s[3])}')
        parts.append(' '.join(d) + ' Z')
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {fmt(w)} {fmt(h)}">\n'
           f'  <title>AudioSync</title>\n'
           f'  <path fill="{a.fill}" d="{" ".join(parts)}"/>\n'
           f'</svg>\n')
    with open(a.dst, 'w') as f:
        f.write(svg)
    segments = sum(len(o['segs']) for o in outlines)
    print(f'{a.dst}: {len(outlines)} shapes, {segments} segments '
          f'({straightened} curves straightened, {snapped} lines snapped), viewBox {fmt(w)} x {fmt(h)}')


if __name__ == '__main__':
    main()
