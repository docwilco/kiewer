//! The camera: where the view looks and how large a millimetre is, in f64
//! on the CPU. Every gesture is a function of screen pixels here; the GPU
//! gets a centre and a scale per frame and nothing accumulates in f32.
//!
//! Screen coordinates are device pixels with the origin at the top left,
//! y down, as the board's own frame is; so a world point is
//! `centre + (screen - viewport / 2) / scale` with no flip.

use bytemuck::{Pod, Zeroable};
use kicad_core::geometry::Bounds;
use kicad_core::model::Point;

/// The zoom factor of one `+` or `-` key press.
pub const STEP: f64 = 1.3;

/// The zoom factor of a double click, at the pointer.
pub const DOUBLE_CLICK: f64 = 2.0;

/// Wheel pixels per doubling of the scale: a mouse notch of 100 pixels is
/// 2^(1/3), a smooth trackpad scroll zooms in proportion.
pub const WHEEL_PIXELS_PER_DOUBLING: f64 = 300.0;

/// How much of the viewport the board leaves free on each side when fitted.
pub const FIT_MARGIN: f64 = 0.05;

/// The scale's range in device pixels per millimetre: far enough out to
/// see any board on any screen, far enough in for the 1000x the tracer
/// asks for and more.
const MIN_SCALE: f64 = 1e-3;
const MAX_SCALE: f64 = 1e7;

/// A view of the board: the world point at the viewport's centre, the
/// scale in device pixels per millimetre, the viewport's size in device
/// pixels, and whether the view is mirrored: seen from the back, with
/// screen x running against world x about the viewport's centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    center: Point,
    scale: f64,
    viewport: Size,
    mirrored: bool,
}

/// A size in device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

/// What the vertex shader needs, laid out as its uniform: the centre to
/// subtract (relative to the scene's origin) and the factors that take
/// millimetres to clip space, y flipped since clip space points up.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct View {
    pub center: [f32; 2],
    pub scale: [f32; 2],
}

impl Camera {
    /// A camera over a `viewport`, looking at the origin at one pixel per
    /// millimetre.
    #[must_use]
    pub fn new(viewport: Size) -> Self {
        Self {
            center: Point::default(),
            scale: 1.0,
            viewport,
            mirrored: false,
        }
    }

    /// The world point at the viewport's centre.
    #[must_use]
    pub fn center(&self) -> Point {
        self.center
    }

    /// Device pixels per millimetre.
    #[must_use]
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Whether the view is mirrored left to right.
    #[must_use]
    pub fn mirrored(&self) -> bool {
        self.mirrored
    }

    /// Mirror the view left to right, or stop: the back of the board is
    /// seen as if turned over. The centre and the scale stay, so the
    /// board flips about the viewport's middle.
    pub fn set_mirrored(&mut self, mirrored: bool) {
        self.mirrored = mirrored;
    }

    /// The viewport changed size: the centre and the scale stay, so the
    /// view grows or shrinks around its middle.
    pub fn set_viewport(&mut self, viewport: Size) {
        self.viewport = viewport;
    }

    /// Which way screen x runs along world x: against it when mirrored.
    fn x_sign(&self) -> f64 {
        if self.mirrored { -1.0 } else { 1.0 }
    }

    /// The world point under a screen point.
    #[must_use]
    pub fn screen_to_world(&self, screen: Point) -> Point {
        Point {
            x: self.center.x + self.x_sign() * (screen.x - self.viewport.width / 2.0) / self.scale,
            y: self.center.y + (screen.y - self.viewport.height / 2.0) / self.scale,
        }
    }

    /// Where a world point lands on the screen.
    #[must_use]
    pub fn world_to_screen(&self, world: Point) -> Point {
        Point {
            x: self.viewport.width / 2.0 + self.x_sign() * (world.x - self.center.x) * self.scale,
            y: self.viewport.height / 2.0 + (world.y - self.center.y) * self.scale,
        }
    }

    /// Multiply the scale by `factor`, keeping the world point under
    /// `screen` where it is: with `p` that point and `t` the centre,
    /// `t' = p - (p - t) / factor`.
    pub fn zoom_at(&mut self, screen: Point, factor: f64) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let pivot = self.screen_to_world(screen);
        let new_scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        let k = self.scale / new_scale;
        self.center = Point {
            x: pivot.x - (pivot.x - self.center.x) * k,
            y: pivot.y - (pivot.y - self.center.y) * k,
        };
        self.scale = new_scale;
    }

    /// A wheel at `screen`: `delta` pixels (positive is scrolling down,
    /// which zooms out) through an exponential, so two half-scrolls are
    /// one scroll wherever they land.
    pub fn wheel(&mut self, screen: Point, delta: f64) {
        self.zoom_at(screen, (-delta / WHEEL_PIXELS_PER_DOUBLING).exp2());
    }

    /// One zoom step at the viewport's centre: `steps` presses of `+`
    /// (positive) or `-` (negative).
    pub fn step(&mut self, steps: i32) {
        let middle = Point {
            x: self.viewport.width / 2.0,
            y: self.viewport.height / 2.0,
        };
        self.zoom_at(middle, STEP.powi(steps));
    }

    /// Move the view so what was under `from` is under `to`: a drag, or
    /// a wheel with ctrl (horizontal) or shift (vertical) held.
    pub fn pan(&mut self, from: Point, to: Point) {
        self.center = Point {
            x: self.center.x - self.x_sign() * (to.x - from.x) / self.scale,
            y: self.center.y - (to.y - from.y) / self.scale,
        };
    }

    /// Two pointers moved from `a0`, `b0` to `a1`, `b1`: the scale grows
    /// by the ratio of their distances and the world point that was under
    /// their midpoint is under the new midpoint.
    pub fn pinch(&mut self, a0: Point, b0: Point, a1: Point, b1: Point) {
        let distance = |a: Point, b: Point| (a.x - b.x).hypot(a.y - b.y);
        let mid = |a: Point, b: Point| Point {
            x: f64::midpoint(a.x, b.x),
            y: f64::midpoint(a.y, b.y),
        };
        let before = distance(a0, b0);
        let after = distance(a1, b1);
        let (m0, m1) = (mid(a0, b0), mid(a1, b1));
        if before > 0.0 && after > 0.0 {
            self.zoom_at(m0, after / before);
        }
        self.pan(m0, m1);
    }

    /// Fit `bounds` in the viewport with [`FIT_MARGIN`] free on each side,
    /// centred.
    pub fn fit(&mut self, bounds: Bounds) {
        let usable = 1.0 - 2.0 * FIT_MARGIN;
        let by_width = self.viewport.width * usable / bounds.width();
        let by_height = self.viewport.height * usable / bounds.height();
        let scale = by_width.min(by_height);
        self.scale = if scale.is_finite() && scale > 0.0 {
            scale.clamp(MIN_SCALE, MAX_SCALE)
        } else {
            1.0
        };
        self.center = bounds.center();
    }

    /// The shader's view of this camera over a scene whose positions are
    /// relative to `origin`: the centre relative to it (a small number
    /// near the board, exact enough in f32) and millimetres to clip space.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the centre is relative to the scene's origin, a few hundred millimetres at most, and the scale factors are of order 1e-3 to 1e4: both well within f32"
    )]
    pub fn view(&self, origin: Point) -> View {
        View {
            center: [
                (self.center.x - origin.x) as f32,
                (self.center.y - origin.y) as f32,
            ],
            scale: [
                (self.x_sign() * 2.0 * self.scale / self.viewport.width) as f32,
                (-2.0 * self.scale / self.viewport.height) as f32,
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: Size = Size {
        width: 1600.0,
        height: 900.0,
    };

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// The hand-off board fitted: about 112 by 58 mm, around (106, 79).
    fn fitted() -> Camera {
        let mut camera = Camera::new(VIEWPORT);
        camera.fit(Bounds {
            min: at(50.0, 50.0),
            max: at(161.9886, 107.9882),
        });
        camera
    }

    fn pixels_apart(camera: &Camera, world: Point, screen: Point) -> f64 {
        let landed = camera.world_to_screen(world);
        (landed.x - screen.x).hypot(landed.y - screen.y)
    }

    #[test]
    fn fit_centres_the_board_and_leaves_the_margin() {
        let camera = fitted();
        let middle = camera.world_to_screen(at(105.9943, 78.9941));
        assert!((middle.x - 800.0).abs() < 1e-9 && (middle.y - 450.0).abs() < 1e-9);
        // The board is wider than tall relative to the viewport, so its
        // width fills 90% of the viewport's.
        let left = camera.world_to_screen(at(50.0, 50.0));
        let right = camera.world_to_screen(at(161.9886, 107.9882));
        assert!((left.x - 80.0).abs() < 1e-9, "{left:?}");
        assert!((right.x - 1520.0).abs() < 1e-9, "{right:?}");
        assert!(left.y > 0.0 && right.y < 900.0, "the height fits inside");
    }

    #[test]
    fn a_wheel_zoom_keeps_the_world_point_under_the_cursor() {
        let mut camera = fitted();
        let cursor = at(1234.0, 123.0);
        let before = camera.screen_to_world(cursor);
        camera.wheel(cursor, -100.0);
        assert!(camera.scale() > fitted().scale(), "scrolling up zooms in");
        assert!(pixels_apart(&camera, before, cursor) < 1e-9);
        camera.wheel(cursor, 250.0);
        assert!(pixels_apart(&camera, before, cursor) < 1e-9);
    }

    #[test]
    fn a_mirrored_camera_flips_x_about_the_middle_and_keeps_every_gesture_true() {
        let mut camera = fitted();
        let left_edge = at(50.0, 79.0);
        let before = camera.world_to_screen(left_edge);
        camera.set_mirrored(true);
        let after = camera.world_to_screen(left_edge);
        // The board's left edge lands where its right edge did: at 1520.
        assert!((before.x - 80.0).abs() < 1e-9 && (after.x - 1520.0).abs() < 1e-9);
        assert!((after.y - before.y).abs() < 1e-9, "y is not mirrored");
        let scale_x = f64::from(camera.view(at(0.0, 0.0)).scale[0]);
        assert!((scale_x + 2.0 * camera.scale() / 1600.0).abs() < 1e-6);
        // A zoom keeps the world point under the cursor where it is.
        let cursor = at(300.0, 200.0);
        let under = camera.screen_to_world(cursor);
        camera.wheel(cursor, -150.0);
        assert!(pixels_apart(&camera, under, cursor) < 1e-9);
        // A drag brings what was under `from` under `to`.
        let (from, to) = (at(400.0, 400.0), at(700.0, 450.0));
        let dragged = camera.screen_to_world(from);
        camera.pan(from, to);
        assert!(pixels_apart(&camera, dragged, to) < 1e-9);
        // Back to normal puts the edge back.
        camera.set_mirrored(false);
        assert!(!camera.mirrored());
    }

    #[test]
    fn wheel_zoom_is_exponential_in_the_delta() {
        let mut halves = fitted();
        halves.wheel(at(100.0, 100.0), -150.0);
        halves.wheel(at(700.0, 300.0), -150.0);
        let mut whole = fitted();
        whole.wheel(at(400.0, 200.0), -300.0);
        assert!((halves.scale() - whole.scale()).abs() < 1e-9);
        assert!(
            (whole.scale() / fitted().scale() - 2.0).abs() < 1e-9,
            "300 pixels double"
        );
        let mut still = fitted();
        still.wheel(at(400.0, 200.0), 0.0);
        assert_eq!(still, fitted());
    }

    #[test]
    fn a_pinch_scales_by_the_finger_distance_and_follows_the_midpoint() {
        let mut camera = fitted();
        let (a0, b0) = (at(600.0, 400.0), at(800.0, 400.0));
        let under_midpoint = camera.screen_to_world(at(700.0, 400.0));
        // The fingers spread to twice the distance and drift right and down.
        let (a1, b1) = (at(550.0, 450.0), at(950.0, 450.0));
        camera.pinch(a0, b0, a1, b1);
        assert!((camera.scale() / fitted().scale() - 2.0).abs() < 1e-9);
        assert!(pixels_apart(&camera, under_midpoint, at(750.0, 450.0)) < 1e-9);
    }

    #[test]
    fn a_pan_moves_what_was_under_the_pointer_with_it() {
        let mut camera = fitted();
        let grabbed = camera.screen_to_world(at(300.0, 300.0));
        camera.pan(at(300.0, 300.0), at(420.0, 250.0));
        assert!(pixels_apart(&camera, grabbed, at(420.0, 250.0)) < 1e-9);
        assert!(
            (camera.scale() - fitted().scale()).abs() < 1e-12,
            "a pan does not zoom"
        );
    }

    #[test]
    fn a_step_zooms_at_the_viewport_centre() {
        let mut camera = fitted();
        let middle = camera.screen_to_world(at(800.0, 450.0));
        camera.step(1);
        assert!((camera.scale() / fitted().scale() - STEP).abs() < 1e-9);
        assert!(pixels_apart(&camera, middle, at(800.0, 450.0)) < 1e-9);
        camera.step(-2);
        assert!((camera.scale() / fitted().scale() - 1.0 / STEP).abs() < 1e-9);
    }

    #[test]
    fn a_resize_keeps_the_centre_and_the_scale() {
        let mut camera = fitted();
        let before = camera.screen_to_world(at(800.0, 450.0));
        camera.set_viewport(Size {
            width: 800.0,
            height: 600.0,
        });
        assert_eq!(camera.screen_to_world(at(400.0, 300.0)), before);
        assert!((camera.scale() - fitted().scale()).abs() < 1e-12);
    }

    #[test]
    fn the_scale_stays_within_its_range() {
        let mut camera = fitted();
        for _ in 0..200 {
            camera.wheel(at(10.0, 10.0), -1000.0);
        }
        assert!(camera.scale() <= MAX_SCALE && camera.scale().is_finite());
        for _ in 0..400 {
            camera.wheel(at(10.0, 10.0), 1000.0);
        }
        assert!(camera.scale() >= MIN_SCALE);
        camera.zoom_at(at(10.0, 10.0), f64::NAN);
        camera.zoom_at(at(10.0, 10.0), 0.0);
        assert!(camera.scale() >= MIN_SCALE && camera.center().x.is_finite());
    }

    /// The same zoom-at-cursor formula with the camera's state in f32, as
    /// a renderer that keeps its camera on the GPU side would have it.
    struct F32Camera {
        center: [f32; 2],
        scale: f32,
    }

    impl F32Camera {
        fn zoom_at(&mut self, screen: Point, factor: f64) {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "screen coordinates are whole pixels under 2^24 and the factor is 1.005, both exact in f32"
            )]
            let (sx, sy, factor) = (screen.x as f32, screen.y as f32, factor as f32);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the viewport is a few thousand pixels"
            )]
            let (half_w, half_h) = (
                (VIEWPORT.width / 2.0) as f32,
                (VIEWPORT.height / 2.0) as f32,
            );
            let pivot = [
                self.center[0] + (sx - half_w) / self.scale,
                self.center[1] + (sy - half_h) / self.scale,
            ];
            let new_scale = self.scale * factor;
            let k = self.scale / new_scale;
            self.center = [
                pivot[0] - (pivot[0] - self.center[0]) * k,
                pivot[1] - (pivot[1] - self.center[1]) * k,
            ];
            self.scale = new_scale;
        }

        fn world_to_screen(&self, world: Point) -> Point {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a world point is millimetres in the hundreds, which f32 holds to 1e-5 mm, below what this measurement resolves"
            )]
            let (wx, wy, half_w, half_h) = (
                world.x as f32,
                world.y as f32,
                (VIEWPORT.width / 2.0) as f32,
                (VIEWPORT.height / 2.0) as f32,
            );
            Point {
                x: f64::from(half_w + (wx - self.center[0]) * self.scale),
                y: f64::from(half_h + (wy - self.center[1]) * self.scale),
            }
        }
    }

    #[test]
    fn a_thousandfold_zoom_in_f64_holds_the_cursor_point_where_f32_lets_it_drift() {
        let mut camera = fitted();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the fitted centre and scale are millimetres in the hundreds and pixels per millimetre near 13, which f32 holds to 1e-5"
        )]
        let mut baseline = F32Camera {
            center: [camera.center().x as f32, camera.center().y as f32],
            scale: camera.scale() as f32,
        };
        // Off-centre, over the far corner of the board, in small steps:
        // 1.005^1400 is just over 1000.
        let cursor = at(1500.0, 120.0);
        let pinned = camera.screen_to_world(cursor);
        let steps = 1400;
        for _ in 0..steps {
            camera.zoom_at(cursor, 1.005);
            baseline.zoom_at(cursor, 1.005);
        }
        assert!(
            camera.scale() / fitted().scale() > 1000.0,
            "{}x",
            camera.scale() / fitted().scale()
        );
        let drift = pixels_apart(&camera, pinned, cursor);
        let landed = baseline.world_to_screen(pinned);
        let baseline_drift = (landed.x - cursor.x).hypot(landed.y - cursor.y);
        assert!(drift < 1e-6, "f64 drift {drift} px");
        assert!(
            baseline_drift > 1e-3,
            "the f32 baseline drifted only {baseline_drift} px; the comparison says nothing"
        );
    }

    #[test]
    fn the_view_is_the_centre_relative_to_the_origin_and_millimetres_to_clip_space() {
        let camera = fitted();
        let view = camera.view(at(100.0, 70.0));
        assert!((f64::from(view.center[0]) - 5.9943).abs() < 1e-4);
        assert!((f64::from(view.center[1]) - 8.9941).abs() < 1e-4);
        // The board's 111.9886 mm span 1440 of 1600 pixels: a millimetre
        // is 2 * 12.859 / 1600 of clip space horizontally, and y flips.
        let scale = 1440.0 / 111.9886;
        assert!((f64::from(view.scale[0]) - 2.0 * scale / 1600.0).abs() < 1e-7);
        assert!((f64::from(view.scale[1]) + 2.0 * scale / 900.0).abs() < 1e-7);
    }
}
