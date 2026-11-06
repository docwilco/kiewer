# Curves are refined in a region around the view, over meshes kept from load

The viewer flattened every curve once, at load, to within 1 µm, so vias, round pads, track caps and arcs went blocky from about 100× fit. Curves are now re-tessellated as the camera zooms, at half a device pixel taken at the top of the scale's power-of-four step, never coarser than 1 µm, and only inside a region: the view plus one view on every side. Every item's mesh, and every zone fill piece's, is built at 1 µm once at load and kept; each is marked at load as curved or not from what its parsed geometry holds. A region rebuild re-tessellates only the curved items and fill pieces that touch the region, flattening each curve fine over the part of its sweep inside the region and at 1 µm over the rest, then stitches every mesh back into the scene's one vertex buffer and one index buffer. The net highlight takes its meshes from the same cache. The camera stops at 1e5 px/mm, where f32 positions still hold. Decided in triage on 2026-11-06 for #51; measured on the hand-off board in Chromium at 4 to 8 ms per rebuild up to 10 000 px/mm and 23 ms at the ceiling, against 79 ms for rebuilding the whole board.

## Considered options

- Re-tessellate the whole board at the fine tolerance: 504 ms and 66 MiB at 10 nm, seconds and hundreds of MiB below that.
- Rebuild the whole scene with two tolerances, fine in the region and 1 µm elsewhere: correct, but 79 ms of every rebuild redoes geometry that did not change.
- Draw vias, round pads and round caps as signed-distance circles in the shader: exact at any zoom for those, but an arc inside a polygon (board edges, zone outlines, `gr_poly`) can only be drawn as a polygon, so re-tessellation is needed anyway, and a second primitive kind per layer has to fit #56's one depth per layer.
- A finer fixed tolerance: three times the vertices per tenfold, blocky again further in.

## Consequences

The scene keeps per-item meshes beside its buffers, so memory holds the 1 µm geometry twice while a region is in force. A zone fill of plain points is never re-tessellated; its clearances stay at the chords KiCad wrote (its `max_error`). A fill from a KiCad that writes arcs is picked up piece by piece without a code change, and a large curved piece costs a whole re-triangulation (about 10 ms for the hand-off's ground pour) until it is measured to need tiling. The hit test keeps its own fixed tolerance; deep zoom there is its own issue.
