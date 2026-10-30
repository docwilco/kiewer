"""Minimal .kicad_pcb reader shared by the SVG tools: s-expression parse,
footprint pads in board coordinates, vias, and the Edge.Cuts origin that
kicad-cli page-size-mode 2 exports use as (0, 0)."""
import math
import re

LAYERS = ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]


def parse(s):
    tok = re.findall(r'\(|\)|"(?:[^"\\]|\\.)*"|[^\s()"]+', s)
    stack = [[]]
    for t in tok:
        if t == "(":
            stack.append([])
        elif t == ")":
            x = stack.pop()
            stack[-1].append(x)
        else:
            stack[-1].append(t[1:-1] if t.startswith('"') else t)
    return stack[0][0]


def load(path):
    return parse(open(path, encoding="utf-8").read().replace("\r", ""))


def kids(n, k):
    return [c for c in n if isinstance(c, list) and c and c[0] == k]


def kid(n, k):
    r = kids(n, k)
    return r[0] if r else None


def netname(n):
    x = kid(n, "net")
    return x[-1] if x else ""


def xy(n, k):
    c = kid(n, k)
    return float(c[1]), float(c[2])


def rot(x, y, a):
    """Rotate (x, y) by a degrees as KiCad does on screen (y down, positive CCW)."""
    a = math.radians(-a)
    return x * math.cos(a) - y * math.sin(a), x * math.sin(a) + y * math.cos(a)


def edge_origin(root):
    xs, ys = [], []
    for g in root:
        if not isinstance(g, list) or not g or not str(g[0]).startswith("gr_"):
            continue
        lay = kid(g, "layer")
        if not lay or lay[1] != "Edge.Cuts":
            continue
        for k in ("start", "end", "mid", "center"):
            if kid(g, k):
                x, y = xy(g, k); xs.append(x); ys.append(y)
        pts = kid(g, "pts")
        if pts:
            for p in pts[1:]:
                xs.append(float(p[1])); ys.append(float(p[2]))
    return min(xs), min(ys)


def pads(root):
    """Every pad in board coordinates: num, net, x, y, w, h, angle, type, layers, drill."""
    for fp in kids(root, "footprint"):
        fx, fy = xy(fp, "at")
        fat = kid(fp, "at")
        fa = float(fat[3]) if len(fat) > 3 else 0.0
        for p in kids(fp, "pad"):
            px, py = xy(p, "at")
            pat = kid(p, "at")
            pa = float(pat[3]) if len(pat) > 3 else 0.0
            rx, ry = rot(px, py, fa)
            w, h = xy(p, "size")
            lay = set()
            for l in kid(p, "layers")[1:]:
                if l == "*.Cu": lay |= set(LAYERS)
                elif l.endswith(".Cu"): lay.add(l)
            if p[2] in ("thru_hole", "np_thru_hole"):
                lay = set(LAYERS)
            d = kid(p, "drill")
            drill = None
            if d:
                drill = (float(d[2]), float(d[3])) if d[1] == "oval" else (float(d[1]), float(d[1]))
            yield dict(num=p[1], net=netname(p), x=fx + rx, y=fy + ry, w=w, h=h, angle=pa,
                       type=p[2], layers=lay, drill=drill)


def vias(root):
    for v in kids(root, "via"):
        x, y = xy(v, "at")
        yield dict(net=netname(v), x=x, y=y, size=float(kid(v, "size")[1]), drill=float(kid(v, "drill")[1]))


def segments(root):
    for sg in kids(root, "segment"):
        yield dict(net=netname(sg), start=xy(sg, "start"), end=xy(sg, "end"),
                   width=float(kid(sg, "width")[1]), layer=kid(sg, "layer")[1])
