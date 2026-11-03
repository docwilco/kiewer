//! Net labels: where a pad's number, a pad's net, a via's net, a track
//! run's net and a zone's net are written on each copper layer, and how
//! large (docs/v1-scope.md, Net labels). Pure functions over the model,
//! run once at parse; the viewer draws every label here and decides per
//! frame, on the GPU, which are large enough on screen to show.
//!
//! The rules, with their constants: a track's text is
//! [`HEIGHT_PER_TRACK_WIDTH`] of its width and is never shrunk to fit
//! its length; a [`Run`] (connected, near-collinear segments of one net
//! on one layer) gets a label only when it is [`RUN_GATE`] text lengths
//! long, one label repeated every [`REPEAT`] text lengths; a via's label
//! fits the via's diameter; a zone gets one label at the pole of
//! inaccessibility of its fill ([`pole_of_inaccessibility`]); a pad shows
//! its number first and its net second only when both fit. Angles are
//! KiCad's (positive turns counter-clockwise on the screen) folded into
//! (-90, 90] by [`normalize_angle`], so a near-vertical label reads
//! bottom to top. Collisions are resolved once, pads over vias over zones
//! over tracks: a track label shifts along its run, then is dropped;
//! anything else that collides with a label of higher priority is
//! dropped ([`labels`]). A mirrored view is the viewer's business: it
//! flips x and negates the angle when it draws.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use crate::font;
use crate::geometry::{Bounds, Transform};
use crate::model::{Board, LayerId, NetId, Pad, Point, Segment, Size};
use crate::tessellate::spans;

/// A track label's text height as a fraction of the track's width.
pub const HEIGHT_PER_TRACK_WIDTH: f64 = 0.7;
/// A run gets a label only when its length is at least this many text
/// lengths.
pub const RUN_GATE: f64 = 1.3;
/// Along a long run, labels repeat about every this many text lengths.
pub const REPEAT: f64 = 8.0;
/// Below this text height, in millimetres, a label is dropped: the
/// hand-off's threshold, under which a label is noise at any zoom.
pub const MIN_HEIGHT: f64 = 0.08;
/// How much of a pad's long side a pad label may span.
pub const PAD_ALONG: f64 = 0.95;
/// How much of a pad's short side a pad label may be tall: two stacked
/// lines fit.
pub const PAD_ACROSS: f64 = 0.45;
/// Where the two lines of a pad sit, as a fraction of its short side on
/// either side of its centre.
pub const PAD_STACK: f64 = 0.25;
/// A pad shows its net beside its number only when the shared height is
/// at least this fraction of the number's own: a long net name does not
/// shrink the number to nothing.
pub const PAD_SHARED_MIN: f64 = 0.5;
/// A via's label is at most this fraction of its diameter tall, and no
/// longer than the diameter.
pub const VIA_HEIGHT_PER_SIZE: f64 = 0.45;
/// How much of the pole's clearance a zone label may use.
pub const ZONE_FIT: f64 = 0.9;
/// A zone label's height is capped here, in millimetres, whatever the
/// zone's size.
pub const ZONE_MAX_HEIGHT: f64 = 2.0;
/// Segment ends this close, in millimetres, are one point.
pub const JOIN_TOLERANCE: f64 = 0.01;
/// Two segments meeting end to end continue one run when their
/// directions differ by no more than this, in degrees.
pub const RUN_BEND: f64 = 5.0;
/// A colliding track label moves along its run in steps of this many
/// text lengths, alternating sides, until it is clear or off the run.
pub const SHIFT_STEP: f64 = 0.5;
/// The pen a label's glyphs are stroked with, as a fraction of the text
/// height: Newstroke's default for a text as wide as it is tall.
pub const PEN_PER_HEIGHT: f64 = 1.0 / 8.0;
/// The halo around a label's glyphs, as a fraction of the text height:
/// the pen the same strokes are drawn with first, in the page's dark.
pub const HALO_PER_HEIGHT: f64 = 1.0 / 4.0;
/// The baseline's distance below a label's centre, in text heights:
/// KiCad's centred text, a height less half its 1.17 first-line height.
pub const BASELINE_BELOW_CENTER: f64 = 1.0 - font::FIRST_LINE_HEIGHT / 2.0;

/// What a label names, and which toggle shows it, in the order the
/// toggles are listed. The rank at collision time is
/// [`LabelClass::priority`], which is not this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LabelClass {
    /// A pad's number, inside the pad.
    PadNumber,
    /// A pad's net, inside the pad under the number.
    PadNet,
    /// A via's net, inside the via.
    ViaNet,
    /// A track run's net, along the run.
    TrackNet,
    /// A zone's net, at the pole of inaccessibility of its fill.
    ZoneNet,
}

impl LabelClass {
    /// Every class, in toggle order.
    pub const ALL: [LabelClass; 5] = [
        LabelClass::PadNumber,
        LabelClass::PadNet,
        LabelClass::ViaNet,
        LabelClass::TrackNet,
        LabelClass::ZoneNet,
    ];

    /// The class's rank at collision time: lower wins. Both pad classes
    /// rank first, then vias, zones, tracks.
    #[must_use]
    pub fn priority(self) -> u8 {
        match self {
            LabelClass::PadNumber | LabelClass::PadNet => 0,
            LabelClass::ViaNet => 1,
            LabelClass::ZoneNet => 2,
            LabelClass::TrackNet => 3,
        }
    }
}

/// One label as placed: text, where, how large, how turned.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    /// What it says: a pad number or a net name.
    pub text: String,
    /// What it names.
    pub class: LabelClass,
    /// The net it belongs to; `None` for the number of an unconnected
    /// pad.
    pub net: Option<NetId>,
    /// The copper layer it is drawn on.
    pub layer: LayerId,
    /// The centre of the text, millimetres.
    pub position: Point,
    /// Degrees, counter-clockwise on the screen, in (-90, 90].
    pub angle: f64,
    /// Text height, millimetres.
    pub height: f64,
    /// Text length at that height, millimetres ([`text_length`]).
    pub length: f64,
}

impl Label {
    /// A label of `text` at `height`, its length measured.
    #[must_use]
    pub fn new(
        text: &str,
        class: LabelClass,
        net: Option<NetId>,
        layer: LayerId,
        position: Point,
        angle: f64,
        height: f64,
    ) -> Self {
        Self {
            text: text.to_owned(),
            length: text_length(text, height),
            class,
            net,
            layer,
            position,
            angle,
            height,
        }
    }

    /// The four corners of the text's box, turned with it.
    #[must_use]
    pub fn corners(&self) -> [Point; 4] {
        let turn = Transform::new(self.position, self.angle);
        let (hx, hy) = (self.length / 2.0, self.height / 2.0);
        [
            turn.apply(Point { x: -hx, y: -hy }),
            turn.apply(Point { x: hx, y: -hy }),
            turn.apply(Point { x: hx, y: hy }),
            turn.apply(Point { x: -hx, y: hy }),
        ]
    }

    /// The axis-aligned box around the corners.
    fn bounds(&self) -> Bounds {
        let [a, b, c, d] = self.corners();
        Bounds::of(a).including(b).including(c).including(d)
    }

    /// Whether the two boxes overlap, by the separating axis test over
    /// both boxes' edges. Boxes that merely touch do not overlap.
    #[must_use]
    pub fn overlaps(&self, other: &Label) -> bool {
        let a = self.corners();
        let b = other.corners();
        let axes = edge_axes(&a).into_iter().chain(edge_axes(&b));
        for axis in axes {
            let (min_a, max_a) = project(&a, axis);
            let (min_b, max_b) = project(&b, axis);
            if max_a <= min_b + 1e-9 || max_b <= min_a + 1e-9 {
                return false;
            }
        }
        true
    }
}

/// The directions of a box's two distinct edges.
fn edge_axes(corners: &[Point; 4]) -> [Point; 2] {
    let [a, b, _, d] = corners;
    [
        Point {
            x: b.x - a.x,
            y: b.y - a.y,
        },
        Point {
            x: d.x - a.x,
            y: d.y - a.y,
        },
    ]
}

/// The least and the greatest dot product of the corners with `axis`.
fn project(corners: &[Point; 4], axis: Point) -> (f64, f64) {
    corners
        .iter()
        .map(|p| p.x * axis.x + p.y * axis.y)
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), d| {
            (lo.min(d), hi.max(d))
        })
}

/// A chain of connected, near-collinear segments of one net on one
/// layer: the unit a track label is placed on.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// The copper layer.
    pub layer: LayerId,
    /// The net.
    pub net: NetId,
    /// The narrowest segment's width.
    pub width: f64,
    /// The polyline, one point per segment end, in order.
    pub points: Vec<Point>,
}

impl Run {
    /// The length along the polyline.
    #[must_use]
    pub fn length(&self) -> f64 {
        self.points
            .windows(2)
            .map(|pair| match pair {
                [a, b] => (b.x - a.x).hypot(b.y - a.y),
                _ => 0.0,
            })
            .sum()
    }

    /// The point `along` millimetres from the start (clamped to the run)
    /// and the label angle of the segment there: the segment's direction
    /// in KiCad's sense, folded by [`normalize_angle`].
    #[must_use]
    pub fn at(&self, along: f64) -> (Point, f64) {
        let mut remaining = along.max(0.0);
        let mut last = None;
        for pair in self.points.windows(2) {
            let [a, b] = pair else { continue };
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let length = dx.hypot(dy);
            if length <= 0.0 {
                continue;
            }
            let angle = normalize_angle(-dy.atan2(dx).to_degrees());
            if remaining <= length {
                let t = remaining / length;
                return (
                    Point {
                        x: a.x + dx * t,
                        y: a.y + dy * t,
                    },
                    angle,
                );
            }
            remaining -= length;
            last = Some((*b, angle));
        }
        last.unwrap_or_else(|| (self.points.first().copied().unwrap_or_default(), 0.0))
    }
}

/// `degrees` folded into (-90, 90]: a label never reads upside down,
/// and a vertical one reads bottom to top.
#[must_use]
pub fn normalize_angle(degrees: f64) -> f64 {
    let angle = degrees.rem_euclid(360.0);
    if angle > 90.0 && angle <= 270.0 {
        angle - 180.0
    } else if angle > 270.0 {
        angle - 360.0
    } else {
        angle
    }
}

/// The length of `text` in Newstroke at `height`, as wide as it is tall:
/// what a label's box is long.
#[must_use]
pub fn text_length(text: &str, height: f64) -> f64 {
    font::block_size(
        text,
        Size {
            width: height,
            height,
        },
    )
    .width
}

/// Where the pen is at the start of each character of `text` at
/// `height`, in millimetres from the text's left end, with the
/// character: the glyph positions a label is drawn from. A space moves
/// the pen and is not listed.
#[must_use]
pub fn glyph_positions(text: &str, height: f64) -> Vec<(char, f64)> {
    font::glyph_cursors(text, height)
        .into_iter()
        .filter(|(c, _)| !matches!(c, '\0' | ' ' | '\t'))
        .collect()
}

/// The text height that fits `text` within `along` by `across`: the
/// height at which its length is `along`, capped at `across`.
fn fit(text: &str, along: f64, across: f64) -> f64 {
    let per_height = text_length(text, 1.0);
    if per_height <= 0.0 {
        across
    } else {
        across.min(along / per_height)
    }
}

/// A placed label with, for a track label, the run it sits on and how
/// far along it: what a collision shifts.
struct Candidate {
    label: Label,
    run: Option<(usize, f64)>,
}

/// One end of a segment: its index among the segments with a net, and
/// 0 for its start or 1 for its end.
type SegmentEnd = (usize, usize);

/// The point at `end` of `segment`.
fn end_point(segment: &Segment, end: usize) -> Point {
    if end == 0 { segment.start } else { segment.end }
}

/// The direction `segment` leaves `end` in.
fn leaving(segment: &Segment, end: usize) -> Point {
    let (from, to) = if end == 0 {
        (segment.start, segment.end)
    } else {
        (segment.end, segment.start)
    };
    Point {
        x: to.x - from.x,
        y: to.y - from.y,
    }
}

/// For every segment, the segment end that continues it past each of
/// its ends, when one does: the segments of one net on one layer whose
/// ends meet within [`JOIN_TOLERANCE`] are clustered, and in each
/// cluster the pair whose directions differ least, by at most
/// [`RUN_BEND`], is linked.
fn links(segments: &[&Segment]) -> Vec<[Option<SegmentEnd>; 2]> {
    let mut clusters: BTreeMap<(LayerId, NetId, i64, i64), Vec<SegmentEnd>> = BTreeMap::new();
    for (index, segment) in segments.iter().enumerate() {
        let Some(net) = segment.net else { continue };
        for end in 0..2 {
            let point = end_point(segment, end);
            clusters
                .entry((segment.layer, net, quantise(point.x), quantise(point.y)))
                .or_default()
                .push((index, end));
        }
    }
    let mut links: Vec<[Option<SegmentEnd>; 2]> = vec![[None, None]; segments.len()];
    for ends in clusters.values() {
        let mut best: Option<(f64, SegmentEnd, SegmentEnd)> = None;
        for (i, &a) in ends.iter().enumerate() {
            for &b in ends.iter().skip(i + 1) {
                let (Some(da), Some(db)) = (
                    segments.get(a.0).map(|s| leaving(s, a.1)),
                    segments.get(b.0).map(|s| leaving(s, b.1)),
                ) else {
                    continue;
                };
                // Continuing means leaving the point in opposite ways.
                let cos = -(da.x * db.x + da.y * db.y) / (da.x.hypot(da.y) * db.x.hypot(db.y));
                let bend = cos.clamp(-1.0, 1.0).acos().to_degrees();
                if bend <= RUN_BEND && best.is_none_or(|(least, _, _)| bend < least) {
                    best = Some((bend, a, b));
                }
            }
        }
        if let Some((_, a, b)) = best {
            if let Some(link) = links.get_mut(a.0).and_then(|ends| ends.get_mut(a.1)) {
                *link = Some(b);
            }
            if let Some(link) = links.get_mut(b.0).and_then(|ends| ends.get_mut(b.1)) {
                *link = Some(a);
            }
        }
    }
    links
}

/// Every segment of `board` with a net chained into runs: two segments
/// of one net on one layer whose ends meet within [`JOIN_TOLERANCE`]
/// continue each other when their directions differ by at most
/// [`RUN_BEND`]; where more than two meet, the straightest pair
/// continues and the rest start runs of their own.
#[must_use]
pub fn runs(board: &Board) -> Vec<Run> {
    let segments: Vec<&Segment> = board
        .segments
        .iter()
        .filter(|segment| {
            segment.net.is_some()
                && (segment.end.x - segment.start.x).hypot(segment.end.y - segment.start.y) > 0.0
        })
        .collect();
    let links = links(&segments);
    let link = |index: usize, end: usize| -> Option<SegmentEnd> {
        links.get(index)?.get(end).copied().flatten()
    };
    let mut visited = vec![false; segments.len()];
    let mut runs = Vec::new();
    for first in 0..segments.len() {
        if visited.get(first).copied().unwrap_or(true) {
            continue;
        }
        // Walk back from the first segment's start to the run's start:
        // leaving a segment by `entered`, the link arrives at the next
        // segment's end, and the walk goes on from its other end.
        let (mut start, mut entered) = (first, 0);
        let mut seen = vec![first];
        while let Some((next, end)) = link(start, entered) {
            if seen.contains(&next) {
                break;
            }
            seen.push(next);
            start = next;
            entered = 1 - end;
        }
        // Forward from there: the dead end is the run's start, and the
        // walk leaves each segment by the end it did not enter through.
        let Some(segment) = segments.get(start) else {
            continue;
        };
        if visited.get(start).copied().unwrap_or(true) {
            continue;
        }
        let mut run = Run {
            layer: segment.layer,
            net: segment.net.unwrap_or(NetId(0)),
            width: f64::INFINITY,
            points: vec![end_point(segment, entered)],
        };
        let (mut current, mut leave_by) = (start, 1 - entered);
        while let Some(segment) = segments.get(current) {
            if visited.get(current).copied().unwrap_or(true) {
                break;
            }
            if let Some(flag) = visited.get_mut(current) {
                *flag = true;
            }
            run.width = run.width.min(segment.width);
            run.points.push(end_point(segment, leave_by));
            match link(current, leave_by) {
                Some((next, end)) => {
                    current = next;
                    leave_by = 1 - end;
                }
                None => break,
            }
        }
        runs.push(run);
    }
    runs
}

/// A coordinate as a key: its position in [`JOIN_TOLERANCE`] steps.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a board coordinate in hundredths of a millimetre is far below i64's range"
)]
fn quantise(value: f64) -> i64 {
    (value / JOIN_TOLERANCE).round() as i64
}

/// The labels of `board` before collisions are resolved: every pad,
/// via, run and zone label the rules place. For counting and testing;
/// [`labels`] is what the viewer draws.
#[must_use]
pub fn candidates(board: &Board) -> Vec<Label> {
    place(board)
        .0
        .into_iter()
        .map(|candidate| candidate.label)
        .collect()
}

/// The labels of `board`, placed and resolved: the candidates in
/// priority order (pads, vias, zones, tracks), each kept when it
/// overlaps no label kept before it on its layer; a track label that
/// does moves along its run in [`SHIFT_STEP`] text lengths, alternating
/// sides, and is dropped when no position on the run is clear.
#[must_use]
pub fn labels(board: &Board) -> Vec<Label> {
    let (candidates, runs) = place(board);
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&index| {
        candidates
            .get(index)
            .map_or(u8::MAX, |candidate| candidate.label.class.priority())
    });
    let mut kept = Kept::default();
    for index in order {
        let Some(candidate) = candidates.get(index) else {
            continue;
        };
        if !kept.collides(&candidate.label) {
            kept.push(candidate.label.clone());
            continue;
        }
        let Some((run_index, along)) = candidate.run else {
            continue;
        };
        let Some(run) = runs.get(run_index) else {
            continue;
        };
        let length = run.length();
        let margin = RUN_GATE * candidate.label.length / 2.0;
        let step = SHIFT_STEP * candidate.label.length;
        // Further out each round, both ways, while either way is still
        // on the run; the first clear spot wins.
        let mut k = 1.0;
        'shifting: loop {
            let tries = [along + k * step, along - k * step]
                .into_iter()
                .filter(|&try_along| try_along >= margin && try_along <= length - margin)
                .collect::<Vec<f64>>();
            if tries.is_empty() {
                break;
            }
            for try_along in tries {
                let (position, angle) = run.at(try_along);
                let moved = Label {
                    position,
                    angle,
                    ..candidate.label.clone()
                };
                if !kept.collides(&moved) {
                    kept.push(moved);
                    break 'shifting;
                }
            }
            k += 1.0;
        }
    }
    kept.labels
}

/// The size of a cell of the collision grid, in millimetres.
const GRID_CELL: f64 = 1.0;

/// The labels kept so far, indexed by layer and grid cell.
#[derive(Default)]
struct Kept {
    labels: Vec<Label>,
    grid: BTreeMap<(LayerId, i64, i64), Vec<usize>>,
}

impl Kept {
    /// The cells `label`'s box reaches.
    fn cells(label: &Label) -> impl Iterator<Item = (LayerId, i64, i64)> {
        let bounds = label.bounds();
        let (x0, x1) = (cell_of(bounds.min.x), cell_of(bounds.max.x));
        let (y0, y1) = (cell_of(bounds.min.y), cell_of(bounds.max.y));
        let layer = label.layer;
        (x0..=x1).flat_map(move |x| (y0..=y1).map(move |y| (layer, x, y)))
    }

    fn collides(&self, label: &Label) -> bool {
        let mut checked = Vec::new();
        for cell in Self::cells(label) {
            for &index in self.grid.get(&cell).into_iter().flatten() {
                if checked.contains(&index) {
                    continue;
                }
                checked.push(index);
                if self
                    .labels
                    .get(index)
                    .is_some_and(|kept| kept.overlaps(label))
                {
                    return true;
                }
            }
        }
        false
    }

    fn push(&mut self, label: Label) {
        let index = self.labels.len();
        for cell in Self::cells(&label) {
            self.grid.entry(cell).or_default().push(index);
        }
        self.labels.push(label);
    }
}

/// The grid cell a coordinate falls in.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a board coordinate in millimetres is far below i64's range"
)]
fn cell_of(value: f64) -> i64 {
    (value / GRID_CELL).floor() as i64
}

/// Every label the rules place, with the runs the track labels sit on.
fn place(board: &Board) -> (Vec<Candidate>, Vec<Run>) {
    let mut placer = Placer {
        board,
        copper: board.copper_layers().map(|(id, _)| id).collect(),
        out: Vec::new(),
    };
    placer.pads();
    placer.vias();
    placer.zones();
    let runs = runs(board);
    placer.tracks(&runs);
    (placer.out, runs)
}

/// The placement in progress: the board, its copper layers in stack
/// order, and the labels placed so far.
struct Placer<'a> {
    board: &'a Board,
    copper: Vec<LayerId>,
    out: Vec<Candidate>,
}

/// One line of a pad's label: its text, class, height, and how far from
/// the pad's centre it sits along the text's own down direction.
struct PadLine {
    text: String,
    class: LabelClass,
    height: f64,
    offset: f64,
}

impl Placer<'_> {
    /// The name of `net`, or `None` for no net or an empty name.
    fn net_name(&self, net: Option<NetId>) -> Option<String> {
        let name = &self.board.net(net?)?.name;
        (!name.is_empty()).then(|| name.clone())
    }

    /// Place a label that no collision moves.
    fn push(&mut self, label: Label) {
        self.out.push(Candidate { label, run: None });
    }

    /// The copper layers among `layers`.
    fn copper_among(&self, layers: &[LayerId]) -> Vec<LayerId> {
        layers
            .iter()
            .filter(|id| self.copper.contains(id))
            .copied()
            .collect()
    }

    /// The lines of a pad's label: the number and the net at one shared
    /// height, stacked, when the net fits at no less than
    /// [`PAD_SHARED_MIN`] of the number's own height; else the number
    /// alone at its own height, or whichever of the two the pad has.
    fn pad_lines(&self, pad: &Pad, along: f64, across: f64) -> Vec<PadLine> {
        let number = (!pad.number.is_empty()).then(|| pad.number.clone());
        let net = self.net_name(pad.net);
        let height_of = |text: &str| fit(text, PAD_ALONG * along, PAD_ACROSS * across);
        let line = |text: String, class, height, offset| PadLine {
            text,
            class,
            height,
            offset,
        };
        match (number, net) {
            (Some(number), Some(net)) => {
                let (number_height, net_height) = (height_of(&number), height_of(&net));
                let shared = number_height.min(net_height);
                if shared >= MIN_HEIGHT && shared >= PAD_SHARED_MIN * number_height {
                    vec![
                        line(number, LabelClass::PadNumber, shared, -PAD_STACK * across),
                        line(net, LabelClass::PadNet, shared, PAD_STACK * across),
                    ]
                } else {
                    vec![line(number, LabelClass::PadNumber, number_height, 0.0)]
                }
            }
            (Some(number), None) => {
                let height = height_of(&number);
                vec![line(number, LabelClass::PadNumber, height, 0.0)]
            }
            (None, Some(net)) => {
                let height = height_of(&net);
                vec![line(net, LabelClass::PadNet, height, 0.0)]
            }
            (None, None) => Vec::new(),
        }
    }

    /// Every pad's number and net, along the pad's long axis (upright for
    /// a round or square pad), on every copper layer the pad reaches.
    fn pads(&mut self) {
        for footprint in &self.board.footprints {
            let frame = Transform::new(footprint.at, footprint.rotation);
            for pad in &footprint.pads {
                let center = frame.apply(pad.at);
                let (w, h) = (pad.size.width.abs(), pad.size.height.abs());
                let (along, across, angle) = if w >= h {
                    (w, h, pad.rotation)
                } else {
                    (h, w, pad.rotation + 90.0)
                };
                // Round and square pads have no long axis: upright.
                let angle = if (w - h).abs() < 0.01 {
                    0.0
                } else {
                    normalize_angle(angle)
                };
                let lines = self.pad_lines(pad, along, across);
                let turn = Transform::new(center, angle);
                for layer in self.copper_among(&pad.layers) {
                    for line in lines.iter().filter(|line| line.height >= MIN_HEIGHT) {
                        let position = turn.apply(Point {
                            x: 0.0,
                            y: line.offset,
                        });
                        self.push(Label::new(
                            &line.text,
                            line.class,
                            pad.net,
                            layer,
                            position,
                            angle,
                            line.height,
                        ));
                    }
                }
            }
        }
    }

    /// Every via's net, upright at its centre, no longer than its
    /// diameter and no taller than [`VIA_HEIGHT_PER_SIZE`] of it, on
    /// every copper layer it spans.
    fn vias(&mut self) {
        for via in &self.board.vias {
            let Some(net) = self.net_name(via.net) else {
                continue;
            };
            let height = fit(&net, via.size, VIA_HEIGHT_PER_SIZE * via.size);
            if height < MIN_HEIGHT {
                continue;
            }
            let layers: Vec<LayerId> = self
                .copper
                .iter()
                .filter(|&&layer| spans(via, &self.copper, layer))
                .copied()
                .collect();
            for layer in layers {
                self.push(Label::new(
                    &net,
                    LabelClass::ViaNet,
                    via.net,
                    layer,
                    via.at,
                    0.0,
                    height,
                ));
            }
        }
    }

    /// Every zone's net, once per copper layer it is filled on, at the
    /// pole of inaccessibility of its fills there, sized to the pole's
    /// clearance and capped at [`ZONE_MAX_HEIGHT`].
    fn zones(&mut self) {
        let board = self.board;
        let zones = board
            .zones
            .iter()
            .map(|zone| (zone, Transform::identity()))
            .chain(board.footprints.iter().flat_map(|footprint| {
                let frame = Transform::new(footprint.at, footprint.rotation);
                footprint.zones.iter().map(move |zone| (zone, frame))
            }));
        for (zone, frame) in zones {
            if zone.keepout {
                continue;
            }
            let Some(net) = self.net_name(zone.net) else {
                continue;
            };
            for layer in self.copper_among(&zone.layers) {
                let pole = zone
                    .fills
                    .iter()
                    .filter(|fill| fill.layer == layer)
                    .filter_map(|fill| {
                        let points: Vec<Point> =
                            fill.points.iter().map(|&p| frame.apply(p)).collect();
                        pole_of_inaccessibility(&points)
                    })
                    .max_by(|a, b| a.1.total_cmp(&b.1));
                let Some((position, radius)) = pole else {
                    continue;
                };
                let height =
                    fit(&net, 2.0 * ZONE_FIT * radius, ZONE_FIT * radius).min(ZONE_MAX_HEIGHT);
                if height < MIN_HEIGHT {
                    continue;
                }
                self.push(Label::new(
                    &net,
                    LabelClass::ZoneNet,
                    zone.net,
                    layer,
                    position,
                    0.0,
                    height,
                ));
            }
        }
    }

    /// Every run's net, [`HEIGHT_PER_TRACK_WIDTH`] of the run's width
    /// tall, when the run is at least [`RUN_GATE`] text lengths long:
    /// one label, repeated about every [`REPEAT`] text lengths, spread
    /// evenly along the run.
    fn tracks(&mut self, runs: &[Run]) {
        for (run_index, run) in runs.iter().enumerate() {
            let Some(net) = self.net_name(Some(run.net)) else {
                continue;
            };
            let height = HEIGHT_PER_TRACK_WIDTH * run.width;
            if height < MIN_HEIGHT {
                continue;
            }
            let text = text_length(&net, height);
            let length = run.length();
            if text <= 0.0 || length < RUN_GATE * text {
                continue;
            }
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the count is a positive whole number, capped at a thousand labels per run"
            )]
            let count = (length / (REPEAT * text)).round().clamp(1.0, 1000.0) as usize;
            for i in 0..count {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "at most a thousand labels on a run"
                )]
                let along = length * (i as f64 + 0.5) / count as f64;
                let (position, angle) = run.at(along);
                self.out.push(Candidate {
                    label: Label::new(
                        &net,
                        LabelClass::TrackNet,
                        Some(run.net),
                        run.layer,
                        position,
                        angle,
                        height,
                    ),
                    run: Some((run_index, along)),
                });
            }
        }
    }
}

/// The point of `polygon` farthest from its edges and that distance: the
/// pole of inaccessibility, found as `polylabel` finds it (a quadtree
/// over the box, each cell's best possible distance bounding the
/// search), to within a hundredth of the box's shorter side. `None` for
/// fewer than three points or a box of no area. The polygon is one ring,
/// holes spliced in as KiCad stores a fill; inside is by the even-odd
/// rule.
#[must_use]
pub fn pole_of_inaccessibility(polygon: &[Point]) -> Option<(Point, f64)> {
    if polygon.len() < 3 {
        return None;
    }
    let bounds = Bounds::around(polygon.iter().copied())?;
    let (width, height) = (bounds.width(), bounds.height());
    let size = width.min(height);
    if size <= 0.0 {
        return None;
    }
    let precision = (size / 100.0).max(0.001);
    let mut queue = BinaryHeap::new();
    let mut x = bounds.min.x;
    while x < bounds.max.x {
        let mut y = bounds.min.y;
        while y < bounds.max.y {
            queue.push(Cell::new(
                Point {
                    x: x + size / 2.0,
                    y: y + size / 2.0,
                },
                size / 2.0,
                polygon,
            ));
            y += size;
        }
        x += size;
    }
    let mut best = Cell::new(bounds.center(), 0.0, polygon);
    while let Some(cell) = queue.pop() {
        if cell.distance > best.distance {
            best = cell;
        }
        if cell.max - best.distance <= precision {
            continue;
        }
        let h = cell.half / 2.0;
        for (dx, dy) in [(-h, -h), (h, -h), (-h, h), (h, h)] {
            queue.push(Cell::new(
                Point {
                    x: cell.center.x + dx,
                    y: cell.center.y + dy,
                },
                h,
                polygon,
            ));
        }
    }
    (best.distance > 0.0).then_some((best.center, best.distance))
}

/// A cell of the pole search: its centre's signed distance to the
/// polygon and the best any point in it could reach.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cell {
    center: Point,
    half: f64,
    distance: f64,
    max: f64,
}

impl Cell {
    fn new(center: Point, half: f64, polygon: &[Point]) -> Self {
        let distance = signed_distance(center, polygon);
        Self {
            center,
            half,
            distance,
            max: distance + half * std::f64::consts::SQRT_2,
        }
    }
}

impl Eq for Cell {}

impl PartialOrd for Cell {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cell {
    fn cmp(&self, other: &Self) -> Ordering {
        self.max.total_cmp(&other.max)
    }
}

/// The distance from `point` to the nearest edge of `polygon`, positive
/// inside and negative outside; inside is by the even-odd rule, so a
/// fill with its holes spliced in reads right. The hit test
/// (`crate::hit`) measures with it too.
pub(crate) fn signed_distance(point: Point, polygon: &[Point]) -> f64 {
    let mut inside = false;
    let mut nearest = f64::INFINITY;
    let edges = polygon
        .iter()
        .zip(polygon.iter().cycle().skip(1))
        .take(polygon.len());
    for (&a, &b) in edges {
        if (a.y > point.y) != (b.y > point.y)
            && point.x < (b.x - a.x) * (point.y - a.y) / (b.y - a.y) + a.x
        {
            inside = !inside;
        }
        nearest = nearest.min(segment_distance_squared(point, a, b));
    }
    let distance = nearest.sqrt();
    if inside { distance } else { -distance }
}

/// The squared distance from `point` to the segment `from`-`to`.
fn segment_distance_squared(point: Point, from: Point, to: Point) -> f64 {
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
    (point.x - nearest.x).powi(2) + (point.y - nearest.y).powi(2)
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use kiewer_assert::assert_is_empty;
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

    /// A four-layer board around `items`.
    fn board(items: &str) -> Board {
        let text = format!(
            r#"(kicad_pcb (version 20241229) (generator "pcbnew")
              (layers (0 "F.Cu" signal) (1 "In1.Cu" signal) (2 "In2.Cu" signal)
                      (3 "B.Cu" signal) (44 "Edge.Cuts" user))
              {items})"#
        );
        parse_board(&text).expect("the synthetic board parses")
    }

    fn segment(x1: f64, y1: f64, x2: f64, y2: f64, width: f64, layer: &str, net: &str) -> String {
        format!(
            r#"(segment (start {x1} {y1}) (end {x2} {y2}) (width {width}) (layer "{layer}") (net "{net}"))"#
        )
    }

    /// Item `index` of `items`: the crate forbids indexing, and a missing
    /// item is a failed test, not a panic in the placement.
    fn at<T>(items: &[T], index: usize) -> &T {
        items
            .get(index)
            .unwrap_or_else(|| panic!("no item {index} among {}", items.len()))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    fn layer(board: &Board, name: &str) -> LayerId {
        board.layer_named(name).expect("the layer exists")
    }

    fn of_class(labels: &[Label], class: LabelClass) -> Vec<&Label> {
        labels.iter().filter(|label| label.class == class).collect()
    }

    #[test_case(0.0, 0.0)]
    #[test_case(90.0, 90.0; "vertical reads bottom to top")]
    #[test_case(-90.0, 90.0; "minus ninety is the same vertical")]
    #[test_case(180.0, 0.0)]
    #[test_case(135.0, -45.0)]
    #[test_case(-45.0, -45.0)]
    #[test_case(270.0, 90.0)]
    #[test_case(100.0, -80.0)]
    #[test_case(-350.0, 10.0)]
    fn an_angle_folds_into_the_readable_half(degrees: f64, expected: f64) {
        assert!(
            close(normalize_angle(degrees), expected),
            "{}",
            normalize_angle(degrees)
        );
    }

    #[test]
    fn the_corners_of_a_turned_label_are_its_box_turned() {
        let label = Label {
            text: "GND".to_owned(),
            class: LabelClass::TrackNet,
            net: None,
            layer: LayerId(0),
            position: Point { x: 10.0, y: 20.0 },
            angle: 90.0,
            height: 1.0,
            length: 2.0,
        };
        // Turned 90 degrees counter-clockwise on the screen, the length
        // runs along y and the height along x.
        for corner in label.corners() {
            assert!(close(corner.x.abs() - 10.0, -9.5) || close((corner.x - 10.0).abs(), 0.5));
            assert!(close((corner.y - 20.0).abs(), 1.0), "{corner:?}");
        }
        let flat = Label {
            angle: 0.0,
            ..label.clone()
        };
        assert!(label.overlaps(&flat));
        let apart = Label {
            position: Point { x: 10.0, y: 22.5 },
            ..label.clone()
        };
        assert!(!label.overlaps(&apart), "touching boxes do not overlap");
        let beside = Label {
            position: Point { x: 11.2, y: 20.0 },
            ..label.clone()
        };
        assert!(!label.overlaps(&beside));
    }

    #[test]
    fn two_collinear_segments_end_to_end_are_one_run() {
        let board = board(&format!(
            "{}{}",
            segment(0.0, 0.0, 10.0, 0.0, 0.5, "F.Cu", "GND"),
            // Reversed: its end meets the first's end.
            segment(20.0, 0.0, 10.0, 0.0, 0.3, "F.Cu", "GND"),
        ));
        let runs = runs(&board);
        assert_eq!(runs.len(), 1, "{runs:?}");
        let run = &at(&runs, 0);
        assert!(close(run.length(), 20.0));
        assert!(close(run.width, 0.3), "the narrowest width");
        assert_eq!(run.points.len(), 3);
        let (point, angle) = run.at(15.0);
        assert!(close(point.x, 15.0) && close(point.y, 0.0), "{point:?}");
        assert!(close(angle, 0.0));
    }

    /// A second segment after `(0, 0)` to `(10, 0)` on F.Cu, net GND.
    #[test_case((10.0, 0.0, 20.0, 0.0), "F.Cu", "GND", 1; "collinear, start at the end")]
    #[test_case((20.0, 0.0, 10.0, 0.0), "F.Cu", "GND", 1; "collinear, end at the end")]
    #[test_case((0.0, 0.0, -10.0, 0.0), "F.Cu", "GND", 1; "collinear, back from the start")]
    #[test_case((10.0, 0.0, 20.0, 0.5), "F.Cu", "GND", 1; "a bend under the limit")]
    #[test_case((10.0, 0.0, 20.0, 2.0), "F.Cu", "GND", 2; "a bend over the limit")]
    #[test_case((10.0, 0.0, 20.0, 10.0), "F.Cu", "GND", 2; "a forty five degree bend")]
    #[test_case((10.0, 0.0, 20.0, 0.0), "F.Cu", "VCC", 2; "another net")]
    #[test_case((10.0, 0.0, 20.0, 0.0), "B.Cu", "GND", 2; "another layer")]
    #[test_case((10.02, 0.0, 20.0, 0.0), "F.Cu", "GND", 2; "a gap past the join tolerance")]
    #[test_case((10.004, 0.0, 20.0, 0.0), "F.Cu", "GND", 1; "a gap within the join tolerance")]
    fn two_segments_are_one_run_only_when_collinear_connected_and_alike(
        second: (f64, f64, f64, f64),
        layer: &str,
        net: &str,
        expected: usize,
    ) {
        let (x1, y1, x2, y2) = second;
        let board = board(&format!(
            "{}{}",
            segment(0.0, 0.0, 10.0, 0.0, 0.5, "F.Cu", "GND"),
            segment(x1, y1, x2, y2, 0.5, layer, net),
        ));
        let runs = runs(&board);
        assert_eq!(runs.len(), expected, "{runs:?}");
    }

    #[test]
    fn at_a_junction_the_straightest_pair_continues_and_the_branch_starts_its_own_run() {
        let board = board(&format!(
            "{}{}{}",
            segment(0.0, 0.0, 10.0, 0.0, 0.5, "F.Cu", "GND"),
            segment(10.0, 0.0, 20.0, 0.0, 0.5, "F.Cu", "GND"),
            segment(10.0, 0.0, 10.0, 10.0, 0.5, "F.Cu", "GND"),
        ));
        let runs = runs(&board);
        assert_eq!(runs.len(), 2, "{runs:?}");
        let lengths: Vec<f64> = runs.iter().map(Run::length).collect();
        assert!(lengths.iter().any(|&l| close(l, 20.0)), "{lengths:?}");
        assert!(lengths.iter().any(|&l| close(l, 10.0)), "{lengths:?}");
        // The vertical branch's label reads bottom to top.
        let vertical = runs
            .iter()
            .find(|run| close(run.length(), 10.0))
            .expect("the branch");
        assert!(close(vertical.at(5.0).1, 90.0));
    }

    #[test]
    fn a_run_shorter_than_the_gate_gets_no_label_and_a_long_one_repeats() {
        let height = 0.7 * 0.25;
        let text = text_length("GND", height);
        // Just under the gate: nothing. Just over: one label, centred.
        let short = board(&segment(0.0, 0.0, 1.29 * text, 0.0, 0.25, "F.Cu", "GND"));
        assert_is_empty!(of_class(&candidates(&short), LabelClass::TrackNet));
        let gate = board(&segment(0.0, 0.0, 1.31 * text, 0.0, 0.25, "F.Cu", "GND"));
        let labels = candidates(&gate);
        let tracks = of_class(&labels, LabelClass::TrackNet);
        assert_eq!(tracks.len(), 1);
        assert!(
            close(at(&tracks, 0).height, height),
            "0.7 of the track width"
        );
        assert!(close(at(&tracks, 0).length, text));
        assert!(close(at(&tracks, 0).position.x, 1.31 * text / 2.0));
        assert_eq!(at(&tracks, 0).text, "GND");
        assert_eq!(
            at(&tracks, 0).net,
            gate.nets
                .iter()
                .position(|net| net.name == "GND")
                .map(|i| NetId(u32::try_from(i).expect("fits")))
        );
        // Sixteen text lengths: two labels, eight apart.
        let long = board(&segment(0.0, 0.0, 0.0, 16.0 * text, 0.25, "F.Cu", "GND"));
        let labels = candidates(&long);
        let tracks = of_class(&labels, LabelClass::TrackNet);
        assert_eq!(tracks.len(), 2, "{tracks:?}");
        assert!(
            close(
                at(&tracks, 1).position.y - at(&tracks, 0).position.y,
                8.0 * text
            ),
            "{tracks:?}"
        );
        assert!(
            close(at(&tracks, 0).angle, 90.0),
            "a vertical run reads bottom to top"
        );
    }

    #[test]
    fn a_track_too_narrow_for_the_minimum_height_gets_no_label() {
        let board = board(&segment(0.0, 0.0, 50.0, 0.0, 0.1, "F.Cu", "GND"));
        assert_is_empty!(candidates(&board));
    }

    fn pad(number: &str, net: &str, w: f64, h: f64, layers: &str, extra: &str) -> String {
        format!(
            r#"(footprint "T" (layer "F.Cu") (at 10 10) {extra}
                (pad "{number}" smd rect (at 0 0) (size {w} {h}) (layers {layers}) (net "{net}")))"#
        )
    }

    #[test]
    fn a_tall_pad_stacks_its_number_and_net_along_its_long_axis() {
        let board = board(&pad("1", "GND", 1.0, 2.0, "\"F.Cu\"", ""));
        let labels = candidates(&board);
        assert_eq!(labels.len(), 2, "{labels:?}");
        let number = &at(&labels, 0);
        let net = &at(&labels, 1);
        assert_eq!(
            (number.class, number.text.as_str()),
            (LabelClass::PadNumber, "1")
        );
        assert_eq!((net.class, net.text.as_str()), (LabelClass::PadNet, "GND"));
        // The long axis is vertical: the text reads bottom to top, and
        // its "down" is to the right, so the net sits right of the number.
        assert!(close(number.angle, 90.0) && close(net.angle, 90.0));
        assert!(close(number.position.x, 10.0 - 0.25) && close(net.position.x, 10.0 + 0.25));
        assert!(close(number.position.y, 10.0) && close(net.position.y, 10.0));
        // One shared height: the net's fit (3 glyphs along 1.9 mm),
        // capped at 0.45 of the short side.
        assert!(close(number.height, net.height));
        assert!(close(
            net.height,
            (0.95 * 2.0 / text_length("GND", 1.0)).min(0.45)
        ));
        assert!(!number.overlaps(net));
        assert_eq!(at(&labels, 0).layer, layer(&board, "F.Cu"));
    }

    #[test]
    fn a_long_net_name_in_a_small_pad_leaves_the_number_alone_at_full_size() {
        let board = board(&pad("12", "Net-(U1-PA15_SWDIO)", 0.6, 0.3, "\"F.Cu\"", ""));
        let labels = candidates(&board);
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert_eq!(at(&labels, 0).class, LabelClass::PadNumber);
        assert!(close(at(&labels, 0).angle, 0.0));
        assert!(close(at(&labels, 0).position.y, 10.0), "centred");
        assert!(close(
            at(&labels, 0).height,
            (0.95 * 0.6 / text_length("12", 1.0)).min(0.45 * 0.3)
        ));
    }

    #[test]
    fn a_square_pad_stays_upright_and_a_through_hole_pad_labels_every_copper_layer() {
        let board = board(&pad("1", "GND", 1.5, 1.5, "\"*.Cu\"", "(at 10 10 45)"));
        let labels = candidates(&board);
        assert_eq!(labels.len(), 8, "two lines on four layers: {labels:?}");
        assert!(labels.iter().all(|label| close(label.angle, 0.0)));
        for name in ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"] {
            assert_eq!(
                labels
                    .iter()
                    .filter(|label| label.layer == layer(&board, name))
                    .count(),
                2
            );
        }
    }

    #[test]
    fn a_pad_follows_its_footprints_rotation() {
        // The pad's rotation in the file is absolute, footprint included.
        let board = board(
            r#"(footprint "T" (layer "F.Cu") (at 10 10 90)
                (pad "1" smd rect (at 2 0 90) (size 2 1) (layers "F.Cu") (net "GND")))"#,
        );
        let labels = candidates(&board);
        // The pad is 2 mm up the screen from the footprint (at 2, 0 turned
        // 90 degrees counter-clockwise), 2 wide along its own x, which the
        // 90 degrees turn vertical.
        assert!(
            close(at(&labels, 0).position.y, 8.0),
            "{:?}",
            at(&labels, 0)
        );
        assert!(close(at(&labels, 0).angle, 90.0));
        // Reading bottom to top, the number sits left of the net.
        assert!(
            close(at(&labels, 0).position.x, 10.0 - 0.25)
                && close(at(&labels, 1).position.x, 10.25)
        );
    }

    #[test]
    fn a_via_is_labelled_on_every_layer_it_spans_with_text_no_longer_than_its_diameter() {
        let board = board(
            r#"(via (at 5 5) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net "GND"))
               (via blind (at 8 8) (size 0.6) (drill 0.3) (layers "F.Cu" "In1.Cu") (net "VCC"))"#,
        );
        let labels = candidates(&board);
        let through = of_class(&labels, LabelClass::ViaNet);
        assert_eq!(through.len(), 6, "{through:?}");
        let gnd: Vec<&&Label> = through.iter().filter(|l| l.text == "GND").collect();
        assert_eq!(gnd.len(), 4);
        assert!(
            gnd.iter()
                .all(|l| l.length <= 0.8 + 1e-9 && l.height <= 0.45 * 0.8 + 1e-9)
        );
        assert!(close(
            at(&gnd, 0).height,
            (0.8 / text_length("GND", 1.0)).min(0.36)
        ));
        let vcc: Vec<&&Label> = through.iter().filter(|l| l.text == "VCC").collect();
        assert_eq!(vcc.len(), 2);
        assert!(vcc.iter().any(|l| l.layer == layer(&board, "F.Cu")));
        assert!(vcc.iter().any(|l| l.layer == layer(&board, "In1.Cu")));
    }

    #[test]
    fn the_pole_of_a_rectangle_is_its_centre_at_half_the_short_side() {
        let rect = [
            Point { x: 0.0, y: 0.0 },
            Point { x: 10.0, y: 0.0 },
            Point { x: 10.0, y: 4.0 },
            Point { x: 0.0, y: 4.0 },
        ];
        let (pole, radius) = pole_of_inaccessibility(&rect).expect("a pole");
        assert!((radius - 2.0).abs() < 0.05, "{radius}");
        assert!((pole.y - 2.0).abs() < 0.05, "{pole:?}");
        assert!(pole.x > 1.9 && pole.x < 8.1, "{pole:?}");
        assert_eq!(
            pole_of_inaccessibility(rect.get(..2).expect("two points")),
            None
        );
    }

    #[test]
    fn a_zone_gets_one_label_at_its_pole_capped_in_height() {
        let board = board(
            r#"(zone (net "GND") (layer "F.Cu") (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
                 (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))))"#,
        );
        let labels = candidates(&board);
        let zones = of_class(&labels, LabelClass::ZoneNet);
        assert_eq!(zones.len(), 1, "{zones:?}");
        assert!(
            (at(&zones, 0).position.x - 5.0).abs() < 0.1
                && (at(&zones, 0).position.y - 5.0).abs() < 0.1
        );
        assert!(close(at(&zones, 0).height, ZONE_MAX_HEIGHT));
        assert!(close(at(&zones, 0).angle, 0.0));
        // A keepout and an unfilled zone get none.
        let none = self::tests::board(
            r#"(zone (net "GND") (layer "F.Cu") (keepout (tracks not_allowed)) (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
                 (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))))
               (zone (net "VCC") (layer "B.Cu") (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))))"#,
        );
        assert_is_empty!(of_class(&candidates(&none), LabelClass::ZoneNet));
    }

    #[test]
    fn a_track_label_shifts_along_its_run_to_clear_a_via_label() {
        let board = board(&format!(
            "{}{}",
            segment(0.0, 0.0, 10.0, 0.0, 1.0, "F.Cu", "GND"),
            r#"(via (at 5 0) (size 1) (drill 0.5) (layers "F.Cu" "B.Cu") (net "GND"))"#,
        ));
        let before = candidates(&board);
        let track_before = (*at(&of_class(&before, LabelClass::TrackNet), 0)).clone();
        assert!(
            close(track_before.position.x, 5.0),
            "centred on the run first"
        );
        let after = labels(&board);
        let f_cu = layer(&board, "F.Cu");
        let on_front: Vec<&Label> = after.iter().filter(|l| l.layer == f_cu).collect();
        let via = on_front
            .iter()
            .find(|l| l.class == LabelClass::ViaNet)
            .expect("the via label stays");
        let track = on_front
            .iter()
            .find(|l| l.class == LabelClass::TrackNet)
            .expect("the track label is kept");
        assert!(!close(track.position.x, 5.0), "moved: {track:?}");
        assert!(close(track.position.y, 0.0), "still on the run");
        assert!(!via.overlaps(track));
        // Via labels on the other layers have no track to fight.
        assert_eq!(
            after
                .iter()
                .filter(|l| l.class == LabelClass::ViaNet)
                .count(),
            4
        );
    }

    #[test]
    fn a_track_label_with_nowhere_clear_on_its_run_is_dropped() {
        let height = 0.7;
        let text = text_length("GND", height);
        let length = 1.5 * text;
        let middle = length / 2.0;
        let board = board(&format!(
            r#"{}(via (at {middle} 0) (size 1) (drill 0.5) (layers "F.Cu" "B.Cu") (net "GND"))"#,
            segment(0.0, 0.0, length, 0.0, 1.0, "F.Cu", "GND"),
        ));
        assert_eq!(of_class(&candidates(&board), LabelClass::TrackNet).len(), 1);
        assert_is_empty!(of_class(&labels(&board), LabelClass::TrackNet));
    }

    #[test]
    fn pads_beat_vias_beat_zones_beat_tracks() {
        // Everything at one point on F.Cu: only the pad's labels and
        // whatever shifts clear survive.
        let board = board(&format!(
            "{}{}{}",
            pad("1", "GND", 1.0, 1.0, "\"F.Cu\"", ""),
            r#"(via (at 10 10) (size 1) (drill 0.5) (layers "F.Cu" "B.Cu") (net "GND"))"#,
            r#"(zone (net "GND") (layer "F.Cu") (polygon (pts (xy 9 9) (xy 11 9) (xy 11 11) (xy 9 11)))
                 (filled_polygon (layer "F.Cu") (pts (xy 9 9) (xy 11 9) (xy 11 11) (xy 9 11))))"#,
        ));
        let f_cu = layer(&board, "F.Cu");
        let kept: Vec<LabelClass> = labels(&board)
            .iter()
            .filter(|l| l.layer == f_cu)
            .map(|l| l.class)
            .collect();
        assert_eq!(
            kept,
            [LabelClass::PadNumber, LabelClass::PadNet],
            "{kept:?}"
        );
    }

    /// The `<text` count of a hand-off label file.
    fn reference_count(layer: &str) -> usize {
        let path = format!(
            "{}/../viewer-drop/exports/labels-{layer}.svg",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path)
            .expect("the reference SVG")
            .matches("<text")
            .count()
    }

    #[test_case("F.Cu")]
    #[test_case("In1.Cu")]
    #[test_case("In2.Cu")]
    #[test_case("B.Cu")]
    fn the_hand_off_board_is_labelled_on_the_order_of_the_reference(name: &str) {
        let board = hand_off();
        let id = layer(board, name);
        let all = candidates(board);
        let on_layer = all.iter().filter(|l| l.layer == id).count();
        let reference = reference_count(name);
        // The reference labels every segment; runs and the gate label
        // fewer, so the count sits below it and well above none.
        assert!(
            on_layer * 4 >= reference && on_layer <= reference,
            "{name}: {on_layer} labels against {reference} in the reference"
        );
        assert!(
            all.iter()
                .any(|l| l.layer == id && l.class == LabelClass::ViaNet)
        );
        let has_tracks = board.segments.iter().any(|segment| segment.layer == id);
        assert_eq!(
            all.iter()
                .any(|l| l.layer == id && l.class == LabelClass::TrackNet),
            has_tracks
        );
    }

    #[test]
    fn the_hand_off_boards_resolved_labels_overlap_nowhere_and_keep_the_rules() {
        let board = hand_off();
        let resolved = labels(board);
        let all = candidates(board);
        assert!(
            resolved.len() * 10 >= all.len() * 7,
            "{} of {} kept",
            resolved.len(),
            all.len()
        );
        for class in LabelClass::ALL {
            assert!(resolved.iter().any(|l| l.class == class), "{class:?}");
        }
        let mut by_layer: BTreeMap<LayerId, Vec<&Label>> = BTreeMap::new();
        for label in &resolved {
            by_layer.entry(label.layer).or_default().push(label);
        }
        for (layer, labels) in &by_layer {
            for (i, a) in labels.iter().enumerate() {
                for b in labels.iter().skip(i + 1) {
                    assert!(!a.overlaps(b), "{layer:?}: {a:?} overlaps {b:?}");
                }
            }
        }
        let widths: Vec<f64> = board.segments.iter().map(|s| s.width).collect();
        for label in resolved.iter().filter(|l| l.class == LabelClass::TrackNet) {
            assert!(
                widths
                    .iter()
                    .any(|w| close(w * HEIGHT_PER_TRACK_WIDTH, label.height))
            );
            assert!(label.angle > -90.0 && label.angle <= 90.0);
            assert!(label.height >= MIN_HEIGHT);
        }
    }

    #[test]
    fn glyph_positions_skip_spaces_and_advance_by_the_glyphs() {
        let positions = glyph_positions("A B", 2.0);
        assert_eq!(positions.len(), 2);
        assert_eq!(at(&positions, 0).0, 'A');
        assert!(close(at(&positions, 0).1, 0.0));
        // A advances 18/21 of the width, a space 16/21.
        assert!(
            close(at(&positions, 1).1, 2.0 * (18.0 + 16.0) / 21.0),
            "{positions:?}"
        );
        let b = font::glyph('B').advance;
        assert!(close(
            text_length("A B", 2.0),
            2.0 * ((18.0 + 16.0) / 21.0 + b)
        ));
    }
}
