#!/usr/bin/env python3
"""Write one SVG per copper layer with visible pad, via and track labels.

Pads get their number and net, vias and tracks their net. Coordinates use
the same frame as the kicad-cli page-size-mode 2 exports (origin at the
Edge.Cuts bounding box), so compose_svg.py stacks them like any layer.

usage: net_labels.py BOARD.kicad_pcb EDGE_CUTS.svg OUT_DIR
writes OUT_DIR/labels-<layer>.svg for F.Cu, In1.Cu, In2.Cu, B.Cu, plus
labels-<layer>-mirror.svg with x flipped for panels viewed from the back
"""
import math
import re
import sys
from xml.sax.saxutils import escape

import kicad_board as kb

LAYERS = kb.LAYERS
FONT = "Consolas, monospace"
CHAR = 0.6          # glyph width as a fraction of font size (monospace)
FILL, HALO = "#F8F8F8", "#202020"
MIN_FS = 0.08       # mm; smaller labels are dropped
def norm(a):
    """Text angle in (-90, 90] so labels never read upside down."""
    a = (a + 180) % 360 - 180
    if a > 90: a -= 180
    if a <= -90: a += 180
    return a


def fit(text, width, height):
    """Font size so text fits width, capped by height."""
    return min(height, width / (CHAR * max(len(text), 1)))


def label(out, x, y, text, fs, angle=0.0):
    if fs >= MIN_FS and text:
        out.append((x, y, text, fs, angle))


def render(x, y, text, fs, angle):
    t = escape(text)
    tr = f' transform="rotate({angle:.1f} {x:.3f} {y:.3f})"' if abs(angle) > 0.05 else ""
    return (f'<text x="{x:.3f}" y="{y:.3f}" dy="0.35em" font-size="{fs:.3f}" text-anchor="middle"'
            f' font-family="{FONT}" fill="{FILL}" stroke="{HALO}" stroke-width="{fs * 0.08:.3f}"'
            f' paint-order="stroke"{tr}>{t}</text>')


def write(path, vb, labels, mirror=False):
    width = float(vb.split()[2])
    rows = [render(width - x, y, t, fs, -a) if mirror else render(x, y, t, fs, a) for x, y, t, fs, a in labels]
    with open(path, "w", encoding="utf-8") as f:
        f.write(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}">\n' + "\n".join(rows) + "\n</svg>\n")
    print(f"{path}: {len(rows)} labels")


def main(board, edge_svg, out_dir):
    root = kb.load(board)
    vb = re.search(r'viewBox="([^"]+)"', open(edge_svg, encoding="utf-8").read()).group(1)
    ox, oy = kb.edge_origin(root)
    per = {l: [] for l in LAYERS}

    for p in kb.pads(root):
        X, Y, w, h, pa = p["x"] - ox, p["y"] - oy, p["w"], p["h"], p["angle"]
        lines = [t for t in (p["num"], p["net"]) if t]
        if not lines:
            continue
        # text runs along the pad's long axis
        along, across, ang = (w, h, -pa) if w >= h else (h, w, -pa - 90)
        ang = 0.0 if abs(w - h) < 0.01 else norm(ang)   # round/square pads: keep text upright
        fs = min(fit(t, along * 0.95, across * 0.45) for t in lines)
        for l in p["layers"]:
            out = per[l]
            if len(lines) == 2:
                # stack number above net, offset along the pad's short axis
                dx, dy = kb.rot(0, across * 0.25, -ang)
                label(out, X - dx, Y - dy, lines[0], fs, ang)
                label(out, X + dx, Y + dy, lines[1], fs, ang)
            else:
                label(out, X, Y, lines[0], fs, ang)

    for v in kb.vias(root):
        fs = fit(v["net"], v["size"] * 1.4, v["size"] * 0.45)
        for l in LAYERS:
            label(per[l], v["x"] - ox, v["y"] - oy, v["net"], fs)

    for sg in kb.segments(root):
        (x1, y1), (x2, y2) = sg["start"], sg["end"]
        length = math.hypot(x2 - x1, y2 - y1)
        fs = fit(sg["net"], length * 0.9, sg["width"] * 0.85)
        ang = norm(math.degrees(math.atan2(y2 - y1, x2 - x1)))
        label(per[sg["layer"]], (x1 + x2) / 2 - ox, (y1 + y2) / 2 - oy, sg["net"], fs, ang)

    for l, out in per.items():
        write(f"{out_dir}/labels-{l}.svg", vb, out)
        write(f"{out_dir}/labels-{l}-mirror.svg", vb, out, mirror=True)


if __name__ == "__main__":
    main(*sys.argv[1:4])
