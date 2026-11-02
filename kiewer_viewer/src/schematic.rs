//! A schematic sheet as the renderer draws it: every layer of
//! `kicad_core::tessellate::schematic` tessellated once, positions
//! relative to the paper's centre, and one draw per layer in KiCad's
//! default schematic theme, in draw order. A sheet has no views, toggles
//! or opacities (those are the board's), so its draws are fixed when it
//! is built.

use kicad_core::geometry::Bounds;
use kicad_core::model::Point;
use kicad_core::schematic::model::Schematic;
use kicad_core::tessellate::Tessellator;
use kicad_core::tessellate::schematic::SchematicLayer;

use crate::render::{Draw, Rgb, Vertex};
use crate::scene::TOLERANCE;

/// KiCad's default schematic colour theme, the one the hand-off's PNG was
/// plotted in.
pub mod palette {
    use super::{Rgb, SchematicLayer};

    /// The paper.
    pub const PAGE: Rgb = Rgb(0xf5, 0xf4, 0xef);

    /// The colour of a layer.
    #[must_use]
    pub fn of(layer: SchematicLayer) -> Rgb {
        match layer {
            SchematicLayer::Background => Rgb(0xff, 0xff, 0xc2),
            SchematicLayer::Device
            | SchematicLayer::Pin
            | SchematicLayer::Sheet
            | SchematicLayer::GlobalLabel => Rgb(0x84, 0x00, 0x00),
            SchematicLayer::Notes => Rgb(0x00, 0x00, 0xc2),
            SchematicLayer::Bus => Rgb(0x00, 0x00, 0xff),
            SchematicLayer::Wire | SchematicLayer::Junction => Rgb(0x00, 0x96, 0x00),
            SchematicLayer::LocalLabel => Rgb(0x00, 0x00, 0x00),
            SchematicLayer::HierLabel | SchematicLayer::SheetFile => Rgb(0x72, 0x56, 0x00),
            SchematicLayer::NoConnect => Rgb(0x00, 0x00, 0x84),
            SchematicLayer::SheetName
            | SchematicLayer::SheetLabel
            | SchematicLayer::PinName
            | SchematicLayer::Value
            | SchematicLayer::Reference => Rgb(0x00, 0x64, 0x64),
            SchematicLayer::PinNumber => Rgb(0xa9, 0x00, 0x00),
            SchematicLayer::Fields => Rgb(0x84, 0x00, 0x84),
            SchematicLayer::Dnp => Rgb(0xdc, 0x09, 0x09),
        }
    }
}

/// The paper colour as the frame is cleared to, for a target of `format`.
#[must_use]
pub fn clear_color(format: wgpu::TextureFormat) -> wgpu::Color {
    palette::PAGE.clear_for_format(format)
}

/// Triangles ready for upload, and the draws that colour them.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    /// The world point the positions are relative to: the middle of the
    /// paper (grown to hold anything drawn off it).
    pub origin: Point,
    /// What the camera fits to, in world millimetres: the paper, grown to
    /// hold anything drawn off it.
    pub bounds: Bounds,
    /// Positions relative to [`Sheet::origin`].
    pub vertices: Vec<Vertex>,
    /// Three per triangle.
    pub indices: Vec<u32>,
    /// One draw per layer, bottom first, coloured for the target format
    /// the sheet was built for.
    pub draws: Vec<Draw>,
}

/// Every layer of `sheet`, coloured for a target of `format`; `None` for
/// a sheet with nothing drawn on it.
#[must_use]
pub fn build(sheet: &Schematic, format: wgpu::TextureFormat) -> Option<Sheet> {
    let tessellator = Tessellator::new(TOLERANCE);
    let layers = tessellator.sheet(sheet);
    let drawn = layers
        .iter()
        .filter_map(|(_, mesh)| mesh.bounds())
        .reduce(Bounds::union)?;
    let paper = Bounds {
        min: Point::default(),
        max: Point {
            x: sheet.paper.size.width,
            y: sheet.paper.size.height,
        },
    };
    let bounds = paper.union(drawn);
    let origin = bounds.center();
    let mut out = Sheet {
        origin,
        bounds,
        vertices: Vec::new(),
        indices: Vec::new(),
        draws: Vec::new(),
    };
    for (layer, mesh) in &layers {
        let base = u32::try_from(out.vertices.len()).unwrap_or(u32::MAX);
        let start = u32::try_from(out.indices.len()).unwrap_or(u32::MAX);
        out.vertices
            .extend(mesh.positions.iter().map(|position| Vertex {
                position: relative(*position, origin),
            }));
        out.indices
            .extend(mesh.indices.iter().map(|index| index + base));
        let end = u32::try_from(out.indices.len()).unwrap_or(u32::MAX);
        out.draws.push(Draw {
            range: start..end,
            color: palette::of(*layer).for_format(format),
        });
    }
    Some(out)
}

/// `position` (millimetres, f32) relative to `origin`, the difference
/// taken in f64.
#[expect(
    clippy::cast_possible_truncation,
    reason = "relative to the sheet's centre a position is under a metre, which f32 holds to a tenth of a micrometre"
)]
fn relative(position: [f32; 2], origin: Point) -> [f32; 2] {
    [
        (f64::from(position[0]) - origin.x) as f32,
        (f64::from(position[1]) - origin.y) as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    const SCHEMATIC_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_sch"
    );

    fn hand_off() -> &'static Schematic {
        static SCHEMATIC: OnceLock<Schematic> = OnceLock::new();
        SCHEMATIC.get_or_init(|| {
            let source = std::fs::read_to_string(SCHEMATIC_PATH)
                .expect("the hand-off schematic is in the repo");
            kicad_core::parse_schematic(&source).expect("the hand-off schematic parses")
        })
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the colours compared were produced by the same conversion of the same bytes, so they are bit-identical"
    )]
    fn the_hand_off_sheet_becomes_a_scene_around_its_a4_paper() {
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let sheet = build(hand_off(), format).expect("a sheet with content");
        // Everything is on the A4 landscape paper, so the paper is the
        // view: 297 by 210 around (148.5, 105).
        assert!((sheet.origin.x - 148.5).abs() < 1e-6, "{:?}", sheet.origin);
        assert!((sheet.origin.y - 105.0).abs() < 1e-6, "{:?}", sheet.origin);
        assert!((sheet.bounds.width() - 297.0).abs() < 1e-6);
        assert!((sheet.bounds.height() - 210.0).abs() < 1e-6);
        assert_eq!(sheet.indices.len() % 3, 0);
        assert!(sheet.vertices.len() > 10_000);
        for vertex in &sheet.vertices {
            assert!(vertex.position[0].abs() <= 148.5, "{:?}", vertex.position);
            assert!(vertex.position[1].abs() <= 105.0, "{:?}", vertex.position);
        }
        let max_index = sheet.indices.iter().max().copied().unwrap_or(0);
        assert!(usize::try_from(max_index).expect("fits") < sheet.vertices.len());
        // The draws tile the index buffer in order, each in its layer's
        // colour, with the body fills first and the DNP crosses last.
        let mut next = 0;
        for draw in &sheet.draws {
            assert_eq!(draw.range.start, next);
            assert_eq!(draw.range.len() % 3, 0);
            next = draw.range.end;
        }
        assert_eq!(usize::try_from(next).expect("fits"), sheet.indices.len());
        let first = sheet.draws.first().expect("draws");
        assert_eq!(
            first.color,
            palette::of(SchematicLayer::Background).for_format(format)
        );
        let last = sheet.draws.last().expect("draws");
        assert_eq!(
            last.color,
            palette::of(SchematicLayer::Dnp).for_format(format)
        );
        for layer in [
            SchematicLayer::Device,
            SchematicLayer::Wire,
            SchematicLayer::GlobalLabel,
            SchematicLayer::Reference,
        ] {
            let color = palette::of(layer).for_format(format);
            assert!(
                sheet.draws.iter().any(|d| d.color == color),
                "{layer:?} is drawn"
            );
        }
        assert!(sheet.draws.iter().all(|d| (d.color[3] - 1.0).abs() < 1e-6));
    }

    #[test]
    fn a_sheet_with_nothing_on_it_is_no_scene() {
        let sheet = kicad_core::parse_schematic(
            r#"(kicad_sch (version 20250114) (generator "eeschema") (lib_symbols))"#,
        )
        .expect("parses");
        assert_eq!(build(&sheet, wgpu::TextureFormat::Rgba8Unorm), None);
    }

    #[test]
    fn the_view_grows_to_hold_what_is_drawn_off_the_paper() {
        let sheet = kicad_core::parse_schematic(
            r#"(kicad_sch (version 20250114) (generator "eeschema") (paper "A4") (lib_symbols)
               (wire (pts (xy -10 5) (xy 300 5)) (stroke (width 0) (type default)) (uuid "w")))"#,
        )
        .expect("parses");
        let sheet = build(&sheet, wgpu::TextureFormat::Rgba8Unorm).expect("a wire");
        assert!(sheet.bounds.min.x < -9.9, "{:?}", sheet.bounds);
        assert!(sheet.bounds.max.x > 299.9, "{:?}", sheet.bounds);
        assert!(
            (sheet.bounds.max.y - 210.0).abs() < 1e-6,
            "{:?}",
            sheet.bounds
        );
    }

    #[test]
    fn the_paper_is_the_clear_colour() {
        let clear = clear_color(wgpu::TextureFormat::Rgba8Unorm);
        assert!((clear.r - 245.0 / 255.0).abs() < 1e-6, "{clear:?}");
        assert!((clear.b - 239.0 / 255.0).abs() < 1e-6, "{clear:?}");
    }
}
