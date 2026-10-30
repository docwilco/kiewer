# midi8knob board viewer: hand-off

Goal: a website, in the spirit of KiCanvas, that shows this KiCad board as
the eight views below plus the schematic, with pan and zoom, layer toggles,
and labels that become readable as you zoom in. Everything in this folder is
the current static pipeline that produces those views; the site replaces the
static output, not the conventions.

Owner: DocWilco. Board: 8 rotary encoders with WS2812B LEDs, STM32G431KBT6,
USB-C, 4 copper layers.

## What is here

| Path | What |
|---|---|
| `board/midi8knob.kicad_pcb` | The board, KiCad 10 format, with the "Rescue" items already stripped (see gotchas). |
| `board/midi8knob.kicad_sch`, `board/midi8knob.kicad_pro` | Schematic and project. |
| `tools/` | The pipeline. `make-svgs.sh` runs everything; read it first. |
| `exports/midi8knob-<layer>.svg` | One kicad-cli export per layer, board-area page, no drawing sheet. |
| `exports/labels-<layer>.svg`, `labels-<layer>-mirror.svg` | Visible pad, via and track labels per copper layer, from `tools/net_labels.py`. |
| `exports/holes.svg` | Drill holes with copper rims on plated ones, from `tools/holes.py`. |
| `exports/views/NN-*.svg`, `exports/midi8knob-layers.svg` | The eight composed views and the tiled overview, from `tools/compose_svg.py`. |
| `exports/png/` | PNG of every view, the overview, and the schematic. The reference rendering: match these. |
| `exports/midi8knob-schematic.svg` | `kicad-cli sch export svg`, one sheet, A4 landscape. |

## The eight views

Layers stack bottom to top. "board" is the brown FR4 fill from the Edge.Cuts
outline; "bare" is outline only. "labels" are the per-layer label files,
drawn last. "holes" is `holes.svg`.

| # | Header | Stack |
|---|---|---|
| 1 | F.Cu + F.Paste | bare, F.Cu `#C83434`, F.Paste silver, labels F |
| 2 | In1.Cu | bare, In1.Cu `#7FC87F`, labels In1 |
| 3 | In2.Cu | bare, In2.Cu `#CE7D2C`, labels In2 |
| 4 | B.Cu | bare, B.Cu `#4D7FC4`, labels B |
| 5 | F.Cu + F.Paste + F.Mask + F.SilkS | board, F.Cu copper, mask overlay with F.Mask openings, F.Paste, F.SilkS, holes, labels F |
| 6 | B.Cu + B.Paste + B.Mask + B.SilkS, mirrored | as 5 with B layers, whole panel flipped left-right, labels from the `-mirror` file drawn unflipped |
| 7 | F.Cu + B.Cu + F.SilkS + B.SilkS + F.CrtYd + B.CrtYd | bare, B.Cu `#4D7FC4`, F.Cu `#C83434` at opacity 0.8, B.SilkS, F.SilkS, B.CrtYd `#26E9FF`, F.CrtYd `#FF26E2`, holes, labels B then F |
| 8 | B.Cu + F.Cu + ... | as 7 with F.Cu underneath and B.Cu on top at 0.8, labels F then B |

Colours (`tools/compose_svg.py`, duplicated in `tools/holes.py`): page
`#2b2b2b`, board `#7a5a32`, copper `#f0c030`, paste `#c9c9c9`, mask
`#1f8a3f` at opacity 0.7, outline `#D0D2CD`. Views 1 to 4 and 7, 8 use the
KiCad default theme colours instead of copper yellow.

## How a view is composed

- **Coordinates.** Every export shares one viewBox, `0 0 111.9886 57.9882`
  mm, origin at the Edge.Cuts bounding box (kicad-cli `--page-size-mode 2`).
  Board coordinates map to it by subtracting the Edge.Cuts minimum, which
  `tools/kicad_board.py` computes. Labels and holes are generated straight
  from the board file in that frame.
- **Board fill.** The Edge.Cuts export is stroked segments and arcs;
  `outline_path()` chains them into one closed path.
- **Mask overlay.** The mask layer in KiCad is the openings. The overlay is
  the board path in mask colour with an SVG luminance `<mask>` that is white
  everywhere and black where the F.Mask/B.Mask export has shapes.
- **Holes.** kicad-cli draws drill marks white on copper layers and black on
  every other layer; the composer recolours both to the page background.
  `holes.svg` redraws them with a copper rim for plated holes, from pad
  type (`thru_hole` plated, `np_thru_hole` bare) and via drill.
- **Labels.** Pad number and net inside the pad, net on vias and along
  track segments. Font size fits the item (monospace, 0.6 em per glyph),
  labels under 0.08 mm are dropped. Angle follows the pad or track long
  axis, normalised to (-90, 90]; round and square pads stay upright.
- **Mirror.** `<g transform="translate(w 0) scale(-1 1)">` around the
  layers; label files for mirrored views have x flipped and angles negated
  so the text reads normally.
- **Zoom.** The composed SVGs declare width and height 10x the viewBox
  because browsers cap zoom relative to the natural size. A site with its
  own pan/zoom drops that.

## kicad-cli SVG conventions worth knowing

- Shapes carry colour in `style="fill:#RRGGBB; ..."` strings, theme colours
  per layer. Recolouring is a string replace on those.
- Oval pads and oval mask openings are round-capped stroked `<path>` lines,
  not filled shapes. Anything that treats "fill" as the only colour misses
  them.
- Text on the silkscreen is already paths; the exports need no font. The
  label files use `font-family="Consolas, monospace"` as live `<text>`.
- `--subtract-soldermask` was on for the silkscreen export.

## Gotchas

- **Rescue layer.** The JLCPCB-converted footprints (J1 USB-C, SW5, SW10)
  carry body art on an undefined "Rescue" layer. The KiCad GUI tolerates it;
  kicad-cli refuses the board. `tools/strip_rescue.py` writes a cleaned copy,
  and `board/midi8knob.kicad_pcb` here is that copy.
- **Fonts.** Silkscreen uses Consolas with knockout. Regenerating the
  exports with kicad-cli needs the font installed (`~/.fonts/consola.ttf`,
  `fc-cache -f`); otherwise it substitutes DejaVu Sans Mono and the labels
  shift. Not in this folder; proprietary.
- **File names.** `kicad-cli sch export svg` names its output after the
  project, `midi8knob.svg`, which collides with the `midi8knob.svg/` output
  directory in the repo. The pipeline exports to a temp dir and moves it.
- **Line endings.** Board and schematic files are CRLF. Nets are written as
  `(net "name")` without numbers in this format version.
- **Dependencies** to rerun the pipeline: kicad-cli 10.0.8 (Ubuntu PPA
  `ppa:kicad/kicad-10.0-releases`), `rsvg-convert` from `librsvg2-bin`,
  Python 3, the font above.

## Open

- Site stack is undecided. KiCanvas itself parses `.kicad_pcb` and renders
  in WebGL; this pipeline instead pre-renders with kicad-cli. Either route
  must reproduce the eight stacks and the label and hole rules above.
- Whether the site reads the board file at runtime or ships the exports.
- Hover or click on a net to highlight it is wanted but not built; the
  label files and `tools/kicad_board.py` give net names per item.
