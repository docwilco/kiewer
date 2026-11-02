//! Net membership on one sheet (docs/v1-scope.md, Net interaction):
//! which wires, junctions, labels, pins and sheet pins are one net, and
//! what it is called: the connections the drawing shows, without KiCad's
//! electrical rules check. Two items whose connection points coincide
//! are joined (a wire's vertices, a junction, a label's anchor, a pin's
//! end, a sheet pin's anchor), and a connection point on the body of a
//! wire joins that wire, so a wire ending on another wire's middle
//! connects (KiCad draws a junction there when it draws the T) and two
//! wires crossing do not. Hidden pins are left out except a power
//! symbol's, which is what names its net, and an ordinary symbol's
//! hidden `power_in` pin, which joins the net of its name without a wire
//! ([`connects`]). Buses and bus entries carry several nets and are not
//! followed.
//!
//! A net's name follows KiCad's driver priority: a global label, then a
//! power symbol's value through a `power_in` pin (GND, +3V3), then one
//! through a `power_out` pin (a power flag), then a local label, then a
//! hierarchical label, then a sheet pin, then the first pin as
//! `Net-(R4-Pad1)`, or `unconnected-(R4-Pad2)` for a pin on its own;
//! ties go to the name that sorts first. A name joins what no wire does:
//! every global label and `power_in` power symbol of one name is one
//! net, every local label of one name on the sheet is one net, and so is
//! every hierarchical label of one name; a power flag and a sheet pin
//! join nothing. Nothing joins across sheets here: a global or
//! hierarchical name is the same on every sheet it appears on, which is
//! as far as the names say.

use std::collections::BTreeMap;

use super::model::{LabelKind, LibrarySymbol, Pin, PinElectrical, Sheet, Symbol, Wire};
use crate::hit::{natural_key, segment_distance};
use crate::model::Point;

/// Two connection points closer than this, in millimetres, are one.
const EPSILON: f64 = 0.01;

/// An item on a sheet that can belong to a net, by its position in the
/// model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SheetItem {
    /// An index into [`Sheet::wires`].
    Wire(usize),
    /// An index into [`Sheet::junctions`].
    Junction(usize),
    /// An index into [`Sheet::labels`].
    Label(usize),
    /// Pin `pin` of unit `unit` (an index into the library symbol's
    /// units) of symbol `symbol` (an index into [`Sheet::symbols`]).
    Pin {
        symbol: usize,
        unit: usize,
        pin: usize,
    },
    /// Pin `pin` of sheet symbol `sheet`, each by index.
    SheetPin { sheet: usize, pin: usize },
}

/// Index of a net in [`Connectivity::nets`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SheetNetId(u32);

impl SheetNetId {
    /// The position in [`Connectivity::nets`].
    ///
    /// Infallible on every target where a `u32` fits a `usize`, which
    /// includes wasm32.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// One net of a sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetNet {
    /// The name, by KiCad's driver priority.
    pub name: String,
    /// Every item on it, in [`SheetItem`] order.
    pub items: Vec<SheetItem>,
}

/// The nets of one sheet and the net of each item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connectivity {
    nets: Vec<SheetNet>,
    of: BTreeMap<SheetItem, SheetNetId>,
}

/// What names a net, in KiCad's driver priority: a stronger kind comes
/// first, and among two of one kind the name that sorts first wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DriverKind {
    /// A global label.
    GlobalLabel,
    /// A power symbol's `power_in` pin: GND, +3V3.
    GlobalPower,
    /// A power symbol's `power_out` pin: a power flag.
    LocalPower,
    /// A local label.
    LocalLabel,
    /// A hierarchical label.
    HierarchicalLabel,
    /// A sheet pin.
    SheetPin,
}

/// The namespace a driver's name joins within: a name shared by two
/// drivers of one scope on the sheet makes them one net.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    /// Global labels and global power pins, which KiCad joins across the
    /// whole schematic.
    Global,
    /// Local labels, joined within the sheet.
    Local,
    /// Hierarchical labels, joined within the sheet (they all reach the
    /// same sheet pin of the parent).
    Hierarchical,
}

impl DriverKind {
    /// The scope the kind joins by name within; `None` for a power flag
    /// and a sheet pin, whose names join nothing.
    fn scope(self) -> Option<Scope> {
        match self {
            DriverKind::GlobalLabel | DriverKind::GlobalPower => Some(Scope::Global),
            DriverKind::LocalLabel => Some(Scope::Local),
            DriverKind::HierarchicalLabel => Some(Scope::Hierarchical),
            DriverKind::LocalPower | DriverKind::SheetPin => None,
        }
    }
}

/// The name a driver gives its net and how strongly, lower first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Driver {
    kind: DriverKind,
    key: Vec<(u64, String)>,
    name: String,
}

impl Driver {
    fn new(kind: DriverKind, name: String) -> Self {
        Self {
            kind,
            key: natural_key(&name),
            name,
        }
    }
}

/// A pin's connection point on the sheet.
#[must_use]
pub fn pin_position(symbol: &Symbol, pin: &Pin) -> Point {
    symbol.frame().apply(pin.at)
}

/// Whether `pin` of `library` takes part in a net: every drawn pin, a
/// power symbol's hidden pin (what names its net), and an ordinary
/// symbol's hidden `power_in` pin, which KiCad joins to the net of its
/// name without a wire (an invisible power pin).
#[must_use]
pub fn connects(pin: &Pin, library: &LibrarySymbol) -> bool {
    !pin.hidden || library.power || pin.electrical == PinElectrical::PowerIn
}

/// Every pin the net rules see: the drawn pins, and the hidden pins of
/// power symbols, each with its item and its connection point.
fn connecting_pins(sheet: &Sheet) -> Vec<(SheetItem, Point)> {
    let mut pins = Vec::new();
    for (symbol_index, symbol) in sheet.symbols.iter().enumerate() {
        let Some(library) = sheet.library_symbol(symbol) else {
            continue;
        };
        for (unit_index, unit) in library.units.iter().enumerate() {
            if !unit.applies_to(symbol.unit, symbol.body_style) {
                continue;
            }
            for (pin_index, pin) in unit.pins.iter().enumerate() {
                if !connects(pin, library) {
                    continue;
                }
                pins.push((
                    SheetItem::Pin {
                        symbol: symbol_index,
                        unit: unit_index,
                        pin: pin_index,
                    },
                    pin_position(symbol, pin),
                ));
            }
        }
    }
    pins
}

/// Union-find over the items' positions in `items`.
struct Joins {
    parent: Vec<usize>,
}

impl Joins {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn root(&mut self, mut index: usize) -> usize {
        while let Some(&parent) = self.parent.get(index)
            && parent != index
        {
            // Halve the path on the way up.
            let grandparent = self.parent.get(parent).copied().unwrap_or(parent);
            if let Some(slot) = self.parent.get_mut(index) {
                *slot = grandparent;
            }
            index = parent;
        }
        index
    }

    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.root(a), self.root(b));
        if a != b
            && let Some(slot) = self.parent.get_mut(a)
        {
            *slot = b;
        }
    }
}

/// The grid cell a connection point falls in: [`EPSILON`]-sized cells,
/// so two points within a cell coincide and neighbours are checked
/// across the cell's edges.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a sheet coordinate in millimetres is far below i64's range"
)]
fn cell_of(point: Point) -> (i64, i64) {
    (
        (point.x / EPSILON).floor() as i64,
        (point.y / EPSILON).floor() as i64,
    )
}

fn close(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() <= EPSILON && (a.y - b.y).abs() <= EPSILON
}

impl Connectivity {
    /// The nets of `sheet`.
    #[must_use]
    pub fn build(sheet: &Sheet) -> Self {
        // Every item once, with its connection points.
        let mut items: Vec<SheetItem> = Vec::new();
        let mut points: Vec<(usize, Point)> = Vec::new();
        let mut push = |item: SheetItem, at: &[Point], items: &mut Vec<SheetItem>| {
            let index = items.len();
            items.push(item);
            points.extend(at.iter().map(|&p| (index, p)));
            index
        };
        // Each wire's position in `items`, for the joins along its body.
        let mut wire_items: Vec<(usize, &Wire)> = Vec::new();
        for (index, wire) in sheet.wires.iter().enumerate() {
            let item = push(SheetItem::Wire(index), &wire.points, &mut items);
            wire_items.push((item, wire));
        }
        for (index, junction) in sheet.junctions.iter().enumerate() {
            push(SheetItem::Junction(index), &[junction.at], &mut items);
        }
        for (index, label) in sheet.labels.iter().enumerate() {
            push(SheetItem::Label(index), &[label.text.at], &mut items);
        }
        for (item, at) in connecting_pins(sheet) {
            push(item, &[at], &mut items);
        }
        for (sheet_index, symbol) in sheet.sheet_symbols.iter().enumerate() {
            for (pin_index, pin) in symbol.pins.iter().enumerate() {
                push(
                    SheetItem::SheetPin {
                        sheet: sheet_index,
                        pin: pin_index,
                    },
                    &[pin.text.at],
                    &mut items,
                );
            }
        }

        let mut joins = Joins::new(items.len());
        // Coinciding connection points join, found through a grid of
        // epsilon cells and their neighbours.
        let mut cells: BTreeMap<(i64, i64), Vec<usize>> = BTreeMap::new();
        for (slot, &(_, point)) in points.iter().enumerate() {
            cells.entry(cell_of(point)).or_default().push(slot);
        }
        for &(index, point) in &points {
            let (cx, cy) = cell_of(point);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &slot in cells.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                        if let Some(&(other, at)) = points.get(slot)
                            && close(point, at)
                        {
                            joins.join(index, other);
                        }
                    }
                }
            }
        }
        // A connection point on a wire's body joins the wire.
        for &(index, point) in &points {
            for &(wire_item, wire) in &wire_items {
                let on_wire = wire.points.windows(2).any(
                    |pair| matches!(pair, [a, b] if segment_distance(point, *a, *b) <= EPSILON),
                );
                if on_wire {
                    joins.join(index, wire_item);
                }
            }
        }
        // A name joins what no wire does: every global label and power
        // symbol of one name is one net, and so is every local label of
        // one name on this sheet, and every hierarchical label.
        let mut by_name: BTreeMap<(Scope, String), usize> = BTreeMap::new();
        for (index, &item) in items.iter().enumerate() {
            let Some(driver) = driver_of(sheet, item) else {
                continue;
            };
            let Some(scope) = driver.kind.scope() else {
                continue;
            };
            match by_name.entry((scope, driver.name)) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(index);
                }
                std::collections::btree_map::Entry::Occupied(first) => {
                    joins.join(index, *first.get());
                }
            }
        }

        // Group by root, in item order, so a net's items and the nets
        // themselves come out in a stable order.
        let mut by_root: BTreeMap<usize, Vec<SheetItem>> = BTreeMap::new();
        for (index, &item) in items.iter().enumerate() {
            by_root.entry(joins.root(index)).or_default().push(item);
        }
        let mut groups: Vec<Vec<SheetItem>> = by_root.into_values().collect();
        for group in &mut groups {
            group.sort_unstable();
        }
        groups.sort_unstable_by(|a, b| a.first().cmp(&b.first()));

        let mut out = Self::default();
        for items in groups {
            let id = SheetNetId(u32::try_from(out.nets.len()).unwrap_or(u32::MAX));
            for &item in &items {
                out.of.insert(item, id);
            }
            let name = net_name(sheet, &items);
            out.nets.push(SheetNet { name, items });
        }
        out
    }

    /// The net `item` is on; `None` for an item the sheet lacks, a hidden
    /// pin, or a unit the symbol does not show.
    #[must_use]
    pub fn net_of(&self, item: SheetItem) -> Option<SheetNetId> {
        self.of.get(&item).copied()
    }

    /// The net `id` names.
    #[must_use]
    pub fn net(&self, id: SheetNetId) -> Option<&SheetNet> {
        self.nets.get(id.index())
    }

    /// Every net, in order of their first item.
    #[must_use]
    pub fn nets(&self) -> &[SheetNet] {
        &self.nets
    }
}

/// The reference and number of a pin item, as `Net-(R4-Pad1)` writes
/// them; `None` for an item that is no pin, or one the sheet lacks.
fn pin_names(sheet: &Sheet, item: SheetItem) -> Option<(String, String)> {
    let SheetItem::Pin { symbol, unit, pin } = item else {
        return None;
    };
    let symbol = sheet.symbols.get(symbol)?;
    let library = sheet.library_symbol(symbol)?;
    let pin = library.units.get(unit)?.pins.get(pin)?;
    Some((
        symbol.reference().unwrap_or("?").to_owned(),
        pin.number.clone(),
    ))
}

/// What `item` names its net, when it names it: a label of any kind, a
/// power symbol's pin (its value, else the pin's name), a sheet pin.
fn driver_of(sheet: &Sheet, item: SheetItem) -> Option<Driver> {
    match item {
        SheetItem::Label(index) => {
            let label = sheet.labels.get(index)?;
            let kind = match label.kind {
                LabelKind::Global(_) => DriverKind::GlobalLabel,
                LabelKind::Local => DriverKind::LocalLabel,
                LabelKind::Hierarchical(_) => DriverKind::HierarchicalLabel,
            };
            Some(Driver::new(kind, label.text.text.clone()))
        }
        SheetItem::Pin { symbol, unit, pin } => {
            let placed = sheet.symbols.get(symbol)?;
            let library = sheet.library_symbol(placed)?;
            let pin = library.units.get(unit)?.pins.get(pin)?;
            if !library.power {
                // An invisible power pin names its net by its own name,
                // as a global power pin does.
                return (pin.hidden && pin.electrical == PinElectrical::PowerIn)
                    .then(|| Driver::new(DriverKind::GlobalPower, pin.name.clone()));
            }
            let value = placed
                .value()
                .filter(|value| !value.is_empty())
                .map_or_else(|| pin.name.clone(), str::to_owned);
            // A power symbol's power_in pin (GND, +3V3) names its net
            // everywhere; a power_out one (a power flag) only where it
            // sits, and only when nothing else does.
            let kind = if pin.electrical == PinElectrical::PowerIn {
                DriverKind::GlobalPower
            } else {
                DriverKind::LocalPower
            };
            Some(Driver::new(kind, value))
        }
        SheetItem::SheetPin { sheet: index, pin } => {
            let pin = sheet.sheet_symbols.get(index)?.pins.get(pin)?;
            Some(Driver::new(DriverKind::SheetPin, pin.text.text.clone()))
        }
        SheetItem::Wire(_) | SheetItem::Junction(_) => None,
    }
}

/// The name of the net made of `items`, by KiCad's driver priority.
fn net_name(sheet: &Sheet, items: &[SheetItem]) -> String {
    let mut drivers: Vec<Driver> = Vec::new();
    let mut pins: Vec<(String, String)> = Vec::new();
    for &item in items {
        if let Some(driver) = driver_of(sheet, item) {
            drivers.push(driver);
        } else if let Some((reference, number)) = pin_names(sheet, item) {
            pins.push((reference, number));
        }
    }
    if let Some(driver) = drivers.into_iter().min() {
        return driver.name;
    }
    pins.sort_by_cached_key(|(reference, number)| (natural_key(reference), natural_key(number)));
    match pins.first() {
        Some((reference, number)) if items.len() == 1 => {
            format!("unconnected-({reference}-Pad{number})")
        }
        Some((reference, number)) => format!("Net-({reference}-Pad{number})"),
        None => String::new(),
    }
}

/// One pin of a net, as the pin list names it.
#[derive(Debug, Clone, PartialEq)]
pub struct NetPin {
    /// The pin's item.
    pub item: SheetItem,
    /// The symbol's reference designator; `?` when it has none.
    pub reference: String,
    /// The pin's number.
    pub number: String,
    /// The connection point on the sheet.
    pub at: Point,
}

/// Every pin on net `id`, in order of reference (its letters, then its
/// number as a number, so `R2` comes before `R10`) and then pin number
/// the same way; empty for a net the sheet lacks.
#[must_use]
pub fn net_pins(sheet: &Sheet, connectivity: &Connectivity, id: SheetNetId) -> Vec<NetPin> {
    let Some(net) = connectivity.net(id) else {
        return Vec::new();
    };
    let mut pins: Vec<NetPin> = net
        .items
        .iter()
        .filter_map(|&item| {
            let SheetItem::Pin { symbol, unit, pin } = item else {
                return None;
            };
            let placed = sheet.symbols.get(symbol)?;
            let library = sheet.library_symbol(placed)?;
            let pin = library.units.get(unit)?.pins.get(pin)?;
            Some(NetPin {
                item,
                reference: placed.reference().unwrap_or("?").to_owned(),
                number: pin.number.clone(),
                at: pin_position(placed, pin),
            })
        })
        .collect();
    pins.sort_by(|a, b| {
        natural_key(&a.reference)
            .cmp(&natural_key(&b.reference))
            .then_with(|| natural_key(&a.number).cmp(&natural_key(&b.number)))
    });
    pins
}
