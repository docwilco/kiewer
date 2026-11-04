//! The Canvas2D glyph atlas (docs/v1-scope.md, Fonts): text in a system
//! face the browser has is rasterised by the page into one texture per
//! face, and the viewer draws each character as a textured quad through
//! the renderer's text pass. Everything here is the pure part of that:
//! which texts a face owns, how its glyph cells pack into the atlas, the
//! size they are rasterised at for the current zoom, and the quads.
//!
//! The split with the page: the page measures each character's advance
//! and ink and the face's ascent and descent with `measureText`, this module
//! plans the atlas ([`plan`]), the page draws every glyph into its
//! planned rectangle and hands back the alpha channel, and [`Quads::add`]
//! places the cells `kicad_core::font::outline` laid out on the board
//! with the texture coordinates of [`Plan::glyphs`]. The texture is
//! coverage only (R8): the colour is the layer's, from the vertex.
//!
//! Sharpness across zoom: a face's atlas is rasterised at a power of two
//! pixels per em, chosen from the largest text in the face at the
//! camera's scale ([`wanted_px`]), and rebuilt when the view needs more
//! than it has or has four times what it needs ([`needs_rebuild`]): a
//! glyph is then never magnified (blurry) and never shrunk below a
//! quarter (where linear filtering without mipmaps starts to shimmer).

use std::collections::{BTreeMap, BTreeSet};

use bytemuck::{Pod, Zeroable};
use kicad_core::font::outline::{EM_PER_HEIGHT, OutlineText};
use kicad_core::font::placed_texts;
use kicad_core::model::{Board, LayerId, Point, Text};

/// The atlas's largest side: the WebGL2 floor's `max_texture_dimension_2d`.
pub const MAX_SIDE: u32 = 2048;
/// The atlas's smallest side: a row of R8 texels is a row of bytes, and
/// `write_texture` wants rows aligned to 256 bytes.
pub const MIN_SIDE: u32 = 256;
/// Empty texels around each cell, so linear filtering at a cell's edge
/// reads nothing of its neighbour. The texel at (0, 0) is always padding,
/// which the knockout strips sample as zero coverage.
pub const PAD: u32 = 1;
/// The fewest pixels per em an atlas is rasterised at.
pub const MIN_PX: u32 = 8;
/// The most, before the pack is asked to fit it; [`plan`] halves from
/// here while the cells overflow [`MAX_SIDE`].
pub const MAX_PX: u32 = 1024;

/// A rectangle of the atlas, in texels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Where every cell went, and how large the atlas is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packing {
    /// Texels across: a power of two from [`MIN_SIDE`] to [`MAX_SIDE`].
    pub width: u32,
    /// Texels down: a power of two from [`MIN_SIDE`] to [`MAX_SIDE`].
    pub height: u32,
    /// One rectangle per input size, in input order.
    pub rects: Vec<Rect>,
}

/// Pack `sizes` (width, height in texels) into shelves: cells go left to
/// right in input order, a row as tall as its tallest cell, [`PAD`]
/// texels between everything and around the edge. Of the widths that
/// fit, the one with the smallest area wins; `None` when no side up to
/// [`MAX_SIDE`] holds them all.
#[must_use]
pub fn pack(sizes: &[(u32, u32)]) -> Option<Packing> {
    let mut best: Option<Packing> = None;
    let mut width = MIN_SIDE;
    while width <= MAX_SIDE {
        if let Some(packing) = pack_into(sizes, width)
            && best
                .as_ref()
                .is_none_or(|b| packing.width * packing.height < b.width * b.height)
        {
            best = Some(packing);
        }
        width *= 2;
    }
    best
}

fn pack_into(sizes: &[(u32, u32)], width: u32) -> Option<Packing> {
    let mut rects = Vec::with_capacity(sizes.len());
    let (mut x, mut y, mut row_height) = (PAD, PAD, 0);
    for &(w, h) in sizes {
        if w + 2 * PAD > width {
            return None;
        }
        if x + w + PAD > width {
            x = PAD;
            y += row_height + PAD;
            row_height = 0;
        }
        rects.push(Rect {
            x,
            y,
            width: w,
            height: h,
        });
        x += w + PAD;
        row_height = row_height.max(h);
    }
    let used = y + row_height + PAD;
    let height = used.max(MIN_SIDE).next_power_of_two();
    (height <= MAX_SIDE).then_some(Packing {
        width,
        height,
        rects,
    })
}

/// A face's atlas as planned: the pixels per em it is rasterised at and
/// where each of its characters goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Pixels per em the atlas is drawn at: `wanted`, or a halving of it
    /// when the cells did not fit.
    pub px: u32,
    /// Pixels per em the view asked for. What a later [`needs_rebuild`]
    /// compares against: a halved `px` is the most this face can have,
    /// and asking again every frame would rebuild the same atlas.
    pub wanted: u32,
    /// The cells, in the order of the characters given to [`plan`].
    pub packing: Packing,
}

/// Where a character's quad samples the atlas, in texels: its cell's
/// origin and its exact size, which the texel grid rounds up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A face's characters in its atlas: where each one samples, and the
/// atlas's size to divide by.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Glyphs {
    /// Texels across.
    pub width: u32,
    /// Texels down.
    pub height: u32,
    /// Each character's sample.
    pub by_char: BTreeMap<char, Sample>,
}

impl Plan {
    /// The glyphs of this plan for `chars`, each `advances` em wide (both
    /// in the order given to [`plan`]), with a line box `line_height` em
    /// tall: the sample of a character is its packed rectangle's origin
    /// and its advance and line height at this plan's pixels per em,
    /// unrounded, so the quad on the board, which is exactly those sizes
    /// in millimetres, samples exactly the glyph and not the texel the
    /// rounding added.
    #[must_use]
    pub fn glyphs(&self, chars: &[char], advances: &[f64], line_height: f64) -> Glyphs {
        let px = f64::from(self.px);
        let by_char = chars
            .iter()
            .zip(advances)
            .zip(&self.packing.rects)
            .map(|((&c, &advance), rect)| {
                (
                    c,
                    Sample {
                        x: f64::from(rect.x),
                        y: f64::from(rect.y),
                        width: advance * px,
                        height: line_height * px,
                    },
                )
            })
            .collect();
        Glyphs {
            width: self.packing.width,
            height: self.packing.height,
            by_char,
        }
    }
}

/// Plan the atlas of a face for `chars`, each `advances` em wide (same
/// order), with a line box `line_height` em tall, at `px` pixels per em
/// or the largest halving of it whose cells fit. `None` when they do
/// not fit even at [`MIN_PX`].
#[must_use]
pub fn plan(advances: &[f64], line_height: f64, px: u32) -> Option<Plan> {
    let wanted = px.clamp(MIN_PX, MAX_PX);
    let mut px = wanted;
    loop {
        let sizes: Vec<(u32, u32)> = advances
            .iter()
            .map(|advance| {
                (
                    texels(advance * f64::from(px)),
                    texels(line_height * f64::from(px)),
                )
            })
            .collect();
        if let Some(packing) = pack(&sizes) {
            return Some(Plan {
                px,
                wanted,
                packing,
            });
        }
        if px <= MIN_PX {
            return None;
        }
        px = (px / 2).max(MIN_PX);
    }
}

/// A length in em-pixels as whole texels, at least one.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "an advance or a line height at MAX_PX pixels per em is a few thousand at most, and a negative or NaN measurement rounds to the one-texel floor"
)]
fn texels(length: f64) -> u32 {
    if length.is_finite() {
        (length.ceil() as u32).max(1)
    } else {
        1
    }
}

/// The pixels per em a face needs for its largest text (`max_height` mm)
/// at `scale` device pixels per millimetre: KiCad's em is 1.4 heights,
/// rounded up to a power of two and held between [`MIN_PX`] and
/// [`MAX_PX`].
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped to MAX_PX before the cast and a NaN or negative one is clamped to MIN_PX"
)]
pub fn wanted_px(max_height: f64, scale: f64) -> u32 {
    let px = (max_height * EM_PER_HEIGHT * scale).ceil();
    if px.is_nan() || px <= f64::from(MIN_PX) {
        return MIN_PX;
    }
    (px.min(f64::from(MAX_PX)) as u32).next_power_of_two()
}

/// Whether an atlas rasterised at `current` pixels per em (`None` when
/// there is none) should be rebuilt for a view that wants `wanted`: when
/// the view wants more (the glyphs would be magnified) or less than a
/// quarter (they would be minified past what linear filtering holds).
#[must_use]
pub fn needs_rebuild(current: Option<u32>, wanted: u32) -> bool {
    match current {
        None => true,
        Some(current) => wanted > current || wanted * 4 <= current,
    }
}

/// The distinct faces named by the visible texts on `layer`, in order
/// of first appearance.
#[must_use]
pub fn faces_on(board: &Board, layer: LayerId) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut faces = Vec::new();
    for text in texts_on(board, layer) {
        if let Some(face) = &text.font.face
            && seen.insert(face.as_str())
        {
            faces.push(face.clone());
        }
    }
    faces
}

/// The visible texts on `layer`, the board's and the footprints', with
/// their content resolved.
pub fn texts_on(board: &Board, layer: LayerId) -> impl Iterator<Item = &Text> {
    board
        .all_texts()
        .filter(move |text| text.layer == layer && !text.hidden)
}

/// The distinct characters the texts in `face` on `layer` draw, their
/// footprint variables resolved, a space among them (the layout needs
/// its advance for tabs and spaces), sorted.
#[must_use]
pub fn chars_in(board: &Board, layer: LayerId, face: &str) -> Vec<char> {
    let mut chars = BTreeSet::from([' ']);
    for (content, _, text) in placed_texts(board, layer) {
        if text.font.face.as_deref() == Some(face) {
            chars.extend(content.chars().filter(|c| !matches!(c, '\n' | '\t')));
        }
    }
    chars.into_iter().collect()
}

/// The tallest text in `face` on `layer`, in millimetres; 0 without one.
#[must_use]
pub fn max_height(board: &Board, layer: LayerId, face: &str) -> f64 {
    texts_on(board, layer)
        .filter(|text| text.font.face.as_deref() == Some(face))
        .map(|text| text.font.size.height.abs())
        .fold(0.0, f64::max)
}

/// `board` with every text in one of `system_faces` hidden, so the
/// Newstroke scene built from it leaves those texts to the atlas.
#[must_use]
pub fn stroke_board(board: &Board, system_faces: &BTreeSet<String>) -> Board {
    let mut board = board.clone();
    let hide = |text: &mut Text| {
        if text
            .font
            .face
            .as_ref()
            .is_some_and(|f| system_faces.contains(f))
        {
            text.hidden = true;
        }
    };
    board.texts.iter_mut().for_each(hide);
    for footprint in &mut board.footprints {
        footprint.texts.iter_mut().for_each(hide);
    }
    board
}

/// One vertex of the text pass: a position in millimetres relative to
/// the scene's origin, where in the atlas it samples (0 to 1), the
/// colour, and whether the coverage is inverted (1 for a knockout's
/// cells and strips, 0 for a glyph).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct TextVertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
    pub color: [f32; 4],
    pub invert: f32,
}

impl TextVertex {
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<TextVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32],
    };
}

/// The quads of one face: two triangles per cell and per strip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quads {
    pub vertices: Vec<TextVertex>,
    pub indices: Vec<u32>,
}

impl Quads {
    /// Append `text`'s cells, each sampling the part of its character's
    /// glyph of `glyphs` its crop names (a knockout's cell is cropped to
    /// the knockout box, so the inverted coverage never fills past it),
    /// and its knockout strips, sampling the empty corner texel with the
    /// coverage inverted; `knockout` inverts the cells too. Positions are
    /// made relative to `origin`. A character the atlas lacks is skipped.
    pub fn add(
        &mut self,
        text: &OutlineText,
        glyphs: &Glyphs,
        origin: Point,
        color: [f32; 4],
        knockout: bool,
    ) {
        let (width, height) = (f64::from(glyphs.width), f64::from(glyphs.height));
        let invert = if knockout { 1.0 } else { 0.0 };
        for cell in &text.cells {
            let Some(sample) = glyphs.by_char.get(&cell.c) else {
                continue;
            };
            let crop = cell.crop;
            let u = |fraction: f64| (sample.x + fraction * sample.width) / width;
            let v = |fraction: f64| (sample.y + fraction * sample.height) / height;
            let (x0, y0) = (u(crop.min.x), v(crop.min.y));
            let (x1, y1) = (u(crop.max.x), v(crop.max.y));
            self.quad(
                cell.corners,
                [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
                origin,
                color,
                invert,
            );
        }
        // The pad texel at (0, 0) has no coverage; inverted, a strip is
        // solid colour. Sampled at its centre, so filtering stays on it.
        let pad = [0.5 / width, 0.5 / height];
        for strip in &text.strips {
            self.quad(*strip, [pad; 4], origin, color, 1.0);
        }
    }

    fn quad(
        &mut self,
        corners: [Point; 4],
        uvs: [[f64; 2]; 4],
        origin: Point,
        color: [f32; 4],
        invert: f32,
    ) {
        let base = u32::try_from(self.vertices.len()).unwrap_or(u32::MAX);
        for (corner, uv) in corners.iter().zip(uvs) {
            self.vertices.push(TextVertex {
                position: relative(*corner, origin),
                uv: uv.map(to_f32),
                color,
                invert,
            });
        }
        self.indices
            .extend([0, 1, 2, 0, 2, 3].map(|index| base + index));
    }
}

/// `point` relative to `origin`, the difference taken in f64.
fn relative(point: Point, origin: Point) -> [f32; 2] {
    [to_f32(point.x - origin.x), to_f32(point.y - origin.y)]
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "a position relative to the board's centre is under a few hundred millimetres and a texture coordinate is 0 to 1, both well within f32"
)]
fn to_f32(value: f64) -> f32 {
    value as f32
}

#[cfg(test)]
mod tests {
    use kicad_core::font::Placement;
    use kicad_core::font::outline::{Cell, FaceMetrics, Ink, WHOLE_CELL, layout};
    use kicad_core::model::{Justify, Size};
    use kiewer_assert::assert_is_empty;
    use test_case::test_case;

    use super::*;

    #[test]
    fn cells_pack_into_shelves_with_padding_in_the_smallest_power_of_two_atlas() {
        let packing = pack(&[(10, 20), (30, 20), (10, 25)]).expect("fits");
        assert_eq!((packing.width, packing.height), (256, 256));
        assert_eq!(
            packing.rects,
            [
                Rect {
                    x: 1,
                    y: 1,
                    width: 10,
                    height: 20
                },
                Rect {
                    x: 12,
                    y: 1,
                    width: 30,
                    height: 20
                },
                Rect {
                    x: 43,
                    y: 1,
                    width: 10,
                    height: 25
                },
            ]
        );
    }

    #[test]
    fn a_full_row_wraps_to_the_next_shelf_below_the_tallest_cell() {
        // 100 texels wide plus padding: two per 256 row, the third wraps.
        let packing = pack(&[(100, 30), (100, 40), (100, 10)]).expect("fits");
        assert_eq!(packing.width, 256);
        let third = packing.rects.get(2).copied().expect("three rects");
        assert_eq!((third.x, third.y), (1, 42));
    }

    #[test]
    fn many_cells_widen_the_atlas_to_the_smallest_area_that_holds_them() {
        // 100 cells of 120 by 120: at 256 wide, two per row, 50 rows of
        // 121 is 6050 texels tall, too tall; at 512 wide, four per row,
        // 25 rows is 3025, still too tall; at 1024 wide, eight per row,
        // 13 rows is 1573, a 2048 texel atlas; at 2048 wide, 16 per row,
        // 7 rows is 847, a 1024 texel atlas: the same area, either wins.
        let sizes = vec![(120, 120); 100];
        let packing = pack(&sizes).expect("fits");
        assert_eq!(packing.width * packing.height, 2048 * 1024);
        assert!(packing.width >= 1024, "{packing:?}");
        assert_eq!(packing.rects.len(), 100);
        for rect in &packing.rects {
            assert!(rect.x + rect.width + PAD <= packing.width);
            assert!(rect.y + rect.height + PAD <= packing.height);
        }
    }

    #[test_case(&vec![(500, 500); 100]; "a hundred cells of 500 overflow the largest side")]
    #[test_case(&[(3000, 10)]; "one cell wider than the largest side")]
    fn cells_that_overflow_the_largest_atlas_do_not_pack(sizes: &[(u32, u32)]) {
        assert_eq!(pack(sizes), None);
    }

    #[test]
    fn no_cells_pack_into_the_smallest_atlas() {
        let empty = pack(&[]).expect("an empty atlas packs");
        assert_eq!((empty.width, empty.height), (256, 256));
    }

    #[test]
    fn a_plan_halves_the_pixel_size_until_the_cells_fit() {
        // Sixty characters 0.6 em wide, a line 1.2 em tall: at 1024 px per
        // em a cell is 615 by 1229, far past any atlas; at 512 it is 308
        // by 615, sixty of which need 60 * 309 * 616 texels, over the
        // 2048 square; at 256, 154 by 308: 13 per 2048 row, five rows of
        // 309 fit in 2048.
        let advances = vec![0.6; 60];
        let plan = plan(&advances, 1.2, MAX_PX).expect("fits at some size");
        assert_eq!(plan.px, 256);
        assert_eq!(plan.wanted, MAX_PX, "what was asked for is kept");
        assert_eq!(plan.packing.rects.len(), 60);
        let first = plan.packing.rects.first().copied().expect("a rect");
        assert_eq!((first.width, first.height), (154, 308));
        // A size below the floor is raised to it.
        assert_eq!(super::plan(&[0.5], 1.0, 1).expect("fits").px, MIN_PX);
    }

    #[test_case(1.0, 1.0, MIN_PX; "a millimetre at one pixel per millimetre is under the floor")]
    #[test_case(1.0, 10.0, 16; "14 em-pixels round up to 16")]
    #[test_case(1.0, 100.0, 256; "140 round up to 256")]
    #[test_case(2.0, 1000.0, MAX_PX; "2800 are capped")]
    #[test_case(0.0, 100.0, MIN_PX; "no text is the floor")]
    #[test_case(f64::NAN, 100.0, MIN_PX; "a NaN height is the floor")]
    fn the_wanted_pixels_per_em_are_the_next_power_of_two_in_range(
        height: f64,
        scale: f64,
        expected: u32,
    ) {
        assert_eq!(wanted_px(height, scale), expected);
    }

    #[test_case(None, 64, true; "no atlas yet")]
    #[test_case(Some(64), 128, true; "the view wants more")]
    #[test_case(Some(64), 64, false; "the view wants what it has")]
    #[test_case(Some(64), 32, false; "half is kept")]
    #[test_case(Some(64), 16, true; "a quarter is rebuilt")]
    fn an_atlas_is_rebuilt_when_magnified_or_minified_past_a_quarter(
        current: Option<u32>,
        wanted: u32,
        expected: bool,
    ) {
        assert_eq!(needs_rebuild(current, wanted), expected);
    }

    const BOARD: &str = r#"(kicad_pcb (version 20241229) (generator "pcbnew")
        (layers (0 "F.Cu" signal) (37 "F.SilkS" user))
        (gr_text "Hi there" (at 0 0) (layer "F.SilkS") (effects (font (face "Consolas") (size 1 1))))
        (gr_text "stroke" (at 0 0) (layer "F.SilkS") (effects (font (size 2 2))))
        (gr_text "gone" (at 0 0) (layer "F.SilkS") (hide yes) (effects (font (face "Arial") (size 1 1))))
        (gr_text "copper" (at 0 0) (layer "F.Cu") (effects (font (face "Georgia") (size 1 1))))
        (footprint "X" (layer "F.Cu") (at 5 5)
          (property "Reference" "R1" (at 0 0) (layer "F.SilkS") (effects (font (face "Consolas") (size 1.5 1.5))))
          (fp_text user "${REFERENCE}!" (at 0 0) (layer "F.SilkS") (effects (font (face "Verdana") (size 1 1))))))"#;

    fn board() -> Board {
        kicad_core::parse_board(BOARD).expect("parses")
    }

    fn silk(board: &Board) -> LayerId {
        board.layer_named("F.SilkS").expect("a silkscreen")
    }

    #[test]
    fn the_faces_on_a_layer_are_the_visible_texts_distinct_in_order() {
        let board = board();
        assert_eq!(faces_on(&board, silk(&board)), ["Consolas", "Verdana"]);
    }

    #[test_case("Consolas", " 1HRehirt"; "the board text and the reference, with a space")]
    #[test_case("Verdana", " !1R"; "a footprint text with its variable resolved")]
    #[test_case("Nothing", " "; "a face no text names has only the space")]
    fn the_characters_of_a_face_are_its_texts_resolved_and_a_space(face: &str, expected: &str) {
        let board = board();
        let chars: String = chars_in(&board, silk(&board), face).into_iter().collect();
        assert_eq!(chars, expected);
    }

    #[test_case("Consolas", 1.5; "the tallest of two")]
    #[test_case("Nothing", 0.0; "none is zero")]
    fn the_tallest_text_of_a_face(face: &str, expected: f64) {
        let board = board();
        assert!((max_height(&board, silk(&board), face) - expected).abs() < 1e-9);
    }

    #[test]
    fn the_stroke_board_hides_the_texts_of_the_system_faces_and_nothing_else() {
        let board = board();
        let stroke = stroke_board(&board, &BTreeSet::from(["Consolas".to_owned()]));
        let visible: Vec<&str> = texts_on(&stroke, silk(&stroke))
            .map(|text| text.text.as_str())
            .collect();
        assert_eq!(visible, ["stroke", "${REFERENCE}!"]);
        assert_eq!(stroke_board(&board, &BTreeSet::new()), board);
    }

    fn placement_at_anchor() -> Placement {
        Placement {
            size: Size {
                width: 1.0,
                height: 1.0,
            },
            pen: 0.15,
            italic: false,
            justify: Justify::default(),
            position: Point { x: 10.0, y: 20.0 },
            angle: 0.0,
        }
    }

    const METRICS: FaceMetrics = FaceMetrics {
        ascent: 0.75,
        descent: 0.25,
    };

    /// Ink that fills the whole of a cell 0.5 em wide in [`METRICS`].
    fn whole_cell(_: char) -> Ink {
        Ink::whole_cell(0.5, METRICS)
    }

    /// Two glyphs 0.5 em wide in a 256 texel atlas at 64 px per em: cells
    /// of 32 by 64 at (1, 1) and (34, 1).
    fn glyphs() -> Glyphs {
        Plan {
            px: 64,
            wanted: 64,
            packing: Packing {
                width: 256,
                height: 256,
                rects: vec![
                    Rect {
                        x: 1,
                        y: 1,
                        width: 32,
                        height: 64,
                    },
                    Rect {
                        x: 34,
                        y: 1,
                        width: 32,
                        height: 64,
                    },
                ],
            },
        }
        .glyphs(&['A', 'B'], &[0.5, 0.5], 1.0)
    }

    #[test]
    fn a_knockouts_quads_sample_each_cell_inverted_and_strips_the_empty_corner() {
        let text = layout(
            "A B",
            &placement_at_anchor(),
            METRICS,
            |_| 0.5,
            whole_cell,
            true,
        );
        let mut quads = Quads::default();
        let color = [0.5, 0.6, 0.7, 1.0];
        quads.add(&text, &glyphs(), Point { x: 10.0, y: 20.0 }, color, true);
        // Two cells, one blank and four strips around the single line.
        assert_eq!(text.strips.len(), 5);
        assert_eq!(quads.vertices.len(), 7 * 4);
        assert_eq!(quads.indices.len(), 7 * 6);
        let a = quads.vertices.first().expect("a vertex");
        assert_eq!(a.color, color);
        assert_eq!(a.invert, 1.0, "a knockout's cells invert");
        assert!((a.uv[0] - 1.0 / 256.0).abs() < 1e-6 && (a.uv[1] - 1.0 / 256.0).abs() < 1e-6);
        let a_bottom_right = quads.vertices.get(2).expect("a vertex");
        assert!((a_bottom_right.uv[0] - 33.0 / 256.0).abs() < 1e-6);
        assert!((a_bottom_right.uv[1] - 65.0 / 256.0).abs() < 1e-6);
        // The position is relative to the origin: the cell "A" starts
        // 1.5 cells left of the anchor.
        assert!(
            (f64::from(a.position[0]) + 1.5 * 0.5 * 1.4).abs() < 1e-6,
            "{a:?}"
        );
        let strip = quads.vertices.get(8).expect("a strip vertex");
        assert!((strip.uv[0] - 0.5 / 256.0).abs() < 1e-6);
        assert_eq!(strip.invert, 1.0);
    }

    #[test]
    fn a_knockouts_cell_cropped_to_its_box_samples_the_same_part_of_its_glyph() {
        // The ink stands 0.25 em above the baseline and nothing below it,
        // 0.05 em in from either side of the cell: the box reaches a ninth
        // of a millimetre past it, so the cell is cropped above and below
        // and its quad samples only those rows of the glyph's rectangle.
        let ink = |_| Ink {
            left: -0.05,
            right: 0.45,
            ascent: 0.25,
            descent: 0.0,
        };
        let text = layout("A", &placement_at_anchor(), METRICS, |_| 0.5, ink, true);
        let mut quads = Quads::default();
        quads.add(&text, &glyphs(), Point::default(), [1.0; 4], true);
        let top_left = quads.vertices.first().expect("a vertex");
        let bottom_right = quads.vertices.get(2).expect("a vertex");
        // The cell is 1.4 mm tall, its baseline 0.75 of it down; the box
        // runs from a ninth above the ink's 0.35 mm to a ninth below the
        // baseline.
        let margin = 1.0 / 9.0 / 1.4;
        let (top, bottom) = (0.5 - margin, 0.75 + margin);
        assert!((f64::from(top_left.uv[1]) - (1.0 + top * 64.0) / 256.0).abs() < 1e-6);
        assert!((f64::from(bottom_right.uv[1]) - (1.0 + bottom * 64.0) / 256.0).abs() < 1e-6);
        // Across, the margin reaches past what the ink leaves of the cell,
        // so the whole width is kept.
        assert!((top_left.uv[0] - 1.0 / 256.0).abs() < 1e-6);
        assert!((bottom_right.uv[0] - 33.0 / 256.0).abs() < 1e-6);
    }

    #[test]
    fn a_plain_texts_quads_are_its_cells_upright() {
        let plain = layout(
            "A",
            &placement_at_anchor(),
            METRICS,
            |_| 0.5,
            whole_cell,
            false,
        );
        let mut quads = Quads::default();
        quads.add(&plain, &glyphs(), Point::default(), [1.0; 4], false);
        assert_eq!(quads.vertices.len(), 4);
        assert!(quads.vertices.iter().all(|v| v.invert == 0.0));
    }

    #[test]
    fn a_character_the_atlas_lacks_draws_nothing() {
        let missing = OutlineText {
            cells: vec![Cell {
                c: 'Z',
                corners: [Point::default(); 4],
                crop: WHOLE_CELL,
            }],
            strips: Vec::new(),
            knockout_box: None,
        };
        let mut quads = Quads::default();
        quads.add(&missing, &glyphs(), Point::default(), [1.0; 4], false);
        assert_is_empty!(quads.vertices);
    }

    #[test]
    fn a_glyph_samples_its_exact_advance_not_the_rounded_cell() {
        // 0.3 em at 64 px is 19.2 texels, packed in a 20 texel cell; the
        // quad on the board is 0.3 em wide, so it samples 19.2.
        let plan = Plan {
            px: 64,
            wanted: 128,
            packing: Packing {
                width: 256,
                height: 256,
                rects: vec![Rect {
                    x: 1,
                    y: 1,
                    width: 20,
                    height: 64,
                }],
            },
        };
        let glyphs = plan.glyphs(&['i'], &[0.3], 1.0);
        let sample = glyphs.by_char.get(&'i').copied().expect("the glyph");
        assert!((sample.width - 19.2).abs() < 1e-9 && (sample.height - 64.0).abs() < 1e-9);
        assert!((sample.x - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_vertex_layout_matches_the_struct() {
        assert_eq!(TextVertex::LAYOUT.array_stride, 36);
        assert_eq!(TextVertex::LAYOUT.attributes.len(), 4);
        assert_eq!(TextVertex::LAYOUT.attributes[1].offset, 8);
        assert_eq!(TextVertex::LAYOUT.attributes[2].offset, 16);
        assert_eq!(TextVertex::LAYOUT.attributes[3].offset, 32);
    }
}
