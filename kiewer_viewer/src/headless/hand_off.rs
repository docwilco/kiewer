//! The hand-off's pages (`viewer-drop/tools/compose_svg.py` and
//! `make-svgs.sh`): a board's views tiled on a page with a margin, a
//! header band over each tile and a gap between them, rasterised at a
//! density in pixels per millimetre the way rsvg-convert did it (the
//! width in whole pixels, the height rounded up to keep the aspect).
//! `exports/png/NN-<view>.png` is one tile per page at 30 px/mm;
//! `exports/png/<board>-layers.png` is every view in two columns at
//! 16 px/mm.
//!
//! This is fixture knowledge, the layout of `viewer-drop/exports/png`:
//! the golden test (`tests/golden.rs`) compares against those files and
//! `kiewer_render_dev` writes the same pages for a look. A thumbnail
//! (ADR 0004) is one fitted [`Frame`], not a page.

use std::ops::Range;

use kicad_core::geometry::Bounds;
use kicad_core::model::Point;

use crate::error::Result;
use crate::headless::{Frame, Headless};
use crate::render::Rgb;
use crate::render::offscreen::Image;
use crate::scene::Scene;
use crate::view::{ViewState, palette};

/// The page margin, in millimetres.
pub const MARGIN: f64 = 6.0;
/// The gap between tiles, in millimetres.
pub const GAP: f64 = 8.0;
/// The header band over each tile, in millimetres; the hand-off writes
/// the view's title there, this renderer leaves it blank.
pub const HEADER: f64 = 7.0;
/// Tiles per row on the overview page.
pub const COLUMNS: usize = 2;
/// Pixels per millimetre of a one-view page.
pub const VIEW_DENSITY: f64 = 30.0;
/// Pixels per millimetre of the overview page.
pub const OVERVIEW_DENSITY: f64 = 16.0;

/// A page as rasterised: its size in pixels, its scale, and where each
/// view's tile sits on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub width: u32,
    pub height: u32,
    /// Pixels per millimetre, the density after the width was made
    /// whole.
    pub scale: f64,
    pub tiles: Vec<Tile>,
}

/// One view's tile: the board's box on the page in pixels, where the
/// tile's SVG clipped it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    /// The view, numbered from 1.
    pub view: usize,
    /// The box's left edge in pixels from the page's left.
    pub x: f64,
    /// The box's top edge in pixels from the page's top.
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A box of whole pixels on a page, the right and bottom edges
/// exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelBox {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl PixelBox {
    /// The columns the box spans.
    #[must_use]
    pub fn columns(&self) -> Range<u32> {
        self.left..self.right
    }

    /// The rows the box spans.
    #[must_use]
    pub fn rows(&self) -> Range<u32> {
        self.top..self.bottom
    }
}

impl Tile {
    /// The pixels whose centres are inside the box: what the tile draws
    /// and what a comparison reads.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the edges are pixel coordinates on a page a few thousand pixels wide, non-negative by construction"
    )]
    pub fn pixel_box(&self) -> PixelBox {
        let edge = |at: f64| (at - 0.5).ceil().max(0.0) as u32;
        PixelBox {
            left: edge(self.x),
            top: edge(self.y),
            right: edge(self.x + self.width),
            bottom: edge(self.y + self.height),
        }
    }

    /// The page pixel the tile's frame starts at: the whole pixel at
    /// the box's left and top.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the edges are pixel coordinates on a page a few thousand pixels wide, non-negative by construction"
    )]
    pub fn origin(&self) -> (u32, u32) {
        (
            self.x.floor().max(0.0) as u32,
            self.y.floor().max(0.0) as u32,
        )
    }

    /// The frame that draws this tile of `scene` at `scale`: it covers
    /// every pixel the box touches from [`Tile::origin`], and its centre
    /// is placed so the board's edges land where the layout put them,
    /// to the fraction of a pixel; `mirrored` as the view is, in which
    /// case the board's right edge is at the box's left.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a frame is a few thousand pixels on a side, positive by construction"
    )]
    pub fn frame(&self, scene: &Scene, scale: f64, mirrored: bool) -> Frame {
        let left = self.x.floor();
        let top = self.y.floor();
        let width = (self.x + self.width).ceil() - left;
        let height = (self.y + self.height).ceil() - top;
        let to_center_x = (left + width / 2.0 - self.x) / scale;
        let to_center_y = (top + height / 2.0 - self.y) / scale;
        Frame {
            width: width as u32,
            height: height as u32,
            center: Point {
                x: if mirrored {
                    scene.bounds.max.x - to_center_x
                } else {
                    scene.bounds.min.x + to_center_x
                },
                y: scene.bounds.min.y + to_center_y,
            },
            scale,
            mirrored,
        }
    }
}

/// Millimetres as the composer wrote them into the page's viewBox, to
/// three decimals.
fn as_written(mm: f64) -> f64 {
    (mm * 1000.0).round() / 1000.0
}

/// A count of tiles, rows or columns as a length factor; a board has a
/// handful of views, so the count fits any width.
fn count(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The page tiling `views` (numbers from 1) over a board of `bounds` in
/// [`COLUMNS`] columns (one view is one tile), at `density` pixels per
/// millimetre.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a page is a few thousand pixels on a side, positive by construction"
)]
pub fn page(bounds: &Bounds, views: &[usize], density: f64) -> Page {
    let (board_width, board_height) = (bounds.width(), bounds.height());
    let columns = views.len().clamp(1, COLUMNS);
    let rows = views.len().div_ceil(columns).max(1);
    let (columns_f, rows_f) = (count(columns), count(rows));
    let page_width = as_written(2.0 * MARGIN + columns_f * board_width + (columns_f - 1.0) * GAP);
    let page_height =
        as_written(2.0 * MARGIN + rows_f * (HEADER + board_height) + (rows_f - 1.0) * GAP);
    let width = (page_width * density).floor().max(1.0);
    let scale = width / page_width;
    let height = (page_height * scale).ceil().max(1.0);
    let tiles = views
        .iter()
        .enumerate()
        .map(|(index, view)| {
            let (row, column) = (count(index / columns), count(index % columns));
            Tile {
                view: *view,
                x: (MARGIN + column * (board_width + GAP)) * scale,
                y: (MARGIN + row * (HEADER + board_height + GAP) + HEADER) * scale,
                width: board_width * scale,
                height: board_height * scale,
            }
        })
        .collect();
    Page {
        width: width as u32,
        height: height as u32,
        scale,
        tiles,
    }
}

/// Lay `page` out: each tile's frame as `render_tile` draws it, laid on
/// the page colour and clipped to the tile's box as the tile's SVG
/// clipped it. A tile whose render is `None` (a view the scene lacks)
/// stays page colour.
///
/// # Errors
///
/// Whatever `render_tile` fails with.
pub fn compose(
    page: &Page,
    mut render_tile: impl FnMut(&Tile) -> Result<Option<Image>>,
) -> Result<Image> {
    let Rgb(r, g, b) = palette::PAGE;
    let mut out = Image::filled(page.width, page.height, [r, g, b, 255]);
    for tile in &page.tiles {
        let Some(image) = render_tile(tile)? else {
            continue;
        };
        let (left, top) = tile.origin();
        let pixels = tile.pixel_box();
        for y in pixels.rows() {
            for x in pixels.columns() {
                if let Some(pixel) = image.pixel(x - left, y - top) {
                    out.put(x, y, pixel);
                }
            }
        }
    }
    Ok(out)
}

/// Draw `page` of `scene`: each tile its view, rendered as its own frame
/// ([`Tile::frame`]) and composed ([`compose`]).
///
/// # Errors
///
/// The readback's ([`Headless::render`]).
pub fn render_page(headless: &mut Headless, scene: &Scene, page: &Page) -> Result<Image> {
    compose(page, |tile| {
        let Some(view) = scene.views.get(tile.view.wrapping_sub(1)) else {
            return Ok(None);
        };
        let frame = tile.frame(scene, page.scale, view.mirrored);
        headless
            .render(scene, &as_composed(tile.view), &frame)
            .map(Some)
    })
}

/// View `view` as the hand-off composes it: its zones drawn like its
/// tracks, at zone opacity 1, where the viewer's default sets them
/// apart (GLOSSARY.md, Zone opacity).
#[must_use]
pub fn as_composed(view: usize) -> ViewState {
    let mut state = ViewState::new(view);
    state.set_zone_opacity(1.0);
    state
}

/// A view's title as the hand-off names its file: ` + ` dropped, every
/// other run of non-alphanumerics an underscore. `F.Cu + F.Paste` is
/// `F_Cu_F_Paste`.
#[must_use]
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut pending = false;
    for c in title.replace(" + ", "+").chars() {
        if c.is_ascii_alphanumeric() {
            if pending && !out.is_empty() {
                out.push('_');
            }
            pending = false;
            out.push(c);
        } else {
            pending = true;
        }
    }
    out
}

/// One rendered page, its layout, and the file name the hand-off gave
/// it.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub name: String,
    pub page: Page,
    pub image: Image,
}

/// Every page of the hand-off for `scene`: each view on its own page at
/// [`VIEW_DENSITY`] as `NN-<slug>.png`, then every view on the overview
/// page at [`OVERVIEW_DENSITY`] as `<stem>-layers.png`.
///
/// # Errors
///
/// The readback's ([`Headless::render`]).
pub fn render_all(headless: &mut Headless, scene: &Scene, stem: &str) -> Result<Vec<Rendered>> {
    let mut out = Vec::new();
    for (index, view) in scene.views.iter().enumerate() {
        let number = index + 1;
        let page = page(&scene.bounds, &[number], VIEW_DENSITY);
        out.push(Rendered {
            name: format!("{number:02}-{}.png", slug(&view.title)),
            image: render_page(headless, scene, &page)?,
            page,
        });
    }
    let all: Vec<usize> = (1..=scene.views.len()).collect();
    let page = page(&scene.bounds, &all, OVERVIEW_DENSITY);
    out.push(Rendered {
        name: format!("{stem}-layers.png"),
        image: render_page(headless, scene, &page)?,
        page,
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    /// The hand-off board's outline box.
    fn board() -> Bounds {
        Bounds {
            min: Point { x: 50.0, y: 50.0 },
            max: Point {
                x: 161.9886,
                y: 107.9882,
            },
        }
    }

    #[test]
    fn a_view_page_is_the_hand_off_png_size_with_the_board_past_the_margin_and_header() {
        let page = page(&board(), &[5], VIEW_DENSITY);
        assert_eq!((page.width, page.height), (3719, 2310));
        assert!(
            (page.scale - 3719.0 / 123.989).abs() < 1e-9,
            "{}",
            page.scale
        );
        assert_eq!(page.tiles.len(), 1);
        let tile = &page.tiles[0];
        assert_eq!(tile.view, 5);
        assert!((tile.x - 6.0 * page.scale).abs() < 1e-9);
        assert!((tile.y - 13.0 * page.scale).abs() < 1e-9);
        assert!((tile.width - 111.9886 * page.scale).abs() < 1e-9);
        assert_eq!(
            tile.pixel_box(),
            PixelBox {
                left: 180,
                top: 390,
                right: 3539,
                bottom: 2129
            }
        );
        assert_eq!(tile.origin(), (179, 389));
    }

    #[test]
    fn the_overview_page_is_the_hand_off_png_size_in_two_columns() {
        let page = page(&board(), &[1, 2, 3, 4, 5, 6, 7, 8], OVERVIEW_DENSITY);
        assert_eq!((page.width, page.height), (3903, 4735));
        assert_eq!(page.tiles.len(), 8);
        assert_eq!(page.tiles[0].view, 1);
        assert_eq!(page.tiles[7].view, 8);
        // The second column starts a board and a gap to the right.
        assert!((page.tiles[1].x - page.tiles[0].x - (111.9886 + 8.0) * page.scale).abs() < 1e-9);
        assert!((page.tiles[1].y - page.tiles[0].y).abs() < 1e-9);
        // The second row starts a header, a board and a gap down.
        assert!(
            (page.tiles[2].y - page.tiles[0].y - (7.0 + 57.9882 + 8.0) * page.scale).abs() < 1e-9
        );
        assert!((page.tiles[2].x - page.tiles[0].x).abs() < 1e-9);
    }

    #[test]
    fn an_odd_number_of_views_leaves_the_last_row_short_and_one_view_is_one_column() {
        let three = page(&board(), &[1, 2, 3], OVERVIEW_DENSITY);
        assert_eq!(three.tiles.len(), 3);
        assert!((three.tiles[2].x - three.tiles[0].x).abs() < 1e-9);
        let one = page(&board(), &[1], OVERVIEW_DENSITY);
        // One column: the page is the margin, the board and the margin.
        assert!((f64::from(one.width) / one.scale - 123.989).abs() < 1e-9);
    }

    #[test]
    fn a_tile_frame_covers_its_box_and_looks_at_the_board_from_the_right_side() {
        let scene = Scene {
            origin: board().center(),
            bounds: board(),
            vertices: Vec::new(),
            indices: Vec::new(),
            slices: Vec::new(),
            stack: crate::view::Stack { copper: Vec::new() },
            views: Vec::new(),
            labels: crate::labels::LabelBatch::default(),
            unfilled_zones: 0,
            sets: Vec::new(),
        };
        let page = page(&board(), &[5], VIEW_DENSITY);
        let tile = &page.tiles[0];
        let frame = tile.frame(&scene, page.scale, false);
        // From pixel 179 to past 179.97 + 3359.3: 3361 wide; from 389 to
        // past 389.9 + 1739.6: 1741 high.
        assert_eq!((frame.width, frame.height), (3361, 1741));
        assert!((frame.scale - page.scale).abs() < 1e-12);
        // The frame's centre is half its width past its origin; the
        // board's left edge is 0.97 px past the origin, so the centre
        // is (1680.5 - 0.97) / scale mm right of the board's left edge.
        let expected_x = 50.0 + (179.0 + 1680.5 - tile.x) / page.scale;
        assert!((frame.center.x - expected_x).abs() < 1e-9);
        let mirrored = tile.frame(&scene, page.scale, true);
        let expected_mirrored_x = 161.9886 - (179.0 + 1680.5 - tile.x) / page.scale;
        assert!((mirrored.center.x - expected_mirrored_x).abs() < 1e-9);
        assert!(mirrored.mirrored);
        assert!((mirrored.center.y - frame.center.y).abs() < 1e-12);
    }

    #[test]
    fn compose_lays_each_tile_on_the_page_colour_clipped_to_its_box() {
        let page = page(&board(), &[1, 2], OVERVIEW_DENSITY);
        let image = compose(&page, |tile| {
            if tile.view == 2 {
                return Ok(None);
            }
            let (left, top) = tile.origin();
            let pixels = tile.pixel_box();
            // A frame larger than the box, all white: only the box shows.
            Ok(Some(Image::filled(
                pixels.right - left + 2,
                pixels.bottom - top + 2,
                [255, 255, 255, 255],
            )))
        })
        .expect("composes");
        assert_eq!((image.width, image.height), (page.width, page.height));
        let page_colour = [0x2b, 0x2b, 0x2b, 255];
        let first = page.tiles[0].pixel_box();
        assert_eq!(image.pixel(first.left, first.top), Some([255; 4]));
        assert_eq!(
            image.pixel(first.right - 1, first.bottom - 1),
            Some([255; 4])
        );
        assert_eq!(image.pixel(first.left - 1, first.top), Some(page_colour));
        assert_eq!(image.pixel(first.right, first.top), Some(page_colour));
        assert_eq!(image.pixel(first.left, first.bottom), Some(page_colour));
        let second = page.tiles[1].pixel_box();
        assert_eq!(
            image.pixel(second.left + 5, second.top + 5),
            Some(page_colour)
        );
        assert_eq!(image.pixel(0, 0), Some(page_colour));
    }

    #[test_case("F.Cu + F.Paste", "F_Cu_F_Paste")]
    #[test_case("In1.Cu", "In1_Cu")]
    #[test_case(
        "B.Cu + B.Paste + B.Mask + B.SilkS, mirrored",
        "B_Cu_B_Paste_B_Mask_B_SilkS_mirrored"
    )]
    #[test_case(
        "F.Cu + B.Cu + F.SilkS + B.SilkS + F.CrtYd + B.CrtYd",
        "F_Cu_B_Cu_F_SilkS_B_SilkS_F_CrtYd_B_CrtYd"
    )]
    #[test_case("  odd  ", "odd")]
    fn a_title_slugs_as_the_hand_off_named_its_files(title: &str, expected: &str) {
        assert_eq!(slug(title), expected);
    }
}
