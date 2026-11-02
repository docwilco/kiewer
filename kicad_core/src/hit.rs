//! Hit testing and net membership (docs/v1-scope.md, Net interaction):
//! what copper is under a world point, which pads a net has, and where a
//! footprint is. An [`Index`] is built once from a board: every pad, via,
//! segment, track arc and zone fill with a net, each with the layers it
//! reaches, its box and its shape, binned into a uniform grid of
//! [`CELL`] millimetre cells, so a query touches the few items near the
//! point and none of the rest. A query names the layers it sees, top
//! first; among the items under the point, a pad wins over a via, a via
//! over a track, a track over a zone ([`Kind`]), and among items of one
//! kind the one on the topmost layer named.
//!
//! Shapes are the model's as the tessellator draws them (the same pad
//! outline, the same via disc, round-ended tracks, the stored fill
//! polygons with their holes spliced in), measured by distance, so a
//! query can carry a tolerance of a few screen pixels: a hairline track
//! is hit without landing on it exactly.

use std::collections::BTreeMap;

use crate::geometry::{Arc, Bounds, Transform};
use crate::labels::signed_distance;
use crate::model::{Board, Footprint, LayerId, NetId, Pad, Point};
use crate::tessellate::{graphic_bounds, pad_frame, pad_outline, spans, transformed};

/// The side of a grid cell, in millimetres: a few items per cell on a
/// dense board, a few hundred cells over a small one.
pub const CELL: f64 = 2.0;

/// How far a flattened curve may sit from the true one here, in
/// millimetres: a hundredth, well under any tolerance a query carries.
const TOLERANCE: f64 = 0.01;

/// What an item is, in the order a query prefers them: a pad over a via
/// over a track over a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// A footprint's pad.
    Pad,
    /// A via.
    Via,
    /// A track: a straight segment or an arc.
    Track,
    /// A zone's stored fill, or its outline where it has none.
    Zone,
}

/// Which item of the board was hit, by its position in the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Item {
    /// Pad `pad` of `footprint`, each an index into the board's
    /// footprints and the footprint's pads.
    Pad { footprint: usize, pad: usize },
    /// An index into [`Board::vias`].
    Via(usize),
    /// An index into [`Board::segments`].
    Segment(usize),
    /// An index into [`Board::arcs`].
    Arc(usize),
    /// Zone `zone` of the board (`footprint` is `None`) or of a
    /// footprint, by index.
    Zone {
        footprint: Option<usize>,
        zone: usize,
    },
}

/// What a query found under its point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The item.
    pub item: Item,
    /// What it is.
    pub kind: Kind,
    /// Its net.
    pub net: NetId,
    /// The layer it was hit on: the topmost of the query's layers it
    /// reaches.
    pub layer: LayerId,
}

/// The geometry an item is measured by.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    /// A disc: a via.
    Disc { center: Point, radius: f64 },
    /// A round-ended stroke along a polyline: a track.
    Stroke { points: Vec<Point>, radius: f64 },
    /// One or more polygons, any of which counts: a pad's outline and
    /// its primitives, a fill.
    Polygons(Vec<Vec<Point>>),
}

impl Shape {
    /// How far `point` is from the shape; 0 inside it.
    fn distance(&self, point: Point) -> f64 {
        match self {
            Shape::Disc { center, radius } => {
                ((point.x - center.x).hypot(point.y - center.y) - radius).max(0.0)
            }
            Shape::Stroke { points, radius } => {
                let nearest = points
                    .windows(2)
                    .map(|pair| match pair {
                        [a, b] => segment_distance(point, *a, *b),
                        _ => f64::INFINITY,
                    })
                    .fold(f64::INFINITY, f64::min);
                let nearest = match points.as_slice() {
                    [only] => (point.x - only.x).hypot(point.y - only.y),
                    _ => nearest,
                };
                (nearest - radius).max(0.0)
            }
            Shape::Polygons(polygons) => polygons
                .iter()
                .filter(|polygon| polygon.len() >= 3)
                .map(|polygon| (-signed_distance(point, polygon)).max(0.0))
                .fold(f64::INFINITY, f64::min),
        }
    }

    /// The box around the shape, its stroke width included.
    fn bounds(&self) -> Option<Bounds> {
        match self {
            Shape::Disc { center, radius } => Some(grow(Bounds::of(*center), *radius)),
            Shape::Stroke { points, radius } => {
                Bounds::around(points.iter().copied()).map(|bounds| grow(bounds, *radius))
            }
            Shape::Polygons(polygons) => {
                Bounds::around(polygons.iter().flat_map(|polygon| polygon.iter().copied()))
            }
        }
    }
}

/// `bounds` grown by `by` on every side.
fn grow(bounds: Bounds, by: f64) -> Bounds {
    Bounds {
        min: Point {
            x: bounds.min.x - by,
            y: bounds.min.y - by,
        },
        max: Point {
            x: bounds.max.x + by,
            y: bounds.max.y + by,
        },
    }
}

/// The distance from `point` to the segment `from`-`to`.
fn segment_distance(point: Point, from: Point, to: Point) -> f64 {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let mut nearest = from;
    if dx != 0.0 || dy != 0.0 {
        let t = ((point.x - from.x) * dx + (point.y - from.y) * dy) / (dx * dx + dy * dy);
        if t > 1.0 {
            nearest = to;
        } else if t > 0.0 {
            nearest.x += dx * t;
            nearest.y += dy * t;
        }
    }
    (point.x - nearest.x).hypot(point.y - nearest.y)
}

/// One indexed item.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    item: Item,
    kind: Kind,
    net: NetId,
    /// The copper layers it reaches.
    layers: Vec<LayerId>,
    bounds: Bounds,
    shape: Shape,
}

/// The board's copper with a net, binned for point queries.
#[derive(Debug, Clone, PartialEq)]
pub struct Index {
    entries: Vec<Entry>,
    /// The entries whose box reaches each cell, by the cell's column and
    /// row ([`cell_of`]).
    cells: BTreeMap<(i64, i64), Vec<usize>>,
}

/// The grid cell a coordinate falls in.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a board coordinate in millimetres is far below i64's range"
)]
fn cell_of(value: f64) -> i64 {
    (value / CELL).floor() as i64
}

impl Index {
    /// Every pad, via, segment, arc and zone fill of `board` that has a
    /// net. A zone with no stored fill on a layer is indexed there by
    /// its outline, as it is drawn hatched; a keepout is not copper and
    /// is left out.
    #[must_use]
    pub fn build(board: &Board) -> Self {
        let copper: Vec<LayerId> = board.copper_layers().map(|(id, _)| id).collect();
        let mut index = Self {
            entries: Vec::new(),
            cells: BTreeMap::new(),
        };
        for (footprint_index, footprint) in board.footprints.iter().enumerate() {
            let frame = Transform::new(footprint.at, footprint.rotation);
            for (pad_index, pad) in footprint.pads.iter().enumerate() {
                let Some(net) = pad.net else { continue };
                let layers: Vec<LayerId> = pad
                    .layers
                    .iter()
                    .filter(|layer| copper.contains(layer))
                    .copied()
                    .collect();
                index.push(
                    Item::Pad {
                        footprint: footprint_index,
                        pad: pad_index,
                    },
                    Kind::Pad,
                    net,
                    layers,
                    Shape::Polygons(pad_polygons(pad, &frame)),
                );
            }
            for (zone_index, zone) in footprint.zones.iter().enumerate() {
                index.zone(
                    Item::Zone {
                        footprint: Some(footprint_index),
                        zone: zone_index,
                    },
                    zone,
                    &frame,
                    &copper,
                );
            }
        }
        for (via_index, via) in board.vias.iter().enumerate() {
            let Some(net) = via.net else { continue };
            let layers: Vec<LayerId> = copper
                .iter()
                .filter(|&&layer| spans(via, &copper, layer))
                .copied()
                .collect();
            index.push(
                Item::Via(via_index),
                Kind::Via,
                net,
                layers,
                Shape::Disc {
                    center: via.at,
                    radius: via.size / 2.0,
                },
            );
        }
        for (segment_index, segment) in board.segments.iter().enumerate() {
            let Some(net) = segment.net else { continue };
            index.push(
                Item::Segment(segment_index),
                Kind::Track,
                net,
                vec![segment.layer],
                Shape::Stroke {
                    points: vec![segment.start, segment.end],
                    radius: segment.width / 2.0,
                },
            );
        }
        for (arc_index, arc) in board.arcs.iter().enumerate() {
            let Some(net) = arc.net else { continue };
            let points = match Arc::through(arc.start, arc.mid, arc.end) {
                Some(arc) => arc.flatten(TOLERANCE),
                None => vec![arc.start, arc.end],
            };
            index.push(
                Item::Arc(arc_index),
                Kind::Track,
                net,
                vec![arc.layer],
                Shape::Stroke {
                    points,
                    radius: arc.width / 2.0,
                },
            );
        }
        for (zone_index, zone) in board.zones.iter().enumerate() {
            index.zone(
                Item::Zone {
                    footprint: None,
                    zone: zone_index,
                },
                zone,
                &Transform::identity(),
                &copper,
            );
        }
        index
    }

    /// `zone` in `frame`: one entry per stored fill on a copper layer,
    /// and its outline on each copper layer it has no fill on.
    fn zone(
        &mut self,
        item: Item,
        zone: &crate::model::Zone,
        frame: &Transform,
        copper: &[LayerId],
    ) {
        let Some(net) = zone.net else { return };
        if zone.keepout {
            return;
        }
        let on_board = |points: &[Point]| points.iter().map(|&p| frame.apply(p)).collect();
        for fill in zone
            .fills
            .iter()
            .filter(|fill| copper.contains(&fill.layer))
        {
            self.push(
                item,
                Kind::Zone,
                net,
                vec![fill.layer],
                Shape::Polygons(vec![on_board(&fill.points)]),
            );
        }
        for &layer in zone.layers.iter().filter(|layer| copper.contains(layer)) {
            if zone.is_unfilled_on(layer) {
                self.push(
                    item,
                    Kind::Zone,
                    net,
                    vec![layer],
                    Shape::Polygons(vec![on_board(&zone.outline)]),
                );
            }
        }
    }

    /// Add an entry, binned into every cell its box reaches. An item on
    /// no copper layer or with no extent is not indexed.
    fn push(&mut self, item: Item, kind: Kind, net: NetId, layers: Vec<LayerId>, shape: Shape) {
        let Some(bounds) = shape.bounds() else { return };
        if layers.is_empty() {
            return;
        }
        let index = self.entries.len();
        for cell in cells_of(bounds) {
            self.cells.entry(cell).or_default().push(index);
        }
        self.entries.push(Entry {
            item,
            kind,
            net,
            layers,
            bounds,
            shape,
        });
    }

    /// How many items are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The item under `point`, or within `tolerance` millimetres of it,
    /// on any of `layers` (the copper layers shown, top first): the
    /// nearest kind by [`Kind`]'s order, and among those the one on the
    /// topmost layer. `None` over empty space.
    #[must_use]
    pub fn hit(&self, point: Point, tolerance: f64, layers: &[LayerId]) -> Option<Hit> {
        let tolerance = tolerance.max(0.0);
        let reach = grow(Bounds::of(point), tolerance);
        let mut best: Option<(Kind, usize, Hit)> = None;
        let mut seen = Vec::new();
        for cell in cells_of(reach) {
            for &index in self.cells.get(&cell).into_iter().flatten() {
                if seen.contains(&index) {
                    continue;
                }
                seen.push(index);
                let Some(entry) = self.entries.get(index) else {
                    continue;
                };
                if !overlaps(entry.bounds, reach) {
                    continue;
                }
                let Some((depth, layer)) = layers
                    .iter()
                    .enumerate()
                    .find(|(_, layer)| entry.layers.contains(layer))
                else {
                    continue;
                };
                if best
                    .is_some_and(|(kind, best_depth, _)| (kind, best_depth) <= (entry.kind, depth))
                {
                    continue;
                }
                if entry.shape.distance(point) > tolerance {
                    continue;
                }
                best = Some((
                    entry.kind,
                    depth,
                    Hit {
                        item: entry.item,
                        kind: entry.kind,
                        net: entry.net,
                        layer: *layer,
                    },
                ));
            }
        }
        best.map(|(_, _, hit)| hit)
    }
}

/// Every cell `bounds` reaches.
fn cells_of(bounds: Bounds) -> impl Iterator<Item = (i64, i64)> {
    let (x0, x1) = (cell_of(bounds.min.x), cell_of(bounds.max.x));
    let (y0, y1) = (cell_of(bounds.min.y), cell_of(bounds.max.y));
    (x0..=x1).flat_map(move |x| (y0..=y1).map(move |y| (x, y)))
}

/// Whether two boxes share any point.
fn overlaps(a: Bounds, b: Bounds) -> bool {
    a.min.x <= b.max.x && b.min.x <= a.max.x && a.min.y <= b.max.y && b.min.y <= a.max.y
}

/// The polygons `pad` covers on the board, in its footprint's `frame`:
/// its outline as the tessellator draws it, and the filled closed
/// primitives of a custom pad. The hole is not cut out: a click in a
/// through-hole pad's hole is a click on the pad.
fn pad_polygons(pad: &Pad, frame: &Transform) -> Vec<Vec<Point>> {
    let shape = pad_frame(pad, frame);
    let mut polygons = vec![pad_outline(pad, &shape, 0.0, TOLERANCE)];
    for primitive in &pad.primitives {
        if let crate::model::Shape::Polygon { points } = &primitive.shape
            && primitive.filled
        {
            polygons.push(points.iter().map(|&p| shape.apply(p)).collect());
        }
    }
    polygons
}

/// The box around `pad` on the board, in its footprint's `frame`.
#[must_use]
pub fn pad_bounds(pad: &Pad, frame: &Transform) -> Option<Bounds> {
    Bounds::around(
        pad_polygons(pad, frame)
            .into_iter()
            .flat_map(Vec::into_iter),
    )
}

/// One pad of a net, as the pad list names it.
#[derive(Debug, Clone, PartialEq)]
pub struct NetPad {
    /// The footprint's index in [`Board::footprints`].
    pub footprint: usize,
    /// The pad's index in the footprint's pads.
    pub pad: usize,
    /// The footprint's reference designator; empty when it has none.
    pub reference: String,
    /// The pad's number.
    pub number: String,
    /// The box around the pad on the board.
    pub bounds: Bounds,
}

/// Every pad on `net`, in order of reference (its letters, then its
/// number as a number, so `R2` comes before `R10`) and then pad number
/// the same way.
#[must_use]
pub fn net_pads(board: &Board, net: NetId) -> Vec<NetPad> {
    let mut pads = Vec::new();
    for (footprint_index, footprint) in board.footprints.iter().enumerate() {
        let frame = Transform::new(footprint.at, footprint.rotation);
        for (pad_index, pad) in footprint.pads.iter().enumerate() {
            if pad.net != Some(net) {
                continue;
            }
            let Some(bounds) = pad_bounds(pad, &frame) else {
                continue;
            };
            pads.push(NetPad {
                footprint: footprint_index,
                pad: pad_index,
                reference: footprint.reference().unwrap_or_default().to_owned(),
                number: pad.number.clone(),
                bounds,
            });
        }
    }
    pads.sort_by(|a, b| {
        natural_key(&a.reference)
            .cmp(&natural_key(&b.reference))
            .then_with(|| natural_key(&a.number).cmp(&natural_key(&b.number)))
    });
    pads
}

/// `text` as a sort key that orders `R2` before `R10`: its runs of
/// letters and of digits, each digit run compared as a number.
fn natural_key(text: &str) -> Vec<(u64, String)> {
    let mut key = Vec::new();
    let mut run = String::new();
    let mut digits = false;
    let flush = |run: &mut String, digits: bool, key: &mut Vec<(u64, String)>| {
        if run.is_empty() {
            return;
        }
        let number = if digits {
            run.parse::<u64>().unwrap_or(u64::MAX)
        } else {
            0
        };
        key.push((number, std::mem::take(run).to_lowercase()));
    };
    for c in text.chars() {
        if c.is_ascii_digit() != digits {
            flush(&mut run, digits, &mut key);
            digits = c.is_ascii_digit();
        }
        run.push(c);
    }
    flush(&mut run, digits, &mut key);
    key
}

/// The index of the footprint whose reference designator is `reference`,
/// letter case aside; the first when two share it.
#[must_use]
pub fn find_footprint(board: &Board, reference: &str) -> Option<usize> {
    let wanted = reference.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    board.footprints.iter().position(|footprint| {
        footprint
            .reference()
            .is_some_and(|r| r.to_lowercase() == wanted)
    })
}

/// The box around `footprint` on the board: its pads and its graphics
/// on every layer (the courtyard and the silkscreen among them), so a
/// part with pads to one side still frames whole. `None` for a
/// footprint with neither.
#[must_use]
pub fn footprint_bounds(footprint: &Footprint) -> Option<Bounds> {
    let frame = Transform::new(footprint.at, footprint.rotation);
    let pads = footprint
        .pads
        .iter()
        .filter_map(|pad| pad_bounds(pad, &frame));
    let graphics = footprint
        .graphics
        .iter()
        .filter_map(|graphic| graphic_bounds(&transformed(&graphic.drawing, &frame), TOLERANCE));
    pads.chain(graphics).reduce(Bounds::union)
}

/// `board` with only the copper of `net`: the segments, arcs, vias and
/// zones on it, and each footprint with only its pads on it, with no
/// graphics, text or other zones. The layer and net tables stay, so the
/// result tessellates like the board. What a highlight of the net is
/// drawn from.
#[must_use]
pub fn net_only(board: &Board, net: NetId) -> Board {
    let on_net = |item: Option<NetId>| item == Some(net);
    Board {
        version: board.version,
        generator: board.generator.clone(),
        generator_version: board.generator_version.clone(),
        thickness: board.thickness,
        layers: board.layers.clone(),
        nets: board.nets.clone(),
        footprints: board
            .footprints
            .iter()
            .filter(|footprint| {
                footprint.pads.iter().any(|pad| on_net(pad.net))
                    || footprint.zones.iter().any(|zone| on_net(zone.net))
            })
            .map(|footprint| Footprint {
                name: footprint.name.clone(),
                layer: footprint.layer,
                at: footprint.at,
                rotation: footprint.rotation,
                attributes: footprint.attributes,
                properties: Vec::new(),
                texts: Vec::new(),
                graphics: Vec::new(),
                pads: footprint
                    .pads
                    .iter()
                    .filter(|pad| on_net(pad.net))
                    .cloned()
                    .collect(),
                zones: footprint
                    .zones
                    .iter()
                    .filter(|zone| on_net(zone.net) && !zone.keepout)
                    .cloned()
                    .collect(),
                clearances: footprint.clearances,
            })
            .collect(),
        graphics: Vec::new(),
        texts: Vec::new(),
        segments: board
            .segments
            .iter()
            .filter(|segment| on_net(segment.net))
            .cloned()
            .collect(),
        arcs: board
            .arcs
            .iter()
            .filter(|arc| on_net(arc.net))
            .cloned()
            .collect(),
        vias: board
            .vias
            .iter()
            .filter(|via| on_net(via.net))
            .cloned()
            .collect(),
        zones: board
            .zones
            .iter()
            .filter(|zone| on_net(zone.net) && !zone.keepout)
            .cloned()
            .collect(),
        clearances: board.clearances,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use test_case::test_case;

    use super::*;
    use crate::parse_board;

    const BOARD_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_pcb"
    );

    fn hand_off() -> &'static Board {
        static BOARD: OnceLock<Board> = OnceLock::new();
        BOARD.get_or_init(|| {
            let source = std::fs::read_to_string(BOARD_PATH).expect("the hand-off board");
            parse_board(&source).expect("parses")
        })
    }

    fn hand_off_index() -> &'static Index {
        static INDEX: OnceLock<Index> = OnceLock::new();
        INDEX.get_or_init(|| Index::build(hand_off()))
    }

    /// A four-layer board around `items`.
    fn board(items: &str) -> Board {
        let text = format!(
            r#"(kicad_pcb (version 20241229) (generator "pcbnew")
              (layers (0 "F.Cu" signal) (1 "In1.Cu" signal) (2 "In2.Cu" signal)
                      (3 "B.Cu" signal) (37 "F.SilkS" user) (44 "Edge.Cuts" user) (46 "F.CrtYd" user))
              {items})"#
        );
        parse_board(&text).expect("the synthetic board parses")
    }

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// Item `index` of `items`: the crate forbids indexing, and a missing
    /// item is a failed test, not a panic in the lookup.
    fn item<T>(items: &[T], index: usize) -> &T {
        items
            .get(index)
            .unwrap_or_else(|| panic!("no item {index} among {}", items.len()))
    }

    fn layer(board: &Board, name: &str) -> LayerId {
        board.layer_named(name).expect("the layer exists")
    }

    fn net(board: &Board, name: &str) -> NetId {
        let index = board
            .nets
            .iter()
            .position(|net| net.name == name)
            .expect("the net exists");
        NetId(u32::try_from(index).expect("fits"))
    }

    /// A pad of R1 at (10, 10), a via, a track and a zone fill, all on
    /// GND and all under (10, 10) on F.Cu; the track runs on to (20, 10)
    /// and the zone fills (0, 0) to (30, 30). A second track on B.Cu, net
    /// VCC, from (10, 20) to (20, 20).
    fn stacked() -> Board {
        board(
            r#"(footprint "T" (layer "F.Cu") (at 10 10) (property "Reference" "R1" (at 0 0) (layer "F.SilkS"))
                 (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "GND")))
               (via (at 10 10) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net "GND"))
               (segment (start 10 10) (end 20 10) (width 0.2) (layer "F.Cu") (net "GND"))
               (segment (start 10 20) (end 20 20) (width 0.2) (layer "B.Cu") (net "VCC"))
               (zone (net "GND") (layer "F.Cu") (polygon (pts (xy 0 0) (xy 30 0) (xy 30 30) (xy 0 30)))
                 (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 30 0) (xy 30 30) (xy 0 30))))"#,
        )
    }

    #[test_case((10.0, 10.0), 0.0, &["F.Cu", "B.Cu"], Some(Kind::Pad), Some("F.Cu"); "the pad wins over everything under it")]
    #[test_case((10.0, 10.0), 0.0, &["B.Cu"], Some(Kind::Via), Some("B.Cu"); "on the back only the via reaches")]
    #[test_case((15.0, 10.0), 0.0, &["F.Cu"], Some(Kind::Track), Some("F.Cu"); "the track wins over the zone")]
    #[test_case((15.0, 10.3), 0.0, &["F.Cu"], Some(Kind::Zone), Some("F.Cu"); "just off the track is the zone")]
    #[test_case((15.0, 10.3), 0.25, &["F.Cu"], Some(Kind::Track), Some("F.Cu"); "a tolerance reaches the track")]
    #[test_case((15.0, 20.0), 0.0, &["F.Cu"], Some(Kind::Zone), Some("F.Cu"); "a hidden layer's track is not hit")]
    #[test_case((15.0, 20.0), 0.0, &["F.Cu", "B.Cu"], Some(Kind::Track), Some("B.Cu"); "the back track shows through the zone")]
    #[test_case((40.0, 40.0), 0.0, &["F.Cu", "B.Cu"], None, None; "empty space")]
    #[test_case((31.0, 10.0), 0.5, &["F.Cu"], None, None; "just past the zone with a small tolerance")]
    #[test_case((31.0, 10.0), 1.5, &["F.Cu"], Some(Kind::Zone), Some("F.Cu"); "a tolerance reaches the zone's edge")]
    fn the_topmost_item_of_the_best_kind_is_hit(
        point: (f64, f64),
        tolerance: f64,
        layers: &[&str],
        kind: Option<Kind>,
        on: Option<&str>,
    ) {
        let board = stacked();
        let index = Index::build(&board);
        let layers: Vec<LayerId> = layers.iter().map(|name| layer(&board, name)).collect();
        let hit = index.hit(at(point.0, point.1), tolerance, &layers);
        assert_eq!(hit.map(|hit| hit.kind), kind, "{hit:?}");
        assert_eq!(hit.map(|hit| hit.layer), on.map(|name| layer(&board, name)));
        if let Some(hit) = hit {
            let expected = if hit.kind == Kind::Track && on == Some("B.Cu") {
                "VCC"
            } else {
                "GND"
            };
            assert_eq!(hit.net, net(&board, expected));
        }
    }

    #[test]
    fn a_hit_names_the_item_in_the_model() {
        let board = stacked();
        let index = Index::build(&board);
        let f_cu = [layer(&board, "F.Cu")];
        assert_eq!(index.len(), 5);
        assert_eq!(
            index.hit(at(10.0, 10.0), 0.0, &f_cu).map(|hit| hit.item),
            Some(Item::Pad {
                footprint: 0,
                pad: 0
            })
        );
        assert_eq!(
            index.hit(at(15.0, 10.0), 0.0, &f_cu).map(|hit| hit.item),
            Some(Item::Segment(0))
        );
        assert_eq!(
            index.hit(at(25.0, 25.0), 0.0, &f_cu).map(|hit| hit.item),
            Some(Item::Zone {
                footprint: None,
                zone: 0
            })
        );
        assert_eq!(index.hit(at(10.0, 10.0), 0.0, &[]), None, "no layers shown");
    }

    #[test]
    fn a_turned_pad_an_arc_and_an_unfilled_zone_are_measured_by_their_shapes() {
        // A 4 by 1 pad turned 90 degrees reaches (10, 8) and not (12, 10).
        let board = board(
            r#"(footprint "T" (layer "F.Cu") (at 10 10 90)
                 (pad "1" smd rect (at 0 0 90) (size 4 1) (layers "F.Cu") (net "A")))
               (arc (start 20 10) (mid 25 5) (end 30 10) (width 0.5) (layer "F.Cu") (net "B"))
               (zone (net "C") (layer "B.Cu") (polygon (pts (xy 40 0) (xy 50 0) (xy 50 10) (xy 40 10))))"#,
        );
        let index = Index::build(&board);
        let f_cu = [layer(&board, "F.Cu")];
        let b_cu = [layer(&board, "B.Cu")];
        let kind = |point: Point, layers: &[LayerId]| index.hit(point, 0.0, layers).map(|h| h.kind);
        assert_eq!(kind(at(10.0, 8.0), &f_cu), Some(Kind::Pad));
        assert_eq!(kind(at(12.0, 10.0), &f_cu), None);
        // The arc's top is at (25, 5).
        assert_eq!(kind(at(25.0, 5.1), &f_cu), Some(Kind::Track));
        assert_eq!(kind(at(25.0, 7.0), &f_cu), None);
        // The unfilled zone is hit by its outline, on its own layer.
        assert_eq!(kind(at(45.0, 5.0), &b_cu), Some(Kind::Zone));
        assert_eq!(kind(at(45.0, 5.0), &f_cu), None);
    }

    #[test]
    fn items_without_a_net_and_keepouts_are_not_indexed() {
        let board = board(
            r#"(footprint "T" (layer "F.Cu") (at 10 10)
                 (pad "" np_thru_hole circle (at 0 0) (size 3 3) (drill 3) (layers "*.Cu")))
               (segment (start 20 10) (end 30 10) (width 0.2) (layer "F.Cu"))
               (zone (layer "F.Cu") (keepout (tracks not_allowed)) (polygon (pts (xy 0 0) (xy 5 0) (xy 5 5) (xy 0 5))))"#,
        );
        assert!(Index::build(&board).is_empty());
    }

    #[test]
    fn the_hand_off_boards_pads_vias_and_zones_are_where_the_file_puts_them() {
        let board = hand_off();
        let index = hand_off_index();
        let f_cu = [layer(board, "F.Cu")];
        // LED6 sits at (120, 53.5); its pad 3 at (2.45, 1.65) is GND.
        let hit = index
            .hit(at(122.45, 55.15), 0.0, &f_cu)
            .expect("LED6's pad 3");
        assert_eq!(hit.kind, Kind::Pad);
        assert_eq!(hit.net, net(board, "GND"));
        let Item::Pad { footprint, pad } = hit.item else {
            panic!("{hit:?}");
        };
        assert_eq!(item(&board.footprints, footprint).reference(), Some("LED6"));
        assert_eq!(
            item(&item(&board.footprints, footprint).pads, pad).number,
            "3"
        );
        // The first via in the file, at (115.7, 51.8), on LED5's VDD.
        let via = index.hit(at(115.7, 51.8), 0.0, &f_cu).expect("a via");
        assert_eq!(via.kind, Kind::Via);
        assert_eq!(via.net, net(board, "Net-(LED5-VDD)"));
        // Outside the outline is empty space, with any tolerance a
        // screen allows.
        assert_eq!(index.hit(at(40.0, 40.0), 0.5, &f_cu), None);
        // Everything with a net is indexed: the board's pads with a net,
        // its vias, segments, and a fill per zone per layer.
        let pads = board.all_pads().filter(|pad| pad.net.is_some()).count();
        let vias = board.vias.iter().filter(|via| via.net.is_some()).count();
        let segments = board.segments.len();
        assert!(index.len() > pads + vias + segments, "{}", index.len());
        assert!(
            index.len() < pads + vias + segments + 100,
            "{}",
            index.len()
        );
    }

    #[test]
    fn the_pads_of_a_net_are_listed_by_reference_and_number_in_natural_order() {
        let board = board(
            r#"(footprint "T" (layer "F.Cu") (at 10 10) (property "Reference" "R10" (at 0 0) (layer "F.SilkS"))
                 (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net "GND"))
                 (pad "1" smd rect (at -1 0) (size 1 1) (layers "F.Cu") (net "VCC")))
               (footprint "T" (layer "F.Cu") (at 20 10) (property "Reference" "R2" (at 0 0) (layer "F.SilkS"))
                 (pad "10" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net "GND"))
                 (pad "9" smd rect (at -1 0) (size 1 1) (layers "F.Cu") (net "GND")))"#,
        );
        let pads = net_pads(&board, net(&board, "GND"));
        let names: Vec<(String, String)> = pads
            .iter()
            .map(|pad| (pad.reference.clone(), pad.number.clone()))
            .collect();
        assert_eq!(
            names,
            [("R2", "9"), ("R2", "10"), ("R10", "2")].map(|(r, n)| (r.to_owned(), n.to_owned()))
        );
        let first = item(&pads, 0);
        assert_eq!((first.footprint, first.pad), (1, 1));
        assert!((first.bounds.center().x - 19.0).abs() < 1e-9);
        assert!((first.bounds.width() - 1.0).abs() < 1e-9);
        assert_eq!(net_pads(&board, net(&board, "VCC")).len(), 1);
    }

    #[test_case("led6", Some("LED6"); "lower case finds it")]
    #[test_case("LED6", Some("LED6"); "as written")]
    #[test_case(" sw7 ", Some("SW7"); "spaces around are ignored")]
    #[test_case("LED60", None; "a reference the board lacks")]
    #[test_case("", None; "nothing asked")]
    fn a_reference_is_found_whatever_its_case(query: &str, found: Option<&str>) {
        let board = hand_off();
        let index = find_footprint(board, query);
        assert_eq!(
            index.and_then(|i| item(&board.footprints, i).reference()),
            found
        );
    }

    #[test]
    fn a_footprints_box_holds_its_pads_and_its_graphics() {
        let board = board(
            r#"(footprint "T" (layer "F.Cu") (at 10 10 90)
                 (fp_rect (start -3 -1) (end 3 1) (stroke (width 0.05) (type solid)) (fill no) (layer "F.CrtYd"))
                 (pad "1" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net "A")))"#,
        );
        let bounds = footprint_bounds(item(&board.footprints, 0)).expect("a box");
        // Turned 90 degrees: the courtyard spans y 7..13 and x 9..11.
        assert!((bounds.min.x - 9.0).abs() < 1e-6, "{bounds:?}");
        assert!((bounds.max.x - 11.0).abs() < 1e-6, "{bounds:?}");
        assert!((bounds.min.y - 7.0).abs() < 1e-6, "{bounds:?}");
        assert!((bounds.max.y - 13.0).abs() < 1e-6, "{bounds:?}");
        let led6 = find_footprint(hand_off(), "LED6").expect("LED6");
        let led6 = footprint_bounds(item(&hand_off().footprints, led6)).expect("a box");
        assert!(led6.min.x < 117.55 && led6.max.x > 123.2, "{led6:?}");
        assert!(led6.width() < 10.0 && led6.height() < 10.0, "{led6:?}");
    }

    #[test]
    fn a_nets_board_keeps_only_its_copper() {
        let board = stacked();
        let gnd = net_only(&board, net(&board, "GND"));
        assert_eq!(gnd.layers, board.layers);
        assert_eq!(gnd.nets, board.nets);
        assert_eq!(gnd.segments.len(), 1);
        assert_eq!(gnd.vias.len(), 1);
        assert_eq!(gnd.zones.len(), 1);
        assert_eq!(gnd.footprints.len(), 1);
        let footprint = item(&gnd.footprints, 0);
        assert_eq!(footprint.pads.len(), 1);
        assert!(footprint.texts.is_empty() && footprint.graphics.is_empty());
        let vcc = net_only(&board, net(&board, "VCC"));
        assert_eq!(vcc.segments.len(), 1);
        assert!(vcc.vias.is_empty() && vcc.zones.is_empty() && vcc.footprints.is_empty());
        // The hand-off's GND reaches every copper layer through its fill.
        let hand_off_gnd = net_only(hand_off(), net(hand_off(), "GND"));
        assert!(hand_off_gnd.zones.iter().any(|zone| zone.layers.len() == 4));
        assert!(hand_off_gnd.footprints.len() > 10);
    }
}
