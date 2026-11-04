//! The schematic parser: a `.kicad_sch` text into a [`Sheet`].
//!
//! The walk is over the [`sexpr`] tree, as the board parser's is, and the
//! rules are the same: known elements are read with every field they need
//! required, unknown elements and unknown fields are skipped, and a format
//! older than KiCad 9 is refused naming its version. Symbols come from the
//! file's own `lib_symbols`; a placed symbol naming one the file lacks is
//! an error, since nothing else could draw it. A derived library symbol
//! (`extends`) gets its parent's body and pins at parse time, so the model
//! holds complete symbols.

pub mod connect;
pub mod hit;
pub mod model;
pub mod sheet_tree;

use std::collections::BTreeMap;
use std::time::Instant;

use crate::board::{at, flag, optional_number, optional_text, point, size};
use crate::error::{Error, MIN_SCHEMATIC_VERSION, Result};
use crate::model::{Font, Horizontal, Justify, Point, Property, Size, Vertical};
use crate::sexpr::{self, Deadline, Node};
use model::{
    BusEntry, Field, Fill, FlagShape, Graphic, Junction, Label, LabelKind, LibrarySymbol,
    LibrarySymbolId, Mirror, Paper, PaperSize, Pin, PinElectrical, PinShape, Sheet, SheetPin,
    SheetSymbol, Symbol, SymbolShape, TextItem, Unit, Wire,
};

/// KiCad's default text size in a schematic.
const DEFAULT_TEXT_SIZE: f64 = 1.27;

/// Where pin names start inside the body when the symbol does not say:
/// KiCad's 20 mils.
const DEFAULT_PIN_NAME_OFFSET: f64 = 0.508;

/// Parses a KiCad 9 or 10 schematic file: one sheet.
///
/// # Errors
/// The text is not a well-formed S-expression, is not a `kicad_sch`, has
/// a format version older than [`MIN_SCHEMATIC_VERSION`]
/// ([`Error::UnsupportedSchematicVersion`]), has a known element with a
/// missing or malformed field, or places a symbol the file's
/// `lib_symbols` does not hold.
pub fn parse_schematic(source: &str) -> Result<Sheet> {
    parse(source, Deadline::NONE)
}

/// [`parse_schematic`], giving up with [`Error::DeadlinePassed`] shortly
/// after `deadline`: the reader looks at the clock every thousand or so
/// items and the walk once per library symbol and top-level element. For
/// a caller that stops waiting at a deadline (the server's upload) and
/// wants the parse to stop with it. Not for wasm32-unknown-unknown, where
/// the clock panics.
///
/// # Errors
/// Those of [`parse_schematic`], and [`Error::DeadlinePassed`].
pub fn parse_schematic_before(source: &str, deadline: Instant) -> Result<Sheet> {
    parse(source, Deadline::at(deadline))
}

fn parse(source: &str, deadline: Deadline) -> Result<Sheet> {
    let root = sexpr::read_by(source, deadline)?;
    let head = root.head()?;
    if head != "kicad_sch" {
        return Err(root.error(format!("expected (kicad_sch ...), found ({head} ...)")));
    }
    let version = root.require("version")?;
    let number = version.integer(0)?;
    let version = u32::try_from(number).map_err(|_| {
        version.error(format!(
            "expected the format version to be a positive integer, found {number}"
        ))
    })?;
    if version < MIN_SCHEMATIC_VERSION {
        return Err(Error::UnsupportedSchematicVersion { version });
    }

    let mut library: Vec<LibrarySymbol> = Vec::new();
    if let Some(table) = root.child("lib_symbols") {
        for node in table.children("symbol") {
            deadline.check()?;
            library.push(library_symbol(node)?);
        }
    }
    resolve_extends(&mut library, deadline)?;
    let mut by_name: BTreeMap<&str, LibrarySymbolId> = BTreeMap::new();
    for (index, symbol) in library.iter().enumerate() {
        let id = u32::try_from(index)
            .map(LibrarySymbolId)
            .map_err(|_| root.error("more library symbols than the model holds"))?;
        by_name.entry(symbol.name.as_str()).or_insert(id);
    }

    let mut schematic = Sheet {
        version,
        uuid: None,
        generator: None,
        generator_version: None,
        paper: Paper::a4(),
        library: Vec::new(),
        symbols: Vec::new(),
        wires: Vec::new(),
        buses: Vec::new(),
        bus_entries: Vec::new(),
        junctions: Vec::new(),
        no_connects: Vec::new(),
        labels: Vec::new(),
        sheet_symbols: Vec::new(),
        texts: Vec::new(),
        graphics: Vec::new(),
    };
    for node in root.lists() {
        deadline.check()?;
        match node.head()? {
            "uuid" => schematic.uuid = Some(node.atom(0)?.to_owned()),
            "generator" => schematic.generator = Some(node.atom(0)?.to_owned()),
            "generator_version" => schematic.generator_version = Some(node.atom(0)?.to_owned()),
            "paper" => schematic.paper = paper(node)?,
            "symbol" => schematic.symbols.push(symbol(node, &by_name)?),
            "wire" => schematic.wires.push(wire(node)?),
            "bus" => schematic.buses.push(wire(node)?),
            "bus_entry" => schematic.bus_entries.push(BusEntry {
                at: point(node.require("at")?)?,
                size: point(node.require("size")?)?,
                width: stroke_width(node)?,
            }),
            "junction" => schematic.junctions.push(Junction {
                at: point(node.require("at")?)?,
                diameter: optional_number(node, "diameter")?.unwrap_or(0.0),
            }),
            "no_connect" => schematic.no_connects.push(point(node.require("at")?)?),
            "label" => schematic.labels.push(label(node, LabelKind::Local)?),
            "global_label" => {
                let kind = LabelKind::Global(flag_shape(node)?);
                schematic.labels.push(label(node, kind)?);
            }
            "hierarchical_label" => {
                let kind = LabelKind::Hierarchical(flag_shape(node)?);
                schematic.labels.push(label(node, kind)?);
            }
            "sheet" => schematic.sheet_symbols.push(sheet(node)?),
            "text" => schematic.texts.push(text_item(node, node.atom(0)?)?),
            "polyline" | "rectangle" | "circle" | "arc" | "bezier" => {
                schematic.graphics.push(graphic(node)?);
            }
            _ => {}
        }
    }
    schematic.library = library;
    Ok(schematic)
}

/// A `(paper "A4" [portrait])` or `(paper "User" w h)`.
fn paper(node: &Node) -> Result<Paper> {
    let name = node.atom(0)?;
    let kind = PaperSize::named(name).ok_or_else(|| {
        node.error(format!(
            "expected a paper size (A0 to A5, A to E, USLetter, USLegal, USLedger, User), \
             found {name:?}"
        ))
    })?;
    let size = match kind.landscape() {
        None => Size {
            width: node.number(1)?,
            height: node.number(2)?,
        },
        Some(size) if node.has_flag("portrait") => Size {
            width: size.height,
            height: size.width,
        },
        Some(size) => size,
    };
    Ok(Paper { kind, size })
}

/// A `(symbol "name" ...)` of `lib_symbols`.
fn library_symbol(node: &Node) -> Result<LibrarySymbol> {
    let name = node.atom(0)?.to_owned();
    let pin_numbers = node.child("pin_numbers");
    let pin_names = node.child("pin_names");
    let hidden = |settings: Option<&Node>| {
        settings.is_some_and(|node| node.has_flag("hide") || flag(node, "hide"))
    };
    let mut symbol = LibrarySymbol {
        name,
        extends: optional_text(node, "extends")?,
        power: node.child("power").is_some(),
        pin_numbers_hidden: hidden(pin_numbers),
        pin_names_hidden: hidden(pin_names),
        pin_name_offset: pin_names
            .map(|names| optional_number(names, "offset"))
            .transpose()?
            .flatten()
            .unwrap_or(DEFAULT_PIN_NAME_OFFSET),
        properties: Vec::new(),
        units: Vec::new(),
    };
    for child in node.lists() {
        match child.head()? {
            "property" => symbol.properties.push(Property {
                name: child.atom(0)?.to_owned(),
                value: child.atom(1)?.to_owned(),
            }),
            "symbol" => symbol.units.push(unit(child)?),
            _ => {}
        }
    }
    Ok(symbol)
}

/// A derived symbol takes its parent's units and pin settings. KiCad
/// allows one level of derivation; a parent the file lacks leaves the
/// symbol as written, with whatever body it has. The clock is read once
/// per derived symbol: the search for a parent by its short name is a
/// scan of every parent.
fn resolve_extends(library: &mut [LibrarySymbol], deadline: Deadline) -> Result<()> {
    let parents: BTreeMap<String, LibrarySymbol> = library
        .iter()
        .filter(|symbol| symbol.extends.is_none())
        .map(|symbol| (symbol.name.clone(), symbol.clone()))
        .collect();
    for symbol in library.iter_mut() {
        let Some(parent_name) = &symbol.extends else {
            continue;
        };
        deadline.check()?;
        // The file names the parent without its library prefix; the
        // parent entry carries it.
        let parent = parents.get(parent_name).or_else(|| {
            parents.values().find(|candidate| {
                candidate
                    .name
                    .rsplit_once(':')
                    .is_some_and(|(_, short)| short == parent_name)
            })
        });
        if let Some(parent) = parent {
            symbol.units = parent.units.clone();
            symbol.power = symbol.power || parent.power;
            symbol.pin_numbers_hidden = parent.pin_numbers_hidden;
            symbol.pin_names_hidden = parent.pin_names_hidden;
            symbol.pin_name_offset = parent.pin_name_offset;
        }
    }
    Ok(())
}

/// A sub-symbol `(symbol "NAME_U_S" ...)`: the unit and body style are the
/// last two numbers of the name.
fn unit(node: &Node) -> Result<Unit> {
    let name = node.atom(0)?;
    let malformed = || {
        node.error(format!(
            "expected a sub-symbol named NAME_UNIT_STYLE, found {name:?}"
        ))
    };
    let number = |part: &str| -> Result<u32> { part.parse().map_err(|_| malformed()) };
    let (rest, style) = name.rsplit_once('_').ok_or_else(malformed)?;
    let (_, unit) = rest.rsplit_once('_').ok_or_else(malformed)?;
    let mut out = Unit {
        unit: number(unit)?,
        body_style: number(style)?,
        graphics: Vec::new(),
        texts: Vec::new(),
        pins: Vec::new(),
    };
    for child in node.lists() {
        match child.head()? {
            "polyline" | "rectangle" | "circle" | "arc" | "bezier" => {
                out.graphics.push(graphic(child)?);
            }
            "text" => out.texts.push(text_item(child, child.atom(0)?)?),
            "pin" => out.pins.push(pin(child)?),
            _ => {}
        }
    }
    Ok(out)
}

/// A library pin: `(pin ELECTRICAL SHAPE (at x y angle) (length l) [hide]
/// (name "n" (effects ...)) (number "n" (effects ...)))`.
fn pin(node: &Node) -> Result<Pin> {
    let electrical = node.atom(0)?;
    let electrical = match electrical {
        "input" => PinElectrical::Input,
        "output" => PinElectrical::Output,
        "bidirectional" => PinElectrical::Bidirectional,
        "tri_state" => PinElectrical::TriState,
        "passive" => PinElectrical::Passive,
        "free" => PinElectrical::Free,
        "unspecified" => PinElectrical::Unspecified,
        "power_in" => PinElectrical::PowerIn,
        "power_out" => PinElectrical::PowerOut,
        "open_collector" => PinElectrical::OpenCollector,
        "open_emitter" => PinElectrical::OpenEmitter,
        "no_connect" => PinElectrical::NoConnect,
        _ => {
            return Err(node.error(format!(
                "expected a pin electrical type (input, output, bidirectional, tri_state, \
                 passive, free, unspecified, power_in, power_out, open_collector, \
                 open_emitter, no_connect), found {electrical:?}"
            )));
        }
    };
    let shape = node.atom(1)?;
    let shape = match shape {
        "line" => PinShape::Line,
        "inverted" => PinShape::Inverted,
        "clock" => PinShape::Clock,
        "inverted_clock" => PinShape::InvertedClock,
        "input_low" => PinShape::InputLow,
        "clock_low" => PinShape::ClockLow,
        "output_low" => PinShape::OutputLow,
        "edge_clock_high" => PinShape::EdgeClockHigh,
        "non_logic" => PinShape::NonLogic,
        _ => {
            return Err(node.error(format!(
                "expected a pin shape (line, inverted, clock, inverted_clock, input_low, \
                 clock_low, output_low, edge_clock_high, non_logic), found {shape:?}"
            )));
        }
    };
    let (at, rotation) = at(node.require("at")?)?;
    let named = |key: &str| -> Result<(String, Size)> {
        let Some(child) = node.child(key) else {
            return Ok((String::new(), default_size()));
        };
        let text = child.atom(0)?.to_owned();
        let size = child
            .child("effects")
            .map(font)
            .transpose()?
            .map_or_else(default_size, |font| font.size);
        Ok((text, size))
    };
    let (name, name_size) = named("name")?;
    let (number, number_size) = named("number")?;
    Ok(Pin {
        number,
        name,
        electrical,
        shape,
        at,
        rotation,
        length: optional_number(node, "length")?.unwrap_or(0.0),
        hidden: node.has_flag("hide") || flag(node, "hide"),
        name_size,
        number_size,
    })
}

fn default_size() -> Size {
    Size {
        width: DEFAULT_TEXT_SIZE,
        height: DEFAULT_TEXT_SIZE,
    }
}

/// KiCad's font when the file says nothing: the stroke font at the
/// default size.
fn default_font() -> Font {
    Font {
        face: None,
        size: default_size(),
        thickness: None,
        bold: false,
        italic: false,
    }
}

/// The `(font ...)` of an `(effects ...)`: size, thickness, bold, italic
/// and face, KiCad's defaults where the file is silent.
fn font(effects: &Node) -> Result<Font> {
    let mut font = default_font();
    if let Some(node) = effects.child("font") {
        font.face = optional_text(node, "face")?;
        if let Some(size_node) = node.child("size") {
            font.size = size(size_node)?;
        }
        font.thickness = optional_number(node, "thickness")?;
        font.bold = node.has_flag("bold") || flag(node, "bold");
        font.italic = node.has_flag("italic") || flag(node, "italic");
    }
    Ok(font)
}

/// The `(justify ...)` of an `(effects ...)`; centred both ways when
/// absent, as KiCad's text attributes default.
fn justify(effects: &Node) -> Justify {
    let mut justify = Justify::default();
    if let Some(node) = effects.child("justify") {
        if node.has_flag("left") {
            justify.horizontal = Horizontal::Left;
        } else if node.has_flag("right") {
            justify.horizontal = Horizontal::Right;
        }
        if node.has_flag("top") {
            justify.vertical = Vertical::Top;
        } else if node.has_flag("bottom") {
            justify.vertical = Vertical::Bottom;
        }
        justify.mirror = node.has_flag("mirror");
    }
    justify
}

/// Whether an item is hidden: `(hide yes)` or a bare `hide` on the item,
/// or either inside its `(effects ...)`, every spelling KiCad has used.
fn hidden(node: &Node) -> bool {
    let in_effects = node
        .child("effects")
        .is_some_and(|effects| effects.has_flag("hide") || flag(effects, "hide"));
    node.has_flag("hide") || flag(node, "hide") || in_effects
}

/// A text item from a node with `(at x y angle)` and `(effects ...)`,
/// reading `text`.
fn text_item(node: &Node, text: &str) -> Result<TextItem> {
    let (at, rotation) = at(node.require("at")?)?;
    let (font, justify) = match node.child("effects") {
        Some(effects) => (self::font(effects)?, self::justify(effects)),
        None => (default_font(), Justify::default()),
    };
    Ok(TextItem {
        text: text.to_owned(),
        at,
        rotation,
        hidden: hidden(node),
        font,
        justify,
    })
}

/// A `(property "Name" "Value" (at ...) [(show_name yes)] [(hide yes)]
/// (effects ...))`.
fn field(node: &Node) -> Result<Field> {
    let name = node.atom(0)?.to_owned();
    let value = node.atom(1)?;
    Ok(Field {
        name,
        text: text_item(node, value)?,
        show_name: flag(node, "show_name"),
    })
}

/// The `(stroke (width w) ...)` width; 0 when absent, the default.
fn stroke_width(node: &Node) -> Result<f64> {
    match node
        .child("stroke")
        .and_then(|stroke| stroke.child("width"))
    {
        Some(width) => width.number(0),
        None => Ok(0.0),
    }
}

/// The `(fill (type ...))` of a graphic. A `color` fill is a custom
/// colour the model does not carry; it is drawn as the body colour.
fn fill(node: &Node) -> Fill {
    match node
        .child("fill")
        .and_then(|fill| fill.child("type"))
        .and_then(|kind| kind.atom_opt(0))
    {
        Some("outline") => Fill::Outline,
        Some("background" | "color") => Fill::Background,
        _ => Fill::None,
    }
}

/// The `(xy ...)` points of a `(pts ...)`.
fn points(node: &Node) -> Result<Vec<Point>> {
    node.children("xy").map(point).collect()
}

/// A graphic shape, in a library symbol or free on the sheet.
fn graphic(node: &Node) -> Result<Graphic> {
    let head = node.head()?;
    let shape = match head {
        "polyline" => SymbolShape::Polyline {
            points: points(node.require("pts")?)?,
        },
        "rectangle" => SymbolShape::Rect {
            start: point(node.require("start")?)?,
            end: point(node.require("end")?)?,
        },
        "circle" => SymbolShape::Circle {
            center: point(node.require("center")?)?,
            radius: node.require("radius")?.number(0)?,
        },
        "arc" => SymbolShape::Arc {
            start: point(node.require("start")?)?,
            mid: point(node.require("mid")?)?,
            end: point(node.require("end")?)?,
        },
        "bezier" => SymbolShape::Bezier {
            points: points(node.require("pts")?)?,
        },
        _ => {
            return Err(node.error(format!(
                "expected a shape (polyline, rectangle, circle, arc, bezier), found ({head} ...)"
            )));
        }
    };
    Ok(Graphic {
        shape,
        width: stroke_width(node)?,
        fill: fill(node),
    })
}

/// A placed `(symbol (lib_id ...) (at ...) ...)`.
fn symbol(node: &Node, by_name: &BTreeMap<&str, LibrarySymbolId>) -> Result<Symbol> {
    let lib_id_node = node.require("lib_id")?;
    let lib_id = lib_id_node.atom(0)?;
    let library = by_name.get(lib_id).copied().ok_or_else(|| {
        lib_id_node.error(format!(
            "the symbol names {lib_id:?}, which is not in lib_symbols"
        ))
    })?;
    let (at, rotation) = at(node.require("at")?)?;
    let mirror = match node
        .child("mirror")
        .map(|mirror| mirror.atom(0))
        .transpose()?
    {
        Some("x") => Mirror::X,
        Some("y") => Mirror::Y,
        _ => Mirror::None,
    };
    let positive = |name: &str, default: u32| -> Result<u32> {
        match node.child(name) {
            Some(child) => {
                let value = child.integer(0)?;
                u32::try_from(value).map_err(|_| {
                    child.error(format!("expected a {name} of 0 or more, found {value}"))
                })
            }
            None => Ok(default),
        }
    };
    let unit = positive("unit", 1)?;
    let body_style = positive("body_style", 1)?;
    let mut fields = Vec::new();
    for child in node.children("property") {
        fields.push(field(child)?);
    }
    Ok(Symbol {
        lib_id: lib_id.to_owned(),
        library,
        at,
        rotation,
        mirror,
        unit,
        body_style,
        dnp: flag(node, "dnp"),
        fields,
    })
}

/// A `(wire (pts ...) (stroke ...))` or a `(bus ...)`.
fn wire(node: &Node) -> Result<Wire> {
    Ok(Wire {
        points: points(node.require("pts")?)?,
        width: stroke_width(node)?,
    })
}

/// The `(shape ...)` of a global or hierarchical label or a sheet pin,
/// read from `word`; `passive` when the file leaves it out.
fn shape_named(node: &Node, word: Option<&str>) -> Result<FlagShape> {
    Ok(match word {
        Some("input") => FlagShape::Input,
        Some("output") => FlagShape::Output,
        Some("bidirectional") => FlagShape::Bidirectional,
        Some("tri_state") => FlagShape::TriState,
        Some("passive") | None => FlagShape::Passive,
        Some(other) => {
            return Err(node.error(format!(
                "expected a label shape (input, output, bidirectional, tri_state, passive), \
                 found {other:?}"
            )));
        }
    })
}

fn flag_shape(node: &Node) -> Result<FlagShape> {
    match node.child("shape") {
        Some(shape) => shape_named(shape, Some(shape.atom(0)?)),
        None => Ok(FlagShape::Passive),
    }
}

/// A `(label "net" (at ...) (effects ...) ...)` of any kind.
fn label(node: &Node, kind: LabelKind) -> Result<Label> {
    let text = text_item(node, node.atom(0)?)?;
    let mut fields = Vec::new();
    for child in node.children("property") {
        fields.push(field(child)?);
    }
    Ok(Label { kind, text, fields })
}

/// A `(sheet (at ...) (size ...) ... (property ...) (pin ...))`.
fn sheet(node: &Node) -> Result<SheetSymbol> {
    let mut sheet = SheetSymbol {
        uuid: optional_text(node, "uuid")?,
        at: point(node.require("at")?)?,
        size: size(node.require("size")?)?,
        width: stroke_width(node)?,
        fields: Vec::new(),
        pins: Vec::new(),
    };
    for child in node.lists() {
        match child.head()? {
            "property" => sheet.fields.push(field(child)?),
            "pin" => sheet.pins.push(SheetPin {
                shape: shape_named(child, child.atom_opt(1))?,
                text: text_item(child, child.atom(0)?)?,
            }),
            _ => {}
        }
    }
    Ok(sheet)
}
