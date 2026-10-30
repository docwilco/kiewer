#!/usr/bin/env python3
"""Tile kicad-cli per-layer SVG exports into one SVG, one headed panel per view.

Each panel is drawn on a board fill taken from the Edge.Cuts export, with the
layer files stacked in the order given. A layer prefixed "mask:" is drawn as a
solder-mask overlay: the board is covered in mask colour except where that
layer has openings. Drill holes (white in kicad-cli output) are recoloured to
the page background so they read as holes.

usage: compose_svg.py OUT.svg COLS EDGE_CUTS.svg [--each DIR] \
         --panel "Header" LAYER [LAYER ...] [--panel ...]
--each DIR: also write every panel as its own SVG, DIR/NN-header.svg
--bare after the header: no board fill, only the Edge.Cuts outline
--mirror after the header: panel flipped left-right (view from the back)
LAYER:   layer.svg                 copper/paste recoloured, others keep theme colour
         flat:layer.svg            not flipped on a --mirror panel (pre-mirrored labels)
         layer.svg@#rrggbb[,0.8]   explicit colour, optional layer opacity
         mask:layer.svg            solder-mask overlay
"""
import re
import sys

BG = "#2b2b2b"          # page background
BOARD = "#7a5a32"       # bare FR4
COPPER = "#f0c030"
PASTE = "#c9c9c9"
MASK = "#1f8a3f"        # translucent, so copper shows through
MASK_OPACITY = "0.7"
ROLE = {"_Cu": COPPER, "_Paste": PASTE}   # filename stem -> colour; other layers keep theme colour
TEXT = "#e6e6e6"
EDGE = "#D0D2CD"        # outline stroke on bare panels
M, GAP, HDR, FONT = 6.0, 8.0, 7.0, 3.2   # mm
SCALE = 10   # document size multiplier: browsers cap zoom relative to the natural size

SVG_OPEN = re.compile(r'<svg\b[^>]*viewBox="([^"]+)"[^>]*>', re.S)


def load(path):
    txt = open(path, encoding="utf-8").read()
    m = SVG_OPEN.search(txt)
    body = txt[m.end():txt.rindex("</svg>")]
    body = re.sub(r"<title>.*?</title>|<desc>.*?</desc>", "", body, flags=re.S)
    return m.group(1), body


def outline_path(edge_body):
    """Chain the Edge.Cuts stroke segments into one closed path."""
    segs = []
    for d in re.findall(r'<path d="([^"]+)"', edge_body):
        toks = d.replace("\n", " ").split()
        start = tuple(float(v) for v in toks[0].lstrip("M").split(","))
        if "A" in d:
            # M x y A rx ry rot large sweep x y   (kicad-cli form, space separated)
            nums = [float(t) for t in " ".join(toks).replace("M", " ").replace("A", " ").split()]
            start, (rx, ry, rot, large, sweep), end = (nums[0], nums[1]), nums[2:7], (nums[7], nums[8])
            segs.append((start, end, ("A", rx, ry, rot, int(large), int(sweep))))
        else:
            nums = [float(t) for t in " ".join(toks).replace("M", " ").replace("L", " ").split()]
            if len(nums) < 4:
                continue
            segs.append(((nums[0], nums[1]), (nums[2], nums[3]), ("L",)))
    if not segs:
        return ""

    def near(a, b):
        return abs(a[0] - b[0]) < 1e-3 and abs(a[1] - b[1]) < 1e-3

    def fmt(seg, end):
        if seg[0] == "L":
            return f"L{end[0]:.4f} {end[1]:.4f}"
        _, rx, ry, rot, large, sweep = seg
        return f"A{rx:.4f} {ry:.4f} {rot} {large} {sweep} {end[0]:.4f} {end[1]:.4f}"

    def flip(seg):
        if seg[0] == "L":
            return seg
        _, rx, ry, rot, large, sweep = seg
        return ("A", rx, ry, rot, large, 1 - sweep)

    start, end, seg = segs.pop(0)
    parts = [f"M{start[0]:.4f} {start[1]:.4f}", fmt(seg, end)]
    cur = end
    while segs:
        for i, (s, e, g) in enumerate(segs):
            if near(s, cur):
                parts.append(fmt(g, e)); cur = e; segs.pop(i); break
            if near(e, cur):
                parts.append(fmt(flip(g), s)); cur = s; segs.pop(i); break
        else:
            # disconnected outline: start a new subpath
            s, e, g = segs.pop(0)
            parts.append(f"M{s[0]:.4f} {s[1]:.4f}"); parts.append(fmt(g, e)); cur = e
    parts.append("Z")
    return " ".join(parts)


def recolour(body, path, colour=None):
    """Copper and paste to their real colours, or everything to an explicit
    colour; drill marks (white on copper layers, black elsewhere) to the page
    background so they read as holes."""
    colour = colour or next((c for k, c in ROLE.items() if k in path), None)
    if colour:
        body = re.sub(r"(fill|stroke):#(?!FFFFFF|000000)[0-9A-Fa-f]{6}", lambda m: f"{m.group(1)}:{colour}", body)
    return re.sub(r"(fill|stroke):#(?:FFFFFF|000000)", lambda m: f"{m.group(1)}:{BG}", body)


def as_mask(body):
    """Every shape fully black: oval openings are stroked lines, not fills."""
    body = re.sub(r"(fill|stroke):#[0-9A-Fa-f]{6}", lambda m: f"{m.group(1)}:#000000", body)
    return re.sub(r"(fill|stroke)-opacity:[0-9.]+", lambda m: f"{m.group(1)}-opacity:1", body)


def slug(label):
    return re.sub(r"[^A-Za-z0-9]+", "_", label.replace(" + ", "+")).strip("_")


def panel_parts(i, label, layers, bare, mirror, vb, board, x, y):
    """SVG fragment for one panel with its header at (x, y)."""
    w, h = [float(v) for v in vb.split()[2:4]]
    parts = [f'<text x="{x:.3f}" y="{y + HDR - 2.2:.3f}" font-family="sans-serif" '
             f'font-size="{FONT}" fill="{TEXT}">{label}</text>',
             f'<svg x="{x:.3f}" y="{y + HDR:.3f}" width="{w:.4f}" height="{h:.4f}" viewBox="{vb}">']
    if mirror:
        parts.append(f'<g transform="translate({w:.4f} 0) scale(-1 1)">')
    flat = []
    if bare:
        parts.append(f'<path d="{board}" fill="none" stroke="{EDGE}" stroke-width="0.1"/>')
    else:
        parts.append(f'<path d="{board}" fill="{BOARD}" stroke="none"/>')
    for n, layer in enumerate(layers):
        if layer.startswith("flat:"):
            flat.append(load(layer[5:])[1])
        elif layer.startswith("mask:"):
            _, mbody = load(layer[5:])
            mid = f"mask{i}_{n}"
            parts.append(f'<mask id="{mid}" maskUnits="userSpaceOnUse" x="0" y="0" width="{w:.4f}" height="{h:.4f}">'
                         f'<rect width="{w:.4f}" height="{h:.4f}" fill="#ffffff"/>{as_mask(mbody)}</mask>')
            parts.append(f'<path d="{board}" fill="{MASK}" fill-opacity="{MASK_OPACITY}" mask="url(#{mid})"/>')
        else:
            path, _, opts = layer.partition("@")
            colour, _, opacity = opts.partition(",")
            _, body = load(path)
            body = recolour(body, path, colour or None)
            parts.append(f'<g opacity="{opacity}">{body}</g>' if opacity else body)
    if mirror:
        parts.append("</g>")
    parts.extend(flat)
    parts.append("</svg>")
    return parts


def document(W, H, parts):
    return "\n".join([f'<?xml version="1.0" encoding="UTF-8"?>\n<svg xmlns="http://www.w3.org/2000/svg" '
                      f'width="{W * SCALE:.1f}mm" height="{H * SCALE:.1f}mm" viewBox="0 0 {W:.3f} {H:.3f}">',
                      f'<rect width="{W:.3f}" height="{H:.3f}" fill="{BG}"/>', *parts, "</svg>\n"])


def main(argv):
    out, cols, edge = argv[0], int(argv[1]), argv[2]
    rest, each = argv[3:], None
    if rest[:1] == ["--each"]:
        each, rest = rest[1], rest[2:]
    panels, cur = [], None
    for a in rest:
        if a == "--panel":
            cur = None
        elif cur is None:
            cur = (a, []); panels.append(cur)
        else:
            cur[1].append(a)

    vb, edge_body = load(edge)
    pw, ph = [float(v) for v in vb.split()[2:4]]
    board = outline_path(edge_body)
    rows = (len(panels) + cols - 1) // cols
    W = M * 2 + cols * pw + (cols - 1) * GAP
    H = M * 2 + rows * (HDR + ph) + (rows - 1) * GAP

    parts = []
    for i, (label, layers) in enumerate(panels):
        bare, mirror = "--bare" in layers, "--mirror" in layers
        layers = [l for l in layers if l not in ("--bare", "--mirror")]
        r, c = divmod(i, cols)
        parts += panel_parts(i, label, layers, bare, mirror, vb, board,
                             M + c * (pw + GAP), M + r * (HDR + ph + GAP))
        if each:
            one = panel_parts(i, label, layers, bare, mirror, vb, board, M, M)
            path = f"{each}/{i + 1:02d}-{slug(label)}.svg"
            open(path, "w", encoding="utf-8").write(document(M * 2 + pw, M * 2 + HDR + ph, one))
    open(out, "w", encoding="utf-8").write(document(W, H, parts))
    print(f"{out}: {len(panels)} panels, {W:.0f} x {H:.0f} mm")


if __name__ == "__main__":
    main(sys.argv[1:])
