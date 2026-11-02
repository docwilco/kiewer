//! The board parser: a `.kicad_pcb` text into a [`Board`].
//!
//! The walk is over the [`sexpr`] tree. Known elements are read with every
//! field they need required; unknown elements and unknown fields of known
//! elements are skipped, so a newer KiCad adding a field does not refuse
//! the board. Layer and net references are resolved against the board's
//! tables as they are met, so the model never holds an id that does not
//! resolve.

use std::collections::BTreeMap;

use crate::error::{Error, MIN_BOARD_VERSION, Result};
use crate::model::{
    Board, Clearances, Drawing, Drill, Fill, Font, Footprint, FootprintAttributes, Graphic,
    Horizontal, Justify, Layer, LayerId, LayerKind, Net, NetId, Pad, PadKind, PadShape, Point,
    Property, Segment, Shape, Size, Text, TextRole, TrackArc, Vertical, Via, ViaKind, Zone,
};
use crate::sexpr::{self, Node};

/// KiCad's default text size, used when a text item carries no `effects`.
const DEFAULT_TEXT_SIZE: f64 = 1.27;

/// Parses a KiCad 9 or 10 board file.
///
/// # Errors
/// The text is not a well-formed S-expression, is not a `kicad_pcb`, has a
/// format version older than [`MIN_BOARD_VERSION`]
/// ([`Error::UnsupportedVersion`]), or has a known element with a missing
/// or malformed field, an unknown layer name among them.
pub fn parse_board(source: &str) -> Result<Board> {
    let root = sexpr::read(source)?;
    let head = root.head()?;
    if head != "kicad_pcb" {
        return Err(root.error(format!("expected (kicad_pcb ...), found ({head} ...)")));
    }
    let version = root.require("version")?;
    let number = version.integer(0)?;
    let version = u32::try_from(number).map_err(|_| {
        version.error(format!(
            "expected the format version to be a positive integer, found {number}"
        ))
    })?;
    if version < MIN_BOARD_VERSION {
        return Err(Error::UnsupportedVersion { version });
    }

    let mut parser = Parser::default();
    parser.read_layer_table(root.require("layers")?)?;
    for net in root.children("net") {
        parser.declare_net(net)?;
    }

    let mut board = Board {
        version,
        generator: None,
        generator_version: None,
        thickness: None,
        layers: Vec::new(),
        nets: Vec::new(),
        footprints: Vec::new(),
        graphics: Vec::new(),
        texts: Vec::new(),
        segments: Vec::new(),
        arcs: Vec::new(),
        vias: Vec::new(),
        zones: Vec::new(),
        clearances: Clearances::default(),
    };
    for node in root.lists() {
        match node.head()? {
            "generator" => board.generator = Some(node.atom(0)?.to_owned()),
            "generator_version" => board.generator_version = Some(node.atom(0)?.to_owned()),
            "general" => board.thickness = optional_number(node, "thickness")?,
            // The board's clearances go by other names than a pad's.
            "setup" => {
                board.clearances = Clearances {
                    solder_mask_margin: optional_number(node, "pad_to_mask_clearance")?,
                    solder_paste_margin: optional_number(node, "pad_to_paste_clearance")?,
                    solder_paste_ratio: optional_number(node, "pad_to_paste_clearance_ratio")?,
                };
            }
            "footprint" => board.footprints.push(parser.footprint(node)?),
            "segment" => board.segments.push(parser.segment(node)?),
            "arc" => board.arcs.push(parser.track_arc(node)?),
            "via" => board.vias.push(parser.via(node)?),
            "zone" => board.zones.push(parser.zone(node)?),
            "gr_text" => board
                .texts
                .push(parser.text(node, node.atom(0)?, TextRole::User)?),
            "gr_line" | "gr_arc" | "gr_circle" | "gr_rect" | "gr_poly" | "gr_curve" => {
                board.graphics.push(parser.graphic(node)?);
            }
            _ => {}
        }
    }
    board.layers = parser.layer_table;
    board.nets = parser.nets;
    Ok(board)
}

/// The tables references resolve against while the tree is walked.
#[derive(Default)]
struct Parser {
    layer_table: Vec<Layer>,
    layer_by_name: BTreeMap<String, LayerId>,
    nets: Vec<Net>,
    net_by_name: BTreeMap<String, NetId>,
    /// Numbered nets from a KiCad 9 net table; absent in KiCad 10 files,
    /// which name nets on every item.
    net_by_number: BTreeMap<i64, NetId>,
}

impl Parser {
    /// The `(layers ...)` table: one `(ordinal "name" type ["user name"])`
    /// per layer, in stack order.
    fn read_layer_table(&mut self, table: &Node) -> Result<()> {
        for entry in table.lists() {
            let ordinal = entry.head()?;
            let ordinal = ordinal
                .parse::<u32>()
                .map_err(|_| entry.error(format!("expected a layer number, found {ordinal:?}")))?;
            let name = entry.atom(0)?.to_owned();
            let kind = entry.atom(1)?;
            let kind = match kind {
                "signal" => LayerKind::Signal,
                "power" => LayerKind::Power,
                "mixed" => LayerKind::Mixed,
                "jumper" => LayerKind::Jumper,
                "user" => LayerKind::User,
                "auxiliary" => LayerKind::Auxiliary,
                _ => {
                    return Err(entry.error(format!(
                        "expected a layer type (signal, power, mixed, jumper, user), found {kind:?}"
                    )));
                }
            };
            let user_name = entry.atom_opt(2).map(str::to_owned);
            let id = LayerId::from_index(self.layer_table.len())
                .ok_or_else(|| entry.error("more layers than the model holds"))?;
            if self.layer_by_name.insert(name.clone(), id).is_some() {
                return Err(entry.error(format!("layer {name:?} is declared twice")));
            }
            self.layer_table.push(Layer {
                ordinal,
                name,
                kind,
                user_name,
            });
        }
        Ok(())
    }

    /// A `(net N "name")` entry of a KiCad 9 net table. Net 0 is "no net"
    /// and gets no id.
    fn declare_net(&mut self, node: &Node) -> Result<()> {
        let number = node.integer(0)?;
        let name = node.atom(1)?;
        if number == 0 {
            return Ok(());
        }
        if self.net_by_number.contains_key(&number) {
            return Err(node.error(format!("net {number} is declared twice")));
        }
        let id = self.intern_net(name, node)?;
        self.net_by_number.insert(number, id);
        Ok(())
    }

    fn intern_net(&mut self, name: &str, at: &Node) -> Result<NetId> {
        if let Some(&id) = self.net_by_name.get(name) {
            return Ok(id);
        }
        let id = u32::try_from(self.nets.len())
            .map(NetId)
            .map_err(|_| at.error("more nets than the model holds"))?;
        self.net_by_name.insert(name.to_owned(), id);
        self.nets.push(Net {
            name: name.to_owned(),
        });
        Ok(id)
    }

    /// The net an item's `(net ...)` names: by name (KiCad 10), or by
    /// number against the net table (KiCad 9). No `(net ...)`, an empty
    /// name or net 0 is no net.
    fn net(&mut self, item: &Node) -> Result<Option<NetId>> {
        let Some(node) = item.child("net") else {
            return Ok(None);
        };
        let atom = node.atom_at(0)?;
        if atom.quoted {
            if atom.text.is_empty() {
                return Ok(None);
            }
            return self.intern_net(&atom.text, node).map(Some);
        }
        let number = node.integer(0)?;
        if number == 0 {
            return Ok(None);
        }
        self.net_by_number
            .get(&number)
            .copied()
            .map(Some)
            .ok_or_else(|| node.error(format!("net {number} is not in the net table")))
    }

    /// The layer a `(layer "name" ...)` names.
    fn layer(&self, node: &Node) -> Result<LayerId> {
        self.layer_named(node, node.atom(0)?)
    }

    /// The layer called `name`; the error points at `node`.
    fn layer_named(&self, node: &Node, name: &str) -> Result<LayerId> {
        self.layer_by_name
            .get(name)
            .copied()
            .ok_or_else(|| node.error(format!("unknown layer {name:?}")))
    }

    /// The layers a `(layers ...)` names, wildcards expanded: `*.Cu` is
    /// every copper layer, `*.Mask` every layer ending in `.Mask`, `F&B.Cu`
    /// the two outer copper layers. A wildcard expands in the table's
    /// stack order. A wildcard that matches nothing adds nothing; a plain
    /// name that is not in the table is an error.
    fn layer_set(&self, node: &Node) -> Result<Vec<LayerId>> {
        let mut ids: Vec<LayerId> = Vec::new();
        let mut add = |id: LayerId| {
            if !ids.contains(&id) {
                ids.push(id);
            }
        };
        for index in 0..node.args().len() {
            let name = node.atom(index)?;
            if let Some(suffix) = name.strip_prefix('*') {
                for layer in &self.layer_table {
                    if layer.name.ends_with(suffix) {
                        add(self.layer_named(node, &layer.name)?);
                    }
                }
            } else if let Some(suffix) = name.strip_prefix("F&B") {
                for side in ['F', 'B'] {
                    add(self.layer_named(node, &format!("{side}{suffix}"))?);
                }
            } else {
                add(self.layer_named(node, name)?);
            }
        }
        Ok(ids)
    }

    fn footprint(&mut self, node: &Node) -> Result<Footprint> {
        let name = node.atom(0)?.to_owned();
        let layer = self.layer(node.require("layer")?)?;
        let (at, rotation) = at(node.require("at")?)?;
        let mut footprint = Footprint {
            name,
            layer,
            at,
            rotation,
            attributes: FootprintAttributes::default(),
            properties: Vec::new(),
            texts: Vec::new(),
            graphics: Vec::new(),
            pads: Vec::new(),
            zones: Vec::new(),
            clearances: clearances(node)?,
        };
        if let Some(attr) = node.child("attr") {
            footprint.attributes = FootprintAttributes {
                smd: attr.has_flag("smd"),
                through_hole: attr.has_flag("through_hole"),
                dnp: attr.has_flag("dnp"),
                exclude_from_bom: attr.has_flag("exclude_from_bom"),
                exclude_from_pos_files: attr.has_flag("exclude_from_pos_files"),
                board_only: attr.has_flag("board_only"),
            };
        }
        for child in node.lists() {
            match child.head()? {
                "property" => {
                    let name = child.atom(0)?;
                    let value = child.atom(1)?;
                    footprint.properties.push(Property {
                        name: name.to_owned(),
                        value: value.to_owned(),
                    });
                    if child.child("layer").is_some() {
                        let role = match name {
                            "Reference" => TextRole::Reference,
                            "Value" => TextRole::Value,
                            other => TextRole::Property(other.to_owned()),
                        };
                        footprint.texts.push(self.text(child, value, role)?);
                    }
                }
                "fp_text" => {
                    let role = match child.atom(0)? {
                        "reference" => TextRole::Reference,
                        "value" => TextRole::Value,
                        _ => TextRole::User,
                    };
                    footprint
                        .texts
                        .push(self.text(child, child.atom(1)?, role)?);
                }
                "fp_line" | "fp_arc" | "fp_circle" | "fp_rect" | "fp_poly" | "fp_curve" => {
                    footprint.graphics.push(self.graphic(child)?);
                }
                "pad" => footprint.pads.push(self.pad(child)?),
                "zone" => footprint.zones.push(self.zone(child)?),
                _ => {}
            }
        }
        Ok(footprint)
    }

    fn pad(&mut self, node: &Node) -> Result<Pad> {
        let number = node.atom(0)?.to_owned();
        let kind = node.atom(1)?;
        let kind = match kind {
            "smd" => PadKind::Smd,
            "thru_hole" => PadKind::ThroughHole,
            "np_thru_hole" => PadKind::NpThroughHole,
            "connect" => PadKind::Connect,
            _ => {
                return Err(node.error(format!(
                    "expected a pad type (smd, thru_hole, np_thru_hole, connect), found {kind:?}"
                )));
            }
        };
        let shape = node.atom(2)?;
        let shape = match shape {
            "circle" => PadShape::Circle,
            "rect" => PadShape::Rect,
            "oval" => PadShape::Oval,
            "trapezoid" => PadShape::Trapezoid,
            "roundrect" if node.child("chamfer").is_some() => PadShape::ChamferedRect,
            "roundrect" => PadShape::RoundRect,
            "custom" => PadShape::Custom,
            _ => {
                return Err(node.error(format!(
                    "expected a pad shape (circle, rect, oval, trapezoid, roundrect, custom), \
                     found {shape:?}"
                )));
            }
        };
        let (at, rotation) = at(node.require("at")?)?;
        let size = size(node.require("size")?)?;
        let drill = node.child("drill").map(drill).transpose()?.flatten();
        let layers = self.layer_set(node.require("layers")?)?;
        let net = self.net(node)?;
        let round_radius_ratio = optional_number(node, "roundrect_rratio")?;
        let mut primitives = Vec::new();
        if let Some(list) = node.child("primitives") {
            // Shapes only: KiCad also writes a (gr_bbox ...) hint there,
            // and may add more, so what is not a shape is skipped.
            for primitive in list.lists() {
                if is_shape(primitive.head()?) {
                    primitives.push(drawing(primitive)?);
                }
            }
        }
        Ok(Pad {
            number,
            kind,
            shape,
            at,
            rotation,
            size,
            drill,
            layers,
            net,
            pin_function: optional_text(node, "pinfunction")?,
            pin_type: optional_text(node, "pintype")?,
            round_radius_ratio,
            primitives,
            clearances: clearances(node)?,
        })
    }

    fn via(&mut self, node: &Node) -> Result<Via> {
        let kind = if node.has_flag("blind") {
            ViaKind::Blind
        } else if node.has_flag("micro") {
            ViaKind::Micro
        } else {
            ViaKind::Through
        };
        let (at, _) = at(node.require("at")?)?;
        let size = node.require("size")?.number(0)?;
        let drill = node.require("drill")?.number(0)?;
        let layers = node.require("layers")?;
        let layers = [
            self.layer_named(layers, layers.atom(0)?)?,
            self.layer_named(layers, layers.atom(1)?)?,
        ];
        Ok(Via {
            kind,
            at,
            size,
            drill,
            layers,
            net: self.net(node)?,
            free: flag(node, "free"),
        })
    }

    fn segment(&mut self, node: &Node) -> Result<Segment> {
        Ok(Segment {
            start: point(node.require("start")?)?,
            end: point(node.require("end")?)?,
            width: node.require("width")?.number(0)?,
            layer: self.layer(node.require("layer")?)?,
            net: self.net(node)?,
        })
    }

    fn track_arc(&mut self, node: &Node) -> Result<TrackArc> {
        Ok(TrackArc {
            start: point(node.require("start")?)?,
            mid: point(node.require("mid")?)?,
            end: point(node.require("end")?)?,
            width: node.require("width")?.number(0)?,
            layer: self.layer(node.require("layer")?)?,
            net: self.net(node)?,
        })
    }

    fn zone(&mut self, node: &Node) -> Result<Zone> {
        let net = self.net(node)?;
        let layers = match node.child("layers") {
            Some(list) => self.layer_set(list)?,
            None => vec![self.layer(node.require("layer")?)?],
        };
        let priority = match node.child("priority") {
            Some(priority) => {
                let value = priority.integer(0)?;
                u32::try_from(value).map_err(|_| {
                    priority.error(format!("expected a priority of 0 or more, found {value}"))
                })?
            }
            None => 0,
        };
        let filled = node.child("fill").is_some_and(|fill| fill.has_flag("yes"));
        let outline = points(node.require("polygon")?.require("pts")?)?;
        let mut fills = Vec::new();
        for fill in node.children("filled_polygon") {
            fills.push(Fill {
                layer: self.layer(fill.require("layer")?)?,
                points: points(fill.require("pts")?)?,
                island: fill.child("island").is_some(),
            });
        }
        Ok(Zone {
            net,
            layers,
            name: optional_text(node, "name")?,
            priority,
            keepout: node.child("keepout").is_some(),
            filled,
            outline,
            fills,
        })
    }

    fn graphic(&self, node: &Node) -> Result<Graphic> {
        Ok(Graphic {
            drawing: drawing(node)?,
            layer: self.layer(node.require("layer")?)?,
        })
    }

    /// A text item: `text` is its content (already read, since `gr_text`,
    /// `fp_text` and `property` keep it at different positions).
    fn text(&self, node: &Node, text: &str, role: TextRole) -> Result<Text> {
        let at_node = node.require("at")?;
        let (at, rotation) = at(at_node)?;
        let layer_node = node.require("layer")?;
        let layer = self.layer(layer_node)?;
        let hidden = node.has_flag("hide") || flag(node, "hide");
        // `(unlocked yes)`, or the word in the `at` of a KiCad 7 file
        // (after the angle when there is one), frees the text from the
        // upright rule.
        let upright = !(flag(node, "unlocked") || at_node.has_flag("unlocked"));
        let mut font = Font {
            face: None,
            size: Size {
                width: DEFAULT_TEXT_SIZE,
                height: DEFAULT_TEXT_SIZE,
            },
            thickness: None,
            bold: false,
            italic: false,
        };
        let mut justify = Justify::default();
        if let Some(effects) = node.child("effects") {
            if let Some(font_node) = effects.child("font") {
                font.face = optional_text(font_node, "face")?;
                if let Some(size_node) = font_node.child("size") {
                    font.size = size(size_node)?;
                }
                font.thickness = optional_number(font_node, "thickness")?;
                font.bold = font_node.has_flag("bold") || flag(font_node, "bold");
                font.italic = font_node.has_flag("italic") || flag(font_node, "italic");
            }
            if let Some(justify_node) = effects.child("justify") {
                if justify_node.has_flag("left") {
                    justify.horizontal = Horizontal::Left;
                } else if justify_node.has_flag("right") {
                    justify.horizontal = Horizontal::Right;
                }
                if justify_node.has_flag("top") {
                    justify.vertical = Vertical::Top;
                } else if justify_node.has_flag("bottom") {
                    justify.vertical = Vertical::Bottom;
                }
                justify.mirror = justify_node.has_flag("mirror");
            }
        }
        Ok(Text {
            text: text.to_owned(),
            role,
            at,
            rotation,
            layer,
            knockout: layer_node.has_flag("knockout"),
            hidden,
            upright,
            font,
            justify,
        })
    }
}

/// A boolean child: `(name yes)` or `(name no)` as KiCad 8 and later write
/// them, or a bare `(name)` from older files, which meant yes. Absent is
/// no. The one reading for `hide`, `free`, `bold` and `italic`.
fn flag(node: &Node, name: &str) -> bool {
    node.child(name)
        .is_some_and(|child| child.atom_opt(0) != Some("no"))
}

/// The first value of the child `name`, when the child is present.
fn optional_text(node: &Node, name: &str) -> Result<Option<String>> {
    node.child(name)
        .map(|child| child.atom(0).map(str::to_owned))
        .transpose()
}

/// The first value of the child `name` as a number, when the child is
/// present.
fn optional_number(node: &Node, name: &str) -> Result<Option<f64>> {
    node.child(name).map(|child| child.number(0)).transpose()
}

/// A pad's or a footprint's local mask and paste clearances, each absent
/// when the file leaves it to the level above.
fn clearances(node: &Node) -> Result<Clearances> {
    Ok(Clearances {
        solder_mask_margin: optional_number(node, "solder_mask_margin")?,
        solder_paste_margin: optional_number(node, "solder_paste_margin")?,
        solder_paste_ratio: optional_number(node, "solder_paste_ratio")?,
    })
}

/// A `(start x y)`, `(end x y)`, `(xy x y)`: two numbers.
fn point(node: &Node) -> Result<Point> {
    Ok(Point {
        x: node.number(0)?,
        y: node.number(1)?,
    })
}

/// A `(size w h)`.
fn size(node: &Node) -> Result<Size> {
    Ok(Size {
        width: node.number(0)?,
        height: node.number(1)?,
    })
}

/// An `(at x y [rotation])`. KiCad 7 wrote `(at x y unlocked)`; that word
/// is not a rotation and is skipped.
fn at(node: &Node) -> Result<(Point, f64)> {
    let rotation = match node.atom_opt(2) {
        None | Some("unlocked") => 0.0,
        Some(_) => node.number(2)?,
    };
    Ok((point(node)?, rotation))
}

/// A `(drill [oval] [d [h]] [(offset x y)])`. KiCad writes each part only
/// when it has something to say: the second size only when it differs
/// from the first, no size at all for a surface-mount pad whose shape is
/// merely offset. No size is no hole.
fn drill(node: &Node) -> Result<Option<Drill>> {
    let oval = node.atom_opt(0) == Some("oval");
    let first = usize::from(oval);
    let Some(_) = node.atom_opt(first) else {
        return Ok(None);
    };
    let width = node.number(first)?;
    let height = match node.atom_opt(first + 1) {
        Some(_) => node.number(first + 1)?,
        None => width,
    };
    let offset = node
        .child("offset")
        .map(point)
        .transpose()?
        .unwrap_or_default();
    Ok(Some(Drill {
        size: Size { width, height },
        offset,
    }))
}

/// Whether a list head names a shape [`drawing`] reads.
fn is_shape(head: &str) -> bool {
    let kind = head
        .strip_prefix("gr_")
        .or_else(|| head.strip_prefix("fp_"))
        .unwrap_or(head);
    matches!(kind, "line" | "arc" | "circle" | "rect" | "poly" | "curve")
}

/// The points of a `(pts ...)`: each `(xy ...)` one point, each
/// `(arc (start) (mid) (end))` its three.
fn points(node: &Node) -> Result<Vec<Point>> {
    let mut points = Vec::new();
    for item in node.lists() {
        match item.head()? {
            "xy" => points.push(point(item)?),
            "arc" => {
                points.push(point(item.require("start")?)?);
                points.push(point(item.require("mid")?)?);
                points.push(point(item.require("end")?)?);
            }
            _ => {}
        }
    }
    Ok(points)
}

/// A `gr_*` or `fp_*` shape with its stroke width and fill. The stroke is
/// `(stroke (width w) ...)`, or a bare `(width w)` in older footprints and
/// in custom pad primitives; neither means a width of 0. The fill is
/// `(fill yes|no)`, `(fill solid|none)` from older files, or
/// `(fill (type solid))`.
fn drawing(node: &Node) -> Result<Drawing> {
    let head = node.head()?;
    let kind = head
        .strip_prefix("gr_")
        .or_else(|| head.strip_prefix("fp_"))
        .unwrap_or(head);
    let shape = match kind {
        "line" => Shape::Line {
            start: point(node.require("start")?)?,
            end: point(node.require("end")?)?,
        },
        "arc" => Shape::Arc {
            start: point(node.require("start")?)?,
            mid: point(node.require("mid")?)?,
            end: point(node.require("end")?)?,
        },
        "circle" => Shape::Circle {
            center: point(node.require("center")?)?,
            end: point(node.require("end")?)?,
        },
        "rect" => Shape::Rect {
            start: point(node.require("start")?)?,
            end: point(node.require("end")?)?,
        },
        "poly" => Shape::Polygon {
            points: points(node.require("pts")?)?,
        },
        "curve" => Shape::Curve {
            points: points(node.require("pts")?)?,
        },
        _ => {
            return Err(node.error(format!(
                "expected a shape (line, arc, circle, rect, poly, curve), found ({head} ...)"
            )));
        }
    };
    let width = match node
        .child("stroke")
        .and_then(|stroke| stroke.child("width"))
    {
        Some(width) => width.number(0)?,
        None => match node.child("width") {
            Some(width) => width.number(0)?,
            None => 0.0,
        },
    };
    let filled = node.child("fill").is_some_and(|fill| {
        fill.has_flag("yes")
            || fill.has_flag("solid")
            || fill
                .child("type")
                .is_some_and(|kind| kind.atom_opt(0) == Some("solid"))
    });
    Ok(Drawing {
        shape,
        width,
        filled,
    })
}
