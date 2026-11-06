//! Triangles from a board: what the viewer uploads to the GPU and the
//! thumbnail renderer will draw. One copper layer at a time (tracks, track
//! arcs, vias, pads and stored zone fills, holes left open; a zone without
//! a stored fill as its outline hatched), the holes drilled through that
//! layer (drawn last, in the page colour, so a track ending in a via does
//! not fill the hole back in), the outline as the strokes of `Edge.Cuts`
//! and the board as the fill of the loops they chain into, a mask layer as
//! the board less its openings (a polygon boolean, so the overlay is one
//! mesh like any other layer), a paste layer as the pads shrunk by the
//! paste clearance, every hole with a copper rim around the plated ones,
//! the graphics of any other layer (a silkscreen's lines and filled
//! shapes), and the text of a layer in Newstroke ([`crate::font`]), a
//! knockout as its box with the glyphs cut out.
//!
//! Everything is computed in f64 and handed to lyon in f32 at the last
//! step; positions in the [`Mesh`] are millimetres in the file's frame,
//! y down, as the model has them.

use std::cell::Cell;

use i_overlay::core::fill_rule::FillRule as OverlayFill;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::single::SingleFloatOverlay as _;
use lyon_tessellation::geometry_builder::{
    BuffersBuilder, FillGeometryBuilder, FillVertexConstructor, GeometryBuilder,
    GeometryBuilderError, StrokeGeometryBuilder, StrokeVertexConstructor, VertexBuffers,
};
use lyon_tessellation::math::{Point as LyonPoint, point};
use lyon_tessellation::path::Path;
use lyon_tessellation::{
    FillOptions, FillRule, FillTessellator, FillVertex, LineCap, LineJoin, StrokeOptions,
    StrokeTessellator, StrokeVertex, VertexId,
};

pub mod schematic;

use crate::font::{self, Placement, TextShape};
use crate::geometry::{Arc, Bounds, Transform, segments_for};
use crate::model::{
    Board, Chain, ChainItem, Drawing, Drill, EDGE_CUTS, Footprint, LayerId, NetId, Pad, PadKind,
    PadShape, Point, Shape, Size, Text, Via, Zone,
};

/// A polygon as `i_overlay` takes it: contours of `[x, y]` pairs, the first
/// the outer boundary and the rest holes.
type OverlayShape = Vec<Vec<[f64; 2]>>;

/// Triangles: positions in millimetres, indices in threes. A mesh from a
/// [`Tessellator`] holds none of zero area.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    /// Every vertex, `[x, y]` in millimetres, y down.
    pub positions: Vec<[f32; 2]>,
    /// Three indices into [`Mesh::positions`] per triangle.
    pub indices: Vec<u32>,
}

impl Mesh {
    /// No triangles at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// The corners of every triangle, in order.
    pub fn triangles(&self) -> impl Iterator<Item = [Point; 3]> + '_ {
        self.indices
            .as_chunks::<3>()
            .0
            .iter()
            .filter_map(|&[a, b, c]| {
                let corner = |index| corner(&self.positions, index);
                Some([corner(a)?, corner(b)?, corner(c)?])
            })
    }

    /// Add `other`'s triangles after this mesh's own: one mesh for a
    /// layer built in two parts (a silkscreen's graphics and its text).
    pub fn append(&mut self, other: &Mesh) {
        let base = u32::try_from(self.positions.len()).unwrap_or(u32::MAX);
        self.positions.extend_from_slice(&other.positions);
        self.indices
            .extend(other.indices.iter().map(|index| index + base));
    }

    /// The box around every vertex, or `None` for a mesh with no vertices.
    /// A mesh can be empty and still have some: a shape that came to
    /// nothing but zero-area triangles keeps its vertices.
    #[must_use]
    pub fn bounds(&self) -> Option<Bounds> {
        Bounds::around(self.positions.iter().copied().map(vertex_point))
    }

    /// The area the triangles cover, in square millimetres, each triangle
    /// counted once whatever its winding. Overlapping triangles count
    /// twice: a test's measure of a mesh built from shapes that do not
    /// overlap.
    #[must_use]
    pub fn area(&self) -> f64 {
        self.triangles()
            .map(|[a, b, c]| (cross(a, b, c) / 2.0).abs())
            .sum()
    }

    /// Whether any triangle holds `point` (its edges included).
    #[must_use]
    pub fn covers(&self, point: Point) -> bool {
        self.triangles().any(|[a, b, c]| {
            let d1 = cross(a, b, point);
            let d2 = cross(b, c, point);
            let d3 = cross(c, a, point);
            let negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
            let positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
            !(negative && positive)
        })
    }
}

/// A mesh vertex as a point.
fn vertex_point([x, y]: [f32; 2]) -> Point {
    Point {
        x: f64::from(x),
        y: f64::from(y),
    }
}

/// The vertex at `index` as a point, or `None` past the end of `positions`.
fn corner(positions: &[[f32; 2]], index: u32) -> Option<Point> {
    positions
        .get(usize::try_from(index).ok()?)
        .copied()
        .map(vertex_point)
}

/// Twice the signed area of the triangle `a`, `b`, `c`.
fn cross(a: Point, b: Point, c: Point) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// The width an outline stroke of width 0 is drawn at: KiCad's own
/// default for a new edge.
const MIN_OUTLINE_WIDTH: f64 = 0.05;

/// How far apart two outline strokes' ends may sit and still join into
/// one loop: KiCad snaps edges to its grid, and an edge drawn by hand can
/// miss by a hundredth of a millimetre.
pub const JOIN_TOLERANCE: f64 = 0.01;

/// The copper a plated hole shows around its drill, in millimetres: the
/// hand-off's `holes.py` draws the same rim, so barrels show on layers
/// where no pad is drawn.
pub const HOLE_RIM: f64 = 0.1;

/// How far apart, measured across them, the 45-degree lines of an
/// unfilled zone's hatch are, in millimetres.
pub const HATCH_PITCH: f64 = 1.0;
/// How wide each hatch line is, in millimetres; the zone's outline is
/// stroked at the same width.
pub const HATCH_WIDTH: f64 = 0.1;

/// Turns board items into a [`Mesh`] at one tolerance: how far a chord
/// may sit from the curve it flattens, in millimetres. A tessellator can
/// be [`refined`](Tessellator::refined) over a region, where the curves
/// are flattened finer (#51, ADR 0006).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tessellator {
    tolerance: f64,
    fine: Option<Fine>,
}

/// A finer tolerance over a region of the board, in millimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Fine {
    tolerance: f64,
    region: Bounds,
}

/// How curves are flattened: to `tolerance`, and to the finer tolerance
/// of `fine` over the part of a curve inside its region. A curve across
/// the region's edge is one outline, fine on the inside and coarse on the
/// outside, the points of both on the curve itself. `touched` notes that
/// a curve was flattened, which is what marks an item curved
/// ([`Unit::curved`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Curves {
    pub(crate) tolerance: f64,
    fine: Option<Fine>,
    touched: Cell<bool>,
}

impl Curves {
    /// Flatten everything to `tolerance`.
    pub(crate) fn at(tolerance: f64) -> Self {
        Self {
            tolerance,
            fine: None,
            touched: Cell::new(false),
        }
    }

    pub(crate) fn of(tessellator: &Tessellator) -> Self {
        Self {
            tolerance: tessellator.tolerance,
            fine: tessellator.fine,
            touched: Cell::new(false),
        }
    }

    /// The fine setting when the region reaches `bounds`, and a note that
    /// a curve was flattened.
    fn refine(&self, bounds: &Bounds) -> Option<Fine> {
        self.touched.set(true);
        self.fine.filter(|fine| touches(&fine.region, bounds))
    }

    /// Whether a curve was flattened since the last call.
    fn take_touched(&self) -> bool {
        self.touched.replace(false)
    }

    /// The parameters in 0 to 1 at which a curve is sampled: the ends of
    /// `coarse` equal steps where `inside` is false and of `fine` equal
    /// steps where it is true, in order. `closed` leaves out the end,
    /// which is the start again. Both sets hold the ends, and the same
    /// position tests the same, so one of them keeps each.
    fn sampled(coarse: usize, fine: usize, closed: bool, inside: impl Fn(f64) -> bool) -> Vec<f64> {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a segment count is small; well under 2^53"
        )]
        let at = |index: usize, count: usize| index as f64 / count as f64;
        let last = |count: usize| if closed { count - 1 } else { count };
        let mut out = Vec::with_capacity(fine + 1);
        let (mut next_coarse, mut next_fine) = (0, 0);
        loop {
            let from_coarse = (next_coarse <= last(coarse)).then(|| at(next_coarse, coarse));
            let from_fine = (next_fine <= last(fine)).then(|| at(next_fine, fine));
            match (from_coarse, from_fine) {
                (None, None) => break,
                (Some(param), Some(other)) if param <= other => {
                    if !inside(param) {
                        out.push(param);
                    }
                    next_coarse += 1;
                }
                (Some(param), None) => {
                    if !inside(param) {
                        out.push(param);
                    }
                    next_coarse += 1;
                }
                (_, Some(param)) => {
                    if inside(param) {
                        out.push(param);
                    }
                    next_fine += 1;
                }
            }
        }
        out
    }
}

/// Whether two boxes share a point.
pub(crate) fn touches(a: &Bounds, b: &Bounds) -> bool {
    a.min.x <= b.max.x && a.max.x >= b.min.x && a.min.y <= b.max.y && a.max.y >= b.min.y
}

/// Whether `bounds` holds `point`.
fn holds(bounds: &Bounds, point: Point) -> bool {
    (bounds.min.x..=bounds.max.x).contains(&point.x)
        && (bounds.min.y..=bounds.max.y).contains(&point.y)
}

/// The part of a board that one run of the scene is built from: what a
/// layer's slice of triangles holds. A part is a list of [`Unit`]s, one
/// per item (or per zone fill piece), in a fixed order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    /// The board itself, the fill of the outline's loops.
    BoardFill,
    /// The strokes of `Edge.Cuts`.
    Outline,
    /// The copper rim of every plated hole.
    Rims,
    /// Every hole drilled through the board.
    AllHoles,
    /// The holes drilled through a layer.
    Holes(LayerId),
    /// The copper of a layer other than its zones.
    CopperRest(LayerId),
    /// The zones of a layer, a unit per fill piece.
    Zones(LayerId),
    /// The solder mask overlay of a mask layer.
    Mask(LayerId),
    /// The solder paste of a paste layer.
    Paste(LayerId),
    /// The graphics of a layer.
    Graphics(LayerId),
    /// The text of a layer.
    Text(LayerId),
}

/// The mesh of one item, kept so that a region of finer tessellation
/// re-tessellates only the items that need it (#51, ADR 0006).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Unit {
    /// The triangles, at the floor tolerance or, inside a refined region,
    /// finer.
    pub mesh: Mesh,
    /// The box around the mesh, grown by the floor tolerance; `None`
    /// for an item that came to nothing.
    pub bounds: Option<Bounds>,
    /// Whether the item holds a curve: a circle, an arc, a Bezier, a
    /// round cap or corner. Only a curved item changes with the
    /// tolerance.
    pub curved: bool,
    /// Whether the mesh was built inside a refined region: it has to be
    /// built again when the region leaves.
    pub fine: bool,
    /// The net the item is copper of, for the highlight.
    pub net: Option<NetId>,
}

impl Unit {
    /// Whether a rebuild with `region` changes this unit's mesh: it is
    /// curved and either reaches the region or was built fine.
    #[must_use]
    pub fn wants(&self, region: Option<&Bounds>) -> bool {
        self.curved
            && (self.fine
                || region
                    .zip(self.bounds.as_ref())
                    .is_some_and(|(region, bounds)| touches(region, bounds)))
    }
}

/// What [`Part::CopperRest`] draws, into `builder`.
fn add_copper_without_zones(builder: &mut MeshBuilder, board: &Board, layer: LayerId) {
    for segment in board.segments.iter().filter(|s| s.layer == layer) {
        builder.unit(segment.net, |builder| {
            builder.stroke(&[segment.start, segment.end], segment.width, false);
        });
    }
    for arc in board.arcs.iter().filter(|a| a.layer == layer) {
        builder.unit(arc.net, |builder| {
            let points = arc_points(arc.start, arc.mid, arc.end, &builder.curves);
            builder.stroke(&points, arc.width, false);
        });
    }
    let copper: Vec<LayerId> = board.copper_layers().map(|(id, _)| id).collect();
    for via in board.vias.iter().filter(|via| spans(via, &copper, layer)) {
        builder.unit(via.net, |builder| builder.via(via));
    }
    for footprint in &board.footprints {
        let frame = Transform::new(footprint.at, footprint.rotation);
        for pad in footprint
            .pads
            .iter()
            .filter(|pad| pad.layers.contains(&layer))
        {
            builder.unit(pad.net, |builder| builder.pad(pad, &frame));
        }
    }
}

/// What [`Part::Zones`] draws, into `builder`.
fn add_zones(builder: &mut MeshBuilder, board: &Board, layer: LayerId) {
    for footprint in &board.footprints {
        let frame = Transform::new(footprint.at, footprint.rotation);
        for zone in &footprint.zones {
            builder.zone(zone, &frame, layer);
        }
    }
    for zone in &board.zones {
        builder.zone(zone, &Transform::identity(), layer);
    }
}

impl Tessellator {
    /// A tessellator flattening curves to within `tolerance` millimetres.
    #[must_use]
    pub fn new(tolerance: f64) -> Self {
        Self {
            tolerance,
            fine: None,
        }
    }

    /// This tessellator, flattening the part of a curve inside `region`
    /// (millimetres, the file's frame) to the finer `tolerance` and the
    /// rest to its own. A curve across the region's edge is one outline.
    #[must_use]
    pub fn refined(self, tolerance: f64, region: Bounds) -> Self {
        Self {
            fine: Some(Fine { tolerance, region }),
            ..self
        }
    }

    /// The units of `part` of `board`, in order. Without `prior`, every
    /// unit is built. With the units a previous call built, only those
    /// that [`Unit::wants`] this tessellator's region are built again and
    /// the rest come back `None`: the caller keeps its own.
    #[must_use]
    pub fn units(&self, board: &Board, part: Part, prior: Option<&[Unit]>) -> Vec<Option<Unit>> {
        let mut builder = MeshBuilder::new(self).with_prior(prior);
        match part {
            Part::BoardFill => builder.unit(None, |builder| {
                if let Some((outer, cutouts)) = outline_loops(board, &builder.curves) {
                    builder.fill(&[outer], &cutouts);
                }
            }),
            Part::Outline => add_outline(&mut builder, board),
            Part::Rims => add_rims(&mut builder, board),
            Part::AllHoles => add_all_holes(&mut builder, board),
            Part::Holes(layer) => add_holes(&mut builder, board, layer),
            Part::CopperRest(layer) => add_copper_without_zones(&mut builder, board, layer),
            Part::Zones(layer) => add_zones(&mut builder, board, layer),
            Part::Mask(layer) => builder.unit(None, |builder| add_mask(builder, board, layer)),
            Part::Paste(layer) => add_paste(&mut builder, board, layer),
            Part::Graphics(layer) => add_graphics(&mut builder, board, layer),
            Part::Text(layer) => add_text(&mut builder, board, layer),
        }
        builder.finish_units()
    }

    /// The mesh of `part`, every unit at this tessellator's tolerances.
    fn mesh(&self, board: &Board, part: Part) -> Mesh {
        let mut mesh = Mesh::default();
        for unit in self.units(board, part, None).into_iter().flatten() {
            mesh.append(&unit.mesh);
        }
        mesh
    }

    /// The copper on `layer`: [`Tessellator::copper_without_zones`] and
    /// [`Tessellator::zones`] as one mesh.
    #[must_use]
    pub fn copper(&self, board: &Board, layer: LayerId) -> Mesh {
        let mut mesh = self.mesh(board, Part::CopperRest(layer));
        mesh.append(&self.mesh(board, Part::Zones(layer)));
        mesh
    }

    /// The copper on `layer` other than zones: tracks and track arcs with
    /// round ends, vias and pads that reach the layer as rings around
    /// their holes. An unplated hole whose pad is no bigger than the hole
    /// is no copper. A net highlight draws this and a net's zones at
    /// different strengths.
    #[must_use]
    pub fn copper_without_zones(&self, board: &Board, layer: LayerId) -> Mesh {
        self.mesh(board, Part::CopperRest(layer))
    }

    /// The zones on `layer`: the fills KiCad stored, and the outline of a
    /// zone without a stored fill on the layer, hatched ([`HATCH_PITCH`]).
    #[must_use]
    pub fn zones(&self, board: &Board, layer: LayerId) -> Mesh {
        self.mesh(board, Part::Zones(layer))
    }

    /// The board itself: the `Edge.Cuts` strokes chained into loops
    /// ([`JOIN_TOLERANCE`]), the largest filled and the others cut out of
    /// it. Empty for a board whose outline does not close.
    #[must_use]
    pub fn board_fill(&self, board: &Board) -> Mesh {
        self.mesh(board, Part::BoardFill)
    }

    /// The solder mask overlay of a mask `layer`: the board
    /// ([`Tessellator::board_fill`]) less every opening on the layer, as
    /// one polygon difference. An opening is a pad that reaches the layer,
    /// grown by its mask clearance ([`Board::pad_clearances`]; its hole
    /// grown the same, so an unplated hole opens the mask), a via that
    /// is not tented on the layer's side, grown by its mask clearance
    /// ([`Board::via_mask_expansion`]), and any graphic drawn on the
    /// layer. Empty for a board without a closed outline.
    #[must_use]
    pub fn mask(&self, board: &Board, layer: LayerId) -> Mesh {
        self.mesh(board, Part::Mask(layer))
    }

    /// The solder paste on a paste `layer`: every pad that reaches it,
    /// grown (shrunk, as the clearance is normally negative) by its paste
    /// clearance, the hole of a through-hole pad left open.
    #[must_use]
    pub fn paste(&self, board: &Board, layer: LayerId) -> Mesh {
        self.mesh(board, Part::Paste(layer))
    }

    /// Every hole drilled through the board, whatever layers it reaches:
    /// vias of every kind and every drilled pad, plated or not, as filled
    /// discs and slots. Drawn in the page colour over the whole stack,
    /// under the rims.
    #[must_use]
    pub fn all_holes(&self, board: &Board) -> Mesh {
        self.mesh(board, Part::AllHoles)
    }

    /// The copper rim of every plated hole: a band [`HOLE_RIM`] wide
    /// around the drill of every via and every through-hole pad. An
    /// unplated hole has none.
    #[must_use]
    pub fn rims(&self, board: &Board) -> Mesh {
        self.mesh(board, Part::Rims)
    }

    /// The holes drilled through `layer`: every via that reaches it and
    /// every drilled pad on it, plated or not, as filled discs and slots.
    /// Drawn after the copper in the page colour: a track or a zone that
    /// runs into a hole is copper right up to the drill, and the drill is
    /// still open.
    #[must_use]
    pub fn holes(&self, board: &Board, layer: LayerId) -> Mesh {
        self.mesh(board, Part::Holes(layer))
    }

    /// The board outline: every `Edge.Cuts` graphic, the board's and the
    /// footprints' (a connector's slot, in the footprint's frame), stroked
    /// at its width (at least 0.05 mm, KiCad's default edge width) with
    /// round ends.
    #[must_use]
    pub fn outline(&self, board: &Board) -> Mesh {
        self.mesh(board, Part::Outline)
    }

    /// The box around the outline's geometry (the centre lines, not their
    /// strokes), or `None` for a board without `Edge.Cuts` graphics.
    #[must_use]
    pub fn outline_bounds(&self, board: &Board) -> Option<Bounds> {
        let edge = board.layer_named(EDGE_CUTS)?;
        layer_drawings(board, edge)
            .filter_map(|drawing| graphic_bounds(&drawing, self.tolerance))
            .reduce(Bounds::union)
    }

    /// The graphics on `layer`, the board's and the footprints' (in their
    /// frames): a filled closed shape as its fill, everything else as a
    /// stroke of its width (at least 0.05 mm) with round ends.
    #[must_use]
    pub fn graphics(&self, board: &Board, layer: LayerId) -> Mesh {
        self.mesh(board, Part::Graphics(layer))
    }

    /// `polylines` stroked at `width` with round ends and joins, in
    /// whatever unit they come in: a glyph's strokes in em units for the
    /// viewer's net labels, which scale each glyph on the GPU.
    #[must_use]
    pub fn polylines(&self, polylines: &[Vec<Point>], width: f64) -> Mesh {
        let mut builder = MeshBuilder::new(self);
        for points in polylines {
            builder.stroke(points, width, false);
        }
        builder.finish()
    }

    /// The text on `layer` in Newstroke: every visible board text and
    /// footprint text (its variables resolved, placed in the footprint's
    /// frame, kept upright when the footprint asks), stroked at its pen
    /// width with round ends. A knockout text is its box with the glyphs
    /// cut out.
    #[must_use]
    pub fn text(&self, board: &Board, layer: LayerId) -> Mesh {
        self.mesh(board, Part::Text(layer))
    }
}

/// What [`Part::Mask`] draws, into `builder`.
fn add_mask(builder: &mut MeshBuilder, board: &Board, layer: LayerId) {
    let Some((outer, cutouts)) = outline_loops(board, &builder.curves) else {
        return;
    };
    let mut openings: Vec<Vec<Point>> = Vec::new();
    for (footprint, pad) in pads_on(board, layer) {
        let shape = pad_frame(pad, &Transform::new(footprint.at, footprint.rotation));
        let grow = board.pad_clearances(footprint, pad).mask_expansion();
        openings.push(builder.pad_outline(pad, &shape, grow));
        if let Some(drill) = pad.drill {
            openings.push(builder.hole(drill, &shape, grow));
        }
        openings.extend(builder.primitive_polygons(pad, &shape));
    }
    for via in &board.vias {
        if let Some(grow) = board.via_mask_expansion(via, layer) {
            openings.push(circle(via.at, via.size / 2.0 + grow, &builder.curves));
        }
    }
    for drawing in layer_drawings(board, layer) {
        openings.extend(drawing_polygons(&drawing, &builder.curves));
    }
    let mut subject: OverlayShape = vec![wound(&outer, true)];
    subject.extend(cutouts.iter().map(|cutout| wound(cutout, false)));
    let clip: OverlayShape = openings
        .iter()
        .map(|polygon| wound(polygon, true))
        .collect();
    let shapes = subject.overlay(&clip, OverlayRule::Difference, OverlayFill::NonZero);
    builder.fill_shapes(shapes);
}

/// What [`Part::Paste`] draws, into `builder`.
fn add_paste(builder: &mut MeshBuilder, board: &Board, layer: LayerId) {
    for (footprint, pad) in pads_on(board, layer) {
        builder.unit(pad.net, |builder| {
            let shape = pad_frame(pad, &Transform::new(footprint.at, footprint.rotation));
            let grow = board
                .pad_clearances(footprint, pad)
                .paste_expansion(pad.size);
            let mut positives = vec![builder.pad_outline(pad, &shape, grow)];
            positives.extend(builder.primitive_polygons(pad, &shape));
            let holes = match pad.drill {
                Some(drill) => vec![builder.hole(drill, &shape, 0.0)],
                None => Vec::new(),
            };
            builder.fill(&positives, &holes);
        });
    }
}

/// What [`Part::AllHoles`] draws, into `builder`.
fn add_all_holes(builder: &mut MeshBuilder, board: &Board) {
    for via in &board.vias {
        builder.unit(None, |builder| {
            let hole = circle(via.at, via.drill / 2.0, &builder.curves);
            builder.fill(&[hole], &[]);
        });
    }
    for footprint in &board.footprints {
        let frame = Transform::new(footprint.at, footprint.rotation);
        for pad in &footprint.pads {
            if let Some(drill) = pad.drill {
                builder.unit(None, |builder| {
                    let hole = builder.hole(drill, &pad_frame(pad, &frame), 0.0);
                    builder.fill(&[hole], &[]);
                });
            }
        }
    }
}

/// What [`Part::Rims`] draws, into `builder`.
fn add_rims(builder: &mut MeshBuilder, board: &Board) {
    for via in &board.vias {
        builder.unit(None, |builder| {
            let ring = circle(via.at, via.drill / 2.0 + HOLE_RIM, &builder.curves);
            let hole = circle(via.at, via.drill / 2.0, &builder.curves);
            builder.fill(&[ring], &[hole]);
        });
    }
    for footprint in &board.footprints {
        let frame = Transform::new(footprint.at, footprint.rotation);
        for pad in footprint
            .pads
            .iter()
            .filter(|pad| pad.kind == PadKind::ThroughHole)
        {
            if let Some(drill) = pad.drill {
                builder.unit(None, |builder| {
                    let shape = pad_frame(pad, &frame);
                    let ring = builder.hole(drill, &shape, HOLE_RIM);
                    let hole = builder.hole(drill, &shape, 0.0);
                    builder.fill(&[ring], &[hole]);
                });
            }
        }
    }
}

/// What [`Part::Holes`] draws, into `builder`.
fn add_holes(builder: &mut MeshBuilder, board: &Board, layer: LayerId) {
    let copper: Vec<LayerId> = board.copper_layers().map(|(id, _)| id).collect();
    for via in board.vias.iter().filter(|via| spans(via, &copper, layer)) {
        builder.unit(via.net, |builder| {
            let hole = circle(via.at, via.drill / 2.0, &builder.curves);
            builder.fill(&[hole], &[]);
        });
    }
    for footprint in &board.footprints {
        let frame = Transform::new(footprint.at, footprint.rotation);
        for pad in footprint
            .pads
            .iter()
            .filter(|pad| pad.layers.contains(&layer))
        {
            if let Some(drill) = pad.drill {
                builder.unit(pad.net, |builder| {
                    let hole = builder.hole(drill, &pad_frame(pad, &frame), 0.0);
                    builder.fill(&[hole], &[]);
                });
            }
        }
    }
}

/// What [`Part::Outline`] draws, into `builder`.
fn add_outline(builder: &mut MeshBuilder, board: &Board) {
    let Some(edge) = board.layer_named(EDGE_CUTS) else {
        return;
    };
    for drawing in layer_drawings(board, edge) {
        builder.unit(None, |builder| {
            let width = drawing.width.max(MIN_OUTLINE_WIDTH);
            let (points, closed) = outline_points(&drawing, &builder.curves);
            builder.stroke(&points, width, closed);
        });
    }
}

/// What [`Part::Graphics`] draws, into `builder`.
fn add_graphics(builder: &mut MeshBuilder, board: &Board, layer: LayerId) {
    for drawing in layer_drawings(board, layer) {
        builder.unit(None, |builder| {
            let (points, closed) = outline_points(&drawing, &builder.curves);
            if drawing.filled && closed {
                builder.fill(&[points], &[]);
            } else {
                let width = drawing.width.max(MIN_OUTLINE_WIDTH);
                builder.stroke(&points, width, closed);
            }
        });
    }
}

/// What [`Part::Text`] draws, into `builder`.
fn add_text(builder: &mut MeshBuilder, board: &Board, layer: LayerId) {
    for text in board.texts.iter().filter(|t| t.layer == layer) {
        builder.unit(None, |builder| {
            builder.text(&text.text, text, &Transform::identity(), false);
        });
    }
    for footprint in &board.footprints {
        let frame = Transform::new(footprint.at, footprint.rotation);
        for text in footprint.texts.iter().filter(|t| t.layer == layer) {
            builder.unit(None, |builder| {
                builder.text(&footprint.resolve_text(text), text, &frame, true);
            });
        }
    }
}

/// Every pad that reaches `layer`, with its footprint.
fn pads_on(board: &Board, layer: LayerId) -> impl Iterator<Item = (&Footprint, &Pad)> {
    board.footprints.iter().flat_map(move |footprint| {
        footprint
            .pads
            .iter()
            .filter(move |pad| pad.layers.contains(&layer))
            .map(move |pad| (footprint, pad))
    })
}

/// The `Edge.Cuts` strokes chained into closed loops: the outer one (the
/// largest by area) and the cutouts inside it, or `None` when no loop
/// closes. A closed shape (a circle, a rectangle, a polygon) is a loop on
/// its own; open strokes join end to end within [`JOIN_TOLERANCE`], and a
/// chain that never meets its start is dropped.
fn outline_loops(board: &Board, curves: &Curves) -> Option<(Vec<Point>, Vec<Vec<Point>>)> {
    let edge = board.layer_named(EDGE_CUTS)?;
    let mut loops: Vec<Vec<Point>> = Vec::new();
    let mut open: Vec<Vec<Point>> = Vec::new();
    for drawing in layer_drawings(board, edge) {
        let (points, closed) = outline_points(&drawing, curves);
        if points.len() < 2 {
            continue;
        }
        if closed {
            loops.push(points);
        } else {
            open.push(points);
        }
    }
    let near = |a: Option<&Point>, b: Point| {
        a.is_some_and(|a| {
            (a.x - b.x).abs() <= JOIN_TOLERANCE && (a.y - b.y).abs() <= JOIN_TOLERANCE
        })
    };
    while let Some(mut chain) = open.pop() {
        while let Some(&end) = chain.last() {
            if chain.len() > 2 && near(chain.first(), end) {
                chain.pop();
                loops.push(chain);
                break;
            }
            let Some(index) = open
                .iter()
                .position(|next| near(next.first(), end) || near(next.last(), end))
            else {
                // A chain that closes on nothing: not an outline.
                break;
            };
            let mut next = open.swap_remove(index);
            if !near(next.first(), end) {
                next.reverse();
            }
            chain.extend(next.into_iter().skip(1));
        }
    }
    let outer = loops
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| signed_area(a).abs().total_cmp(&signed_area(b).abs()))
        .map(|(index, _)| index)?;
    let outer = loops.swap_remove(outer);
    Some((outer, loops))
}

/// `polygon` as `i_overlay` takes it, wound counter-clockwise in the file's
/// frame when `positive` and clockwise otherwise, so a positive and a
/// hole cancel under the non-zero rule.
fn wound(polygon: &[Point], positive: bool) -> Vec<[f64; 2]> {
    let mut points: Vec<[f64; 2]> = polygon.iter().map(|p| [p.x, p.y]).collect();
    if (signed_area(polygon) > 0.0) != positive {
        points.reverse();
    }
    points
}

/// The polygons a drawing covers, for a boolean: a filled closed shape is
/// its outline plus its stroke when it has a width (KiCad draws both),
/// anything else the stadium of each stroked segment at the drawing's
/// width (at least [`MIN_OUTLINE_WIDTH`]).
fn drawing_polygons(drawing: &Drawing, curves: &Curves) -> Vec<Vec<Point>> {
    let (points, closed) = outline_points(drawing, curves);
    let filled = drawing.filled && closed;
    if filled && drawing.width <= 0.0 {
        return vec![points];
    }
    let width = drawing.width.max(MIN_OUTLINE_WIDTH);
    let mut polygons: Vec<Vec<Point>> = filled.then(|| points.clone()).into_iter().collect();
    polygons.extend(points.windows(2).filter_map(|pair| match pair {
        [a, b] => Some(stadium(*a, *b, width, curves)),
        _ => None,
    }));
    if closed && let (Some(&last), Some(&first)) = (points.last(), points.first()) {
        polygons.push(stadium(last, first, width, curves));
    }
    polygons
}

/// `size` grown by `grow` on every side, and `radius` with it: the one
/// rule a pad's outline, its mask opening, its paste and its hole's rim
/// share. KiCad rounds a grown corner by the growth.
fn grown(size: Size, radius: f64, grow: f64) -> (Size, f64) {
    (
        Size {
            width: size.width + 2.0 * grow,
            height: size.height + 2.0 * grow,
        },
        radius + grow,
    )
}

/// Every drawing on `layer`: the board's own as written, a footprint's
/// carried into the board's frame.
fn layer_drawings(board: &Board, layer: LayerId) -> impl Iterator<Item = Drawing> + '_ {
    let own = board
        .graphics
        .iter()
        .filter(move |graphic| graphic.layer == layer)
        .map(|graphic| graphic.drawing.clone());
    let placed = board.footprints.iter().flat_map(move |footprint| {
        let frame = Transform::new(footprint.at, footprint.rotation);
        footprint
            .graphics
            .iter()
            .filter(move |graphic| graphic.layer == layer)
            .map(move |graphic| transformed(&graphic.drawing, &frame))
    });
    own.chain(placed)
}

/// `drawing` with its points carried through `frame`. A rectangle turns
/// into the polygon of its corners, since a turned rectangle is no
/// longer axis-aligned.
pub(crate) fn transformed(drawing: &Drawing, frame: &Transform) -> Drawing {
    let f = |p: &Point| frame.apply(*p);
    let shape = match &drawing.shape {
        Shape::Line { start, end } => Shape::Line {
            start: f(start),
            end: f(end),
        },
        Shape::Arc { start, mid, end } => Shape::Arc {
            start: f(start),
            mid: f(mid),
            end: f(end),
        },
        Shape::Circle { center, end } => Shape::Circle {
            center: f(center),
            end: f(end),
        },
        Shape::Rect { start, end } => Shape::Polygon {
            points: Chain::new(
                [
                    *start,
                    Point {
                        x: end.x,
                        y: start.y,
                    },
                    *end,
                    Point {
                        x: start.x,
                        y: end.y,
                    },
                ]
                .iter()
                .map(|p| ChainItem::Point(f(p)))
                .collect(),
            ),
        },
        Shape::Polygon { points } => Shape::Polygon {
            points: points.map(|p| f(&p)),
        },
        Shape::Curve {
            start,
            control1,
            control2,
            end,
        } => Shape::Curve {
            start: f(start),
            control1: f(control1),
            control2: f(control2),
            end: f(end),
        },
    };
    Drawing {
        shape,
        width: drawing.width,
        filled: drawing.filled,
    }
}

/// The outline of `pad`'s shape on the board (in its `shape` frame,
/// [`pad_frame`]), grown by `grow` on every side and flattened to within
/// `tolerance`: the one outline the copper, the mask, the paste and the
/// hit test (`crate::hit`) share. See `MeshBuilder::pad_outline`.
pub(crate) fn pad_outline(pad: &Pad, shape: &Transform, grow: f64, tolerance: f64) -> Vec<Point> {
    pad_outline_in(pad, shape, grow, &Curves::at(tolerance))
}

/// [`pad_outline`] with the curves flattened as `curves` say.
fn pad_outline_in(pad: &Pad, shape: &Transform, grow: f64, curves: &Curves) -> Vec<Point> {
    let min_side = pad.size.width.min(pad.size.height);
    let radius = match pad.shape {
        PadShape::Circle => pad.size.width / 2.0,
        // A custom pad's anchor as well: KiCad's default anchor is a
        // circle and the model does not carry the choice, so the
        // stadium of the (usually square) anchor size stands for it.
        PadShape::Oval | PadShape::Custom => min_side / 2.0,
        // The chamfer sizes and the trapezoid's delta are not in the
        // model; the plain rectangle is the nearest drawing.
        PadShape::Rect | PadShape::Trapezoid | PadShape::ChamferedRect => 0.0,
        PadShape::RoundRect => pad.round_radius_ratio.unwrap_or(0.25) * min_side,
    };
    let (size, radius) = grown(pad.size, radius, grow);
    rounded_rect(size, radius, shape, curves)
        .into_iter()
        .map(|p| shape.apply(p))
        .collect()
}

/// Where `pad` sits on the board, given its footprint's `frame`. The pad's
/// rotation is absolute in the file, the footprint's included, so the
/// shape turns by it on the board, not in the footprint's frame.
pub(crate) fn pad_frame(pad: &Pad, frame: &Transform) -> Transform {
    let shape = frame.then(Transform::new(pad.at, 0.0));
    Transform::new(shape.origin(), pad.rotation)
}

/// Whether `via` reaches `layer`: its two layers bound a span of the
/// copper stack (`copper` in stack order) and `layer` is in it.
pub(crate) fn spans(via: &Via, copper: &[LayerId], layer: LayerId) -> bool {
    let position = |id: LayerId| copper.iter().position(|&other| other == id);
    let (Some(a), Some(b), Some(l)) = (
        position(via.layers[0]),
        position(via.layers[1]),
        position(layer),
    ) else {
        return false;
    };
    (a.min(b)..=a.max(b)).contains(&l)
}

/// The centre line of an outline graphic as a polyline, and whether it
/// closes on itself.
fn outline_points(drawing: &Drawing, curves: &Curves) -> (Vec<Point>, bool) {
    match &drawing.shape {
        Shape::Line { start, end } => (vec![*start, *end], false),
        Shape::Arc { start, mid, end } => (arc_points(*start, *mid, *end, curves), false),
        Shape::Circle { center, end } => {
            let radius = (end.x - center.x).hypot(end.y - center.y);
            (circle(*center, radius, curves), true)
        }
        Shape::Rect { start, end } => (
            vec![
                *start,
                Point {
                    x: end.x,
                    y: start.y,
                },
                *end,
                Point {
                    x: start.x,
                    y: end.y,
                },
            ],
            true,
        ),
        Shape::Polygon { points } => (chain_points(points, curves), true),
        Shape::Curve {
            start,
            control1,
            control2,
            end,
        } => (bezier([*start, *control1, *control2, *end], curves), false),
    }
}

/// The box around an outline drawing's centre line: exact for an arc and
/// a circle and a polygon's arcs, the flattened points (within
/// `tolerance`) for a curve.
pub(crate) fn graphic_bounds(drawing: &Drawing, tolerance: f64) -> Option<Bounds> {
    match &drawing.shape {
        Shape::Arc { start, mid, end } => Some(match Arc::through(*start, *mid, *end) {
            Some(arc) => arc.bounds(),
            None => Bounds::of(*start).including(*end),
        }),
        Shape::Circle { center, end } => {
            let radius = (end.x - center.x).hypot(end.y - center.y);
            Some(Bounds {
                min: Point {
                    x: center.x - radius,
                    y: center.y - radius,
                },
                max: Point {
                    x: center.x + radius,
                    y: center.y + radius,
                },
            })
        }
        Shape::Polygon { points } => points.bounds(),
        Shape::Line { .. } | Shape::Rect { .. } | Shape::Curve { .. } => {
            let (points, _) = outline_points(drawing, &Curves::at(tolerance));
            Bounds::around(points)
        }
    }
}

/// How many chords flatten a cubic Bezier: enough that a board-edge
/// curve looks smooth at any zoom the outline is drawn at.
const BEZIER_SEGMENTS: usize = 32;

/// A cubic Bezier through its four control points as a polyline. At the
/// floor tolerance it is [`BEZIER_SEGMENTS`] chords; inside a refined
/// region as many as the finer tolerance asks for (`bezier_segments`),
/// the rest of the curve keeping the floor's chords.
pub(crate) fn bezier([p0, p1, p2, p3]: [Point; 4], curves: &Curves) -> Vec<Point> {
    let at = |t: f64| {
        let u = 1.0 - t;
        let weight = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
        Point {
            x: weight[0] * p0.x + weight[1] * p1.x + weight[2] * p2.x + weight[3] * p3.x,
            y: weight[0] * p0.y + weight[1] * p1.y + weight[2] * p2.y + weight[3] * p3.y,
        }
    };
    // A Bezier lies in the hull of its control points.
    let hull = Bounds::around([p0, p1, p2, p3]).unwrap_or_else(|| Bounds::of(p0));
    let samples: Vec<f64> = match curves.refine(&hull) {
        None => (0..=BEZIER_SEGMENTS)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "i is at most 32")]
                let t = i as f64 / BEZIER_SEGMENTS as f64;
                t
            })
            .collect(),
        Some(fine) => {
            let segments = bezier_segments([p0, p1, p2, p3], fine.tolerance).max(BEZIER_SEGMENTS);
            Curves::sampled(BEZIER_SEGMENTS, segments, false, |t| {
                holds(&fine.region, at(t))
            })
        }
    };
    samples.into_iter().map(at).collect()
}

/// How many equal steps of the parameter keep every chord of a cubic
/// Bezier within `tolerance` of it: a chord of a step `h` sits at most
/// `h^2 / 8` times the curve's greatest second derivative away, and that
/// is at most six times the greater of the two control polygon's second
/// differences.
fn bezier_segments([p0, p1, p2, p3]: [Point; 4], tolerance: f64) -> usize {
    let second =
        |a: Point, b: Point, c: Point| (a.x - 2.0 * b.x + c.x).hypot(a.y - 2.0 * b.y + c.y);
    let greatest = second(p0, p1, p2).max(second(p1, p2, p3));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the root is positive and finite, and ceil leaves an integer"
    )]
    let segments = (0.75 * greatest / tolerance).sqrt().ceil() as usize;
    segments.max(1)
}

/// A chain as a polygon, each arc flattened as [`arc_points`] does, so a
/// polygon's arc is as fine as a track's inside a refined region. An arc
/// with no circle through its points gives straight edges start, mid,
/// end, as [`Chain::flatten`] does.
pub(crate) fn chain_points(chain: &Chain, curves: &Curves) -> Vec<Point> {
    let mut points = Vec::with_capacity(chain.items().len());
    for item in chain.items() {
        match *item {
            ChainItem::Point(point) => points.push(point),
            ChainItem::Arc { start, mid, end } => {
                if Arc::through(start, mid, end).is_some() {
                    points.extend(arc_points(start, mid, end, curves));
                } else {
                    points.extend([start, mid, end]);
                }
            }
        }
    }
    points
}

/// The points of the arc from `start` through `mid` to `end`, or its two
/// ends when the three are collinear. Inside a refined region the arc is
/// flattened to the finer tolerance, outside it to the floor's, one
/// polyline with a vertex on the arc at every joint.
pub(crate) fn arc_points(start: Point, mid: Point, end: Point, curves: &Curves) -> Vec<Point> {
    let Some(arc) = Arc::through(start, mid, end) else {
        return vec![start, end];
    };
    let Some(fine) = curves.refine(&arc.bounds()) else {
        return arc.flatten(curves.tolerance);
    };
    let sweep = arc.sweep.abs();
    let coarse = segments_for(arc.radius, sweep, curves.tolerance);
    let finer = segments_for(arc.radius, sweep, fine.tolerance);
    let at = |t: f64| arc.point_at(arc.start_angle + arc.sweep * t);
    Curves::sampled(coarse, finer, false, |t| holds(&fine.region, at(t)))
        .into_iter()
        .map(at)
        .collect()
}

/// A circle as a polygon, points in increasing angle.
pub(crate) fn circle(center: Point, radius: f64, curves: &Curves) -> Vec<Point> {
    rounded_rect(
        Size {
            width: 2.0 * radius,
            height: 2.0 * radius,
        },
        radius,
        &Transform::new(center, 0.0),
        curves,
    )
    .into_iter()
    .map(|p| Point {
        x: p.x + center.x,
        y: p.y + center.y,
    })
    .collect()
}

/// A rectangle of `size` centred on the origin with corners rounded to
/// `radius` (clamped to half the shorter side), as a polygon in
/// increasing angle. Radius 0 is the rectangle; the radius of half the
/// shorter side is a stadium, or a circle when the sides are equal.
/// `frame` carries the origin to the board, for the refined region's
/// sake; the points come back in the rectangle's own frame.
fn rounded_rect(size: Size, radius: f64, frame: &Transform, curves: &Curves) -> Vec<Point> {
    let half_w = size.width / 2.0;
    let half_h = size.height / 2.0;
    if half_w <= 0.0 || half_h <= 0.0 {
        // A pad shrunk past nothing (a paste clearance larger than the
        // pad) is no shape.
        return Vec::new();
    }
    let radius = radius.clamp(0.0, half_w.min(half_h));
    let inner_w = half_w - radius;
    let inner_h = half_h - radius;
    // The corner centres in increasing angle: the arcs they carry span
    // one quarter turn each, starting at angle 0.
    let corners = [
        (inner_w, inner_h),
        (-inner_w, inner_h),
        (-inner_w, -inner_h),
        (inner_w, -inner_h),
    ];
    let mut points = Vec::new();
    if radius <= 0.0 {
        points.extend(corners.map(|(x, y)| Point { x, y }));
        return points;
    }
    let reach = Bounds::around(
        [
            (half_w, half_h),
            (-half_w, half_h),
            (-half_w, -half_h),
            (half_w, -half_h),
        ]
        .map(|(x, y)| frame.apply(Point { x, y })),
    );
    let fine = reach.and_then(|reach| curves.refine(&reach));
    if let Some(fine) = fine {
        return refined_rounded_rect(radius, corners, frame, curves, &fine);
    }
    let segments = segments_for(radius, std::f64::consts::FRAC_PI_2, curves.tolerance);
    for (i, (cx, cy)) in corners.into_iter().enumerate() {
        #[expect(clippy::cast_precision_loss, reason = "i is at most 3")]
        let from = i as f64 * std::f64::consts::FRAC_PI_2;
        for step in 0..=segments {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a segment count is small; well under 2^53"
            )]
            let angle = from + std::f64::consts::FRAC_PI_2 * step as f64 / segments as f64;
            let p = Point {
                x: cx + radius * angle.cos(),
                y: cy + radius * angle.sin(),
            };
            if points.last().is_some_and(|last| same_point(*last, p)) {
                continue;
            }
            points.push(p);
        }
    }
    // The last point of the fourth arc meets the first of the first arc.
    let closes = matches!(
        (points.first(), points.last()),
        (Some(first), Some(last)) if same_point(*first, *last)
    );
    if closes {
        points.pop();
    }
    points
}

/// [`rounded_rect`] where the region reaches the shape: each corner's
/// arc is sampled fine inside the region and at the floor outside. A
/// circle is sampled as one sweep, so its chords are the full circle's
/// count at the fine tolerance, not four quarters' rounded up.
fn refined_rounded_rect(
    radius: f64,
    corners: [(f64, f64); 4],
    frame: &Transform,
    curves: &Curves,
    fine: &Fine,
) -> Vec<Point> {
    use std::f64::consts::{FRAC_PI_2, TAU};
    let inside = |p: Point| holds(&fine.region, frame.apply(p));
    let quarter = segments_for(radius, FRAC_PI_2, curves.tolerance);
    if corners.iter().all(|&(x, y)| x == 0.0 && y == 0.0) {
        let at = |t: f64| Point {
            x: radius * (TAU * t).cos(),
            y: radius * (TAU * t).sin(),
        };
        let finer = segments_for(radius, TAU, fine.tolerance);
        return Curves::sampled(4 * quarter, finer, true, |t| inside(at(t)))
            .into_iter()
            .map(at)
            .collect();
    }
    let finer = segments_for(radius, FRAC_PI_2, fine.tolerance);
    let mut points: Vec<Point> = Vec::new();
    for (i, (cx, cy)) in corners.into_iter().enumerate() {
        #[expect(clippy::cast_precision_loss, reason = "i is at most 3")]
        let from = i as f64 * FRAC_PI_2;
        let at = |t: f64| {
            let angle = from + FRAC_PI_2 * t;
            Point {
                x: cx + radius * angle.cos(),
                y: cy + radius * angle.sin(),
            }
        };
        for t in Curves::sampled(quarter, finer, false, |t| inside(at(t))) {
            let p = at(t);
            if points.last().is_some_and(|last| same_point(*last, p)) {
                continue;
            }
            points.push(p);
        }
    }
    let closes = matches!(
        (points.first(), points.last()),
        (Some(first), Some(last)) if same_point(*first, *last)
    );
    if closes {
        points.pop();
    }
    points
}

/// The outline of a round-ended stroke from `a` to `b` of `width`: a
/// stadium, as a polygon in increasing angle. Two coincident ends give
/// the disc.
fn stadium(a: Point, b: Point, width: f64, curves: &Curves) -> Vec<Point> {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx.hypot(dy);
    let mid = Point {
        x: f64::midpoint(a.x, b.x),
        y: f64::midpoint(a.y, b.y),
    };
    // `Transform` turns by minus its angle in the file's frame, so the
    // segment's own angle goes in negated.
    let along = Transform::new(mid, -dy.atan2(dx).to_degrees());
    rounded_rect(
        Size {
            width: length + width,
            height: width,
        },
        width / 2.0,
        &along,
        curves,
    )
    .into_iter()
    .map(|p| along.apply(p))
    .collect()
}

/// Whether two points coincide to within a picometre: a generator's
/// repeated corner, never two distinct points of a board.
fn same_point(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() < 1e-12 && (a.y - b.y).abs() < 1e-12
}

/// Twice the signed area of a polygon (the shoelace sum): positive when
/// the points run in increasing angle in the file's frame.
fn signed_area(polygon: &[Point]) -> f64 {
    polygon
        .iter()
        .zip(polygon.iter().cycle().skip(1))
        .map(|(a, b)| a.x * b.y - b.x * a.y)
        .sum()
}

/// A [`BuffersBuilder`] that drops every triangle of zero area as lyon
/// hands it over. The stroke tessellator emits them at a join of collinear
/// edges, its join quad with coincident corners and an arc fan over a zero
/// angle; the fill tessellator has no area check either, so both go
/// through here. Such a triangle draws nothing, but a point test takes
/// three zero cross products for inside, so it would claim every point on
/// the triangle's line, or every point at all when its corners coincide.
/// The area check is exact, in the arithmetic of [`Mesh::covers`]. The
/// vertices stay, so the bounds of a mesh do not move.
struct ZeroAreaFilter<'a, Ctor>(BuffersBuilder<'a, [f32; 2], u32, Ctor>);

impl<Ctor> GeometryBuilder for ZeroAreaFilter<'_, Ctor> {
    fn begin_geometry(&mut self) {
        self.0.begin_geometry();
    }

    fn end_geometry(&mut self) {
        self.0.end_geometry();
    }

    fn abort_geometry(&mut self) {
        self.0.abort_geometry();
    }

    fn add_triangle(&mut self, a: VertexId, b: VertexId, c: VertexId) {
        let positions = &self.0.buffers().vertices;
        let corners = [a, b, c].map(|id| corner(positions, id.0));
        if let [Some(first), Some(second), Some(third)] = corners
            && cross(first, second, third) == 0.0
        {
            return;
        }
        self.0.add_triangle(a, b, c);
    }
}

impl<Ctor: FillVertexConstructor<[f32; 2]>> FillGeometryBuilder for ZeroAreaFilter<'_, Ctor> {
    fn add_fill_vertex(&mut self, vertex: FillVertex) -> Result<VertexId, GeometryBuilderError> {
        self.0.add_fill_vertex(vertex)
    }
}

impl<Ctor: StrokeVertexConstructor<[f32; 2]>> StrokeGeometryBuilder for ZeroAreaFilter<'_, Ctor> {
    fn add_stroke_vertex(
        &mut self,
        vertex: StrokeVertex,
    ) -> Result<VertexId, GeometryBuilderError> {
        self.0.add_stroke_vertex(vertex)
    }
}

/// Accumulates fills and strokes into meshes, one per [`Unit`]: what is
/// drawn between the start and the end of [`MeshBuilder::unit`] is one.
struct MeshBuilder<'p> {
    curves: Curves,
    /// The tolerance lyon rounds a stroke's caps and joins to: the fine
    /// one for a unit the region reaches.
    round: f64,
    fill: FillTessellator,
    stroke: StrokeTessellator,
    buffers: VertexBuffers<[f32; 2], u32>,
    units: Vec<Option<Unit>>,
    /// The units a previous build made: only those they ask for are
    /// built again.
    prior: Option<&'p [Unit]>,
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "millimetres in f32 keep ten nanometres over a board; the GPU takes f32"
)]
fn to_lyon(p: Point) -> LyonPoint {
    point(p.x as f32, p.y as f32)
}

impl<'p> MeshBuilder<'p> {
    fn new(tessellator: &Tessellator) -> Self {
        Self {
            curves: Curves::of(tessellator),
            round: tessellator
                .fine
                .map_or(tessellator.tolerance, |f| f.tolerance),
            fill: FillTessellator::new(),
            stroke: StrokeTessellator::new(),
            buffers: VertexBuffers::new(),
            units: Vec::new(),
            prior: None,
        }
    }

    /// The tolerance lyon rounds a stroke's caps and joins to from now on.
    pub(crate) fn set_round(&mut self, round: f64) {
        self.round = round;
    }

    fn with_prior(mut self, prior: Option<&'p [Unit]>) -> Self {
        self.prior = prior;
        self
    }

    /// One unit: what `draw` adds is its mesh. With a prior, a unit the
    /// region does not change is not drawn at all.
    fn unit(&mut self, net: Option<NetId>, draw: impl FnOnce(&mut Self)) {
        let region = self.curves.fine.map(|fine| fine.region);
        if let Some(prior) = self.prior {
            let wanted = prior
                .get(self.units.len())
                .filter(|unit| unit.wants(region.as_ref()));
            let Some(unit) = wanted else {
                self.units.push(None);
                return;
            };
            let reached = region
                .zip(unit.bounds)
                .is_some_and(|(region, bounds)| touches(&region, &bounds));
            self.round = match self.curves.fine {
                Some(fine) if reached => fine.tolerance,
                _ => self.curves.tolerance,
            };
        }
        self.curves.take_touched();
        draw(self);
        let curved = self.curves.take_touched();
        let unit = self.close_unit(net, curved);
        self.units.push(Some(unit));
    }

    /// Take what has been drawn since the last unit as one: `curved`
    /// says whether the item holds a curve besides what this builder
    /// flattened itself.
    pub(crate) fn close_unit(&mut self, net: Option<NetId>, curved: bool) -> Unit {
        let curved = self.curves.take_touched() || curved;
        let mesh = Mesh {
            positions: std::mem::take(&mut self.buffers.vertices),
            indices: std::mem::take(&mut self.buffers.indices),
        };
        let floor = self.curves.tolerance;
        let bounds = mesh.bounds().map(|bounds| Bounds {
            min: Point {
                x: bounds.min.x - floor,
                y: bounds.min.y - floor,
            },
            max: Point {
                x: bounds.max.x + floor,
                y: bounds.max.y + floor,
            },
        });
        let fine = curved
            && self
                .curves
                .fine
                .zip(bounds)
                .is_some_and(|(fine, bounds)| touches(&fine.region, &bounds));
        Unit {
            mesh,
            bounds,
            curved,
            fine,
            net,
        }
    }

    /// The units built, in order.
    fn finish_units(self) -> Vec<Option<Unit>> {
        debug_assert!(self.buffers.indices.is_empty(), "a mesh outside a unit");
        self.units
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "a tolerance is a small positive millimetre value, exact enough in f32"
    )]
    fn lyon_tolerance(&self) -> f32 {
        self.round as f32
    }

    /// Fill `positives` with `holes` cut out: positives wound one way,
    /// holes the other, under the non-zero rule, so overlapping positives
    /// stay filled and a hole in one of them stays open. The winding is
    /// imposed here, whatever way a polygon arrived (a custom pad's
    /// primitive keeps the file's order), so no shape cancels another.
    fn fill(&mut self, positives: &[Vec<Point>], holes: &[Vec<Point>]) {
        let mut path = Path::builder();
        for polygon in positives {
            add_polygon(&mut path, polygon, signed_area(polygon) < 0.0);
        }
        for polygon in holes {
            add_polygon(&mut path, polygon, signed_area(polygon) > 0.0);
        }
        let path = path.build();
        let options =
            FillOptions::tolerance(self.lyon_tolerance()).with_fill_rule(FillRule::NonZero);
        // A degenerate shape (every point the same, a zero-area sliver)
        // contributes no triangles, which is the right drawing of it.
        let _ = self.fill.tessellate_path(
            &path,
            &options,
            &mut ZeroAreaFilter(BuffersBuilder::new(&mut self.buffers, |v: FillVertex| {
                v.position().to_array()
            })),
        );
    }

    /// Stroke a polyline of `width` with round ends and joins.
    fn stroke(&mut self, points: &[Point], width: f64, closed: bool) {
        let Some((first, rest)) = points.split_first() else {
            return;
        };
        // A stroke's caps and joins are round: a curve.
        self.curves.touched.set(true);
        let mut path = Path::builder();
        path.begin(to_lyon(*first));
        for p in rest {
            path.line_to(to_lyon(*p));
        }
        path.end(closed);
        let path = path.build();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a track width is millimetres, exact enough in f32"
        )]
        let options = StrokeOptions::tolerance(self.lyon_tolerance())
            .with_line_width(width as f32)
            .with_line_cap(LineCap::Round)
            .with_line_join(LineJoin::Round);
        // As in `fill`: a degenerate stroke draws nothing.
        let _ = self.stroke.tessellate_path(
            &path,
            &options,
            &mut ZeroAreaFilter(BuffersBuilder::new(&mut self.buffers, |v: StrokeVertex| {
                v.position().to_array()
            })),
        );
    }

    fn via(&mut self, via: &Via) {
        let ring = circle(via.at, via.size / 2.0, &self.curves);
        let hole = circle(via.at, via.drill / 2.0, &self.curves);
        self.fill(&[ring], &[hole]);
    }

    /// A pad in its footprint's `frame`: the outline of its shape, the
    /// primitives of a custom pad, and its hole cut out.
    fn pad(&mut self, pad: &Pad, frame: &Transform) {
        let shape = pad_frame(pad, frame);
        if pad.kind == PadKind::NpThroughHole
            && pad.drill.is_some_and(|drill| {
                drill.size.width >= pad.size.width && drill.size.height >= pad.size.height
            })
        {
            return;
        }
        let mut positives = vec![self.pad_outline(pad, &shape, 0.0)];
        let mut strokes = Vec::new();
        for primitive in &pad.primitives {
            self.primitive(primitive, &shape, &mut positives, &mut strokes);
        }
        let holes = match pad.drill {
            Some(drill) => vec![self.hole(drill, &shape, 0.0)],
            None => Vec::new(),
        };
        self.fill(&positives, &holes);
        for (points, width) in strokes {
            self.stroke(&points, width, false);
        }
    }

    /// The outline of `pad`'s shape on the board (in its `shape` frame),
    /// grown by `grow` on every side: a mask opening's margin, a paste
    /// clearance (negative), or 0 for the copper. Growing rounds the
    /// corners by the same amount, as KiCad's own expansion does; a shape
    /// shrunk to nothing is empty.
    fn pad_outline(&self, pad: &Pad, shape: &Transform, grow: f64) -> Vec<Point> {
        pad_outline_in(pad, shape, grow, &self.curves)
    }

    /// The polygons a custom pad's primitives cover on the board, for a
    /// mask opening or a paste shape: filled closed shapes as they are,
    /// strokes as stadiums. Empty for any other pad.
    fn primitive_polygons(&self, pad: &Pad, shape: &Transform) -> Vec<Vec<Point>> {
        pad.primitives
            .iter()
            .flat_map(|primitive| drawing_polygons(primitive, &self.curves))
            .map(|polygon| polygon.into_iter().map(|p| shape.apply(p)).collect())
            .collect()
    }

    /// A custom pad's primitive, in the pad's `shape` frame: a filled
    /// shape joins `positives`, a stroked one `strokes`.
    fn primitive(
        &self,
        primitive: &Drawing,
        shape: &Transform,
        positives: &mut Vec<Vec<Point>>,
        strokes: &mut Vec<(Vec<Point>, f64)>,
    ) {
        let on_board = |points: Vec<Point>| points.into_iter().map(|p| shape.apply(p)).collect();
        let (points, closed) = outline_points(primitive, &self.curves);
        if primitive.filled && closed {
            positives.push(on_board(points));
        } else if primitive.filled {
            // A filled open shape (an arc, a line) is its fill of the
            // chord: nothing to fill, its stroke is what shows.
            strokes.push((on_board(points), primitive.width));
        } else if primitive.width > 0.0 {
            let mut points = on_board(points);
            if closed && let Some(first) = points.first().copied() {
                points.push(first);
            }
            strokes.push((points, primitive.width));
        }
    }

    /// A pad's hole on the board, grown by `grow` on every side (0 for the
    /// hole itself, [`HOLE_RIM`] for the outside of its rim): a circle, or
    /// a stadium for a slot, offset from the pad's centre in the pad's
    /// frame.
    fn hole(&self, drill: Drill, shape: &Transform, grow: f64) -> Vec<Point> {
        let min_side = drill.size.width.min(drill.size.height);
        let turned = shape.then(Transform::new(drill.offset, 0.0));
        let (size, radius) = grown(drill.size, min_side / 2.0, grow);
        rounded_rect(size, radius, &turned, &self.curves)
            .into_iter()
            .map(|p| turned.apply(p))
            .collect()
    }

    /// `zone` on `layer`, in `frame`: its stored fills, or its outline
    /// hatched when it has none there.
    fn zone(&mut self, zone: &Zone, frame: &Transform, layer: LayerId) {
        if zone.is_unfilled_on(layer) {
            self.unit(zone.net, |builder| builder.zone_hatch(zone, frame));
            return;
        }
        for fill in zone.fills.iter().filter(|fill| fill.layer == layer) {
            self.unit(zone.net, |builder| {
                let carried = fill.points.map(|p| frame.apply(p));
                let polygon = chain_points(&carried, &builder.curves);
                builder.fill(&[polygon], &[]);
            });
        }
    }

    /// An unfilled zone's outline, stroked at [`HATCH_WIDTH`], with lines
    /// at 45 degrees every [`HATCH_PITCH`] across it, clipped to the
    /// outline. The lines are `x - y = k * pitch * sqrt(2)` for integer
    /// `k`, so one passes through the origin and the pattern of two zones
    /// lines up.
    fn zone_hatch(&mut self, zone: &Zone, frame: &Transform) {
        let carried = zone.outline.map(|p| frame.apply(p));
        let outline = chain_points(&carried, &self.curves);
        let Some(bounds) = Bounds::around(outline.iter().copied()) else {
            return;
        };
        self.stroke(&outline, HATCH_WIDTH, true);
        // Along the lines the spacing is the pitch times the square root
        // of two; across them, the pitch.
        let step = HATCH_PITCH * std::f64::consts::SQRT_2;
        let first = ((bounds.min.x - bounds.max.y) / step).ceil();
        let last = ((bounds.max.x - bounds.min.y) / step).floor();
        // Half the line width, along the normal (1, -1) / sqrt(2).
        let normal = HATCH_WIDTH / 2.0 / std::f64::consts::SQRT_2;
        let mut stripes: OverlayShape = Vec::new();
        let mut k = first;
        while k <= last {
            let c = k * step;
            // The line from left of the box to right of it.
            let (x0, x1) = (bounds.min.x - 1.0, bounds.max.x + 1.0);
            let (a, b) = (Point { x: x0, y: x0 - c }, Point { x: x1, y: x1 - c });
            let stripe = vec![
                Point {
                    x: a.x + normal,
                    y: a.y - normal,
                },
                Point {
                    x: b.x + normal,
                    y: b.y - normal,
                },
                Point {
                    x: b.x - normal,
                    y: b.y + normal,
                },
                Point {
                    x: a.x - normal,
                    y: a.y + normal,
                },
            ];
            stripes.push(wound(&stripe, true));
            k += 1.0;
        }
        let subject: OverlayShape = vec![wound(&outline, true)];
        let shapes = subject.overlay(&stripes, OverlayRule::Intersect, OverlayFill::NonZero);
        self.fill_shapes(shapes);
    }

    /// Fill the shapes a boolean produced: each one its outer contour with
    /// the rest as holes.
    fn fill_shapes(&mut self, shapes: Vec<OverlayShape>) {
        for polygons in shapes {
            let mut contours = polygons.into_iter().map(|contour| {
                contour
                    .into_iter()
                    .map(|[x, y]| Point { x, y })
                    .collect::<Vec<Point>>()
            });
            let Some(outer) = contours.next() else {
                continue;
            };
            let holes: Vec<Vec<Point>> = contours.collect();
            self.fill(&[outer], &holes);
        }
    }

    /// A text item reading `content`, anchored in `frame` (a footprint's,
    /// or the identity for a board text). Nothing for a hidden or empty
    /// text.
    fn text(&mut self, content: &str, text: &Text, frame: &Transform, in_footprint: bool) {
        if text.hidden || content.is_empty() {
            return;
        }
        let shape = font::shape(
            content,
            &Placement::of(text, frame.apply(text.at), in_footprint),
        );
        if text.knockout {
            self.knockout(&shape);
        } else {
            self.strokes(&shape);
        }
    }

    /// The strokes of a laid-out text; a stroke of one point is a dot.
    fn strokes(&mut self, shape: &TextShape) {
        for stroke in &shape.strokes {
            match stroke.as_slice() {
                [] => {}
                [dot] => {
                    let disc = circle(*dot, shape.pen / 2.0, &self.curves);
                    self.fill(&[disc], &[]);
                }
                _ => self.stroke(stroke, shape.pen, false),
            }
        }
    }

    /// A knockout: the text's box less the union of its strokes, each
    /// stroke the stadiums of its segments. The union is a boolean
    /// difference, not a fill rule: strokes overlap where glyphs cross
    /// and join, and any winding count would fill those overlaps back in.
    fn knockout(&mut self, shape: &TextShape) {
        let Some(corners) = shape.knockout_box else {
            return;
        };
        let to_pair = |p: Point| [p.x, p.y];
        let subject: OverlayShape = vec![corners.iter().copied().map(to_pair).collect()];
        let mut clip: OverlayShape = Vec::new();
        for stroke in &shape.strokes {
            match stroke.as_slice() {
                [] => {}
                [dot] => clip.push(
                    circle(*dot, shape.pen / 2.0, &self.curves)
                        .into_iter()
                        .map(to_pair)
                        .collect(),
                ),
                points => {
                    for pair in points.windows(2) {
                        let (Some(a), Some(b)) = (pair.first(), pair.get(1)) else {
                            continue;
                        };
                        clip.push(
                            stadium(*a, *b, shape.pen, &self.curves)
                                .into_iter()
                                .map(to_pair)
                                .collect(),
                        );
                    }
                }
            }
        }
        let shapes = subject.overlay(&clip, OverlayRule::Difference, OverlayFill::NonZero);
        self.fill_shapes(shapes);
    }

    /// Everything drawn, units and the rest, as one mesh.
    fn finish(self) -> Mesh {
        let mut mesh = Mesh::default();
        for unit in self.units.iter().flatten() {
            mesh.append(&unit.mesh);
        }
        mesh.append(&Mesh {
            positions: self.buffers.vertices,
            indices: self.buffers.indices,
        });
        mesh
    }
}

/// One closed sub-path of `polygon`, reversed when `reverse`.
fn add_polygon(
    path: &mut lyon_tessellation::path::path::Builder,
    polygon: &[Point],
    reverse: bool,
) {
    let mut points: Vec<LyonPoint> = polygon.iter().map(|&p| to_lyon(p)).collect();
    if reverse {
        points.reverse();
    }
    let Some((first, rest)) = points.split_first() else {
        return;
    };
    path.begin(*first);
    for p in rest {
        path.line_to(*p);
    }
    path.end(true);
}
