#!/usr/bin/env bash
# Export per-layer SVGs, the tiled layer overview, one SVG and PNG per view,
# and the schematic as SVG and PNG, all into midi8knob.svg/.
# Needs kicad-cli 10 and rsvg-convert (librsvg2-bin) on PATH, plus the
# Consolas font for the silkscreen. Works on a Rescue-stripped copy of the board.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=midi8knob.svg
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

python3 tools/strip_rescue.py midi8knob.kicad_pcb "$TMP/midi8knob.kicad_pcb"
mkdir -p "$OUT/views" "$OUT/png"
rm -f "$OUT"/*.svg "$OUT"/views/* "$OUT"/png/*
kicad-cli pcb export svg --mode-multi \
  --layers F.Cu,In1.Cu,In2.Cu,B.Cu,F.SilkS,B.SilkS,F.Mask,B.Mask,F.Paste,B.Paste,Edge.Cuts,F.CrtYd,B.CrtYd,F.Fab,B.Fab \
  --page-size-mode 2 --exclude-drawing-sheet --subtract-soldermask \
  -o "$OUT" "$TMP/midi8knob.kicad_pcb"

L=$OUT/midi8knob
N=$OUT/labels
python3 tools/net_labels.py midi8knob.kicad_pcb "$L-Edge_Cuts.svg" "$OUT"
python3 tools/holes.py midi8knob.kicad_pcb "$L-Edge_Cuts.svg" "$OUT/holes.svg"
python3 tools/compose_svg.py "$L-layers.svg" 2 "$L-Edge_Cuts.svg" --each "$OUT/views" \
  --panel "F.Cu + F.Paste" --bare "$L-F_Cu.svg@#C83434" "$L-F_Paste.svg" "$N-F.Cu.svg" \
  --panel "In1.Cu" --bare "$L-In1_Cu.svg@#7FC87F" "$N-In1.Cu.svg" \
  --panel "In2.Cu" --bare "$L-In2_Cu.svg@#CE7D2C" "$N-In2.Cu.svg" \
  --panel "B.Cu" --bare "$L-B_Cu.svg@#4D7FC4" "$N-B.Cu.svg" \
  --panel "F.Cu + F.Paste + F.Mask + F.SilkS" "$L-F_Cu.svg" "mask:$L-F_Mask.svg" "$L-F_Paste.svg" "$L-F_Silkscreen.svg" "$OUT/holes.svg" "$N-F.Cu.svg" \
  --panel "B.Cu + B.Paste + B.Mask + B.SilkS, mirrored" --mirror "$L-B_Cu.svg" "mask:$L-B_Mask.svg" "$L-B_Paste.svg" "$L-B_Silkscreen.svg" "$OUT/holes.svg" "flat:$N-B.Cu-mirror.svg" \
  --panel "F.Cu + B.Cu + F.SilkS + B.SilkS + F.CrtYd + B.CrtYd" --bare \
    "$L-B_Cu.svg@#4D7FC4" "$L-F_Cu.svg@#C83434,0.8" \
    "$L-B_Silkscreen.svg" "$L-F_Silkscreen.svg" \
    "$L-B_Courtyard.svg@#26E9FF" "$L-F_Courtyard.svg@#FF26E2" \
    "$OUT/holes.svg" "$N-B.Cu.svg" "$N-F.Cu.svg" \
  --panel "B.Cu + F.Cu + B.SilkS + F.SilkS + B.CrtYd + F.CrtYd" --bare \
    "$L-F_Cu.svg@#C83434" "$L-B_Cu.svg@#4D7FC4,0.8" \
    "$L-F_Silkscreen.svg" "$L-B_Silkscreen.svg" \
    "$L-F_Courtyard.svg@#FF26E2" "$L-B_Courtyard.svg@#26E9FF" \
    "$OUT/holes.svg" "$N-F.Cu.svg" "$N-B.Cu.svg"

# schematic: kicad-cli names the file after the project, which collides with
# the midi8knob.svg/ directory, so export to the temp dir and move
kicad-cli sch export svg --exclude-drawing-sheet -o "$TMP/sch" midi8knob.kicad_sch > /dev/null
mv "$TMP/sch/midi8knob.svg" "$L-schematic.svg"

# PNGs: views at 30 px/mm, overview at 16 px/mm, schematic at 20 px/mm
mm_to_px() { python3 -c "import re,sys; vb=re.search(r'viewBox=\"([^\"]+)\"', open(sys.argv[1]).read()).group(1); print(int(float(vb.split()[2]) * float(sys.argv[2])))" "$1" "$2"; }
for v in "$OUT"/views/*.svg; do
  rsvg-convert -w "$(mm_to_px "$v" 30)" "$v" -o "$OUT/png/$(basename "${v%.svg}").png"
done
rsvg-convert -w "$(mm_to_px "$L-layers.svg" 16)" "$L-layers.svg" -o "$OUT/png/midi8knob-layers.png"
rsvg-convert -w "$(mm_to_px "$L-schematic.svg" 20)" "$L-schematic.svg" -o "$OUT/png/midi8knob-schematic.png"
ls -la "$OUT/png"
