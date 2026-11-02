//! Hit testing on a sheet (docs/v1-scope.md, Net interaction): what is
//! under a world point, the sheet symbol a click enters, and where a
//! symbol found by its reference sits. The sheet variant of
//! [`crate::hit`]: a [`SheetIndex`] holds every wire, junction, label,
//! pin and sheet pin that is on a net ([`Connectivity`]), each with its
//! box and the shape the tessellator draws it by (a wire's stroke, a
//! junction's disc, a label's flag or text box, a pin's line, a sheet
//! pin's flag), measured by distance so a query can carry a tolerance of
//! a few screen pixels. A sheet has hundreds of items where a board has
//! tens of thousands, so the index is a list scanned by box, not a grid.
//! Among the items under the point, a pin wins over a sheet pin, a sheet
//! pin over a junction, a junction over a label, a label over a wire
//! ([`SheetKind`]).

use super::connect::{Connectivity, SheetItem, SheetNetId};
use super::model::{Sheet, Symbol, SymbolShape};
use crate::geometry::Bounds;
use crate::hit::{Shape, finite, grow, overlaps};
use crate::model::Point;
use crate::tessellate::schematic::{
    DEFAULT_JUNCTION_DIAMETER, DEFAULT_LINE_WIDTH, DEFAULT_WIRE_WIDTH, default, label_outline,
    pin_ends, sheet_pin_flag,
};

/// What an item is, in the order a query prefers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SheetKind {
    /// A symbol's pin.
    Pin,
    /// A sheet symbol's pin.
    SheetPin,
    /// A junction dot.
    Junction,
    /// A label of any kind.
    Label,
    /// A wire.
    Wire,
}

impl SheetItem {
    /// What the item is, for the order a query prefers.
    #[must_use]
    pub fn kind(self) -> SheetKind {
        match self {
            SheetItem::Pin { .. } => SheetKind::Pin,
            SheetItem::SheetPin { .. } => SheetKind::SheetPin,
            SheetItem::Junction(_) => SheetKind::Junction,
            SheetItem::Label(_) => SheetKind::Label,
            SheetItem::Wire(_) => SheetKind::Wire,
        }
    }
}

/// What a query found under its point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SheetHit {
    /// The item.
    pub item: SheetItem,
    /// Its net.
    pub net: SheetNetId,
}

/// One indexed item.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    item: SheetItem,
    net: SheetNetId,
    bounds: Bounds,
    shape: Shape,
}

/// The sheet's items with a net, and its sheet symbols' boxes. The
/// default is empty: nothing to hit, for a viewer without a sheet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SheetIndex {
    entries: Vec<Entry>,
    /// Each sheet symbol's box, by index.
    sheets: Vec<Bounds>,
}

impl SheetIndex {
    /// Every item of `sheet` that `connectivity` puts on a net, and the
    /// sheet symbols' boxes.
    #[must_use]
    pub fn build(sheet: &Sheet, connectivity: &Connectivity) -> Self {
        let mut index = Self::default();
        for net in connectivity.nets() {
            let Some(id) = net
                .items
                .first()
                .and_then(|&item| connectivity.net_of(item))
            else {
                continue;
            };
            for &item in &net.items {
                if let Some(shape) = shape_of(sheet, item) {
                    index.push(item, id, shape);
                }
            }
        }
        index.sheets = sheet
            .sheet_symbols
            .iter()
            .map(|symbol| Bounds {
                min: symbol.at,
                max: Point {
                    x: symbol.at.x + symbol.size.width,
                    y: symbol.at.y + symbol.size.height,
                },
            })
            .collect();
        index
    }

    fn push(&mut self, item: SheetItem, net: SheetNetId, shape: Shape) {
        let Some(bounds) = shape.bounds() else { return };
        if !finite(bounds) {
            return;
        }
        self.entries.push(Entry {
            item,
            net,
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

    /// The item under `point`, or within `tolerance` millimetres of it:
    /// the nearest kind by [`SheetKind`]'s order, the first in the sheet
    /// among those. `None` over empty space.
    #[must_use]
    pub fn hit(&self, point: Point, tolerance: f64) -> Option<SheetHit> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return None;
        }
        let tolerance = if tolerance.is_finite() {
            tolerance.max(0.0)
        } else {
            0.0
        };
        let reach = grow(Bounds::of(point), tolerance);
        let mut best: Option<(SheetKind, SheetHit)> = None;
        for entry in &self.entries {
            if !overlaps(entry.bounds, reach) {
                continue;
            }
            let kind = entry.item.kind();
            if best.is_some_and(|(best_kind, _)| best_kind <= kind) {
                continue;
            }
            if entry.shape.distance(point) > tolerance {
                continue;
            }
            best = Some((
                kind,
                SheetHit {
                    item: entry.item,
                    net: entry.net,
                },
            ));
        }
        best.map(|(_, hit)| hit)
    }

    /// The sheet symbol whose box holds `point`, by index in the sheet's
    /// `sheet_symbols`; the first when two overlap.
    #[must_use]
    pub fn sheet_symbol_at(&self, point: Point) -> Option<usize> {
        self.sheets
            .iter()
            .position(|bounds| overlaps(*bounds, Bounds::of(point)))
    }
}

/// The shape an item is measured by, as the tessellator draws it;
/// `None` for an item the sheet lacks or one with no extent.
fn shape_of(sheet: &Sheet, item: SheetItem) -> Option<Shape> {
    match item {
        SheetItem::Wire(index) => {
            let wire = sheet.wires.get(index)?;
            Some(Shape::Stroke {
                points: wire.points.clone(),
                radius: default(wire.width, DEFAULT_WIRE_WIDTH) / 2.0,
            })
        }
        SheetItem::Junction(index) => {
            let junction = sheet.junctions.get(index)?;
            Some(Shape::Disc {
                center: junction.at,
                radius: default(junction.diameter, DEFAULT_JUNCTION_DIAMETER) / 2.0,
            })
        }
        SheetItem::Label(index) => {
            let outline = label_outline(sheet.labels.get(index)?);
            (outline.len() >= 3).then(|| Shape::Polygons(vec![outline]))
        }
        SheetItem::Pin { symbol, unit, pin } => {
            let placed = sheet.symbols.get(symbol)?;
            let library = sheet.library_symbol(placed)?;
            let pin = library.units.get(unit)?.pins.get(pin)?;
            // An invisible power pin is on a net but nowhere to point
            // at; a power symbol's hidden pin sits where its wire ends.
            if pin.hidden && !library.power {
                return None;
            }
            let (pos, end) = pin_ends(pin, &placed.frame());
            Some(Shape::Stroke {
                points: vec![pos, end],
                radius: DEFAULT_LINE_WIDTH / 2.0,
            })
        }
        SheetItem::SheetPin { sheet: index, pin } => {
            let pin = sheet.sheet_symbols.get(index)?.pins.get(pin)?;
            let outline = sheet_pin_flag(pin);
            (outline.len() >= 3).then(|| Shape::Polygons(vec![outline]))
        }
    }
}

/// The index of the symbol whose reference designator is `reference`,
/// letter case aside; the first when two share it.
#[must_use]
pub fn find_symbol(sheet: &Sheet, reference: &str) -> Option<usize> {
    let wanted = reference.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    sheet.symbols.iter().position(|symbol| {
        symbol
            .reference()
            .is_some_and(|r| r.to_lowercase() == wanted)
    })
}

/// The box around `symbol` on the sheet: its body's graphics and its
/// pins through its frame, texts aside. `None` for a symbol with
/// neither, or one whose library symbol the sheet lacks.
#[must_use]
pub fn symbol_bounds(sheet: &Sheet, symbol: &Symbol) -> Option<Bounds> {
    let library = sheet.library_symbol(symbol)?;
    let frame = symbol.frame();
    let mut points: Vec<Point> = Vec::new();
    for unit in library.units_for(symbol.unit, symbol.body_style) {
        for graphic in &unit.graphics {
            match &graphic.shape {
                SymbolShape::Polyline { points: own } | SymbolShape::Bezier { points: own } => {
                    points.extend(own.iter().map(|&p| frame.apply(p)));
                }
                SymbolShape::Rect { start, end } => {
                    points.extend(
                        [
                            *start,
                            *end,
                            Point {
                                x: start.x,
                                y: end.y,
                            },
                            Point {
                                x: end.x,
                                y: start.y,
                            },
                        ]
                        .map(|p| frame.apply(p)),
                    );
                }
                SymbolShape::Circle { center, radius } => {
                    let c = frame.apply(*center);
                    points.extend([
                        Point {
                            x: c.x - radius,
                            y: c.y - radius,
                        },
                        Point {
                            x: c.x + radius,
                            y: c.y + radius,
                        },
                    ]);
                }
                SymbolShape::Arc { start, mid, end } => {
                    points.extend([*start, *mid, *end].map(|p| frame.apply(p)));
                }
            }
        }
        for pin in &unit.pins {
            if pin.hidden {
                continue;
            }
            let (pos, end) = pin_ends(pin, &frame);
            points.extend([pos, end]);
        }
    }
    Bounds::around(points)
}
