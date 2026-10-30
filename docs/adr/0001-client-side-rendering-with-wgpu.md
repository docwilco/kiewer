# Client-side rendering with wgpu at the WebGL2 floor

The hand-off pipeline pre-rendered the board with kicad-cli into SVGs; the alternative was a renderer in the browser, as KiCanvas does. We render in the browser: `kicad-core` parses `.kicad_pcb` and `.kicad_sch` and tessellates geometry in wasm at load, and `viewer` draws it with wgpu. The server never runs kicad-cli, so uploads need no KiCad install, no proprietary fonts, and no SVG post-processing, and the renderer knows the net of every shape, which makes net highlight and sane labels possible.

wgpu is coded once to the WebGL2 floor (`Limits::downlevel_webgl2_defaults()`: no compute, no storage buffers, instanced vertex buffers and data textures instead) and uses WebGPU when the browser has it. One shader set, one code path; the same code renders natively for thumbnails and golden-image tests. A beacon per page load reports which backend ran so the fallback can be dropped when nobody uses it.

Text that names no face renders in Newstroke. The glyph table is regenerated from KiCad's CC0 `tools/newstroke/*.kicad_sym` sources with our own converter, because KiCad's generated `newstroke_font.cpp` carries GPL and OFL terms the glyph data itself does not. Credit Vladimir Uryvaev and KiCad in the about page.

## Considered options

- Pre-rendered SVG per view, served statically: no net information per shape, labels baked at one size, kicad-cli and Consolas on the server.
- Raw WebGL2 through web-sys: smaller wasm, no native path for thumbnails or tests.
- wgpu with a WebGPU-only compute path and a separate fallback: two shader sets to test for no visible gain on static geometry.
