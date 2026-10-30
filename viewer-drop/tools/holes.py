#!/usr/bin/env python3
"""Write holes.svg: every drill as a page-background hole, plated ones with a
copper rim, so barrels show on layers without a pad (inner layers, mask and
silk views). Give it to compose_svg.py as the last layer of a panel.

usage: holes.py BOARD.kicad_pcb EDGE_CUTS.svg OUT.svg
"""
import re
import sys

import kicad_board as kb

BG = "#2b2b2b"      # must match compose_svg.BG
COPPER = "#f0c030"
RIM = 0.1           # mm, copper showing around a plated hole


def shape(x, y, dw, dh, angle, colour, grow):
    if abs(dw - dh) < 1e-6:
        return f'<circle cx="{x:.4f}" cy="{y:.4f}" r="{dw / 2 + grow / 2:.4f}" fill="{colour}"/>'
    # slot: a round-capped line along the long axis
    if dw >= dh:
        half, width = (dw - dh) / 2, dh
        dx, dy = kb.rot(half, 0, angle)
    else:
        half, width = (dh - dw) / 2, dw
        dx, dy = kb.rot(0, half, angle)
    return (f'<line x1="{x - dx:.4f}" y1="{y - dy:.4f}" x2="{x + dx:.4f}" y2="{y + dy:.4f}" '
            f'stroke="{colour}" stroke-width="{width + grow:.4f}" stroke-linecap="round"/>')


def main(board, edge_svg, out):
    root = kb.load(board)
    vb = re.search(r'viewBox="([^"]+)"', open(edge_svg, encoding="utf-8").read()).group(1)
    ox, oy = kb.edge_origin(root)
    holes = []   # (x, y, dw, dh, angle, plated)
    for p in kb.pads(root):
        if p["drill"]:
            holes.append((p["x"] - ox, p["y"] - oy, *p["drill"], p["angle"], p["type"] == "thru_hole"))
    for v in kb.vias(root):
        holes.append((v["x"] - ox, v["y"] - oy, v["drill"], v["drill"], 0.0, True))
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}">']
    for x, y, dw, dh, a, plated in holes:
        if plated:
            parts.append(shape(x, y, dw, dh, a, COPPER, 2 * RIM))
        parts.append(shape(x, y, dw, dh, a, BG, 0))
    parts.append("</svg>\n")
    open(out, "w", encoding="utf-8").write("\n".join(parts))
    print(f"{out}: {len(holes)} holes, {sum(1 for h in holes if h[5])} plated")


if __name__ == "__main__":
    main(*sys.argv[1:4])
