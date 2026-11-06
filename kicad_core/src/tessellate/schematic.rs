//! Triangles from a schematic sheet: every symbol's body, pins and texts
//! carried onto the sheet by its [`Frame`], the wires, buses, junctions
//! and no-connect marks, the labels in their flags, the sheet symbols
//! with their pins, and the free text and graphics, each on the
//! [`SchematicLayer`] KiCad colours it by.
//!
//! The placement rules are KiCad's `SCH_PAINTER`: pin names and numbers
//! sit along the pin by its drawn orientation, a symbol's fields and body
//! texts are drawn centred on their transformed text box (so a mirrored
//! symbol keeps its text readable), a label's flag is sized by the text
//! and the label size ratio, and a do-not-populate symbol gets a cross
//! over its body. Widths and sizes of 0 in the file mean KiCad's defaults,
//! named below.

use std::collections::{BTreeMap, BTreeSet};

use super::{Curves, ItemMesh, Mesh, MeshBuilder, Tessellator, arc_points, bezier, circle};
use crate::font::{self, Placement, block_size};
use crate::geometry::{Bounds, Transform};
use crate::model::{Font, Horizontal, Justify, Point, Size, Vertical};
use crate::schematic::connect::{SheetItem, SheetNet};
use crate::schematic::model::{
    Fill, FlagShape, Frame, Graphic, Label, LabelKind, LibrarySymbol, Mirror, Pin, PinShape, Sheet,
    SheetPin, SheetSymbol, Spin, Symbol, SymbolShape, TextItem, label_vertical,
};

/// KiCad's default line width, 6 mils: symbol strokes written as 0, pin
/// lines, no-connect marks, the free graphics.
pub const DEFAULT_LINE_WIDTH: f64 = 0.1524;
/// KiCad's default wire width, 6 mils.
pub const DEFAULT_WIRE_WIDTH: f64 = 0.1524;
/// KiCad's default bus width, 12 mils.
pub const DEFAULT_BUS_WIDTH: f64 = 0.3048;
/// KiCad's default junction diameter, 36 mils.
pub const DEFAULT_JUNCTION_DIAMETER: f64 = 0.9144;
/// KiCad's no-connect mark, 48 mils corner to corner.
pub const NO_CONNECT_SIZE: f64 = 1.2192;
/// The gap between a pin line and the text beside it, 4 mils.
const PIN_TEXT_MARGIN: f64 = 0.1016;
/// A label's flag grows by this fraction of the text height on each side.
const LABEL_SIZE_RATIO: f64 = 0.375;
/// A label's text sits this fraction of its height off its wire.
const TEXT_OFFSET_RATIO: f64 = 0.15;
/// The do-not-populate cross: three default line widths.
const DNP_STROKE: f64 = 3.0 * DEFAULT_LINE_WIDTH;

/// The colour classes of a sheet, KiCad's schematic layers, declared in
/// the order they are painted from the bottom up: a layer compares less
/// than every layer painted over it, which is the order
/// [`Tessellator::sheet`] hands them out in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SchematicLayer {
    /// Symbol body fills.
    Background,
    /// Symbol outlines, outline fills and body text.
    Device,
    /// Free text and graphics.
    Notes,
    /// Buses.
    Bus,
    /// Wires and bus entries.
    Wire,
    /// Local labels.
    LocalLabel,
    /// Global labels, flag and text.
    GlobalLabel,
    /// Hierarchical labels, flag and text.
    HierLabel,
    /// No-connect marks.
    NoConnect,
    /// Junction dots.
    Junction,
    /// Sheet symbol borders.
    SheetSymbol,
    /// Sheet names.
    SheetName,
    /// Sheet file names.
    SheetFile,
    /// Sheet pins, flag and text.
    SheetLabel,
    /// Pin lines and their decorations.
    Pin,
    /// Pin names.
    PinName,
    /// Pin numbers.
    PinNumber,
    /// Every field but the reference and the value, on symbols and sheets.
    Fields,
    /// Value fields.
    Value,
    /// Reference fields.
    Reference,
    /// Do-not-populate crosses.
    Dnp,
}

/// The mesh of one item of a sheet on one layer, kept so that a region of
/// finer tessellation builds again only the curved items it reaches (#51).
#[derive(Debug, Clone, PartialEq)]
pub struct SheetMesh {
    /// The item's place in the order the sheet draws them in.
    pub item: usize,
    /// The net item it is, for the highlight; `None` for one that is on
    /// no net, or drawn with others (a symbol's body, a pin's texts).
    pub net_item: Option<SheetItem>,
    /// The triangles.
    pub item_mesh: ItemMesh,
}

/// Every [`SheetMesh`] of a sheet, by layer in draw order and by item
/// within a layer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SheetMeshes {
    layers: BTreeMap<SchematicLayer, Vec<SheetMesh>>,
}

impl SheetMeshes {
    /// Every layer that has something on it as one mesh, in draw order.
    #[must_use]
    pub fn layers(&self) -> Vec<(SchematicLayer, Mesh)> {
        self.layers
            .iter()
            .map(|(layer, units)| {
                let mut mesh = Mesh::default();
                for unit in units {
                    mesh.append(&unit.item_mesh.mesh);
                }
                (*layer, mesh)
            })
            .filter(|(_, mesh)| !mesh.is_empty())
            .collect()
    }

    /// The geometry of `net`, by layer in draw order: its items' units,
    /// in the order the net lists the items.
    #[must_use]
    pub fn net(&self, net: &SheetNet) -> Vec<(SchematicLayer, Mesh)> {
        let mut by_item: BTreeMap<SheetItem, Vec<(SchematicLayer, &SheetMesh)>> = BTreeMap::new();
        for (layer, units) in &self.layers {
            for unit in units {
                if let Some(key) = unit.net_item {
                    by_item.entry(key).or_default().push((*layer, unit));
                }
            }
        }
        let mut out: BTreeMap<SchematicLayer, Mesh> = BTreeMap::new();
        for item in &net.items {
            for (layer, unit) in by_item.get(item).into_iter().flatten() {
                out.entry(*layer).or_default().append(&unit.item_mesh.mesh);
            }
        }
        out.into_iter()
            .filter(|(_, mesh)| !mesh.is_empty())
            .collect()
    }
}

impl Tessellator {
    /// Every layer of `sheet` that has something on it, in draw order
    /// (the [`SchematicLayer`] order).
    #[must_use]
    pub fn sheet(&self, sheet: &Sheet) -> Vec<(SchematicLayer, Mesh)> {
        self.sheet_units(sheet).layers()
    }

    /// Every item of `sheet` as the units it is drawn from.
    #[must_use]
    pub fn sheet_units(&self, sheet: &Sheet) -> SheetMeshes {
        let mut layers = Layers::new(self);
        layers.sheet(sheet);
        layers.into_units()
    }

    /// Build again, in `units`, every curved item this tessellator's
    /// region reaches and every item built fine before: the rest keep
    /// their meshes. True when anything changed.
    pub fn refine_sheet(&self, sheet: &Sheet, units: &mut SheetMeshes) -> bool {
        let curves = Curves::of(self);
        // The items to build, with the tolerance a stroke's caps take.
        let mut only: BTreeMap<usize, f64> = BTreeMap::new();
        for unit in units.layers.values().flatten() {
            if !unit.item_mesh.wants(self.fine) {
                continue;
            }
            let round = curves.round_for(unit.item_mesh.bounds.as_ref());
            let entry = only.entry(unit.item).or_insert(round);
            *entry = entry.min(round);
        }
        if only.is_empty() {
            return false;
        }
        let mut layers = Layers::new(self);
        layers.only = Some(only);
        layers.sheet(sheet);
        let fresh = layers.into_units();
        let built: BTreeSet<usize> = fresh
            .layers
            .values()
            .flatten()
            .map(|unit| unit.item)
            .collect();
        for list in units.layers.values_mut() {
            list.retain(|unit| !built.contains(&unit.item));
        }
        for (layer, list) in fresh.layers {
            units.layers.entry(layer).or_default().extend(list);
        }
        for list in units.layers.values_mut() {
            list.sort_by_key(|unit| unit.item);
        }
        true
    }

    /// The box around everything `sheet` draws, or `None` for an empty
    /// sheet.
    #[must_use]
    pub fn sheet_bounds(&self, sheet: &Sheet) -> Option<Bounds> {
        self.sheet(sheet)
            .iter()
            .filter_map(|(_, mesh)| mesh.bounds())
            .reduce(Bounds::union)
    }
}

/// `value`, or `fallback` when the file wrote 0 for "the default".
pub(crate) fn default(value: f64, fallback: f64) -> f64 {
    if value > 0.0 { value } else { fallback }
}

/// How a text is laid out: the font as the file gives it (its pen from
/// [`font::pen_width`]), anchored at `position` with `justify`, turned by
/// `angle`.
fn placement(font: &Font, justify: Justify, position: Point, angle: f64) -> Placement {
    Placement {
        size: font.size,
        pen: font::pen_width(font),
        italic: font.italic,
        justify,
        position,
        angle,
    }
}

/// A frame to carry points by: a symbol's [`Frame`], or the identity for
/// an item drawn on the sheet itself.
struct Carried(Frame);

impl Carried {
    /// The frame of an item on the sheet: its points are already sheet
    /// points. `Frame::new` flips y for the library's axis, so the flip
    /// is undone by a mirror in x.
    fn sheet() -> Self {
        Carried(Frame::new(Point::default(), 0.0, Mirror::X))
    }
}

impl From<Frame> for Carried {
    fn from(frame: Frame) -> Self {
        Carried(frame)
    }
}

/// One mesh builder per layer, filled in any order, and the units each
/// item of the sheet made of them.
struct Layers {
    tessellator: Tessellator,
    curves: Curves,
    builders: BTreeMap<SchematicLayer, MeshBuilder<'static>>,
    /// The ordinal of the next item.
    item: usize,
    /// The tolerance a stroke's caps take.
    round: f64,
    /// When building again: the items to build, by ordinal, with the
    /// tolerance for their caps. The others are not drawn at all.
    only: Option<BTreeMap<usize, f64>>,
    units: Vec<(SchematicLayer, SheetMesh)>,
}

impl Layers {
    fn new(tessellator: &Tessellator) -> Self {
        Self {
            tessellator: *tessellator,
            curves: Curves::of(tessellator),
            builders: BTreeMap::new(),
            item: 0,
            round: tessellator
                .fine
                .map_or(tessellator.tolerance, |fine| fine.tolerance),
            only: None,
            units: Vec::new(),
        }
    }

    fn on(&mut self, layer: SchematicLayer) -> &mut MeshBuilder<'static> {
        let tessellator = self.tessellator;
        let round = self.round;
        let builder = self
            .builders
            .entry(layer)
            .or_insert_with(|| MeshBuilder::new(&tessellator));
        builder.set_round(round);
        builder
    }

    /// One item of the sheet: what `draw` adds, on whatever layers, is
    /// a unit on each. `key` is the net item it is, when it is one.
    fn item(&mut self, key: Option<SheetItem>, draw: impl FnOnce(&mut Self)) {
        let ordinal = self.item;
        self.item += 1;
        if let Some(only) = &self.only {
            let Some(&round) = only.get(&ordinal) else {
                return;
            };
            self.round = round;
        }
        self.curves.take_touched();
        draw(self);
        let curved = self.curves.take_touched();
        for (layer, builder) in &mut self.builders {
            let unit = builder.close_item(None, curved);
            if !unit.mesh.positions.is_empty() {
                self.units.push((
                    *layer,
                    SheetMesh {
                        item: ordinal,
                        net_item: key,
                        item_mesh: unit,
                    },
                ));
            }
        }
    }

    fn into_units(self) -> SheetMeshes {
        let mut units = SheetMeshes::default();
        for (layer, unit) in self.units {
            units.layers.entry(layer).or_default().push(unit);
        }
        units
    }

    /// Every item of `sheet`, in the order it draws them.
    fn sheet(&mut self, sheet: &Sheet) {
        for (index, symbol) in sheet.symbols.iter().enumerate() {
            if let Some(library) = sheet.library_symbol(symbol) {
                self.symbol(index, symbol, library);
            }
        }
        for (index, wire) in sheet.wires.iter().enumerate() {
            self.item(Some(SheetItem::Wire(index)), |layers| {
                let width = default(wire.width, DEFAULT_WIRE_WIDTH);
                layers
                    .on(SchematicLayer::Wire)
                    .stroke(&wire.points, width, false);
            });
        }
        for bus in &sheet.buses {
            self.item(None, |layers| {
                let width = default(bus.width, DEFAULT_BUS_WIDTH);
                layers
                    .on(SchematicLayer::Bus)
                    .stroke(&bus.points, width, false);
            });
        }
        for entry in &sheet.bus_entries {
            self.item(None, |layers| {
                let end = Point {
                    x: entry.at.x + entry.size.x,
                    y: entry.at.y + entry.size.y,
                };
                let width = default(entry.width, DEFAULT_WIRE_WIDTH);
                layers
                    .on(SchematicLayer::Wire)
                    .stroke(&[entry.at, end], width, false);
            });
        }
        for (index, junction) in sheet.junctions.iter().enumerate() {
            self.item(Some(SheetItem::Junction(index)), |layers| {
                let diameter = default(junction.diameter, DEFAULT_JUNCTION_DIAMETER);
                let dot = circle(junction.at, diameter / 2.0, &layers.curves);
                layers.on(SchematicLayer::Junction).fill(&[dot], &[]);
            });
        }
        for &at in &sheet.no_connects {
            self.item(None, |layers| layers.no_connect(at));
        }
        for (index, label) in sheet.labels.iter().enumerate() {
            self.item(Some(SheetItem::Label(index)), |layers| layers.label(label));
        }
        for (index, sheet_symbol) in sheet.sheet_symbols.iter().enumerate() {
            self.sheet_symbol(index, sheet_symbol);
        }
        for text in &sheet.texts {
            if !text.hidden {
                self.item(None, |layers| {
                    let placement = placement(&text.font, text.justify, text.at, text.rotation);
                    layers.text(SchematicLayer::Notes, &text.text, &placement);
                });
            }
        }
        for graphic in &sheet.graphics {
            self.item(None, |layers| {
                layers.graphic(graphic, &Carried::sheet(), SchematicLayer::Notes, None);
            });
        }
    }

    /// Text at `placement` on `layer`; a stroke of one point is a dot.
    fn text(&mut self, layer: SchematicLayer, content: &str, placement: &Placement) {
        if content.is_empty() {
            return;
        }
        let shape = font::shape(content, placement);
        self.on(layer).strokes(&shape);
    }

    /// A no-connect mark: a cross of [`NO_CONNECT_SIZE`].
    fn no_connect(&mut self, at: Point) {
        let d = NO_CONNECT_SIZE / 2.0;
        let corner = |dx: f64, dy: f64| Point {
            x: at.x + dx,
            y: at.y + dy,
        };
        let builder = self.on(SchematicLayer::NoConnect);
        builder.stroke(&[corner(-d, -d), corner(d, d)], DEFAULT_LINE_WIDTH, false);
        builder.stroke(&[corner(d, -d), corner(-d, d)], DEFAULT_LINE_WIDTH, false);
    }

    /// A graphic carried by `frame` onto `layer`: its fill (a background
    /// fill on [`SchematicLayer::Background`], an outline fill on the
    /// layer itself) and its stroke. `bounds` grows to hold it when given.
    fn graphic(
        &mut self,
        graphic: &Graphic,
        frame: &Carried,
        layer: SchematicLayer,
        bounds: Option<&mut Option<Bounds>>,
    ) {
        let f = |p: &Point| frame.0.apply(*p);
        let (points, closed) = match &graphic.shape {
            SymbolShape::Polyline { points } => (points.iter().map(f).collect::<Vec<_>>(), false),
            SymbolShape::Rect { start, end } => (
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
                .map(f)
                .collect(),
                true,
            ),
            SymbolShape::Circle { center, radius } => {
                (circle(f(center), *radius, &self.curves), true)
            }
            SymbolShape::Arc { start, mid, end } => {
                (arc_points(f(start), f(mid), f(end), &self.curves), false)
            }
            SymbolShape::Bezier { points } => {
                let control: Vec<Point> = points.iter().map(f).collect();
                // A symbol's curve of any other count draws as its points.
                let curve = match <[Point; 4]>::try_from(control) {
                    Ok(four) => bezier(four, &self.curves),
                    Err(other) => other,
                };
                (curve, false)
            }
        };
        if let Some(bounds) = bounds {
            let own = Bounds::around(points.iter().copied());
            *bounds = match (*bounds, own) {
                (Some(a), Some(b)) => Some(a.union(b)),
                (a, b) => a.or(b),
            };
        }
        let width = default(graphic.width, DEFAULT_LINE_WIDTH);
        match graphic.fill {
            Fill::None => {}
            // A free graphic's background is the page: nothing to paint.
            Fill::Background if layer == SchematicLayer::Notes => {}
            Fill::Background => self
                .on(SchematicLayer::Background)
                .fill(std::slice::from_ref(&points), &[]),
            Fill::Outline => self.on(layer).fill(std::slice::from_ref(&points), &[]),
        }
        self.on(layer).stroke(&points, width, closed);
    }

    /// A placed symbol: its applicable units' graphics, body texts and
    /// pins through its frame, its fields, and its cross when it is not
    /// populated.
    fn symbol(&mut self, index: usize, symbol: &Symbol, library: &LibrarySymbol) {
        let frame = symbol.frame();
        let carried = Carried::from(frame);
        // A do-not-populate cross is sized by the body and the pins, which
        // an item built on its own (a rebuild) has not drawn to measure.
        let (body, pins) = if symbol.dnp {
            self.symbol_extent(symbol, library, &frame)
        } else {
            (None, None)
        };
        let units = library
            .units
            .iter()
            .enumerate()
            .filter(|(_, unit)| unit.applies_to(symbol.unit, symbol.body_style));
        for (unit_index, unit) in units {
            self.item(None, |layers| {
                for graphic in &unit.graphics {
                    layers.graphic(graphic, &carried, SchematicLayer::Device, None);
                }
                for text in &unit.texts {
                    if !text.hidden {
                        let (centre, angle) = placed_text_centre(&frame, text);
                        let placement = placement(&text.font, Justify::default(), centre, angle);
                        layers.text(SchematicLayer::Device, &text.text, &placement);
                    }
                }
            });
            for (pin_index, pin) in unit.pins.iter().enumerate() {
                if pin.hidden {
                    continue;
                }
                // The pin's line is the item a net lights; its texts are
                // another.
                let key = SheetItem::Pin {
                    symbol: index,
                    unit: unit_index,
                    pin: pin_index,
                };
                self.item(Some(key), |layers| {
                    layers.pin_line(pin, &frame, &mut None);
                });
                let (pos, d, len) = pin_axis(pin, &frame);
                self.item(None, |layers| layers.pin_texts(pin, library, pos, d, len));
            }
        }
        self.item(None, |layers| {
            for field in &symbol.fields {
                if field.text.hidden {
                    continue;
                }
                let layer = match field.name.as_str() {
                    "Reference" => SchematicLayer::Reference,
                    "Value" => SchematicLayer::Value,
                    _ => SchematicLayer::Fields,
                };
                // The field's anchor is written on the sheet; KiCad keeps it
                // in the library frame and places the text by the centre of
                // its box carried through the frame, as for a body text.
                let local = TextItem {
                    at: frame.unapply(field.text.at),
                    ..field.text.clone()
                };
                let (centre, angle) = placed_text_centre(&frame, &local);
                let placement = placement(&field.text.font, Justify::default(), centre, angle);
                layers.text(layer, &field.shown(), &placement);
            }
        });
        if symbol.dnp {
            self.item(None, |layers| layers.dnp_cross(body, pins));
        }
    }

    /// The box around a symbol's body and the box around its pins, as
    /// they are drawn: what a do-not-populate cross is sized by.
    fn symbol_extent(
        &self,
        symbol: &Symbol,
        library: &LibrarySymbol,
        frame: &Frame,
    ) -> (Option<Bounds>, Option<Bounds>) {
        let carried = Carried::from(*frame);
        // At the floor tolerance whatever the region, so a rebuild
        // measures what the first build did.
        let floor = Tessellator {
            fine: None,
            ..self.tessellator
        };
        let mut scratch = Self::new(&floor);
        let mut body = None;
        let mut pins = None;
        for unit in library.units_for(symbol.unit, symbol.body_style) {
            for graphic in &unit.graphics {
                scratch.graphic(graphic, &carried, SchematicLayer::Device, Some(&mut body));
            }
            for pin in unit.pins.iter().filter(|pin| !pin.hidden) {
                scratch.pin_line(pin, frame, &mut pins);
            }
        }
        (body, pins)
    }

    /// A pin's line and the decoration its shape asks for, without its
    /// texts: where it starts, the unit direction it extends in, and how
    /// far.
    fn pin_line(
        &mut self,
        pin: &Pin,
        frame: &Frame,
        bounds: &mut Option<Bounds>,
    ) -> (Point, Point, f64) {
        let (pos, d, len) = pin_axis(pin, frame);
        let end = pin_ends(pin, frame).1;
        for point in [pos, end] {
            *bounds = Some(bounds.map_or(Bounds::of(point), |b| b.including(point)));
        }
        let along = |from: Point, distance: f64| Point {
            x: from.x + d.x * distance,
            y: from.y + d.y * distance,
        };
        let perp = Point { x: -d.y, y: d.x };
        let across = |from: Point, distance: f64| Point {
            x: from.x + perp.x * distance,
            y: from.y + perp.y * distance,
        };
        // KiCad's decoration sizes: half the number's height outside the
        // body, half the name's inside.
        let outer = pin.number_size.height / 2.0;
        let inner = if pin.name_size.height > 0.0 {
            pin.name_size.height / 2.0
        } else {
            outer
        };
        let curves = self.curves.clone();
        let builder = self.on(SchematicLayer::Pin);
        match pin.shape {
            PinShape::Inverted | PinShape::InvertedClock => {
                let bubble = circle(along(end, -outer), outer, &curves);
                builder.stroke(&bubble, DEFAULT_LINE_WIDTH, true);
                builder.stroke(&[pos, along(end, -2.0 * outer)], DEFAULT_LINE_WIDTH, false);
            }
            PinShape::NonLogic => {
                let r = outer;
                let diagonal = Point {
                    x: (d.x + perp.x) * r,
                    y: (d.y + perp.y) * r,
                };
                let other = Point {
                    x: (d.x - perp.x) * r,
                    y: (d.y - perp.y) * r,
                };
                for v in [diagonal, other] {
                    builder.stroke(
                        &[
                            Point {
                                x: end.x - v.x,
                                y: end.y - v.y,
                            },
                            Point {
                                x: end.x + v.x,
                                y: end.y + v.y,
                            },
                        ],
                        DEFAULT_LINE_WIDTH,
                        false,
                    );
                }
                builder.stroke(&[pos, end], DEFAULT_LINE_WIDTH, false);
            }
            // The low-active and edge shapes draw their line; their small
            // triangles are left out.
            PinShape::Line
            | PinShape::Clock
            | PinShape::InputLow
            | PinShape::ClockLow
            | PinShape::OutputLow
            | PinShape::EdgeClockHigh => {
                builder.stroke(&[pos, end], DEFAULT_LINE_WIDTH, false);
            }
        }
        if matches!(pin.shape, PinShape::Clock | PinShape::InvertedClock) {
            // The clock mark: a chevron inside the body at the pin's end.
            let apex = along(end, inner);
            builder.stroke(
                &[across(end, inner), apex, across(end, -inner)],
                DEFAULT_LINE_WIDTH,
                false,
            );
        }
        (pos, d, len)
    }

    /// The name and number of a pin at `pos` extending along `d` for
    /// `len`: KiCad's table by drawn orientation, with the name inside
    /// the body when the library gives an offset and above the pin (the
    /// number below) when it does not.
    fn pin_texts(&mut self, pin: &Pin, library: &LibrarySymbol, pos: Point, d: Point, len: f64) {
        let name = if library.pin_names_hidden {
            ""
        } else {
            pin.shown_name()
        };
        let number = if library.pin_numbers_hidden {
            ""
        } else {
            pin.number.as_str()
        };
        let name_font = text_font(pin.name_size);
        let number_font = text_font(pin.number_size);
        let line = DEFAULT_LINE_WIDTH;
        let inside_offset = library.pin_name_offset;
        // Which text goes where: inside the body, above the pin, below it.
        let (inside, above, below): (PinText<'_>, PinText<'_>, PinText<'_>) = if inside_offset > 0.0
        {
            (Some((name, &name_font)), Some((number, &number_font)), None)
        } else {
            (None, Some((name, &name_font)), Some((number, &number_font)))
        };
        let offset_for = |text: PinText<'_>| {
            text.map_or(0.0, |(_, font)| {
                PIN_TEXT_MARGIN + f64::midpoint(font::pen_width(font), line)
            })
        };
        let above_offset = offset_for(above);
        let below_offset = offset_for(below);
        let orientation = Orientation::of(d);
        let angle = if orientation.is_vertical() { 90.0 } else { 0.0 };
        let place = |along: f64, across: f64| orientation.place(pos, along, across);
        if let Some((text, font)) = inside {
            let placement = placement(
                font,
                Justify {
                    horizontal: orientation.inside_justify(),
                    vertical: Vertical::Center,
                    mirror: false,
                },
                place(inside_offset + len, 0.0),
                angle,
            );
            self.text(SchematicLayer::PinName, text, &placement);
        }
        let mid = len / 2.0;
        if let Some((text, font)) = above {
            let placement = placement(
                font,
                Justify {
                    horizontal: Horizontal::Center,
                    vertical: Vertical::Bottom,
                    mirror: false,
                },
                place(mid, -above_offset),
                angle,
            );
            let layer = if inside.is_some() {
                SchematicLayer::PinNumber
            } else {
                SchematicLayer::PinName
            };
            self.text(layer, text, &placement);
        }
        if let Some((text, font)) = below {
            let placement = placement(
                font,
                Justify {
                    horizontal: Horizontal::Center,
                    vertical: Vertical::Top,
                    mirror: false,
                },
                place(mid, below_offset),
                angle,
            );
            self.text(SchematicLayer::PinNumber, text, &placement);
        }
    }

    /// KiCad's do-not-populate cross: the body's box grown towards the
    /// pins' (by at least six tenths of the way, less the stroke), with
    /// its two diagonals in three line widths.
    fn dnp_cross(&mut self, body: Option<Bounds>, pins: Option<Bounds>) {
        let Some(body) = body.or(pins) else {
            return;
        };
        let pins = pins.map_or(body, |p| p.union(body));
        let margin = |body_min: f64, body_max: f64, pins_min: f64, pins_max: f64| {
            let m = (body_min - pins_min).max(pins_max - body_max).max(0.0);
            (m * 0.6).max(m - DNP_STROKE)
        };
        let mx = margin(body.min.x, body.max.x, pins.min.x, pins.max.x);
        let my = margin(body.min.y, body.max.y, pins.min.y, pins.max.y);
        let min = Point {
            x: body.min.x - mx,
            y: body.min.y - my,
        };
        let max = Point {
            x: body.max.x + mx,
            y: body.max.y + my,
        };
        let builder = self.on(SchematicLayer::Dnp);
        builder.stroke(&[min, max], DNP_STROKE, false);
        builder.stroke(
            &[Point { x: max.x, y: min.y }, Point { x: min.x, y: max.y }],
            DNP_STROKE,
            false,
        );
    }

    /// A label of any kind: its text beside the anchor as its spin says,
    /// and the flag of a global or hierarchical one.
    fn label(&mut self, label: &Label) {
        let (layer, placement) = label_placement(label);
        let text = &label.text;
        self.text(layer, &text.text, &placement);
        if label.kind != LabelKind::Local {
            let outline = label_flag(label);
            let pen = font::pen_width(&text.font);
            self.on(layer).stroke(&outline, pen, true);
        }
    }

    /// A sheet symbol: its border, its fields, and its pins as inverted
    /// hierarchical flags with the text inside the sheet.
    fn sheet_symbol(&mut self, index: usize, sheet: &SheetSymbol) {
        self.item(None, |layers| layers.sheet_border(sheet));
        for (pin_index, pin) in sheet.pins.iter().enumerate() {
            let key = SheetItem::SheetPin {
                sheet: index,
                pin: pin_index,
            };
            self.item(Some(key), |layers| layers.sheet_pin(pin));
        }
    }

    /// A sheet symbol's border and fields.
    fn sheet_border(&mut self, sheet: &SheetSymbol) {
        let corners = [
            sheet.at,
            Point {
                x: sheet.at.x + sheet.size.width,
                y: sheet.at.y,
            },
            Point {
                x: sheet.at.x + sheet.size.width,
                y: sheet.at.y + sheet.size.height,
            },
            Point {
                x: sheet.at.x,
                y: sheet.at.y + sheet.size.height,
            },
        ];
        let width = default(sheet.width, DEFAULT_LINE_WIDTH);
        self.on(SchematicLayer::SheetSymbol)
            .stroke(&corners, width, true);
        for field in &sheet.fields {
            if field.text.hidden {
                continue;
            }
            let layer = match field.name.as_str() {
                "Sheetname" => SchematicLayer::SheetName,
                "Sheetfile" => SchematicLayer::SheetFile,
                _ => SchematicLayer::Fields,
            };
            let text = &field.text;
            let placement = placement(&text.font, text.justify, text.at, text.rotation);
            self.text(layer, &field.shown(), &placement);
        }
    }

    fn sheet_pin(&mut self, pin: &SheetPin) {
        let text = &pin.text;
        let spin = sheet_pin_spin(pin);
        let h = text.font.size.height;
        let pen = font::pen_width(&text.font);
        let distance = TEXT_OFFSET_RATIO * h + text.font.size.width;
        let dir = spin.direction();
        let anchor = Point {
            x: text.at.x + dir.x * distance,
            y: text.at.y + dir.y * distance,
        };
        let justify = Justify {
            horizontal: spin.justify(),
            vertical: Vertical::Center,
            mirror: false,
        };
        let placement = placement(&text.font, justify, anchor, spin.angle());
        self.text(SchematicLayer::SheetLabel, &text.text, &placement);
        let outline = sheet_pin_flag(pin);
        self.on(SchematicLayer::SheetLabel)
            .stroke(&outline, pen, true);
    }
}

/// Where a pin starts, the unit direction it extends in, and how far.
fn pin_axis(pin: &Pin, frame: &Frame) -> (Point, Point, f64) {
    (
        pin_ends(pin, frame).0,
        pin_direction(pin, frame),
        pin.length,
    )
}

/// Where a pin starts (its connection point) and ends (at the body) on
/// the sheet, through its symbol's `frame`.
#[must_use]
pub fn pin_ends(pin: &Pin, frame: &Frame) -> (Point, Point) {
    let pos = frame.apply(pin.at);
    let d = pin_direction(pin, frame);
    let end = Point {
        x: pos.x + d.x * pin.length,
        y: pos.y + d.y * pin.length,
    };
    (pos, end)
}

/// The unit direction a pin extends in on the sheet, towards the body.
fn pin_direction(pin: &Pin, frame: &Frame) -> Point {
    let (sin, cos) = pin.rotation.to_radians().sin_cos();
    frame.rotate(Point { x: cos, y: sin })
}

/// Where a label's text is drawn and on which layer: beside its anchor
/// as its spin says, a local label standing off its wire across the
/// reading direction, a flagged one moved along it past the flag.
fn label_placement(label: &Label) -> (SchematicLayer, Placement) {
    let spin = label.spin();
    let text = &label.text;
    let h = text.font.size.height;
    let pen = font::pen_width(&text.font);
    let (layer, distance) = match label.kind {
        LabelKind::Local => (SchematicLayer::LocalLabel, TEXT_OFFSET_RATIO * h + pen),
        LabelKind::Global(shape) => {
            let margin = LABEL_SIZE_RATIO * h;
            let room = match shape {
                FlagShape::Input | FlagShape::Bidirectional | FlagShape::TriState => h * 3.0 / 4.0,
                FlagShape::Output | FlagShape::Passive => 0.0,
            };
            (SchematicLayer::GlobalLabel, margin + room)
        }
        LabelKind::Hierarchical(_) => (
            SchematicLayer::HierLabel,
            TEXT_OFFSET_RATIO * h + text.font.size.width,
        ),
    };
    let dir = spin.direction();
    let anchor = match label.kind {
        LabelKind::Local => match spin {
            Spin::Right | Spin::Left => Point {
                x: text.at.x,
                y: text.at.y - distance,
            },
            Spin::Up | Spin::Bottom => Point {
                x: text.at.x - distance,
                y: text.at.y,
            },
        },
        _ => Point {
            x: text.at.x + dir.x * distance,
            y: text.at.y + dir.y * distance,
        },
    };
    let justify = Justify {
        horizontal: spin.justify(),
        vertical: label_vertical(label.kind),
        mirror: false,
    };
    (layer, placement(&text.font, justify, anchor, spin.angle()))
}

/// The flag around a global or hierarchical label; empty for a local
/// one, which has none.
fn label_flag(label: &Label) -> Vec<Point> {
    let spin = label.spin();
    let text = &label.text;
    match label.kind {
        LabelKind::Local => Vec::new(),
        LabelKind::Global(shape) => global_flag(&text.text, &text.font, shape, spin, text.at),
        LabelKind::Hierarchical(shape) => {
            hierarchical_flag(text.font.size.height, shape, spin, text.at)
        }
    }
}

/// The outline a label is hit by: its flag, or for a local label the
/// box around its text's strokes, half a pen wider on every side.
#[must_use]
pub fn label_outline(label: &Label) -> Vec<Point> {
    if label.kind != LabelKind::Local {
        return label_flag(label);
    }
    let (_, placement) = label_placement(label);
    let shape = font::shape(&label.text.text, &placement);
    let Some(bounds) = Bounds::around(shape.strokes.iter().flatten().copied()) else {
        return Vec::new();
    };
    let half = shape.pen / 2.0;
    vec![
        Point {
            x: bounds.min.x - half,
            y: bounds.min.y - half,
        },
        Point {
            x: bounds.max.x + half,
            y: bounds.min.y - half,
        },
        Point {
            x: bounds.max.x + half,
            y: bounds.max.y + half,
        },
        Point {
            x: bounds.min.x - half,
            y: bounds.max.y + half,
        },
    ]
}

/// Which way a sheet pin's text reads: the pin's edge, from its angle,
/// puts the text inside the sheet.
fn sheet_pin_spin(pin: &SheetPin) -> Spin {
    match (pin.text.rotation.rem_euclid(360.0) / 90.0).round() {
        r if (r - 1.0).abs() < 0.5 => Spin::Bottom,
        r if (r - 2.0).abs() < 0.5 => Spin::Right,
        r if (r - 3.0).abs() < 0.5 => Spin::Up,
        _ => Spin::Left,
    }
}

/// The flag of a sheet pin, drawn the other way round from a
/// hierarchical label's: an input into the sheet points in.
#[must_use]
pub fn sheet_pin_flag(pin: &SheetPin) -> Vec<Point> {
    let shape = match pin.shape {
        FlagShape::Input => FlagShape::Output,
        FlagShape::Output => FlagShape::Input,
        other => other,
    };
    hierarchical_flag(
        pin.text.font.size.height,
        shape,
        sheet_pin_spin(pin),
        pin.text.at,
    )
}

/// A font of `size` with KiCad's defaults otherwise: pin names and
/// numbers carry only their size.
fn text_font(size: Size) -> Font {
    Font {
        face: None,
        size,
        thickness: None,
        bold: false,
        italic: false,
    }
}

/// Which way a pin points on the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Orientation {
    Right,
    Left,
    Up,
    Down,
}

impl Orientation {
    /// The orientation nearest the unit direction `d` (sheet frame, y
    /// down).
    fn of(d: Point) -> Self {
        if d.x.abs() >= d.y.abs() {
            if d.x >= 0.0 {
                Orientation::Right
            } else {
                Orientation::Left
            }
        } else if d.y >= 0.0 {
            Orientation::Down
        } else {
            Orientation::Up
        }
    }

    fn is_vertical(self) -> bool {
        matches!(self, Orientation::Up | Orientation::Down)
    }

    /// A point beside a pin at `pos` pointing this way: `along` the pin
    /// towards the body, and `across` it, where "above" on the sheet
    /// (up, or left for a vertical pin) is negative.
    fn place(self, pos: Point, along: f64, across: f64) -> Point {
        match self {
            Orientation::Right => Point {
                x: pos.x + along,
                y: pos.y + across,
            },
            Orientation::Left => Point {
                x: pos.x - along,
                y: pos.y + across,
            },
            Orientation::Down => Point {
                x: pos.x + across,
                y: pos.y + along,
            },
            Orientation::Up => Point {
                x: pos.x + across,
                y: pos.y - along,
            },
        }
    }

    /// How a pin's inside name anchors at the body's edge so it reads
    /// away from the pin: left for a pin pointing right or up, right for
    /// one pointing left or down (vertical text reads upward, so "left"
    /// is its bottom).
    fn inside_justify(self) -> Horizontal {
        match self {
            Orientation::Right | Orientation::Up => Horizontal::Left,
            Orientation::Left | Orientation::Down => Horizontal::Right,
        }
    }
}

/// A pin's name or number with its font, or nothing to draw.
type PinText<'a> = Option<(&'a str, &'a Font)>;

/// Where a symbol's text (a body text, a field) is drawn and at what
/// angle: the centre of its text box, laid out in the library frame with
/// its own justification and angle, carried through `frame`; the angle
/// swapped between horizontal and vertical when the frame turns by a
/// quarter. KiCad draws it centred there, unmirrored, so it stays
/// readable however the symbol is placed.
fn placed_text_centre(frame: &Frame, text: &TextItem) -> (Point, f64) {
    let block = block_size(&text.text, text.font.size);
    let dx = match text.justify.horizontal {
        Horizontal::Left => block.width / 2.0,
        Horizontal::Center => 0.0,
        Horizontal::Right => -block.width / 2.0,
    };
    let dy = match text.justify.vertical {
        Vertical::Top => block.height / 2.0,
        Vertical::Center => 0.0,
        Vertical::Bottom => -block.height / 2.0,
    };
    let turned = Transform::new(Point::default(), text.rotation).rotate(Point { x: dx, y: dy });
    // The offset was laid out with y down, as text is; the anchor's
    // numbers are the library's, where y is up. KiCad mirrors the box
    // about the anchor here (SCH_FIELD::GetBoundingBox) before the
    // frame's own flip, so a right-justified vertical field on a turned
    // symbol ends at its anchor instead of starting there.
    let centre = frame.apply(Point {
        x: text.at.x + turned.x,
        y: text.at.y - turned.y,
    });
    let angle = text.rotation.rem_euclid(360.0);
    let horizontal = angle.rem_euclid(180.0) < 1.0 || angle.rem_euclid(180.0) > 179.0;
    let vertical = (angle.rem_euclid(180.0) - 90.0).abs() < 1.0;
    let angle = if frame.swaps_axes() {
        if horizontal {
            90.0
        } else if vertical {
            0.0
        } else {
            angle
        }
    } else if vertical {
        90.0
    } else if horizontal {
        0.0
    } else {
        angle
    };
    (centre, angle)
}

/// The outline of a global label's flag around `text`, as KiCad's
/// `SCH_GLOBALLABEL::CreateGraphicShape` builds it: a box of the text's
/// width plus margins, half a text height plus the margin above and
/// below, pointed at the anchor for an input, away from it for an output,
/// both ways for a bidirectional or tri-state, and plain for a passive.
fn global_flag(text: &str, font: &Font, shape: FlagShape, spin: Spin, anchor: Point) -> Vec<Point> {
    let h = font.size.height;
    let margin = LABEL_SIZE_RATIO * h;
    let half = h / 2.0 + margin;
    let pen = font::pen_width(font);
    let length = block_size(text, font.size).width + 2.0 * margin;
    let x = length + pen;
    let y = half + pen;
    // The base shape sits to the left of the anchor (KiCad's "left" spin).
    let mut points = [
        Point { x: 0.0, y: 0.0 },
        Point { x: 0.0, y: -y },
        Point { x: -x, y: -y },
        Point { x: -x, y: 0.0 },
        Point { x: -x, y },
        Point { x: 0.0, y },
    ];
    let mut shift = 0.0;
    match shape {
        FlagShape::Input => {
            shift = -half;
            points[0].x += half;
        }
        FlagShape::Output => points[3].x -= half,
        FlagShape::Bidirectional | FlagShape::TriState => {
            shift = -half;
            points[0].x += half;
            points[3].x -= half;
        }
        FlagShape::Passive => {}
    }
    points
        .iter()
        .map(|p| {
            let p = Point {
                x: p.x + shift,
                y: p.y,
            };
            let turned = match spin {
                Spin::Left => p,
                Spin::Up => Point { x: -p.y, y: p.x },
                Spin::Right => Point { x: -p.x, y: -p.y },
                Spin::Bottom => Point { x: p.y, y: -p.x },
            };
            Point {
                x: turned.x + anchor.x,
                y: turned.y + anchor.y,
            }
        })
        .collect()
}

/// The outline of a hierarchical label's or sheet pin's flag: KiCad's
/// template shapes in units of half the text height, by shape and spin.
fn hierarchical_flag(height: f64, shape: FlagShape, spin: Spin, anchor: Point) -> Vec<Point> {
    const IN: [[(f64, f64); 5]; 4] = [
        [
            (0.0, 0.0),
            (-1.0, -1.0),
            (-2.0, -1.0),
            (-2.0, 1.0),
            (-1.0, 1.0),
        ],
        [
            (0.0, 0.0),
            (1.0, -1.0),
            (1.0, -2.0),
            (-1.0, -2.0),
            (-1.0, -1.0),
        ],
        [(0.0, 0.0), (1.0, 1.0), (2.0, 1.0), (2.0, -1.0), (1.0, -1.0)],
        [(0.0, 0.0), (1.0, 1.0), (1.0, 2.0), (-1.0, 2.0), (-1.0, 1.0)],
    ];
    const OUT: [[(f64, f64); 5]; 4] = [
        [
            (-2.0, 0.0),
            (-1.0, 1.0),
            (0.0, 1.0),
            (0.0, -1.0),
            (-1.0, -1.0),
        ],
        [
            (0.0, -2.0),
            (1.0, -1.0),
            (1.0, 0.0),
            (-1.0, 0.0),
            (-1.0, -1.0),
        ],
        [(2.0, 0.0), (1.0, -1.0), (0.0, -1.0), (0.0, 1.0), (1.0, 1.0)],
        [(0.0, 2.0), (1.0, 1.0), (1.0, 0.0), (-1.0, 0.0), (-1.0, 1.0)],
    ];
    const BIDI: [[(f64, f64); 4]; 4] = [
        [(0.0, 0.0), (-1.0, -1.0), (-2.0, 0.0), (-1.0, 1.0)],
        [(0.0, 0.0), (-1.0, -1.0), (0.0, -2.0), (1.0, -1.0)],
        [(0.0, 0.0), (1.0, -1.0), (2.0, 0.0), (1.0, 1.0)],
        [(0.0, 0.0), (-1.0, 1.0), (0.0, 2.0), (1.0, 1.0)],
    ];
    const PLAIN: [[(f64, f64); 4]; 4] = [
        [(0.0, -1.0), (-2.0, -1.0), (-2.0, 1.0), (0.0, 1.0)],
        [(1.0, 0.0), (1.0, -2.0), (-1.0, -2.0), (-1.0, 0.0)],
        [(0.0, -1.0), (2.0, -1.0), (2.0, 1.0), (0.0, 1.0)],
        [(1.0, 0.0), (1.0, 2.0), (-1.0, 2.0), (-1.0, 0.0)],
    ];
    let index = match spin {
        Spin::Left => 0,
        Spin::Up => 1,
        Spin::Right => 2,
        Spin::Bottom => 3,
    };
    let half = height / 2.0;
    let template: &[(f64, f64)] = match shape {
        FlagShape::Input => IN.get(index).map_or(&[], |t| t.as_slice()),
        FlagShape::Output => OUT.get(index).map_or(&[], |t| t.as_slice()),
        FlagShape::Bidirectional | FlagShape::TriState => {
            BIDI.get(index).map_or(&[], |t| t.as_slice())
        }
        FlagShape::Passive => PLAIN.get(index).map_or(&[], |t| t.as_slice()),
    };
    template
        .iter()
        .map(|&(x, y)| Point {
            x: anchor.x + half * x,
            y: anchor.y + half * y,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientations_follow_the_nearest_axis() {
        assert_eq!(
            Orientation::of(Point { x: 1.0, y: 0.0 }),
            Orientation::Right
        );
        assert_eq!(
            Orientation::of(Point { x: -1.0, y: 0.0 }),
            Orientation::Left
        );
        assert_eq!(Orientation::of(Point { x: 0.0, y: -1.0 }), Orientation::Up);
        assert_eq!(Orientation::of(Point { x: 0.0, y: 1.0 }), Orientation::Down);
        assert!(Orientation::Up.is_vertical() && !Orientation::Left.is_vertical());
    }

    #[test]
    fn an_input_flag_to_the_right_has_its_tip_at_the_anchor() {
        let font = text_font(Size {
            width: 1.27,
            height: 1.27,
        });
        let flag = global_flag("I", &font, FlagShape::Input, Spin::Right, Point::default());
        assert_eq!(flag.len(), 6);
        let tip = flag.first().expect("six points");
        assert!(tip.x.abs() < 1e-9 && tip.y.abs() < 1e-9, "{tip:?}");
        assert!(flag.iter().all(|p| p.x >= -1e-9), "{flag:?}");
        let far = flag.iter().map(|p| p.x).fold(0.0, f64::max);
        // halfSize 1.111 for the tip, the glyph's advance, two margins,
        // and the pen.
        let expected = 1.111_25 + 10.0 / 21.0 * 1.27 + 2.0 * 0.476_25 + 0.158_75;
        assert!((far - expected).abs() < 1e-6, "{far} vs {expected}");
    }

    #[test]
    fn a_hierarchical_input_flag_is_two_half_heights_long() {
        let flag = hierarchical_flag(
            1.27,
            FlagShape::Input,
            Spin::Right,
            Point { x: 5.0, y: 5.0 },
        );
        let far = flag.iter().map(|p| p.x).fold(0.0, f64::max);
        assert!((far - 6.27).abs() < 1e-9, "{far}");
        assert!(
            flag.iter()
                .any(|p| (p.x - 5.0).abs() < 1e-9 && (p.y - 5.0).abs() < 1e-9)
        );
    }
}
