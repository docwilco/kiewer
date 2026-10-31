//! A board as the renderer draws it: the outline and the front copper
//! tessellated and coloured, positions relative to the board's centre so
//! the GPU's f32 holds them to a few nanometres.

use kicad_core::geometry::Bounds;
use kicad_core::model::{Board, Point};
use kicad_core::tessellate::{Mesh, Tessellator};

use crate::render::{Rgb, Vertex};

/// How far a flattened curve may sit from the true one, in millimetres:
/// a micrometre, a tenth of a pixel at the thousandfold zoom the tracer
/// asks for.
pub const TOLERANCE: f64 = 0.001;

/// The colours of the hand-off's first view (viewer-drop/README.md).
pub mod palette {
    use super::Rgb;

    /// The page behind the board.
    pub const PAGE: Rgb = Rgb(0x2b, 0x2b, 0x2b);
    /// The board outline, `Edge.Cuts`.
    pub const OUTLINE: Rgb = Rgb(0xd0, 0xd2, 0xcd);
    /// The front copper, KiCad's default theme.
    pub const F_CU: Rgb = Rgb(0xc8, 0x34, 0x34);
}

/// The outline layer's name and the copper layer the tracer shows.
pub const COPPER_LAYER: &str = "F.Cu";

/// Triangles ready for upload, with where they came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    /// The world point the positions are relative to: the middle of the
    /// outline's box (or of the copper's, for a board without an outline).
    pub origin: Point,
    /// What the camera fits to, in world millimetres.
    pub bounds: Bounds,
    /// Positions relative to [`Scene::origin`], coloured.
    pub vertices: Vec<Vertex>,
    /// Three per triangle.
    pub indices: Vec<u32>,
}

impl Scene {
    /// The outline and the front copper of `board`, coloured for a target
    /// of `format`; `None` for a board with neither.
    #[must_use]
    pub fn build(board: &Board, format: wgpu::TextureFormat) -> Option<Self> {
        let tessellator = Tessellator::new(TOLERANCE);
        let outline = tessellator.outline(board);
        let copper = board
            .layer_named(COPPER_LAYER)
            .map(|layer| tessellator.copper(board, layer))
            .unwrap_or_default();
        let bounds = Tessellator::outline_bounds(board)
            .or_else(|| outline.bounds())
            .or_else(|| copper.bounds())?;
        let origin = bounds.center();
        let mut scene = Self {
            origin,
            bounds,
            vertices: Vec::new(),
            indices: Vec::new(),
        };
        scene.add(&outline, palette::OUTLINE.for_format(format));
        scene.add(&copper, palette::F_CU.for_format(format));
        Some(scene)
    }

    /// Append `mesh` in `color`, positions made relative to the origin.
    fn add(&mut self, mesh: &Mesh, color: [f32; 4]) {
        let base = u32::try_from(self.vertices.len()).unwrap_or(u32::MAX);
        self.vertices
            .extend(mesh.positions.iter().map(|position| Vertex {
                position: relative(*position, self.origin),
                color,
            }));
        self.indices
            .extend(mesh.indices.iter().map(|index| index + base));
    }
}

/// `position` (millimetres, f32) relative to `origin`: the difference is
/// taken in f64 so no precision is lost before the result is stored.
#[expect(
    clippy::cast_possible_truncation,
    reason = "relative to the board's centre a position is under a few hundred millimetres, which f32 holds to ten nanometres"
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

    const BOARD_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_pcb"
    );

    fn hand_off() -> &'static Board {
        static BOARD: OnceLock<Board> = OnceLock::new();
        BOARD.get_or_init(|| {
            let source =
                std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo");
            kicad_core::parse_board(&source).expect("the hand-off board parses")
        })
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the colours compared were produced by the same conversion of the same bytes, so they are bit-identical"
    )]
    fn the_hand_off_board_becomes_a_scene_around_its_centre() {
        let scene = Scene::build(hand_off(), wgpu::TextureFormat::Bgra8UnormSrgb)
            .expect("a board with an outline");
        // The outline runs from (50, 50) to (162, 108).
        assert!((scene.origin.x - 106.0).abs() < 1e-6, "{:?}", scene.origin);
        assert!((scene.origin.y - 79.0).abs() < 1e-6, "{:?}", scene.origin);
        assert!((scene.bounds.width() - 112.0).abs() < 1e-6);
        assert!((scene.bounds.height() - 58.0).abs() < 1e-6);
        assert_eq!(scene.indices.len() % 3, 0);
        assert!(scene.vertices.len() > 10_000);
        // Every position is within the board, relative to its centre.
        for vertex in &scene.vertices {
            assert!(vertex.position[0].abs() <= 57.0, "{:?}", vertex.position);
            assert!(vertex.position[1].abs() <= 30.0, "{:?}", vertex.position);
        }
        let max_index = scene.indices.iter().max().copied().unwrap_or(0);
        assert!(usize::try_from(max_index).expect("fits") < scene.vertices.len());
        // Both colours are present: the outline's and the copper's.
        let outline = palette::OUTLINE.for_format(wgpu::TextureFormat::Bgra8UnormSrgb);
        let copper = palette::F_CU.for_format(wgpu::TextureFormat::Bgra8UnormSrgb);
        assert!(scene.vertices.iter().any(|v| v.color == outline));
        assert!(scene.vertices.iter().any(|v| v.color == copper));
    }

    #[test]
    fn a_board_with_nothing_to_draw_is_no_scene() {
        let board = kicad_core::parse_board(
            r#"(kicad_pcb (version 20241229) (generator "pcbnew") (layers (0 "F.Cu" signal)))"#,
        )
        .expect("parses");
        assert_eq!(Scene::build(&board, wgpu::TextureFormat::Rgba8Unorm), None);
    }
}
