//! Headless rendering: the viewer's frames on a machine with no display
//! and no GPU, through wgpu's Vulkan backend on whatever driver the
//! loader finds, which on CI and in the server image is Mesa's lavapipe
//! (ADR 0004). A [`Headless`] is an adapter, a device and a [`Renderer`]
//! on the offscreen format; [`Headless::render`] draws one view of a
//! scene as a [`Frame`] places it and reads the pixels back. [`hand_off`]
//! lays out the reference pages of `viewer-drop/exports/png`, so the
//! golden-image test and the `kiewer_render_dev` executable draw the
//! same pixels from the same function; a thumbnail is one [`Frame::fit`].
//!
//! Nothing is drawn that the browser would not draw with the stroke font:
//! no named-face text (the page rasterises those from its own fonts) and
//! no labels, which the frame has no batch of.

pub mod hand_off;

use kicad_core::geometry::Bounds;
use kicad_core::model::Point;

use crate::camera::{Camera, Size};
use crate::error::{Error, Result};
use crate::render::offscreen::{Image, Offscreen};
use crate::render::{Backend, Draw, MSAA_SAMPLES, Renderer, Vertex, device_descriptor_for};
use crate::scene::{Scene, clear_color};
use crate::schematic::SheetScene;
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

impl Frame {
    /// A frame of `width` by `height` pixels with `bounds` fitted in it
    /// as the viewer fits a board on a fresh canvas ([`Camera::fit`]:
    /// centred, the fit margin free on each side): a thumbnail.
    #[must_use]
    pub fn fit(width: u32, height: u32, bounds: Bounds) -> Self {
        let mut camera = Camera::new(Size {
            width: f64::from(width.max(1)),
            height: f64::from(height.max(1)),
        });
        camera.fit(bounds);
        Self {
            width,
            height,
            center: camera.center(),
            scale: camera.scale(),
            mirrored: false,
        }
    }
}

/// Triangles to draw, as a board scene and a sheet scene both hold
/// them: positions relative to `origin`, three indices per triangle.
#[derive(Debug, Clone, Copy)]
pub struct Mesh<'a> {
    pub origin: Point,
    pub vertices: &'a [Vertex],
    pub indices: &'a [u32],
}

impl<'a> From<&'a Scene> for Mesh<'a> {
    fn from(scene: &'a Scene) -> Self {
        Self {
            origin: scene.origin,
            vertices: &scene.vertices,
            indices: &scene.indices,
        }
    }
}

impl<'a> From<&'a SheetScene> for Mesh<'a> {
    fn from(sheet: &'a SheetScene) -> Self {
        Self {
            origin: sheet.origin,
            vertices: &sheet.vertices,
            indices: &sheet.indices,
        }
    }
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
        // A page at the hand-off's density is wider than the floor's
        // 2048 pixels; the frame is the one texture that grows with the
        // page, so its limit is the adapter's own.
        let descriptor = device_descriptor_for(&adapter);
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
    /// [`Headless::render_draws`]'s.
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
        self.render_mesh(scene.into(), clear_color(Offscreen::FORMAT), draws, frame)
    }

    /// Draw `mesh` as `draws` say over `clear` into `frame` and read it
    /// back: a board scene's triangles with its draw list, or a sheet
    /// scene's (`crate::schematic::SheetScene`) with its own draws and
    /// paper colour, which is how a thumbnail of a schematic is drawn.
    ///
    /// # Errors
    ///
    /// As [`Headless::render_draws`].
    pub fn render_mesh(
        &mut self,
        mesh: Mesh<'_>,
        clear: wgpu::Color,
        draws: &[Draw],
        frame: &Frame,
    ) -> Result<Image> {
        let limit = self.device.limits().max_texture_dimension_2d;
        if frame.width > limit || frame.height > limit {
            return Err(Error::FrameTooLarge {
                width: frame.width,
                height: frame.height,
                limit,
            });
        }
        self.renderer
            .upload(&self.device, mesh.vertices, mesh.indices);
        self.draw(mesh.origin, clear, draws, frame)
    }

    /// Draw `draws` of the geometry the renderer holds, around `origin`,
    /// over `clear` into `frame` and read it back.
    fn draw(
        &mut self,
        origin: Point,
        clear: wgpu::Color,
        draws: &[Draw],
        frame: &Frame,
    ) -> Result<Image> {
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
            &offscreen.target(clear),
            camera.view(origin),
            draws,
            false,
        );
        offscreen.read(&self.device, &self.queue)
    }
}

#[cfg(test)]
mod tests {
    use kicad_core::model::Board;
    use kiewer_assert::assert_is_not_empty;

    use super::*;
    use crate::highlight::Highlight;
    use crate::labels::LabelBatch;
    use crate::pick::Gesture;
    use crate::render::{Rgb, Splice, Vertex};
    use crate::scene::{Paint, Slice};
    use crate::view::{Layer, Side, Stack, palette};

    /// A board-shaped scene: one rectangle, the board fill, 112 by 58 mm
    /// around (106, 79) like the hand-off board's outline box, and no
    /// copper, so there is nothing to tessellate and nothing to parse.
    fn board_scene() -> Scene {
        let bounds = Bounds {
            min: Point { x: 50.0, y: 50.0 },
            max: Point { x: 162.0, y: 108.0 },
        };
        let stack = Stack { copper: Vec::new() };
        Scene {
            origin: bounds.center(),
            bounds,
            vertices: [(-56.0, -29.0), (56.0, -29.0), (56.0, 29.0), (-56.0, 29.0)]
                .map(|(x, y)| Vertex { position: [x, y] })
                .to_vec(),
            indices: vec![0, 1, 2, 0, 2, 3],
            slices: vec![Slice {
                layer: Layer::Board,
                paint: Paint::Layer,
                range: 0..6,
            }],
            views: stack.views(),
            stack,
            unfilled_zones: 0,
            labels: LabelBatch::default(),
        }
    }

    /// A headless renderer, or `None` with the reason printed when the
    /// box has no Vulkan driver; on CI (`CI` set) that is a failure, since
    /// the runner installs lavapipe. `tests/golden.rs` carries the same
    /// gate: the two cannot share it, a test support crate for the viewer
    /// alone being more than ten lines are worth.
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
    fn a_fitted_frame_centres_the_bounds_at_the_camera_fit_scale() {
        let scene = board_scene();
        let frame = Frame::fit(1200, 630, scene.bounds);
        assert_eq!((frame.width, frame.height), (1200, 630));
        assert_eq!(frame.center, scene.bounds.center());
        // The width binds: 112 mm into 90% of 1200 px.
        assert!(
            (frame.scale - 1080.0 / 112.0).abs() < 1e-9,
            "{}",
            frame.scale
        );
        assert!(!frame.mirrored);
        let square = Frame::fit(400, 400, scene.bounds);
        assert!((square.scale - 360.0 / 112.0).abs() < 1e-9);
    }

    #[test]
    fn a_frame_reads_back_the_page_colour_and_a_draw_in_its_colour() {
        let Some(mut headless) = headless_or_skip() else {
            return;
        };
        let scene = board_scene();
        let frame = Frame {
            width: 300,
            height: 170,
            center: scene.bounds.center(),
            scale: 2.0,
            mirrored: false,
        };
        let empty = headless.render_draws(&scene, &[], &frame).expect("renders");
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
        let draws = [Draw {
            range: 0..6,
            color: [1.0, 0.0, 0.0, 1.0],
            group: 0,
        }];
        let red = headless
            .render_draws(&scene, &draws, &frame)
            .expect("renders");
        assert_eq!(red.pixel(150, 85), Some([255, 0, 0, 255]));
        assert_eq!(red.pixel(2, 2), Some([r, g, b, 255]));
        // The board is 112 by 58 mm at 2 px/mm: 224 by 116 pixels, so
        // its left edge is at 38 px: red at 40, page at 36.
        assert_eq!(red.pixel(40, 85), Some([255, 0, 0, 255]));
        assert_eq!(red.pixel(36, 85), Some([r, g, b, 255]));
        // Mirrored, the same: the board is symmetric about its centre.
        let mirrored = headless
            .render_draws(
                &scene,
                &draws,
                &Frame {
                    mirrored: true,
                    ..frame
                },
            )
            .expect("renders");
        assert_eq!(mirrored.pixel(40, 85), Some([255, 0, 0, 255]));
        assert_eq!(mirrored.pixel(36, 85), Some([r, g, b, 255]));
        // A frame past the adapter's textures is refused, not a panic.
        let huge = Frame {
            width: u32::MAX,
            ..frame
        };
        assert!(matches!(
            headless.render_draws(&scene, &[], &huge),
            Err(Error::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn a_splice_past_the_buffers_grows_them_and_keeps_what_it_keeps() {
        let Some(mut headless) = headless_or_skip() else {
            return;
        };
        let scene = board_scene();
        let frame = Frame {
            width: 300,
            height: 170,
            center: scene.bounds.center(),
            scale: 2.0,
            mirrored: false,
        };
        let square = |left: f32, right: f32, base: u32, keep: (u32, u32)| Splice {
            keep_vertices: keep.0,
            keep_indices: keep.1,
            vertices: [(left, -10.0), (right, -10.0), (right, 10.0), (left, 10.0)]
                .map(|(x, y)| Vertex { position: [x, y] })
                .to_vec(),
            indices: [0, 1, 2, 0, 2, 3].map(|index| index + base).to_vec(),
        };
        let red = Rgb(255, 0, 0).for_format(Offscreen::FORMAT);
        let green = Rgb(0, 255, 0).for_format(Offscreen::FORMAT);
        let draws = [
            Draw {
                range: 0..6,
                color: palette::BOARD.for_format(Offscreen::FORMAT),
                group: 0,
            },
            Draw {
                range: 6..12,
                color: red,
                group: 1,
            },
            Draw {
                range: 12..18,
                color: green,
                group: 2,
            },
        ];
        let clear = clear_color(Offscreen::FORMAT);
        let Headless {
            device,
            queue,
            renderer,
            ..
        } = &mut headless;
        renderer.upload(device, &scene.vertices, &scene.indices);
        // The buffers hold the scene exactly: the first splice has to grow
        // them, the second fits and leaves the first where it is.
        renderer.splice(device, queue, &square(-40.0, -20.0, 4, (4, 6)));
        renderer.splice(device, queue, &square(20.0, 40.0, 8, (8, 12)));
        let image = headless
            .draw(scene.origin, clear, &draws, &frame)
            .expect("renders");
        let at = |x: u32| image.pixel(x, 85).expect("inside the frame");
        assert_eq!(at(150)[..3], [0x7a, 0x5a, 0x32], "the scene survives");
        assert_eq!(at(90)[..3], [255, 0, 0], "the first net survives");
        assert_eq!(at(210)[..3], [0, 255, 0], "the second is written");
        // Rewriting the second slot replaces it in place.
        let Headless {
            device,
            queue,
            renderer,
            ..
        } = &mut headless;
        renderer.splice(device, queue, &square(0.0, 10.0, 8, (8, 12)));
        let image = headless
            .draw(scene.origin, clear, &draws, &frame)
            .expect("renders");
        let at = |x: u32| image.pixel(x, 85).expect("inside the frame");
        assert_eq!(at(160)[..3], [0, 255, 0]);
        assert_eq!(at(210)[..3], [0x7a, 0x5a, 0x32]);
    }

    /// A two-layer board around `items`, and its scene. The views of a
    /// two-layer board: 1 is F.Cu with F.Paste, 3 the front as built, 5
    /// both coppers with the front on top.
    fn two_layers(items: &str) -> (Board, Scene) {
        let text = format!(
            r#"(kicad_pcb (version 20241229) (generator "pcbnew")
              (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user) (35 "F.Fab" user))
              {items})"#
        );
        let board = kicad_core::parse_board(&text).expect("the synthetic board parses");
        let scene = Scene::build(&board).expect("a board with copper");
        (board, scene)
    }

    /// The synthetic boards' frame: (0, 0) to (20, 20) mm at 10 px/mm.
    const SYNTHETIC_FRAME: Frame = Frame {
        width: 200,
        height: 200,
        center: Point { x: 10.0, y: 10.0 },
        scale: 10.0,
        mirrored: false,
    };

    /// The colour of `image` at the world point (`x`, `y`) in millimetres,
    /// drawn in [`SYNTHETIC_FRAME`].
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the points are inside the 20 mm frame, so the pixel is 0 to 199"
    )]
    fn at(image: &Image, x: f64, y: f64) -> [u8; 3] {
        let pixel = |world: f64| ((world - 10.0) * SYNTHETIC_FRAME.scale + 100.0) as u32;
        let rgba = image.pixel(pixel(x), pixel(y)).expect("inside the frame");
        [rgba[0], rgba[1], rgba[2]]
    }

    /// `top` at `alpha` over `under`, channel by channel: one blend, on
    /// the sRGB-encoded values the offscreen format blends.
    fn over(top: Rgb, alpha: f64, under: [f64; 3]) -> [f64; 3] {
        let Rgb(r, g, b) = top;
        let mut out = [0.0; 3];
        for ((out, top), under) in out.iter_mut().zip([r, g, b]).zip(under) {
            *out = f64::from(top) * alpha + under * (1.0 - alpha);
        }
        out
    }

    /// `rgb` as it reads where it covers everything under it.
    fn solid(rgb: Rgb) -> [f64; 3] {
        over(rgb, 1.0, [0.0; 3])
    }

    /// Within two of `expected` on every channel: the blend's rounding.
    fn assert_reads(actual: [u8; 3], expected: [f64; 3], what: &str) {
        let near = actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| (f64::from(*actual) - expected).abs() <= 2.0);
        assert!(near, "{what}: read {actual:?}, expected {expected:?}");
    }

    #[test]
    fn two_overlapping_shapes_of_one_layer_at_half_blend_once_in_the_overlap() {
        let Some(mut headless) = headless_or_skip() else {
            return;
        };
        // Two tracks crossing at (10, 10).
        let (_, scene) = two_layers(
            r#"(segment (start 4 10) (end 16 10) (width 2) (layer "F.Cu") (net "A"))
               (segment (start 10 4) (end 10 16) (width 2) (layer "F.Cu") (net "A"))"#,
        );
        let mut state = ViewState::new(1);
        state.set(&scene.views[0], Layer::Copper("F.Cu".to_owned()), Some(0.5));
        let image = headless
            .render(&scene, &state, &SYNTHETIC_FRAME)
            .expect("renders");
        let once = over(palette::F_CU, 0.5, solid(palette::PAGE));
        assert_reads(at(&image, 6.0, 10.0), once, "one track");
        assert_reads(at(&image, 10.0, 10.0), once, "where the tracks cross");
    }

    #[test]
    fn a_hole_in_a_translucent_copper_layer_reads_the_page_at_its_opacity() {
        let Some(mut headless) = headless_or_skip() else {
            return;
        };
        // A track through a via, on the board fill.
        let (_, scene) = two_layers(
            r#"(gr_rect (start 0 0) (end 20 20) (stroke (width 0.1) (type default)) (fill no) (layer "Edge.Cuts"))
               (via (at 10 10) (size 1.2) (drill 0.6) (layers "F.Cu" "B.Cu") (net "A"))
               (segment (start 4 10) (end 16 10) (width 2) (layer "F.Cu") (net "A"))"#,
        );
        // The front as built with only the board fill and F.Cu at 0.8.
        let view = &scene.views[2];
        let mut state = ViewState::new(3);
        state.set(view, Layer::Copper("F.Cu".to_owned()), Some(0.8));
        for layer in [
            Layer::Mask(Side::Front),
            Layer::Paste(Side::Front),
            Layer::Silk(Side::Front),
            Layer::Holes,
        ] {
            state.set(view, layer, None);
        }
        let image = headless
            .render(&scene, &state, &SYNTHETIC_FRAME)
            .expect("renders");
        let board = solid(palette::BOARD);
        assert_reads(
            at(&image, 6.0, 10.0),
            over(palette::COPPER, 0.8, board),
            "the track",
        );
        assert_reads(
            at(&image, 10.0, 10.0),
            over(palette::PAGE, 0.8, board),
            "the hole, with no copper in it",
        );
    }

    #[test]
    fn a_track_on_a_zone_owns_its_area_and_the_zone_around_it_reads_the_zone_opacity() {
        let Some(mut headless) = headless_or_skip() else {
            return;
        };
        let (_, scene) = two_layers(
            r#"(zone (net "A") (layer "F.Cu") (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))
                 (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20))))
               (segment (start 4 10) (end 16 10) (width 2) (layer "F.Cu") (net "A"))"#,
        );
        let mut state = ViewState::new(1);
        state.set(&scene.views[0], Layer::Copper("F.Cu".to_owned()), Some(0.8));
        let image = headless
            .render(&scene, &state, &SYNTHETIC_FRAME)
            .expect("renders");
        let page = solid(palette::PAGE);
        assert_reads(
            at(&image, 10.0, 10.0),
            over(palette::F_CU, 0.8, page),
            "the track",
        );
        // The zone opacity is 0.66 unless the link says otherwise.
        assert_reads(
            at(&image, 10.0, 16.0),
            over(palette::F_CU, 0.8 * 0.66, page),
            "the zone around it",
        );
    }

    #[test]
    fn a_lit_nets_far_side_pad_reads_the_same_on_its_zone_and_off_it() {
        let Some(mut headless) = headless_or_skip() else {
            return;
        };
        // Net A on B.Cu: a zone over the left half, one pad in it and one
        // past it. Net B, a track on F.Cu, is pinned first, so A takes a
        // generated slot and its B.Cu copper is the far side.
        let (board, scene) = two_layers(
            r#"(footprint "T" (layer "B.Cu") (at 5 10) (property "Reference" "R1" (at 0 0) (layer "F.Fab"))
                 (pad "1" smd rect (at 0 0) (size 2 2) (layers "B.Cu") (net "A")))
               (footprint "T" (layer "B.Cu") (at 15 10) (property "Reference" "R2" (at 0 0) (layer "F.Fab"))
                 (pad "1" smd rect (at 0 0) (size 2 2) (layers "B.Cu") (net "A")))
               (zone (net "A") (layer "B.Cu") (polygon (pts (xy 0 0) (xy 10 0) (xy 10 20) (xy 0 20)))
                 (filled_polygon (layer "B.Cu") (pts (xy 0 0) (xy 10 0) (xy 10 20) (xy 0 20))))
               (segment (start 12 3) (end 18 3) (width 1) (layer "F.Cu") (net "B"))"#,
        );
        let layer = |name: &str| vec![board.layer_named(name).expect("the layer")];
        let mut highlight = Highlight::new(&board);
        highlight.pick_mut().set_instant(true);
        let track = highlight
            .index()
            .hit(Point { x: 15.0, y: 3.0 }, 0.0, &layer("F.Cu"))
            .expect("net B's track");
        let pad = highlight
            .index()
            .hit(Point { x: 15.0, y: 10.0 }, 0.0, &layer("B.Cu"))
            .expect("net A's pad");
        for hit in [track, pad] {
            highlight
                .pick_mut()
                .press(Gesture::ModifiedClick, Some(hit));
        }
        let splice = highlight
            .geometry(&board, &scene)
            .expect("the lit nets' copper");
        let draws = highlight.draws(&scene, &ViewState::new(5), Offscreen::FORMAT);
        let Headless {
            device,
            queue,
            renderer,
            ..
        } = &mut headless;
        renderer.upload(device, &scene.vertices, &scene.indices);
        renderer.splice(device, queue, &splice);
        let image = headless
            .draw(
                scene.origin,
                clear_color(Offscreen::FORMAT),
                &draws,
                &SYNTHETIC_FRAME,
            )
            .expect("renders");
        let on_zone = at(&image, 5.0, 10.0);
        let off_zone = at(&image, 15.0, 10.0);
        let zone = at(&image, 5.0, 16.0);
        assert_reads(
            on_zone,
            off_zone.map(f64::from),
            "the pad on the zone against the pad off it",
        );
        assert_ne!(on_zone, zone, "the zone reads apart from the pad");
    }

    #[test]
    fn the_adapter_is_the_software_one_when_the_box_has_it() {
        let Some(headless) = headless_or_skip() else {
            return;
        };
        let info = headless.adapter();
        assert_is_not_empty!(info.name);
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
