//! The typed schematic model: what one `.kicad_sch` sheet says, in the
//! shape the viewer draws it.
//!
//! Lengths are millimetres. Sheet positions (symbols, wires, labels) have
//! the y axis pointing down, as in the file. A library symbol's own
//! geometry (its [`Unit`]s' graphics, texts and pins) is in KiCad's
//! library frame, where y points up; a placed [`Symbol`]'s [`Frame`]
//! carries it onto the sheet, flipping, turning and mirroring as the
//! instance says. Every placed symbol names a [`LibrarySymbol`] of the
//! file's own `lib_symbols` by index, resolved at parse time: no external
//! library is ever consulted.

use crate::model::{Font, Horizontal, Justify, Point, Property, Size, Vertical};

/// Index of a library symbol in [`Sheet::library`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LibrarySymbolId(pub(crate) u32);

impl LibrarySymbolId {
    /// The position in [`Sheet::library`].
    ///
    /// Infallible on every target where a `u32` fits a `usize`, which
    /// includes wasm32.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The paper sizes KiCad names, as `(paper "A4")` writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PaperSize {
    /// `A5`.
    A5,
    /// `A4`, KiCad's default.
    A4,
    /// `A3`.
    A3,
    /// `A2`.
    A2,
    /// `A1`.
    A1,
    /// `A0`.
    A0,
    /// `A`, the ANSI size, the same as US Letter.
    A,
    /// `B`, the ANSI size, the same as US Ledger.
    B,
    /// `C`.
    C,
    /// `D`.
    D,
    /// `E`.
    E,
    /// `USLetter`.
    UsLetter,
    /// `USLegal`.
    UsLegal,
    /// `USLedger`.
    UsLedger,
    /// `User`: the size is written after the name.
    User,
}

impl PaperSize {
    /// The size the file writes as `name`, or `None` for a name KiCad does
    /// not define.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Some(match name {
            "A5" => PaperSize::A5,
            "A4" => PaperSize::A4,
            "A3" => PaperSize::A3,
            "A2" => PaperSize::A2,
            "A1" => PaperSize::A1,
            "A0" => PaperSize::A0,
            "A" => PaperSize::A,
            "B" => PaperSize::B,
            "C" => PaperSize::C,
            "D" => PaperSize::D,
            "E" => PaperSize::E,
            "USLetter" => PaperSize::UsLetter,
            "USLegal" => PaperSize::UsLegal,
            "USLedger" => PaperSize::UsLedger,
            "User" => PaperSize::User,
            _ => return None,
        })
    }

    /// The name as the file writes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PaperSize::A5 => "A5",
            PaperSize::A4 => "A4",
            PaperSize::A3 => "A3",
            PaperSize::A2 => "A2",
            PaperSize::A1 => "A1",
            PaperSize::A0 => "A0",
            PaperSize::A => "A",
            PaperSize::B => "B",
            PaperSize::C => "C",
            PaperSize::D => "D",
            PaperSize::E => "E",
            PaperSize::UsLetter => "USLetter",
            PaperSize::UsLegal => "USLegal",
            PaperSize::UsLedger => "USLedger",
            PaperSize::User => "User",
        }
    }

    /// The landscape extent, or `None` for [`PaperSize::User`], whose
    /// extent is in the file.
    #[must_use]
    pub fn landscape(self) -> Option<Size> {
        let (width, height) = match self {
            PaperSize::A5 => (210.0, 148.0),
            PaperSize::A4 => (297.0, 210.0),
            PaperSize::A3 => (420.0, 297.0),
            PaperSize::A2 => (594.0, 420.0),
            PaperSize::A1 => (841.0, 594.0),
            PaperSize::A0 => (1189.0, 841.0),
            PaperSize::A | PaperSize::UsLetter => (279.4, 215.9),
            PaperSize::B | PaperSize::UsLedger => (431.8, 279.4),
            PaperSize::C => (558.8, 431.8),
            PaperSize::D => (863.6, 558.8),
            PaperSize::E => (1117.6, 863.6),
            PaperSize::UsLegal => (355.6, 215.9),
            PaperSize::User => return None,
        };
        Some(Size { width, height })
    }
}

/// The sheet's paper: the `(paper ...)` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Paper {
    /// The named size.
    pub kind: PaperSize,
    /// The sheet's extent, orientation applied.
    pub size: Size,
}

impl Paper {
    /// KiCad's default: A4 landscape.
    #[must_use]
    pub fn a4() -> Self {
        Self {
            kind: PaperSize::A4,
            size: Size {
                width: 297.0,
                height: 210.0,
            },
        }
    }
}

/// How a symbol graphic's interior is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fill {
    /// Strokes only.
    None,
    /// Filled in the outline's own colour.
    Outline,
    /// Filled in the symbol body colour.
    Background,
}

/// The geometry of a schematic graphic, in the frame of whatever holds it.
#[derive(Debug, Clone, PartialEq)]
pub enum SymbolShape {
    /// An open chain of straight strokes; closed only by its fill.
    Polyline {
        /// The vertices in order.
        points: Vec<Point>,
    },
    /// An axis-aligned rectangle between two corners.
    Rect {
        /// One corner.
        start: Point,
        /// The opposite corner.
        end: Point,
    },
    /// A circle.
    Circle {
        /// The centre.
        center: Point,
        /// The radius.
        radius: f64,
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
    /// A cubic Bezier curve.
    Bezier {
        /// The four control points.
        points: Vec<Point>,
    },
}

/// A drawn shape with its stroke width and fill: a symbol's body line, a
/// sheet's free graphic.
#[derive(Debug, Clone, PartialEq)]
pub struct Graphic {
    /// The geometry.
    pub shape: SymbolShape,
    /// Stroke width in millimetres; 0 is the schematic's default width.
    pub width: f64,
    /// How the interior is painted.
    pub fill: Fill,
}

/// A text item: a symbol's graphic text, a sheet's free text, or the
/// content of a [`Field`].
#[derive(Debug, Clone, PartialEq)]
pub struct TextItem {
    /// The text as written; `${...}` variables are not expanded.
    pub text: String,
    /// The anchor, in the frame of whatever holds the item.
    pub at: Point,
    /// Rotation in degrees, counter-clockwise as seen on the sheet.
    pub rotation: f64,
    /// Not shown.
    pub hidden: bool,
    /// The font.
    pub font: Font,
    /// The anchoring.
    pub justify: Justify,
}

/// A named property placed as text: a symbol's reference or value, a
/// sheet's name, a label's intersheet references.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The property's name: `Reference`, `Value`, `Sheetname`.
    pub name: String,
    /// The value and where it is drawn.
    pub text: TextItem,
    /// Whether the name is shown before the value, as `Name: value`.
    pub show_name: bool,
}

impl Field {
    /// The text the field draws: the value, prefixed by the name when
    /// [`Field::show_name`] asks.
    #[must_use]
    pub fn shown(&self) -> String {
        if self.show_name {
            format!("{}: {}", self.name, self.text.text)
        } else {
            self.text.text.clone()
        }
    }
}

/// A pin's electrical type, the first word of a `(pin ...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinElectrical {
    /// `input`.
    Input,
    /// `output`.
    Output,
    /// `bidirectional`.
    Bidirectional,
    /// `tri_state`.
    TriState,
    /// `passive`.
    Passive,
    /// `free`.
    Free,
    /// `unspecified`.
    Unspecified,
    /// `power_in`.
    PowerIn,
    /// `power_out`.
    PowerOut,
    /// `open_collector`.
    OpenCollector,
    /// `open_emitter`.
    OpenEmitter,
    /// `no_connect`.
    NoConnect,
}

/// How a pin is drawn, the second word of a `(pin ...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinShape {
    /// A plain line.
    Line,
    /// A line with an inversion circle at the body.
    Inverted,
    /// A line with a clock mark inside the body.
    Clock,
    /// Both.
    InvertedClock,
    /// `input_low`.
    InputLow,
    /// `clock_low`.
    ClockLow,
    /// `output_low`.
    OutputLow,
    /// `edge_clock_high`.
    EdgeClockHigh,
    /// `non_logic`: a cross at the body.
    NonLogic,
}

/// One pin of a library symbol, in the library frame (y up).
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    /// The pin number as written: `1`, `A5`.
    pub number: String,
    /// The pin name; empty or `~` for none.
    pub name: String,
    /// The electrical type.
    pub electrical: PinElectrical,
    /// How it is drawn.
    pub shape: PinShape,
    /// The connection point.
    pub at: Point,
    /// The direction the pin extends from the connection point towards
    /// the body, in degrees: 0 right, 90 up, 180 left, 270 down.
    pub rotation: f64,
    /// How far it extends.
    pub length: f64,
    /// Not drawn on the sheet.
    pub hidden: bool,
    /// Glyph size of the name.
    pub name_size: Size,
    /// Glyph size of the number.
    pub number_size: Size,
}

impl Pin {
    /// The pin's name as drawn: nothing for an empty or `~` name.
    #[must_use]
    pub fn shown_name(&self) -> &str {
        if self.name == "~" { "" } else { &self.name }
    }
}

/// One sub-symbol of a library symbol: the graphics, texts and pins of one
/// unit in one body style.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    /// The unit, 1-based; 0 is shared by every unit.
    pub unit: u32,
    /// The body style, 1-based; 0 is shared by every style.
    pub body_style: u32,
    /// The body's shapes.
    pub graphics: Vec<Graphic>,
    /// Text drawn as part of the body.
    pub texts: Vec<TextItem>,
    /// The pins.
    pub pins: Vec<Pin>,
}

impl Unit {
    /// Whether this sub-symbol is drawn for an instance of `unit` in
    /// `body_style`.
    #[must_use]
    pub fn applies_to(&self, unit: u32, body_style: u32) -> bool {
        (self.unit == 0 || self.unit == unit)
            && (self.body_style == 0 || self.body_style == body_style)
    }
}

/// One entry of the file's `lib_symbols`: everything a placed symbol
/// needs, with a derived symbol's parent already folded in.
#[derive(Debug, Clone, PartialEq)]
pub struct LibrarySymbol {
    /// The name placed symbols refer to: `Device:R_Small`, `power:GND`.
    pub name: String,
    /// The symbol this one derives from, when the file says `extends`;
    /// its units and pin settings are already copied in.
    pub extends: Option<String>,
    /// A power symbol: its value is a net name.
    pub power: bool,
    /// Pin numbers are not drawn.
    pub pin_numbers_hidden: bool,
    /// Pin names are not drawn.
    pub pin_names_hidden: bool,
    /// How far inside the body a pin's name starts; 0 puts the names
    /// outside, above the pins.
    pub pin_name_offset: f64,
    /// The default properties, by name.
    pub properties: Vec<Property>,
    /// The sub-symbols.
    pub units: Vec<Unit>,
}

impl LibrarySymbol {
    /// The sub-symbols drawn for an instance of `unit` in `body_style`.
    pub fn units_for(&self, unit: u32, body_style: u32) -> impl Iterator<Item = &Unit> {
        self.units
            .iter()
            .filter(move |u| u.applies_to(unit, body_style))
    }
}

/// How a placed symbol is mirrored, after its rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mirror {
    /// Not mirrored.
    #[default]
    None,
    /// Flipped top to bottom (`mirror x`).
    X,
    /// Flipped left to right (`mirror y`).
    Y,
}

/// A placed symbol's frame: library points to sheet points. KiCad's
/// transform in order: the library's y-up flipped to the sheet's y-down,
/// the rotation (counter-clockwise on the sheet), then the mirror.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// The linear part, row-major: `[[a, b], [c, d]]` takes `(x, y)` to
    /// `(a x + b y, c x + d y)`.
    matrix: [[f64; 2]; 2],
    origin: Point,
}

impl Frame {
    /// The frame placing a symbol at `origin` turned by `rotation` degrees
    /// and mirrored as `mirror` says.
    #[must_use]
    pub fn new(origin: Point, rotation: f64, mirror: Mirror) -> Self {
        // The flip, then the rotation in quarter turns (a symbol's
        // rotation is one of 0, 90, 180, 270), then the mirror.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "rem_euclid(360) / 90 is in [0, 4), so round leaves 0 to 4, a u8"
        )]
        let quarter_turns = (rotation.rem_euclid(360.0) / 90.0).round() as u8 % 4;
        let rotate = match quarter_turns {
            1 => [[0.0, 1.0], [-1.0, 0.0]],
            2 => [[-1.0, 0.0], [0.0, -1.0]],
            3 => [[0.0, -1.0], [1.0, 0.0]],
            _ => [[1.0, 0.0], [0.0, 1.0]],
        };
        let mirror = match mirror {
            Mirror::None => [[1.0, 0.0], [0.0, 1.0]],
            Mirror::X => [[1.0, 0.0], [0.0, -1.0]],
            Mirror::Y => [[-1.0, 0.0], [0.0, 1.0]],
        };
        let flip = [[1.0, 0.0], [0.0, -1.0]];
        Self {
            matrix: multiply(mirror, multiply(rotate, flip)),
            origin,
        }
    }

    /// A library point on the sheet.
    #[must_use]
    pub fn apply(&self, point: Point) -> Point {
        let turned = self.rotate(point);
        Point {
            x: turned.x + self.origin.x,
            y: turned.y + self.origin.y,
        }
    }

    /// A library direction on the sheet: turned, flipped and mirrored,
    /// not moved.
    #[must_use]
    pub fn rotate(&self, vector: Point) -> Point {
        Point {
            x: self.matrix[0][0] * vector.x + self.matrix[0][1] * vector.y,
            y: self.matrix[1][0] * vector.x + self.matrix[1][1] * vector.y,
        }
    }

    /// The symbol's position on the sheet.
    #[must_use]
    pub fn origin(&self) -> Point {
        self.origin
    }

    /// Whether the frame turns by a quarter: a horizontal library text
    /// reads vertically on the sheet and the other way about.
    #[must_use]
    pub fn swaps_axes(&self) -> bool {
        self.matrix[0][1].abs() > 0.5
    }

    /// A sheet point in library terms: the inverse.
    #[must_use]
    pub fn unapply(&self, point: Point) -> Point {
        let [[xx, xy], [yx, yy]] = self.matrix;
        let det = xx * yy - xy * yx;
        let v = Point {
            x: point.x - self.origin.x,
            y: point.y - self.origin.y,
        };
        Point {
            x: (yy * v.x - xy * v.y) / det,
            y: (-yx * v.x + xx * v.y) / det,
        }
    }
}

/// The product `left * right`: the frame that applies `right` first.
fn multiply(left: [[f64; 2]; 2], right: [[f64; 2]; 2]) -> [[f64; 2]; 2] {
    let [[la, lb], [lc, ld]] = left;
    let [[ra, rb], [rc, rd]] = right;
    [
        [la * ra + lb * rc, la * rb + lb * rd],
        [lc * ra + ld * rc, lc * rb + ld * rd],
    ]
}

/// A symbol placed on the sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    /// The library symbol's name, as the file's `lib_id` writes it.
    pub lib_id: String,
    /// The library symbol it resolves to.
    pub library: LibrarySymbolId,
    /// Where its origin sits on the sheet.
    pub at: Point,
    /// Rotation in degrees, counter-clockwise on the sheet.
    pub rotation: f64,
    /// Mirroring, applied after the rotation.
    pub mirror: Mirror,
    /// The unit shown, 1-based.
    pub unit: u32,
    /// The body style shown, 1-based.
    pub body_style: u32,
    /// Do not populate: drawn with a cross through it.
    pub dnp: bool,
    /// The fields, placed on the sheet with absolute positions.
    pub fields: Vec<Field>,
}

impl Symbol {
    /// The frame carrying the library symbol onto the sheet.
    #[must_use]
    pub fn frame(&self) -> Frame {
        Frame::new(self.at, self.rotation, self.mirror)
    }

    /// The value of the field named `name`, when present.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.text.text.as_str())
    }

    /// The reference designator: `R1`, `U4`.
    #[must_use]
    pub fn reference(&self) -> Option<&str> {
        self.field("Reference")
    }

    /// The value: `10k`, `STM32G431KBTx`.
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.field("Value")
    }
}

/// A wire or a bus: a chain of straight segments.
#[derive(Debug, Clone, PartialEq)]
pub struct Wire {
    /// The vertices in order; KiCad writes two per wire.
    pub points: Vec<Point>,
    /// Stroke width; 0 is the schematic's default.
    pub width: f64,
}

/// A wire's entry onto a bus: a short diagonal.
#[derive(Debug, Clone, PartialEq)]
pub struct BusEntry {
    /// Where it starts.
    pub at: Point,
    /// The offset to its other end.
    pub size: Point,
    /// Stroke width; 0 is the schematic's default.
    pub width: f64,
}

/// A junction dot where wires meet.
#[derive(Debug, Clone, PartialEq)]
pub struct Junction {
    /// The centre.
    pub at: Point,
    /// The diameter; 0 is the schematic's default.
    pub diameter: f64,
}

/// The outline a global label, a hierarchical label or a sheet pin is
/// drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlagShape {
    /// Pointed at the wire.
    Input,
    /// Pointed away from the wire.
    Output,
    /// Pointed at both ends.
    Bidirectional,
    /// Pointed at both ends, as a bidirectional.
    TriState,
    /// A plain box.
    Passive,
}

/// What a label is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LabelKind {
    /// A net name on this sheet: text above the wire, no outline.
    Local,
    /// A net name across every sheet, in a flag.
    Global(FlagShape),
    /// A net name connecting to the parent sheet's pin, in a flag.
    Hierarchical(FlagShape),
}

/// Which side of its anchor a label's text sits on, and which way it
/// reads: KiCad's spin style, from the angle and the horizontal
/// justification the file writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Spin {
    /// Text to the right of the anchor, reading left to right.
    Right,
    /// Text to the left of the anchor, reading left to right.
    Left,
    /// Text above the anchor, reading bottom to top.
    Up,
    /// Text below the anchor, reading bottom to top.
    Bottom,
}

impl Spin {
    /// The spin a label's angle and justification name. A label turned
    /// by 180 or 270 is drawn upright, so those angles read as 0 and 90;
    /// the justification alone says which side of the anchor.
    #[must_use]
    pub fn of(rotation: f64, justify: Horizontal) -> Self {
        let vertical = (rotation.rem_euclid(180.0) - 90.0).abs() < 45.0;
        match (vertical, justify) {
            (false, Horizontal::Right) => Spin::Left,
            (false, _) => Spin::Right,
            (true, Horizontal::Right) => Spin::Bottom,
            (true, _) => Spin::Up,
        }
    }

    /// The angle the text draws at: 0 or 90 degrees.
    #[must_use]
    pub fn angle(self) -> f64 {
        match self {
            Spin::Right | Spin::Left => 0.0,
            Spin::Up | Spin::Bottom => 90.0,
        }
    }

    /// The text's horizontal anchoring.
    #[must_use]
    pub fn justify(self) -> Horizontal {
        match self {
            Spin::Right | Spin::Up => Horizontal::Left,
            Spin::Left | Spin::Bottom => Horizontal::Right,
        }
    }

    /// The unit step away from the anchor towards the text.
    #[must_use]
    pub fn direction(self) -> Point {
        match self {
            Spin::Right => Point { x: 1.0, y: 0.0 },
            Spin::Left => Point { x: -1.0, y: 0.0 },
            Spin::Up => Point { x: 0.0, y: -1.0 },
            Spin::Bottom => Point { x: 0.0, y: 1.0 },
        }
    }
}

/// A label: a net name on a wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    /// What kind of label, with its flag shape where it has one.
    pub kind: LabelKind,
    /// The net name as a text item: its anchor is the point on the wire,
    /// its rotation one of 0, 90, 180 and 270 as written, and the
    /// horizontal part of its anchoring picks the side of the anchor.
    pub text: TextItem,
    /// The label's fields (a global label's intersheet references).
    pub fields: Vec<Field>,
}

impl Label {
    /// Which side of the anchor the text is on.
    #[must_use]
    pub fn spin(&self) -> Spin {
        Spin::of(self.text.rotation, self.text.justify.horizontal)
    }
}

/// One pin on a sheet symbol's edge.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetPin {
    /// The flag shape.
    pub shape: FlagShape,
    /// The net name as a text item: its anchor is the point on the sheet
    /// symbol's edge, its rotation the edge as KiCad writes it (0 right,
    /// 90 top, 180 left, 270 bottom).
    pub text: TextItem,
}

/// A hierarchical sheet symbol: a box standing for another sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetSymbol {
    /// The top-left corner.
    pub at: Point,
    /// Width and height.
    pub size: Size,
    /// Border width; 0 is the schematic's default.
    pub width: f64,
    /// The fields: `Sheetname`, `Sheetfile` and any the user added.
    pub fields: Vec<Field>,
    /// The pins on its edges.
    pub pins: Vec<SheetPin>,
}

impl SheetSymbol {
    /// The value of the field named `name`, when present.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.text.text.as_str())
    }

    /// The sheet's name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.field("Sheetname")
    }

    /// The file the sheet is in.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        self.field("Sheetfile")
    }
}

/// One parsed sheet of a schematic.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    /// The file format version, `20250114` and later.
    pub version: u32,
    /// The program that wrote the file.
    pub generator: Option<String>,
    /// That program's version.
    pub generator_version: Option<String>,
    /// The paper.
    pub paper: Paper,
    /// The embedded library, in file order.
    pub library: Vec<LibrarySymbol>,
    /// The placed symbols.
    pub symbols: Vec<Symbol>,
    /// The wires.
    pub wires: Vec<Wire>,
    /// The buses.
    pub buses: Vec<Wire>,
    /// Wire-to-bus entries.
    pub bus_entries: Vec<BusEntry>,
    /// Junction dots.
    pub junctions: Vec<Junction>,
    /// No-connect marks, by position.
    pub no_connects: Vec<Point>,
    /// Labels of every kind.
    pub labels: Vec<Label>,
    /// Hierarchical sheet symbols.
    pub sheet_symbols: Vec<SheetSymbol>,
    /// Free text.
    pub texts: Vec<TextItem>,
    /// Free graphics: lines, boxes, circles, arcs.
    pub graphics: Vec<Graphic>,
}

impl Sheet {
    /// The library symbol a placed symbol draws.
    #[must_use]
    pub fn library_symbol(&self, symbol: &Symbol) -> Option<&LibrarySymbol> {
        self.library.get(symbol.library.index())
    }

    /// Every field drawn on the sheet: the symbols' and the sheets'.
    pub fn all_fields(&self) -> impl Iterator<Item = &Field> {
        self.symbols
            .iter()
            .flat_map(|symbol| symbol.fields.iter())
            .chain(
                self.sheet_symbols
                    .iter()
                    .flat_map(|sheet| sheet.fields.iter()),
            )
    }
}

/// The vertical anchoring a label kind draws with, whatever the file
/// says: a local label stands on its wire, the flagged kinds are centred
/// in their flag.
#[must_use]
pub fn label_vertical(kind: LabelKind) -> Vertical {
    match kind {
        LabelKind::Local => Vertical::Bottom,
        LabelKind::Global(_) | LabelKind::Hierarchical(_) => Vertical::Center,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9
    }

    #[test]
    fn an_unturned_frame_flips_the_library_y_axis() {
        let frame = Frame::new(Point { x: 10.0, y: 20.0 }, 0.0, Mirror::None);
        // A pin above the origin in the library is above on the sheet,
        // which is smaller y.
        assert!(close(
            frame.apply(Point { x: 0.0, y: 2.54 }),
            Point { x: 10.0, y: 17.46 }
        ));
        assert!(!frame.swaps_axes());
    }

    #[test]
    fn a_quarter_turn_is_counter_clockwise_on_the_sheet() {
        let frame = Frame::new(Point::default(), 90.0, Mirror::None);
        // Library right (1, 0) turns to sheet up (0, -1).
        assert!(close(
            frame.rotate(Point { x: 1.0, y: 0.0 }),
            Point { x: 0.0, y: -1.0 }
        ));
        // Library up (0, 1) is sheet up before the turn, left after it.
        assert!(close(
            frame.rotate(Point { x: 0.0, y: 1.0 }),
            Point { x: -1.0, y: 0.0 }
        ));
        assert!(frame.swaps_axes());
        let frame = Frame::new(Point::default(), 270.0, Mirror::None);
        assert!(close(
            frame.rotate(Point { x: 1.0, y: 0.0 }),
            Point { x: 0.0, y: 1.0 }
        ));
    }

    #[test]
    fn a_mirror_applies_after_the_turn() {
        // Mirror y flips left and right on the sheet.
        let frame = Frame::new(Point::default(), 0.0, Mirror::Y);
        assert!(close(
            frame.rotate(Point { x: 1.0, y: 1.0 }),
            Point { x: -1.0, y: -1.0 }
        ));
        // Turned a quarter and then mirrored in y: library right went
        // up, and up stays up under a left-right mirror.
        let frame = Frame::new(Point::default(), 90.0, Mirror::Y);
        assert!(close(
            frame.rotate(Point { x: 1.0, y: 0.0 }),
            Point { x: 0.0, y: -1.0 }
        ));
        assert!(close(
            frame.rotate(Point { x: 0.0, y: 1.0 }),
            Point { x: 1.0, y: 0.0 }
        ));
        let frame = Frame::new(Point::default(), 0.0, Mirror::X);
        assert!(close(
            frame.rotate(Point { x: 1.0, y: 1.0 }),
            Point { x: 1.0, y: 1.0 }
        ));
    }

    #[test_case(0.0, Mirror::None; "unturned")]
    #[test_case(90.0, Mirror::Y; "a quarter and mirrored in y")]
    #[test_case(180.0, Mirror::X; "a half and mirrored in x")]
    #[test_case(270.0, Mirror::None; "three quarters")]
    fn unapply_inverts_apply(rotation: f64, mirror: Mirror) {
        let frame = Frame::new(Point { x: 3.0, y: -4.0 }, rotation, mirror);
        let p = Point { x: 1.5, y: 2.5 };
        assert!(close(frame.unapply(frame.apply(p)), p));
    }

    #[test]
    fn a_label_turned_half_way_reads_upright_on_the_other_side() {
        assert_eq!(Spin::of(0.0, Horizontal::Left), Spin::Right);
        assert_eq!(Spin::of(0.0, Horizontal::Right), Spin::Left);
        assert_eq!(Spin::of(180.0, Horizontal::Right), Spin::Left);
        assert_eq!(Spin::of(90.0, Horizontal::Left), Spin::Up);
        assert_eq!(Spin::of(270.0, Horizontal::Right), Spin::Bottom);
        assert_eq!(Spin::Left.justify(), Horizontal::Right);
        assert!((Spin::Up.angle() - 90.0).abs() < 1e-12);
    }

    #[test]
    fn a_field_shows_its_name_only_when_asked() {
        let field = Field {
            name: "Reference".to_owned(),
            text: TextItem {
                text: "R1".to_owned(),
                at: Point::default(),
                rotation: 0.0,
                hidden: false,
                font: Font {
                    face: None,
                    size: Size {
                        width: 1.27,
                        height: 1.27,
                    },
                    thickness: None,
                    bold: false,
                    italic: false,
                },
                justify: Justify::default(),
            },
            show_name: false,
        };
        assert_eq!(field.shown(), "R1");
        let named = Field {
            show_name: true,
            ..field
        };
        assert_eq!(named.shown(), "Reference: R1");
    }

    #[test]
    fn named_papers_are_landscape_and_user_has_no_size_of_its_own() {
        let a4 = PaperSize::named("A4").expect("A4");
        assert_eq!(a4, PaperSize::A4);
        assert_eq!(a4.as_str(), "A4");
        let size = a4.landscape().expect("a named size");
        assert!((size.width - 297.0).abs() < 1e-12 && (size.height - 210.0).abs() < 1e-12);
        assert_eq!(PaperSize::named("User"), Some(PaperSize::User));
        assert_eq!(PaperSize::User.landscape(), None);
        assert_eq!(PaperSize::named("B9"), None);
        assert_eq!(Paper::a4().size, size);
    }
}
