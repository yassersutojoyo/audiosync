"""Writes Sources/AudioSync/DeviceIcons.swift from the device icons in brand/icons.

`swift build` does not compile asset catalogs, so the Mac app draws the icons
as CGPaths in points, 1:1 with the SVG units. Each SVG arc becomes an addArc
around its center, so the curves stay exact.

Usage: python3 -I scripts/brand/device_icons_swift.py   (from the repository root)
"""
import math
import os
import re

ICONS = 'brand/icons'
SWIFT = 'Sources/AudioSync/DeviceIcons.swift'


def n(v):
    s = f'{v:.4f}'.rstrip('0').rstrip('.')
    return '0' if s in ('-0', '') else s


def read(name):
    text = open(os.path.join(ICONS, name + '.svg'), encoding='utf-8').read()
    rule = re.search(r'fill-rule="(\w+)"', text).group(1)
    return rule == 'evenodd', re.search(r' d="([^"]+)"', text).group(1)


def arc_center(x1, y1, r, large, sweep, x2, y2):
    """SVG endpoint arc (rx = ry, no rotation) to center and angles (SVG spec F.6.5)."""
    hx, hy = (x1 - x2) / 2, (y1 - y2) / 2
    d2 = hx * hx + hy * hy
    r = max(r, math.sqrt(d2))
    coef = math.sqrt(max(0.0, (r * r - d2) / d2)) * (-1 if large == sweep else 1)
    cx, cy = coef * hy + (x1 + x2) / 2, -coef * hx + (y1 + y2) / 2
    a0 = math.degrees(math.atan2(y1 - cy, x1 - cx))
    a1 = math.degrees(math.atan2(y2 - cy, x2 - cx))
    span = (a1 - a0) % 360 if sweep else (a0 - a1) % 360
    assert (span > 180 + 1e-6) == bool(large) or abs(span - 180) < 1e-6, 'arc flags disagree with its center'
    return cx, cy, r, a0, a1


def calls(d):
    tokens = re.findall(r'[MLHVAZ]|-?\d*\.?\d+', d)
    assert not re.search(r'[^MLHVAZ0-9.\s-]', d), f'unsupported path command: {d}'
    out, i, x, y, sx, sy = [], 0, 0.0, 0.0, 0.0, 0.0

    def num():
        nonlocal i
        i += 1
        return float(tokens[i - 1])

    while i < len(tokens):
        cmd = tokens[i]
        i += 1
        if cmd == 'M':
            x, y = num(), num()
            sx, sy = x, y
            out.append(f'm({n(x)}, {n(y)})')
        elif cmd == 'L':
            x, y = num(), num()
            out.append(f'l({n(x)}, {n(y)})')
        elif cmd == 'H':
            x = num()
            out.append(f'l({n(x)}, {n(y)})')
        elif cmd == 'V':
            y = num()
            out.append(f'l({n(x)}, {n(y)})')
        elif cmd == 'A':
            rx, ry, rot, large, sweep, x2, y2 = (num() for _ in range(7))
            assert rx == ry and rot == 0, d
            cx, cy, r, a0, a1 = arc_center(x, y, rx, int(large), int(sweep), x2, y2)
            out.append(f'a({n(cx)}, {n(cy)}, {n(r)}, {n(a0)}, {n(a1)}, {"true" if sweep else "false"})')
            x, y = x2, y2
        elif cmd == 'Z':
            x, y = sx, sy
            out.append('z()')
    return out


def body(name, indent):
    lines, row = [], []
    for c in calls(read(name)[1]):
        if row and len(indent + '; '.join(row + ['p.' + c])) > 118:
            lines.append(indent + '; '.join(row))
            row = []
        row.append('p.' + c)
    return lines + [indent + '; '.join(row)]


def enum(name, doc, grids, names):
    """`grids`: size -> file prefix, one drawing of every icon per size."""
    cases = [(re.sub(r'-(\w)', lambda m: m.group(1).upper(), k), k) for k in names]
    sizes = sorted(grids)
    lines = [f'/// {doc}', f'enum {name}: String, CaseIterable {{']
    for case, k in cases:
        lines.append(f'    case {case}' + (f' = "{k}"' if case != k else ''))
    lines.append('')
    if len(sizes) == 1:
        lines.append(f'    static let size: CGFloat = {sizes[0]}')
    else:
        lines += [f'    static let sizes: [CGFloat] = [{", ".join(str(s) for s in sizes)}]', '',
                  '    /// The grid for a circle `circle` pt across: the one nearest 18/32 of it, as on the sheet.',
                  '    static func size(circle: CGFloat) -> CGFloat {',
                  '        sizes.min { abs($0 - circle * 18 / 32) < abs($1 - circle * 18 / 32) } ?? 18',
                  '    }']
    rules = {}
    for k in names:
        found = {read(grids[s] + k)[0] for s in sizes}
        assert len(found) == 1, f'{k}: fill rule differs between grids'
        rules[k] = found.pop()
    even = [c for c, k in cases if rules[k]]
    odd = [c for c, k in cases if not rules[k]]
    lines += ['', '    /// Even-odd cuts the holes; nonzero joins the overlapping strokes of the line marks.',
              '    var evenOdd: Bool {', '        switch self {']
    if even:
        lines.append('        case ' + ', '.join('.' + c for c in even) + ': return true')
    if odd:
        lines.append('        case ' + ', '.join('.' + c for c in odd) + ': return false')
    lines += ['        }', '    }', '']
    if len(sizes) == 1:
        lines += ['    var path: CGPath {', '        let p = IconPath()', '        switch self {']
        for case, k in cases:
            lines.append(f'        case .{case}:')
            lines += body(grids[sizes[0]] + k, '            ')
        lines += ['        }', '        return p.path', '    }', '}']
        return lines
    lines += ['    /// Drawn on the grid `size`, one of `sizes`; any other size draws the largest.',
              '    func path(size: CGFloat) -> CGPath {', '        let p = IconPath()']
    for i, s in enumerate(sizes):
        if i < len(sizes) - 1:
            lines.append(('        if' if i == 0 else '        } else if') + f' size == {s} {{')
        else:
            lines.append('        } else {')
        lines.append('            switch self {')
        for case, k in cases:
            lines.append(f'            case .{case}:')
            lines += body(grids[s] + k, '                ')
        lines.append('            }')
    lines += ['        }', '        return p.path', '    }', '}']
    return lines


def main():
    files = sorted(f[:-4] for f in os.listdir(ICONS) if f.endswith('.svg'))
    glyphs = [f for f in files if not f.startswith('badge-')]
    badges = [f[len('badge-'):] for f in files if f.startswith('badge-')]
    # The Mac's circles are 32, 26 and 24 pt; the 16 px set is for the Windows flyout.
    grids = {18: '', 14: '14/'}
    lines = [
        '// Generated by scripts/brand/device_icons_swift.py from brand/icons. Do not edit.',
        'import CoreGraphics',
        '',
    ]
    lines += enum('DeviceIcon', 'A device glyph for the round speaker icon, one drawing per grid.', grids, glyphs)
    lines.append('')
    lines += enum('DeviceBadge', "A device badge after a speaker's name, drawn on a 12 pt grid.", {12: 'badge-'}, badges)
    lines += [
        '',
        '/// Builds an icon in points, y down, angles in degrees.',
        'private final class IconPath {',
        '    let path = CGMutablePath()',
        '',
        '    func m(_ x: CGFloat, _ y: CGFloat) { path.move(to: CGPoint(x: x, y: y)) }',
        '    func l(_ x: CGFloat, _ y: CGFloat) { path.addLine(to: CGPoint(x: x, y: y)) }',
        '    func z() { path.closeSubpath() }',
        '',
        '    /// `increasing`: the SVG sweep flag; the angle grows, clockwise on screen.',
        '    func a(_ cx: CGFloat, _ cy: CGFloat, _ r: CGFloat, _ from: CGFloat, _ to: CGFloat, _ increasing: Bool) {',
        '        path.addArc(center: CGPoint(x: cx, y: cy), radius: r, startAngle: from * .pi / 180,',
        '                    endAngle: to * .pi / 180, clockwise: !increasing)',
        '    }',
        '}',
    ]
    open(SWIFT, 'w', encoding='utf-8', newline='\n').write('\n'.join(lines) + '\n')
    print(f'{len(glyphs)} glyphs, {len(badges)} badges -> {SWIFT}')


main()
