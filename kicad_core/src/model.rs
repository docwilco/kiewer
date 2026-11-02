//! The typed board model: what a `.kicad_pcb` says, in the shape the viewer
//! and the server read it.
//!
//! Lengths are millimetres and the y axis points down, as in the file.
//! Positions inside a [`Footprint`] (its pads, texts and graphics) are in
//! the footprint's own frame: offset by its `at` and turned by its
//! `rotation`. Layers and nets are referenced by index into the board's
//! tables ([`LayerId`], [`NetId`]), resolved at parse time: a model never
//! holds a reference to a layer or net that does not exist.

/// A point in millimetres, y down.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    /// Horizontal, millimetres.
    pub x: f64,
    /// Vertical, millimetres, downwards.
    pub y: f64,
}

/// Width and height in millimetres.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    /// Horizontal extent.
    pub width: f64,
    /// Vertical extent.
    pub height: f64,
}

/// Index of a layer in [`Board::layers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayerId(pub(crate) u16);

impl LayerId {
    /// The position in [`Board::layers`].
    #[must_use]
    pub fn index(self) -> usize {
        usize::from(self.0)
    }

    /// The id of the layer at `index`, or `None` past what an id can hold.
    pub(crate) fn from_index(index: usize) -> Option<Self> {
        u16::try_from(index).ok().map(LayerId)
    }
}

/// Index of a net in [`Board::nets`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NetId(pub(crate) u32);

impl NetId {
    /// The position in [`Board::nets`].
    ///
    /// Infallible on every target where a `u32` fits a `usize`, which
    /// includes wasm32.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a layer is for, the third value of a `(layers ...)` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayerKind {
    /// Copper carrying signals.
    Signal,
    /// Copper carrying power.
    Power,
    /// Copper carrying both.
    Mixed,
    /// Copper used for jumpers.
    Jumper,
    /// Anything that is not copper: mask, paste, silkscreen, courtyard,
    /// fabrication, edge cuts, user drawings.
    User,
    /// Auxiliary copper, a type KiCad names and rarely writes.
    Auxiliary,
}

impl LayerKind {
    /// Whether layers of this kind are copper.
    #[must_use]
    pub fn is_copper(self) -> bool {
        matches!(
            self,
            LayerKind::Signal | LayerKind::Power | LayerKind::Mixed | LayerKind::Jumper
        )
    }
}

/// One entry of the board's layer table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// KiCad's own number for the layer, the first value of the entry.
    pub ordinal: u32,
    /// The canonical name items refer to: `F.Cu`, `In1.Cu`, `Edge.Cuts`.
    pub name: String,
    /// What the layer is for.
    pub kind: LayerKind,
    /// The name the user gave the layer, when it differs from the canonical.
    pub user_name: Option<String>,
}

impl Layer {
    /// Whether this is a copper layer.
    #[must_use]
    pub fn is_copper(&self) -> bool {
        self.kind.is_copper()
    }
}

/// A net: the name every connected pad, via, track and zone shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    /// The net's name, as the file writes it: `GND`, `Net-(D15-K)`.
    pub name: String,
}

/// The geometry of a drawn shape, without its layer.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// A straight stroke.
    Line {
        /// One end.
        start: Point,
        /// The other end.
        end: Point,
    },
    /// A circular arc through three points.
    Arc {
        /// Where the arc starts.
        start: Point,
        /// A point on the arc between the ends.
        mid: Point,
        /// Where the arc ends.
        end: Point,
    },
    /// A circle given by its centre and a point on it.
    Circle {
        /// The centre.
        center: Point,
        /// Any point on the circumference.
        end: Point,
    },
    /// An axis-aligned rectangle between two corners.
    Rect {
        /// One corner.
        start: Point,
        /// The opposite corner.
        end: Point,
    },
    /// A closed polygon.
    Polygon {
        /// The vertices in order; the last joins the first.
        points: Vec<Point>,
    },
    /// A cubic Bezier curve.
    Curve {
        /// The four control points.
        points: Vec<Point>,
    },
}

/// A shape with its stroke width and fill, without a layer: what a custom
/// pad's primitives are made of.
#[derive(Debug, Clone, PartialEq)]
pub struct Drawing {
    /// The geometry.
    pub shape: Shape,
    /// Stroke width in millimetres; 0 for a fill-only shape.
    pub width: f64,
    /// Whether the interior is filled.
    pub filled: bool,
}

/// A drawing on a layer: a board or footprint graphic.
#[derive(Debug, Clone, PartialEq)]
pub struct Graphic {
    /// The geometry, stroke and fill.
    pub drawing: Drawing,
    /// The layer it is drawn on.
    pub layer: LayerId,
}

/// The font of a text item.
#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    /// The font face the file names; `None` is KiCad's stroke font.
    pub face: Option<String>,
    /// Glyph size in millimetres.
    pub size: Size,
    /// Stroke thickness in millimetres, when the file gives one.
    pub thickness: Option<f64>,
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
}

/// Horizontal anchoring of a text item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Horizontal {
    /// Anchored at its left edge.
    Left,
    /// Anchored at its middle.
    #[default]
    Center,
    /// Anchored at its right edge.
    Right,
}

/// Vertical anchoring of a text item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Vertical {
    /// Anchored at its top edge.
    Top,
    /// Anchored at its middle.
    #[default]
    Center,
    /// Anchored at its bottom edge.
    Bottom,
}

/// How a text item sits on its position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Justify {
    /// Horizontal anchoring.
    pub horizontal: Horizontal,
    /// Vertical anchoring.
    pub vertical: Vertical,
    /// Whether the text is mirrored, as on a back layer.
    pub mirror: bool,
}

/// What a text item is to its footprint, if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextRole {
    /// Free text on the board or in a footprint.
    User,
    /// A footprint's reference designator.
    Reference,
    /// A footprint's value.
    Value,
    /// Another footprint property shown as text, by name.
    Property(String),
}

/// A text item: a board `gr_text`, a footprint `fp_text`, or a footprint
/// property that is placed on a layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    /// The text as written; `${REFERENCE}` and the like are not expanded.
    pub text: String,
    /// What the text is to its footprint.
    pub role: TextRole,
    /// Where it is anchored: on the board, or in the footprint's frame for
    /// a footprint's text.
    pub at: Point,
    /// Rotation in degrees, as the file writes it: for a footprint's text
    /// that is the absolute angle, the footprint's rotation included.
    pub rotation: f64,
    /// The layer it is drawn on.
    pub layer: LayerId,
    /// Drawn as a cut-out of a filled box rather than as strokes.
    pub knockout: bool,
    /// Not shown.
    pub hidden: bool,
    /// Kept readable: a footprint's text with this set draws at its
    /// rotation folded into (-90, 90], so it reads from the left or from
    /// below whatever way the footprint turns. KiCad's default; the file
    /// writes `(unlocked yes)` to turn it off. Board-level text ignores
    /// it.
    pub upright: bool,
    /// The font.
    pub font: Font,
    /// The anchoring.
    pub justify: Justify,
}

/// How a pad connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PadKind {
    /// Surface mount.
    Smd,
    /// Plated through hole.
    ThroughHole,
    /// Unplated hole, no copper.
    NpThroughHole,
    /// Copper with no paste or mask: an edge connector or test point.
    Connect,
}

/// A pad's outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PadShape {
    /// A circle of the pad's width.
    Circle,
    /// A rectangle.
    Rect,
    /// A stadium: a rectangle with semicircular ends.
    Oval,
    /// A trapezoid.
    Trapezoid,
    /// A rectangle with rounded corners of [`Pad::round_radius_ratio`].
    RoundRect,
    /// A rectangle with chamfered corners.
    ChamferedRect,
    /// Built from [`Pad::primitives`] around an anchor of the pad's size.
    Custom,
}

/// A pad's hole.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drill {
    /// Diameter, or width and height for a slot.
    pub size: Size,
    /// Offset of the hole from the pad's centre.
    pub offset: Point,
}

/// One pad of a footprint, in the footprint's frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Pad {
    /// The pad number or name; empty for an unnumbered mounting hole.
    pub number: String,
    /// How it connects.
    pub kind: PadKind,
    /// Its outline.
    pub shape: PadShape,
    /// Centre, in the footprint's frame.
    pub at: Point,
    /// Rotation in degrees, absolute as the file writes it.
    pub rotation: f64,
    /// Width and height.
    pub size: Size,
    /// The hole, for through-hole pads.
    pub drill: Option<Drill>,
    /// Every layer the pad is on, wildcards expanded.
    pub layers: Vec<LayerId>,
    /// The net, or `None` for an unconnected pad.
    pub net: Option<NetId>,
    /// The pin function from the schematic, when written.
    pub pin_function: Option<String>,
    /// The pin's electrical type from the schematic, when written.
    pub pin_type: Option<String>,
    /// Corner radius as a fraction of the shorter side, for
    /// [`PadShape::RoundRect`].
    pub round_radius_ratio: Option<f64>,
    /// The shapes a [`PadShape::Custom`] pad is built from, in the pad's
    /// frame.
    pub primitives: Vec<Drawing>,
    /// The pad's own mask and paste clearances; `None` for each the pad
    /// leaves to its footprint and the board.
    pub clearances: Clearances,
}

/// How far a solder mask opening grows past the copper and how far the
/// paste shrinks inside it, as KiCad's local clearance overrides: each
/// level (pad, footprint, board) sets what it sets and leaves the rest to
/// the level above. Millimetres.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Clearances {
    /// The mask opening's growth around the pad; negative shrinks it.
    pub solder_mask_margin: Option<f64>,
    /// The paste's growth from the pad's edge; negative, as usual, shrinks.
    pub solder_paste_margin: Option<f64>,
    /// The same as a fraction of the pad's size, added to the margin.
    pub solder_paste_ratio: Option<f64>,
}

impl Clearances {
    /// These clearances with every unset value taken from `fallback`: the
    /// pad's over the footprint's, the footprint's over the board's.
    #[must_use]
    pub fn or(self, fallback: Clearances) -> Clearances {
        Clearances {
            solder_mask_margin: self.solder_mask_margin.or(fallback.solder_mask_margin),
            solder_paste_margin: self.solder_paste_margin.or(fallback.solder_paste_margin),
            solder_paste_ratio: self.solder_paste_ratio.or(fallback.solder_paste_ratio),
        }
    }

    /// How far the mask opening of a pad of `size` sits past its edge: the
    /// margin, 0 when none is set.
    #[must_use]
    pub fn mask_expansion(self) -> f64 {
        self.solder_mask_margin.unwrap_or(0.0)
    }

    /// How far the paste of a pad of `size` sits past its edge (negative:
    /// inside it): the margin plus the ratio of the pad's shorter side,
    /// each 0 when unset. KiCad applies the ratio per axis; one value of
    /// the shorter side is what a uniform offset can take.
    #[must_use]
    pub fn paste_expansion(self, size: Size) -> f64 {
        self.solder_paste_margin.unwrap_or(0.0)
            + self.solder_paste_ratio.unwrap_or(0.0) * size.width.min(size.height)
    }
}

/// Flags from a footprint's `(attr ...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "KiCad writes these as six independent flags, each present or absent on its own; no two of them form a state an enum would name"
)]
pub struct FootprintAttributes {
    /// Surface mount.
    pub smd: bool,
    /// Through hole.
    pub through_hole: bool,
    /// Do not populate.
    pub dnp: bool,
    /// Left out of the bill of materials.
    pub exclude_from_bom: bool,
    /// Left out of position files.
    pub exclude_from_pos_files: bool,
    /// Exists on the board only, not in the schematic.
    pub board_only: bool,
}

/// A footprint property, placed on a layer or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// The property's name: `Reference`, `Value`, `Datasheet`.
    pub name: String,
    /// Its value.
    pub value: String,
}

/// A placed footprint.
#[derive(Debug, Clone, PartialEq)]
pub struct Footprint {
    /// The library identifier: `LED_SMD:LED_WS2812B_PLCC4_5.0x5.0mm_P3.2mm`.
    pub name: String,
    /// The side it sits on: `F.Cu` or `B.Cu`.
    pub layer: LayerId,
    /// Where its origin is on the board.
    pub at: Point,
    /// Rotation in degrees.
    pub rotation: f64,
    /// Flags from `(attr ...)`.
    pub attributes: FootprintAttributes,
    /// Every property by name, placed or not.
    pub properties: Vec<Property>,
    /// Text items in the footprint's frame, placed properties included.
    pub texts: Vec<Text>,
    /// Graphics in the footprint's frame.
    pub graphics: Vec<Graphic>,
    /// The pads.
    pub pads: Vec<Pad>,
    /// Zones the footprint carries, in the footprint's frame.
    pub zones: Vec<Zone>,
    /// The footprint's own mask and paste clearances, for pads that set
    /// none.
    pub clearances: Clearances,
}

impl Footprint {
    /// `text`'s content with the footprint's variables expanded:
    /// `${REFERENCE}`, `${VALUE}` and `${<property name>}` become the
    /// property's value; a variable the footprint does not define stays
    /// as written.
    #[must_use]
    pub fn resolve_text(&self, text: &Text) -> String {
        let mut out = String::new();
        let mut rest = text.text.as_str();
        while let Some(start) = rest.find("${") {
            let (before, from_var) = rest.split_at(start);
            out.push_str(before);
            let Some(end) = from_var.find('}') else {
                out.push_str(from_var);
                return out;
            };
            let name = from_var.get(2..end).unwrap_or_default();
            let value = match name {
                "REFERENCE" => self.reference(),
                "VALUE" => self.value(),
                other => self.property(other),
            };
            match value {
                Some(value) => out.push_str(value),
                None => out.push_str(from_var.get(..=end).unwrap_or_default()),
            }
            rest = from_var.get(end + 1..).unwrap_or_default();
        }
        out.push_str(rest);
        out
    }

    /// The value of the property named `name`, when present.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|property| property.name == name)
            .map(|property| property.value.as_str())
    }

    /// The reference designator: `LED6`, `U1`.
    #[must_use]
    pub fn reference(&self) -> Option<&str> {
        self.property("Reference")
    }

    /// The value: `10k`, `STM32G431KBT6`.
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.property("Value")
    }
}

/// A via's reach through the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViaKind {
    /// Through every layer.
    Through,
    /// From an outer layer to an inner one.
    Blind,
    /// Between adjacent layers.
    Micro,
}

/// A via.
#[derive(Debug, Clone, PartialEq)]
pub struct Via {
    /// Its reach.
    pub kind: ViaKind,
    /// Centre.
    pub at: Point,
    /// Outer diameter.
    pub size: f64,
    /// Hole diameter.
    pub drill: f64,
    /// The two layers it spans, as written: usually outer to outer.
    pub layers: [LayerId; 2],
    /// The net, or `None` for a via on no net.
    pub net: Option<NetId>,
    /// Placed by hand, not by the router.
    pub free: bool,
}

/// A straight track segment.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// One end.
    pub start: Point,
    /// The other end.
    pub end: Point,
    /// Track width; the ends are round.
    pub width: f64,
    /// The copper layer.
    pub layer: LayerId,
    /// The net, or `None` for copper on no net.
    pub net: Option<NetId>,
}

/// A curved track.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackArc {
    /// Where the arc starts.
    pub start: Point,
    /// A point on the arc between the ends.
    pub mid: Point,
    /// Where the arc ends.
    pub end: Point,
    /// Track width; the ends are round.
    pub width: f64,
    /// The copper layer.
    pub layer: LayerId,
    /// The net, or `None` for copper on no net.
    pub net: Option<NetId>,
}

/// One filled polygon of a zone, as KiCad last computed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    /// The layer the fill is on.
    pub layer: LayerId,
    /// The outline; holes are spliced in as KiCad writes them.
    pub points: Vec<Point>,
    /// An island KiCad flagged.
    pub island: bool,
}

/// A copper zone (or a keepout), with the fills stored in the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Zone {
    /// The net, or `None` for a zone on no net or a keepout.
    pub net: Option<NetId>,
    /// Every layer the zone is on.
    pub layers: Vec<LayerId>,
    /// The zone's name, when given.
    pub name: Option<String>,
    /// Fill priority; higher fills first.
    pub priority: u32,
    /// A keepout area rather than copper.
    pub keepout: bool,
    /// Whether KiCad was asked to fill it.
    pub filled: bool,
    /// The outline the user drew. An arc in the outline contributes its
    /// start, mid and end points.
    pub outline: Vec<Point>,
    /// The fills stored in the file; empty when the zone was never filled.
    pub fills: Vec<Fill>,
}

impl Zone {
    /// Whether the zone is copper on `layer` with no stored fill there: a
    /// zone saved before its fill was computed, or unfilled on purpose.
    /// A keepout is never unfilled; it is not copper.
    #[must_use]
    pub fn is_unfilled_on(&self, layer: LayerId) -> bool {
        !self.keepout
            && self.layers.contains(&layer)
            && !self.fills.iter().any(|fill| fill.layer == layer)
    }
}

/// A parsed board.
#[derive(Debug, Clone, PartialEq)]
pub struct Board {
    /// The file format version, `20241229` and later.
    pub version: u32,
    /// The program that wrote the file.
    pub generator: Option<String>,
    /// That program's version.
    pub generator_version: Option<String>,
    /// Board thickness in millimetres.
    pub thickness: Option<f64>,
    /// The layer table, in file order.
    pub layers: Vec<Layer>,
    /// Every net an item names, in order of first appearance (or in table
    /// order for a file with a net table).
    pub nets: Vec<Net>,
    /// The footprints.
    pub footprints: Vec<Footprint>,
    /// Board-level graphics, the outline included.
    pub graphics: Vec<Graphic>,
    /// Board-level text items.
    pub texts: Vec<Text>,
    /// Straight tracks.
    pub segments: Vec<Segment>,
    /// Curved tracks.
    pub arcs: Vec<TrackArc>,
    /// Vias.
    pub vias: Vec<Via>,
    /// Board-level zones.
    pub zones: Vec<Zone>,
    /// The board's mask and paste clearances from `(setup ...)`, the last
    /// fallback for pads and footprints that set none.
    pub clearances: Clearances,
}

/// The canonical name of the layer the board outline is drawn on.
pub const EDGE_CUTS: &str = "Edge.Cuts";

impl Board {
    /// The layer an id names.
    #[must_use]
    pub fn layer(&self, id: LayerId) -> Option<&Layer> {
        self.layers.get(id.index())
    }

    /// The net an id names.
    #[must_use]
    pub fn net(&self, id: NetId) -> Option<&Net> {
        self.nets.get(id.index())
    }

    /// The id of the layer with a canonical name.
    #[must_use]
    pub fn layer_named(&self, name: &str) -> Option<LayerId> {
        self.layers
            .iter()
            .position(|layer| layer.name == name)
            .and_then(LayerId::from_index)
    }

    /// The copper layers, in file order.
    pub fn copper_layers(&self) -> impl Iterator<Item = (LayerId, &Layer)> {
        self.layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| layer.is_copper())
            .filter_map(|(index, layer)| Some((LayerId::from_index(index)?, layer)))
    }

    /// The board-level graphics on `Edge.Cuts`: the outline and its
    /// cutouts. Cutouts drawn inside a footprint are in that footprint's
    /// [`Footprint::graphics`], in its frame.
    pub fn outline(&self) -> impl Iterator<Item = &Graphic> {
        let edge = self.layer_named(EDGE_CUTS);
        self.graphics
            .iter()
            .filter(move |graphic| Some(graphic.layer) == edge)
    }

    /// Every text item, board-level and in footprints.
    pub fn all_texts(&self) -> impl Iterator<Item = &Text> {
        self.texts
            .iter()
            .chain(self.footprints.iter().flat_map(|fp| fp.texts.iter()))
    }

    /// Every pad of every footprint.
    pub fn all_pads(&self) -> impl Iterator<Item = &Pad> {
        self.footprints.iter().flat_map(|fp| fp.pads.iter())
    }

    /// Every zone, board-level and in footprints.
    pub fn all_zones(&self) -> impl Iterator<Item = &Zone> {
        self.zones
            .iter()
            .chain(self.footprints.iter().flat_map(|fp| fp.zones.iter()))
    }

    /// How many zones lack a stored fill on at least one of their copper
    /// layers ([`Zone::is_unfilled_on`]): the viewer draws them hatched
    /// and says so.
    #[must_use]
    pub fn unfilled_zones(&self) -> usize {
        self.all_zones()
            .filter(|zone| zone.layers.iter().any(|&layer| zone.is_unfilled_on(layer)))
            .count()
    }

    /// The clearances `pad` of `footprint` is drawn with: its own, then
    /// the footprint's, then the board's.
    #[must_use]
    pub fn pad_clearances(&self, footprint: &Footprint, pad: &Pad) -> Clearances {
        pad.clearances.or(footprint.clearances).or(self.clearances)
    }
}
