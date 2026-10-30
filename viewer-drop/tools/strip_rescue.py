#!/usr/bin/env python3
"""Copy a .kicad_pcb with every footprint/board graphic on the "Rescue" layer removed.

The JLCPCB-converted footprints carry body art on an undefined "Rescue" layer.
The PCB editor tolerates it; kicad-cli refuses to load the board. This writes
a cleaned copy for kicad-cli and leaves the real board file alone.

usage: strip_rescue.py IN.kicad_pcb OUT.kicad_pcb
"""
import re
import sys

NODE = re.compile(r"\((?:fp|gr)_(?:circle|poly|line|rect|arc|text)\b")


def node_end(text, start):
    """Index of the ')' closing the s-expression that opens at start."""
    depth, in_str, i = 0, False, start
    while i < len(text):
        c = text[i]
        if in_str:
            if c == "\\":
                i += 1
            elif c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise ValueError("unbalanced s-expression")


def main(src, dst):
    text = open(src, encoding="utf-8").read()
    out, pos, removed = [], 0, 0
    for m in NODE.finditer(text):
        if m.start() < pos:
            continue
        end = node_end(text, m.start())
        if '(layer "Rescue")' in text[m.start():end + 1]:
            out.append(text[pos:m.start()])
            pos = end + 1
            removed += 1
    out.append(text[pos:])
    open(dst, "w", encoding="utf-8", newline="").write("".join(out))
    print(f"{dst}: removed {removed} Rescue items")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
