//! Triangles from a board: what the viewer uploads to the GPU and the
//! thumbnail renderer will draw. One copper layer at a time (tracks, track
//! arcs, vias, pads and stored zone fills, holes left open), the holes
//! drilled through that layer (drawn last, in the page colour, so a track
//! ending in a via does not fill the hole back in), the outline as the
//! strokes of `Edge.Cuts`, the graphics of any other layer (a silkscreen's
//! lines and filled shapes), and the text of a layer in Newstroke
//! ([`crate::font`]), a knockout as its box with the glyphs cut out.
//!
//! Everything is computed in f64 and handed to lyon in f32 at the last
//! step; positions in the [`Mesh`] are millimetres in the file's frame,
//! y down, as the model has them.

use i_overlay::core::fill_rule::FillRule as OverlayFill;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::single::SingleFloatOverlay as _;
use lyon_tessellation::geometry_builder::{BuffersBuilder, VertexBuffers};
use lyon_tessellation::math::{Point as LyonPoint, point};
use lyon_tessellation::path::Path;
use lyon_tessellation::{
    FillOptions, FillRule, FillTessellator, FillVertex, LineCap, LineJoin, StrokeOptions,
    StrokeTessellator, StrokeVertex,
};

use crate::font::{self, Placement, TextShape};
use crate::geometry::{Arc, Bounds, Transform, segments_for};
use crate::model::{
    Board, Drawing, Drill, EDGE_CUTS, LayerId, Pad, PadKind, PadShape, Point, Shape, Size, Text,
    Via, Zone,
};

/// Triangles: positions in millimetres, indices in threes.
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
                let corner = |index: u32| {
                    let position = self.positions.get(usize::try_from(index).ok()?)?;
                    Some(Point {
                        x: f64::from(position[0]),
                        y: f64::from(position[1]),
                    })
                };
                Some([corner(a)?, corner(b)?, corner(c)?])
            })
    }

    /// The box around every vertex, or `None` for an empty mesh.
    #[must_use]
    pub fn bounds(&self) -> Option<Bounds> {
        Bounds::around(self.positions.iter().map(|position| Point {
            x: f64::from(position[0]),
            y: f64::from(position[1]),
        }))
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

/// Twice the signed area of the triangle `a`, `b`, `c`.
fn cross(a: Point, b: Point, c: Point) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// The width an outline stroke of width 0 is drawn at: KiCad's own
/// default for a new edge.
const MIN_OUTLINE_WIDTH: f64 = 0.05;

/// Turns board items into a [`Mesh`] at one tolerance: how far a chord
/// may sit from the curve it flattens, in millimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tessellator {
    tolerance: f64,
}

impl Tessellator {
    /// A tessellator flattening curves to within `tolerance` millimetres.
    #[must_use]
    pub fn new(tolerance: f64) -> Self {
        Self { tolerance }
    }

    /// The copper on `layer`: tracks and track arcs with round ends, vias
    /// and pads that reach the layer as rings around their holes, and the
    /// zone fills KiCad stored. An unplated hole whose pad is no bigger
    /// than the hole is no copper.
    #[must_use]
    pub fn copper(&self, board: &Board, layer: LayerId) -> Mesh {
        let mut builder = MeshBuilder::new(self.tolerance);
        for segment in board.segments.iter().filter(|s| s.layer == layer) {
            builder.stroke(&[segment.start, segment.end], segment.width, false);
        }
        for arc in board.arcs.iter().filter(|a| a.layer == layer) {
            let points = match Arc::through(arc.start, arc.mid, arc.end) {
                Some(arc) => arc.flatten(self.tolerance),
                None => vec![arc.start, arc.end],
            };
            builder.stroke(&points, arc.width, false);
        }
        let copper: Vec<LayerId> = board.copper_layers().map(|(id, _)| id).collect();
        for via in board.vias.iter().filter(|via| spans(via, &copper, layer)) {
            builder.via(via);
        }
        for footprint in &board.footprints {
            let frame = Transform::new(footprint.at, footprint.rotation);
            for pad in footprint
                .pads
                .iter()
                .filter(|pad| pad.layers.contains(&layer))
            {
                builder.pad(pad, &frame);
            }
            for zone in &footprint.zones {
                builder.zone_fills(zone, &frame, layer);
            }
        }
        for zone in &board.zones {
            builder.zone_fills(zone, &Transform::identity(), layer);
        }
        builder.finish()
    }

    /// The holes drilled through `layer`: every via that reaches it and
    /// every drilled pad on it, plated or not, as filled discs and slots.
    /// Drawn after the copper in the page colour: a track or a zone that
    /// runs into a hole is copper right up to the drill, and the drill is
    /// still open.
    #[must_use]
    pub fn holes(&self, board: &Board, layer: LayerId) -> Mesh {
        let mut builder = MeshBuilder::new(self.tolerance);
        let copper: Vec<LayerId> = board.copper_layers().map(|(id, _)| id).collect();
        for via in board.vias.iter().filter(|via| spans(via, &copper, layer)) {
            let hole = circle(via.at, via.drill / 2.0, self.tolerance);
            builder.fill(&[hole], &[]);
        }
        for footprint in &board.footprints {
            let frame = Transform::new(footprint.at, footprint.rotation);
            for pad in footprint
                .pads
                .iter()
                .filter(|pad| pad.layers.contains(&layer))
            {
                if let Some(drill) = pad.drill {
                    let hole = builder.hole(drill, &pad_frame(pad, &frame));
                    builder.fill(&[hole], &[]);
                }
            }
        }
        builder.finish()
    }

    /// The board outline: every `Edge.Cuts` graphic, the board's and the
    /// footprints' (a connector's slot, in the footprint's frame), stroked
    /// at its width (at least 0.05 mm, KiCad's default edge width) with
    /// round ends.
    #[must_use]
    pub fn outline(&self, board: &Board) -> Mesh {
        let mut builder = MeshBuilder::new(self.tolerance);
        for drawing in outline_drawings(board) {
            let width = drawing.width.max(MIN_OUTLINE_WIDTH);
            let (points, closed) = outline_points(&drawing, self.tolerance);
            builder.stroke(&points, width, closed);
        }
        builder.finish()
    }

    /// The box around the outline's geometry (the centre lines, not their
    /// strokes), or `None` for a board without `Edge.Cuts` graphics.
    #[must_use]
    pub fn outline_bounds(&self, board: &Board) -> Option<Bounds> {
        outline_drawings(board)
            .filter_map(|drawing| graphic_bounds(&drawing, self.tolerance))
            .reduce(Bounds::union)
    }

    /// The graphics on `layer`, the board's and the footprints' (in their
    /// frames): a filled closed shape as its fill, everything else as a
    /// stroke of its width (at least 0.05 mm) with round ends.
    #[must_use]
    pub fn graphics(&self, board: &Board, layer: LayerId) -> Mesh {
        let mut builder = MeshBuilder::new(self.tolerance);
        for drawing in layer_drawings(board, layer) {
            let (points, closed) = outline_points(&drawing, self.tolerance);
            if drawing.filled && closed {
                builder.fill(&[points], &[]);
            } else {
                let width = drawing.width.max(MIN_OUTLINE_WIDTH);
                builder.stroke(&points, width, closed);
            }
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
        let mut builder = MeshBuilder::new(self.tolerance);
        for text in board.texts.iter().filter(|t| t.layer == layer) {
            builder.text(&text.text, text, &Transform::identity(), false);
        }
        for footprint in &board.footprints {
            let frame = Transform::new(footprint.at, footprint.rotation);
            for text in footprint.texts.iter().filter(|t| t.layer == layer) {
                builder.text(&footprint.resolve_text(text), text, &frame, true);
            }
        }
        builder.finish()
    }
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

/// Every `Edge.Cuts` drawing on the board: the board's own as written,
/// a footprint's carried into the board's frame.
fn outline_drawings(board: &Board) -> impl Iterator<Item = Drawing> + '_ {
    let edge = board.layer_named(EDGE_CUTS);
    let own = board.outline().map(|graphic| graphic.drawing.clone());
    let placed = board.footprints.iter().flat_map(move |footprint| {
        let frame = Transform::new(footprint.at, footprint.rotation);
        footprint
            .graphics
            .iter()
            .filter(move |graphic| Some(graphic.layer) == edge)
            .map(move |graphic| transformed(&graphic.drawing, &frame))
    });
    own.chain(placed)
}

/// `drawing` with its points carried through `frame`. A rectangle turns
/// into the polygon of its corners, since a turned rectangle is no
/// longer axis-aligned.
fn transformed(drawing: &Drawing, frame: &Transform) -> Drawing {
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
            points: [
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
            .map(f)
            .collect(),
        },
        Shape::Polygon { points } => Shape::Polygon {
            points: points.iter().map(f).collect(),
        },
        Shape::Curve { points } => Shape::Curve {
            points: points.iter().map(f).collect(),
        },
    };
    Drawing {
        shape,
        width: drawing.width,
        filled: drawing.filled,
    }
}

/// Where `pad` sits on the board, given its footprint's `frame`. The pad's
/// rotation is absolute in the file, the footprint's included, so the
/// shape turns by it on the board, not in the footprint's frame.
fn pad_frame(pad: &Pad, frame: &Transform) -> Transform {
    let shape = frame.then(Transform::new(pad.at, 0.0));
    Transform::new(shape.origin(), pad.rotation)
}

/// Whether `via` reaches `layer`: its two layers bound a span of the
/// copper stack (`copper` in stack order) and `layer` is in it.
fn spans(via: &Via, copper: &[LayerId], layer: LayerId) -> bool {
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
fn outline_points(drawing: &Drawing, tolerance: f64) -> (Vec<Point>, bool) {
    match &drawing.shape {
        Shape::Line { start, end } => (vec![*start, *end], false),
        Shape::Arc { start, mid, end } => match Arc::through(*start, *mid, *end) {
            Some(arc) => (arc.flatten(tolerance), false),
            None => (vec![*start, *end], false),
        },
        Shape::Circle { center, end } => {
            let radius = (end.x - center.x).hypot(end.y - center.y);
            (circle(*center, radius, tolerance), true)
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
        Shape::Polygon { points } => (points.clone(), true),
        Shape::Curve { points } => (bezier(points), false),
    }
}

/// The box around an outline drawing's centre line: exact for an arc and
/// a circle, the flattened points (within `tolerance`) for a curve.
fn graphic_bounds(drawing: &Drawing, tolerance: f64) -> Option<Bounds> {
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
        Shape::Line { .. } | Shape::Rect { .. } | Shape::Polygon { .. } | Shape::Curve { .. } => {
            let (points, _) = outline_points(drawing, tolerance);
            Bounds::around(points)
        }
    }
}

/// How many chords flatten a cubic Bezier: enough that a board-edge
/// curve looks smooth at any zoom the outline is drawn at.
const BEZIER_SEGMENTS: usize = 32;

/// A cubic Bezier through its four control points as a polyline; fewer
/// than four points come back as they are.
fn bezier(points: &[Point]) -> Vec<Point> {
    let [p0, p1, p2, p3] = match points {
        [p0, p1, p2, p3] => [*p0, *p1, *p2, *p3],
        _ => return points.to_vec(),
    };
    (0..=BEZIER_SEGMENTS)
        .map(|i| {
            #[expect(clippy::cast_precision_loss, reason = "i is at most 32")]
            let t = i as f64 / BEZIER_SEGMENTS as f64;
            let u = 1.0 - t;
            let weight = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
            Point {
                x: weight[0] * p0.x + weight[1] * p1.x + weight[2] * p2.x + weight[3] * p3.x,
                y: weight[0] * p0.y + weight[1] * p1.y + weight[2] * p2.y + weight[3] * p3.y,
            }
        })
        .collect()
}

/// A circle as a polygon, points in increasing angle.
fn circle(center: Point, radius: f64, tolerance: f64) -> Vec<Point> {
    rounded_rect(
        Size {
            width: 2.0 * radius,
            height: 2.0 * radius,
        },
        radius,
        tolerance,
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
fn rounded_rect(size: Size, radius: f64, tolerance: f64) -> Vec<Point> {
    let half_w = size.width / 2.0;
    let half_h = size.height / 2.0;
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
    let segments = segments_for(radius, std::f64::consts::FRAC_PI_2, tolerance);
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

/// The outline of a round-ended stroke from `a` to `b` of `width`: a
/// stadium, as a polygon in increasing angle. Two coincident ends give
/// the disc.
fn stadium(a: Point, b: Point, width: f64, tolerance: f64) -> Vec<Point> {
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
        tolerance,
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

/// Accumulates fills and strokes into one mesh.
struct MeshBuilder {
    tolerance: f64,
    fill: FillTessellator,
    stroke: StrokeTessellator,
    buffers: VertexBuffers<[f32; 2], u32>,
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "millimetres in f32 keep ten nanometres over a board; the GPU takes f32"
)]
fn to_lyon(p: Point) -> LyonPoint {
    point(p.x as f32, p.y as f32)
}

impl MeshBuilder {
    fn new(tolerance: f64) -> Self {
        Self {
            tolerance,
            fill: FillTessellator::new(),
            stroke: StrokeTessellator::new(),
            buffers: VertexBuffers::new(),
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "a tolerance is a small positive millimetre value, exact enough in f32"
    )]
    fn lyon_tolerance(&self) -> f32 {
        self.tolerance as f32
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
            &mut BuffersBuilder::new(&mut self.buffers, |v: FillVertex| v.position().to_array()),
        );
    }

    /// Stroke a polyline of `width` with round ends and joins.
    fn stroke(&mut self, points: &[Point], width: f64, closed: bool) {
        let Some((first, rest)) = points.split_first() else {
            return;
        };
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
            &mut BuffersBuilder::new(&mut self.buffers, |v: StrokeVertex| v.position().to_array()),
        );
    }

    fn via(&mut self, via: &Via) {
        let ring = circle(via.at, via.size / 2.0, self.tolerance);
        let hole = circle(via.at, via.drill / 2.0, self.tolerance);
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
        let min_side = pad.size.width.min(pad.size.height);
        let local = match pad.shape {
            PadShape::Circle => rounded_rect(pad.size, pad.size.width / 2.0, self.tolerance),
            // A custom pad's anchor as well: KiCad's default anchor is a
            // circle and the model does not carry the choice, so the
            // stadium of the (usually square) anchor size stands for it.
            PadShape::Oval | PadShape::Custom => {
                rounded_rect(pad.size, min_side / 2.0, self.tolerance)
            }
            // The chamfer sizes and the trapezoid's delta are not in the
            // model; the plain rectangle is the nearest drawing.
            PadShape::Rect | PadShape::Trapezoid | PadShape::ChamferedRect => {
                rounded_rect(pad.size, 0.0, self.tolerance)
            }
            PadShape::RoundRect => rounded_rect(
                pad.size,
                pad.round_radius_ratio.unwrap_or(0.25) * min_side,
                self.tolerance,
            ),
        };
        let mut positives = vec![local.into_iter().map(|p| shape.apply(p)).collect()];
        let mut strokes = Vec::new();
        for primitive in &pad.primitives {
            self.primitive(primitive, &shape, &mut positives, &mut strokes);
        }
        let holes = match pad.drill {
            Some(drill) => vec![self.hole(drill, &shape)],
            None => Vec::new(),
        };
        self.fill(&positives, &holes);
        for (points, width) in strokes {
            self.stroke(&points, width, false);
        }
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
        let (points, closed) = outline_points(primitive, self.tolerance);
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

    /// A pad's hole on the board: a circle, or a stadium for a slot,
    /// offset from the pad's centre in the pad's frame.
    fn hole(&self, drill: Drill, shape: &Transform) -> Vec<Point> {
        let min_side = drill.size.width.min(drill.size.height);
        let turned = shape.then(Transform::new(drill.offset, 0.0));
        rounded_rect(drill.size, min_side / 2.0, self.tolerance)
            .into_iter()
            .map(|p| turned.apply(p))
            .collect()
    }

    /// The stored fills of `zone` on `layer`, in `frame`.
    fn zone_fills(&mut self, zone: &Zone, frame: &Transform, layer: LayerId) {
        for fill in zone.fills.iter().filter(|fill| fill.layer == layer) {
            let polygon: Vec<Point> = fill.points.iter().map(|&p| frame.apply(p)).collect();
            self.fill(&[polygon], &[]);
        }
    }

    /// A text item reading `content`, anchored in `frame` (a footprint's,
    /// or the identity for a board text). Nothing for a hidden or empty
    /// text.
    fn text(&mut self, content: &str, text: &Text, frame: &Transform, in_footprint: bool) {
        if text.hidden || content.is_empty() {
            return;
        }
        let placement = Placement {
            size: text.font.size,
            pen: font::pen_width(&text.font),
            italic: text.font.italic,
            justify: text.justify,
            position: frame.apply(text.at),
            angle: font::draw_angle(text.rotation, in_footprint && text.upright),
        };
        let shape = font::shape(content, &placement);
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
                    let disc = circle(*dot, shape.pen / 2.0, self.tolerance);
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
        let subject: Vec<Vec<[f64; 2]>> = vec![corners.iter().copied().map(to_pair).collect()];
        let mut clip: Vec<Vec<[f64; 2]>> = Vec::new();
        for stroke in &shape.strokes {
            match stroke.as_slice() {
                [] => {}
                [dot] => clip.push(
                    circle(*dot, shape.pen / 2.0, self.tolerance)
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
                            stadium(*a, *b, shape.pen, self.tolerance)
                                .into_iter()
                                .map(to_pair)
                                .collect(),
                        );
                    }
                }
            }
        }
        let shapes = subject.overlay(&clip, OverlayRule::Difference, OverlayFill::NonZero);
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

    fn finish(self) -> Mesh {
        Mesh {
            positions: self.buffers.vertices,
            indices: self.buffers.indices,
        }
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
