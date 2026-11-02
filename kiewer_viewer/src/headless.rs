//! Headless rendering: the viewer's frames on a machine with no display
//! and no GPU, through wgpu's Vulkan backend on whatever driver the
//! loader finds, which on CI and in the server image is Mesa's lavapipe
//! (ADR 0004). A [`Headless`] is an adapter, a device and a [`Renderer`]
//! on the offscreen format; [`Headless::render`] draws one view of a
//! scene as a [`Frame`] places it and reads the pixels back. [`hand_off`]
//! lays out the reference pages of `viewer-drop/exports/png`, so the
//! golden-image test and the `kiewer_render` executable draw the same
//! pixels from the same function.
//!
//! Nothing is drawn that the browser would not draw with the stroke font:
//! no named-face text (the page rasterises those from its own fonts) and
//! no labels, which the frame has no batch of.

use kicad_core::model::Point;

use crate::camera::{Camera, Size};
use crate::error::{Error, Result};
use crate::render::offscreen::{Image, Offscreen};
use crate::render::{Backend, Draw, MSAA_SAMPLES, Renderer, device_descriptor};
use crate::scene::{Scene, clear_color};
use crate::view::ViewState;

/// Where a frame looks: its size in pixels, the world point at its
/// centre, its scale in pixels per millimetre, and whether it is seen
/// from the back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub center: Point,
    pub scale: f64,
    pub mirrored: bool,
}

/// A device without a surface, and the renderer on it.
pub struct Headless {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    info: wgpu::AdapterInfo,
}

impl Headless {
    /// An adapter over Vulkan, a software one (lavapipe) before any
    /// other so the pixels are the same on every machine, and a device
    /// at the viewer's limits.
    ///
    /// # Errors
    ///
    /// [`Error::NoAdapter`] when the Vulkan loader finds no driver,
    /// [`Error::Device`] when the adapter refuses the viewer's limits.
    pub fn new() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapters =
            futures_executor::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
        let adapter = adapters
            .into_iter()
            .min_by_key(|adapter| match adapter.get_info().device_type {
                wgpu::DeviceType::Cpu => 0,
                _ => 1,
            })
            .ok_or_else(|| Error::NoAdapter {
                reasons: vec![
                    "Vulkan: no adapter: the loader found no driver (on Ubuntu, apt install mesa-vulkan-drivers gives it lavapipe)"
                        .to_owned(),
                ],
            })?;
        let info = adapter.get_info();
        // The viewer's limits, the WebGL2 floor, cap a texture at 2048
        // pixels on a side, and a page at the hand-off's density is wider
        // than that; the frame is the one texture that grows with the
        // page, so its limit is the adapter's own. Everything else stays
        // at the floor: the shaders and the buffers are the browser's.
        let mut descriptor = device_descriptor();
        descriptor.required_limits.max_texture_dimension_2d =
            adapter.limits().max_texture_dimension_2d;
        let (device, queue) = futures_executor::block_on(adapter.request_device(&descriptor))
            .map_err(|source| Error::Device {
                backend: Backend::Vulkan,
                source,
            })?;
        let samples = if adapter
            .get_texture_format_features(Offscreen::FORMAT)
            .flags
            .sample_count_supported(MSAA_SAMPLES)
        {
            MSAA_SAMPLES
        } else {
            1
        };
        let renderer = Renderer::new(&device, Offscreen::FORMAT, samples);
        Ok(Self {
            device,
            queue,
            renderer,
            info,
        })
    }

    /// The adapter drawing: its name and its kind (`Cpu` for lavapipe).
    #[must_use]
    pub fn adapter(&self) -> &wgpu::AdapterInfo {
        &self.info
    }

    /// Samples per pixel the frames are drawn with.
    #[must_use]
    pub fn sample_count(&self) -> u32 {
        self.renderer.sample_count()
    }

    /// Draw `scene` under `state` into `frame` and read it back.
    ///
    /// # Errors
    ///
    /// The readback's ([`Offscreen::read`]).
    pub fn render(&mut self, scene: &Scene, state: &ViewState, frame: &Frame) -> Result<Image> {
        let draws = scene.draw_list(state, Offscreen::FORMAT);
        self.render_draws(scene, &draws, frame)
    }

    /// Draw `scene`'s geometry as `draws` say (the list [`Scene::draw_list`]
    /// gives, or one a test has recoloured) into `frame` and read it back.
    ///
    /// # Errors
    ///
    /// [`Error::FrameTooLarge`] for a frame wider or taller than the
    /// adapter's textures go; the readback's ([`Offscreen::read`]).
    pub fn render_draws(&mut self, scene: &Scene, draws: &[Draw], frame: &Frame) -> Result<Image> {
        let limit = self.device.limits().max_texture_dimension_2d;
        if frame.width > limit || frame.height > limit {
            return Err(Error::FrameTooLarge {
                width: frame.width,
                height: frame.height,
                limit,
            });
        }
        self.renderer
            .upload(&self.device, &scene.vertices, &scene.indices);
        let mut camera = Camera::new(Size {
            width: f64::from(frame.width.max(1)),
            height: f64::from(frame.height.max(1)),
        });
        camera.set_mirrored(frame.mirrored);
        camera.look_at(frame.center, frame.scale);
        let offscreen = Offscreen::new(&self.device, frame.width, frame.height);
        self.renderer.render(
            &self.device,
            &self.queue,
            &offscreen.target(clear_color(Offscreen::FORMAT)),
            camera.view(scene.origin),
            draws,
            false,
        );
        offscreen.read(&self.device, &self.queue)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::render::Rgb;
    use crate::view::palette;

    const BOARD_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_pcb"
    );

    fn hand_off_scene() -> &'static Scene {
        static SCENE: OnceLock<Scene> = OnceLock::new();
        SCENE.get_or_init(|| {
            let source =
                std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo");
            let board = kicad_core::parse_board(&source).expect("the hand-off board parses");
            Scene::build(&board).expect("a board with an outline")
        })
    }

    /// A headless renderer, or `None` with the reason printed when the
    /// box has no Vulkan driver; on CI (`CI` set) that is a failure, since
    /// the runner installs lavapipe.
    fn headless_or_skip() -> Option<Headless> {
        match Headless::new() {
            Ok(headless) => Some(headless),
            Err(error) => {
                assert!(
                    std::env::var_os("CI").is_none(),
                    "CI has lavapipe installed, so this is a failure, not a skip: {error}"
                );
                eprintln!("skipped: {error}");
                None
            }
        }
    }

    #[test]
    fn a_frame_reads_back_the_page_colour_and_a_draw_in_its_colour() {
        let Some(mut headless) = headless_or_skip() else {
            return;
        };
        let scene = hand_off_scene();
        let frame = Frame {
            width: 300,
            height: 170,
            center: scene.bounds.center(),
            scale: 2.0,
            mirrored: false,
        };
        let empty = headless.render_draws(scene, &[], &frame).expect("renders");
        assert_eq!((empty.width, empty.height), (300, 170));
        let Rgb(r, g, b) = palette::PAGE;
        assert!(
            empty
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| *px == [r, g, b, 255]),
            "no draws is the page colour everywhere"
        );
        // The board fill in an opaque red: the middle of the frame is
        // red, the corner outside the board is still the page.
        let fill = scene
            .slices
            .iter()
            .find(|slice| slice.layer == crate::view::Layer::Board)
            .expect("the hand-off board has a fill");
        let draws = [crate::render::Draw {
            range: fill.range.clone(),
            color: [1.0, 0.0, 0.0, 1.0],
        }];
        let red = headless
            .render_draws(scene, &draws, &frame)
            .expect("renders");
        assert_eq!(red.pixel(150, 85), Some([255, 0, 0, 255]));
        assert_eq!(red.pixel(2, 2), Some([r, g, b, 255]));
        // The board is 112 by 58 mm at 2 px/mm: 224 by 116 pixels, so
        // the edge is red at 40 px in from the frame's left edge and page
        // at 36.
        assert_eq!(red.pixel(40, 85), Some([255, 0, 0, 255]));
        assert_eq!(red.pixel(36, 85), Some([r, g, b, 255]));
        // Mirrored, the same: the board is symmetric about its centre.
        let mirrored = headless
            .render_draws(
                scene,
                &draws,
                &Frame {
                    mirrored: true,
                    ..frame
                },
            )
            .expect("renders");
        assert_eq!(mirrored.pixel(40, 85), Some([255, 0, 0, 255]));
        assert_eq!(mirrored.pixel(36, 85), Some([r, g, b, 255]));
    }

    #[test]
    fn the_adapter_is_the_software_one_when_the_box_has_it() {
        let Some(headless) = headless_or_skip() else {
            return;
        };
        let info = headless.adapter();
        assert!(!info.name.is_empty());
        if info.device_type == wgpu::DeviceType::Cpu {
            assert!(
                info.name.to_lowercase().contains("llvmpipe"),
                "lavapipe reports itself as llvmpipe: {}",
                info.name
            );
        }
        assert!(matches!(headless.sample_count(), 1 | MSAA_SAMPLES));
    }
}

/// The hand-off's pages (`viewer-drop/tools/compose_svg.py` and
/// `make-svgs.sh`): a board's views tiled on a page with a margin, a
/// header band over each tile and a gap between them, rasterised at a
/// density in pixels per millimetre the way rsvg-convert did it (the
/// width in whole pixels, the height rounded up to keep the aspect).
/// `exports/png/NN-<view>.png` is one tile per page at 30 px/mm;
/// `exports/png/<board>-layers.png` is every view in two columns at
/// 16 px/mm.
pub mod hand_off {
    use kicad_core::geometry::Bounds;
    use kicad_core::model::Point;

    use super::{Frame, Headless};
    use crate::error::Result;
    use crate::render::Rgb;
    use crate::render::offscreen::Image;
    use crate::scene::Scene;
    use crate::view::{ViewState, palette};

    /// The page margin, in millimetres.
    pub const MARGIN: f64 = 6.0;
    /// The gap between tiles, in millimetres.
    pub const GAP: f64 = 8.0;
    /// The header band over each tile, in millimetres; the hand-off
    /// writes the view's title there, this renderer leaves it blank.
    pub const HEADER: f64 = 7.0;
    /// Tiles per row on the overview page.
    pub const COLUMNS: usize = 2;
    /// Pixels per millimetre of a one-view page.
    pub const VIEW_DENSITY: f64 = 30.0;
    /// Pixels per millimetre of the overview page.
    pub const OVERVIEW_DENSITY: f64 = 16.0;

    /// A page as rasterised: its size in pixels, its scale, and where
    /// each view's tile sits on it.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Page {
        pub width: u32,
        pub height: u32,
        /// Pixels per millimetre, the density after the width was made
        /// whole.
        pub scale: f64,
        pub tiles: Vec<Tile>,
    }

    /// One view's tile: the board's box on the page in pixels, where
    /// the tile's SVG clipped it.
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

    impl Tile {
        /// The pixels whose centres are inside the box, as
        /// `(left, top, right, bottom)` with the right and bottom
        /// exclusive: what the tile draws and what a comparison reads.
        #[must_use]
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the edges are pixel coordinates on a page a few thousand pixels wide, non-negative by construction"
        )]
        pub fn pixel_box(&self) -> (u32, u32, u32, u32) {
            let left = (self.x - 0.5).ceil().max(0.0) as u32;
            let top = (self.y - 0.5).ceil().max(0.0) as u32;
            let right = (self.x + self.width - 0.5).ceil().max(0.0) as u32;
            let bottom = (self.y + self.height - 0.5).ceil().max(0.0) as u32;
            (left, top, right, bottom)
        }
    }

    /// Millimetres as the composer wrote them into the page's viewBox,
    /// to three decimals.
    fn as_written(mm: f64) -> f64 {
        (mm * 1000.0).round() / 1000.0
    }

    /// A count of tiles, rows or columns as a length factor; a board has
    /// a handful of views, so the count fits any width.
    fn count(n: usize) -> f64 {
        f64::from(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// The page tiling `views` (numbers from 1) over a board of `bounds`
    /// in [`COLUMNS`] columns (one view is one tile), at `density`
    /// pixels per millimetre.
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
        let page_width =
            as_written(2.0 * MARGIN + columns_f * board_width + (columns_f - 1.0) * GAP);
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

    /// Draw `page` of `scene`: each tile rendered as its own frame and
    /// laid on the page colour, clipped to its box as the tile's SVG
    /// clipped it.
    ///
    /// # Errors
    ///
    /// The readback's ([`Headless::render`]).
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "tile edges are pixel coordinates on a page a few thousand pixels wide, non-negative by construction"
    )]
    pub fn render_page(headless: &mut Headless, scene: &Scene, page: &Page) -> Result<Image> {
        let Rgb(r, g, b) = palette::PAGE;
        let mut rgba = [r, g, b, 255].repeat(page.width as usize * page.height as usize);
        for tile in &page.tiles {
            let Some(view) = scene.views.get(tile.view.wrapping_sub(1)) else {
                continue;
            };
            // The frame covers every pixel the box touches; its centre
            // is placed so the box's left and top edges land where the
            // layout put them, to the fraction of a pixel.
            let left = tile.x.floor();
            let top = tile.y.floor();
            let frame_width = (tile.x + tile.width).ceil() - left;
            let frame_height = (tile.y + tile.height).ceil() - top;
            let to_center_x = (left + frame_width / 2.0 - tile.x) / page.scale;
            let to_center_y = (top + frame_height / 2.0 - tile.y) / page.scale;
            let center = Point {
                x: if view.mirrored {
                    scene.bounds.max.x - to_center_x
                } else {
                    scene.bounds.min.x + to_center_x
                },
                y: scene.bounds.min.y + to_center_y,
            };
            let frame = Frame {
                width: frame_width as u32,
                height: frame_height as u32,
                center,
                scale: page.scale,
                mirrored: view.mirrored,
            };
            let image = headless.render(scene, &ViewState::new(tile.view), &frame)?;
            let (box_left, box_top, box_right, box_bottom) = tile.pixel_box();
            for y in box_top..box_bottom.min(page.height) {
                for x in box_left..box_right.min(page.width) {
                    let Some(pixel) = image.pixel(x - left as u32, y - top as u32) else {
                        continue;
                    };
                    let at = (y as usize * page.width as usize + x as usize) * 4;
                    rgba[at..at + 4].copy_from_slice(&pixel);
                }
            }
        }
        Ok(Image {
            width: page.width,
            height: page.height,
            rgba,
        })
    }

    /// A view's title as the hand-off names its file: ` + ` dropped,
    /// every other run of non-alphanumerics an underscore.
    /// `F.Cu + F.Paste` is `F_Cu_F_Paste`.
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

    /// One rendered page, its layout, and the file name the hand-off
    /// gave it.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Rendered {
        pub name: String,
        pub page: Page,
        pub image: Image,
    }

    /// Every page of the hand-off for `scene`: each view on its own
    /// page at [`VIEW_DENSITY`] as `NN-<slug>.png`, then every view on
    /// the overview page at [`OVERVIEW_DENSITY`] as `<stem>-layers.png`.
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
            let (left, top, right, bottom) = tile.pixel_box();
            assert_eq!((left, top), (180, 390));
            assert_eq!((right, bottom), (3539, 2129));
        }

        #[test]
        fn the_overview_page_is_the_hand_off_png_size_in_two_columns() {
            let page = page(&board(), &[1, 2, 3, 4, 5, 6, 7, 8], OVERVIEW_DENSITY);
            assert_eq!((page.width, page.height), (3903, 4735));
            assert_eq!(page.tiles.len(), 8);
            assert_eq!(page.tiles[0].view, 1);
            assert_eq!(page.tiles[7].view, 8);
            // The second column starts a board and a gap to the right.
            assert!(
                (page.tiles[1].x - page.tiles[0].x - (111.9886 + 8.0) * page.scale).abs() < 1e-9
            );
            assert!((page.tiles[1].y - page.tiles[0].y).abs() < 1e-9);
            // The second row starts a header, a board and a gap down.
            assert!(
                (page.tiles[2].y - page.tiles[0].y - (7.0 + 57.9882 + 8.0) * page.scale).abs()
                    < 1e-9
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
}
