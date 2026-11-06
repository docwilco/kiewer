//! When the viewer builds the curves finer (#51, ADR 0006): the tolerance
//! the camera's scale asks for, the region it applies to, and the moments
//! a rebuild is due. This is only the decision; `Scene::refine` and
//! `SheetScene::refine` do the work.
//!
//! The tolerance is half a device pixel taken at the top of the scale's
//! power-of-four step, never coarser than the floor. The step in force
//! changes on entering a higher one (refine) or on falling two steps
//! below it (coarsen), so a zoom within a step builds nothing. The
//! region is the view plus one view on every side, centred on the view
//! when it is made. It is made again when the view leaves it, when the
//! step changes, and when a pan stops (no input for [`PAN_STOP`]) with
//! the view's centre off the region's.

use kicad_core::geometry::Bounds;
use kicad_core::tessellate::Fine;

use crate::scene::TOLERANCE;

/// How long without a drag, wheel or pinch input counts as a pan stopped,
/// in milliseconds.
pub const PAN_STOP: f64 = 250.0;

/// The highest step whose tolerance is the floor's: 4^4 = 256 pixels per
/// millimetre, where half a pixel is 1.95 micrometres and the floor, one,
/// is finer.
const FLOOR_STEP: i32 = 4;

/// The step of `scale` (device pixels per millimetre): the exponent of
/// the least power of four that holds it.
#[must_use]
pub fn step_of(scale: f64) -> i32 {
    // Walked in exact multiples of four, so a power of four is its own
    // step and not the next one by a rounding of a logarithm.
    let (mut step, mut top) = (0, 1.0_f64);
    while top < scale {
        top *= 4.0;
        step += 1;
    }
    while top / 4.0 >= scale {
        top /= 4.0;
        step -= 1;
    }
    step
}

/// The tolerance of `step`, in millimetres: half a device pixel at the
/// scale of its top, 4^step pixels per millimetre, and never coarser than
/// the floor.
#[must_use]
pub fn tolerance_of(step: i32) -> f64 {
    (0.5 / 4.0_f64.powi(step)).min(TOLERANCE)
}

/// What to build: the tolerance inside a region, or the floor everywhere
/// when there is none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    /// The finer tolerance and its region, or `None` for no refinement:
    /// what `Scene::refine` takes.
    pub fine: Option<Fine>,
}

/// The decision, kept between frames.
#[derive(Debug, Clone, PartialEq)]
pub struct Refiner {
    /// The step in force, not below [`FLOOR_STEP`].
    step: i32,
    region: Option<Bounds>,
    /// When the last drag, wheel or pinch input came, in the page's
    /// milliseconds.
    last_input: Option<f64>,
}

impl Default for Refiner {
    fn default() -> Self {
        Self {
            step: FLOOR_STEP,
            region: None,
            last_input: None,
        }
    }
}

impl Refiner {
    /// A drag, a wheel or a pinch came at `now`.
    pub fn input(&mut self, now: f64) {
        self.last_input = Some(now);
    }

    /// Forget the region: a new design was loaded, or the scene was built
    /// afresh at the floor.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether the tolerance in force is finer than the floor.
    #[must_use]
    pub fn refining(&self) -> bool {
        self.region.is_some()
    }

    /// What to build now, when anything is due: the camera shows `view`
    /// (world millimetres) at `scale` at the time `now`.
    pub fn check(&mut self, view: Bounds, scale: f64, now: f64) -> Option<Plan> {
        // The hysteresis reads the raw step: a step clamped to the floor
        // first would never fall two below a step 5 that holds.
        let wanted = step_of(scale);
        let stepped = if wanted > self.step || wanted + 2 <= self.step {
            self.step = wanted.max(FLOOR_STEP);
            true
        } else {
            false
        };
        if self.step == FLOOR_STEP {
            // The floor needs no region. Leaving a region for it is a
            // rebuild, once.
            return (stepped && self.region.take().is_some()).then_some(Plan { fine: None });
        }
        // A view that left the region is only worth a rebuild where the
        // floor would show: past 500 px/mm, where half a pixel is finer
        // than the floor. Zooming out within a step takes the view past
        // the region (a step is four times the scale, the region three
        // times the view) with nothing to see for it.
        let shows = 0.5 / scale < TOLERANCE;
        let due = stepped
            || self.region.is_none_or(|region| {
                (shows && !region.contains_bounds(&view)) || self.pan_stopped(&view, &region, now)
            });
        due.then(|| {
            // The view plus one view on every side.
            let region = view.grown(view.width(), view.height());
            self.region = Some(region);
            Plan {
                fine: Some(Fine {
                    tolerance: tolerance_of(self.step),
                    region,
                }),
            }
        })
    }

    /// Whether a pan stopped with the view off the region's centre.
    fn pan_stopped(&self, view: &Bounds, region: &Bounds, now: f64) -> bool {
        let idle = self.last_input.is_none_or(|last| now - last >= PAN_STOP);
        let (a, b) = (view.center(), region.center());
        let slack = view.width().max(view.height()) * 1e-9;
        idle && ((a.x - b.x).abs() > slack || (a.y - b.y).abs() > slack)
    }

    /// The milliseconds from `now` until a pan stop could make a rebuild
    /// due, when one is waiting for it: the view is in the region and off
    /// its centre, and input came lately. `None` when nothing waits.
    #[must_use]
    pub fn wait(&self, view: &Bounds, now: f64) -> Option<f64> {
        let region = self.region.as_ref()?;
        let last = self.last_input?;
        let left = PAN_STOP - (now - last);
        (left > 0.0 && self.pan_stopped(view, region, last + PAN_STOP)).then_some(left)
    }
}

#[cfg(test)]
mod tests {
    use kicad_core::model::Point;
    use test_case::test_case;

    use super::*;

    fn view(x: f64, y: f64, size: f64) -> Bounds {
        Bounds {
            min: Point { x, y },
            max: Point {
                x: x + size,
                y: y + size,
            },
        }
    }

    #[test_case(0.001, -4; "the camera's least scale")]
    #[test_case(1.0, 0; "one pixel per millimetre")]
    #[test_case(4.0, 1; "four")]
    #[test_case(4096.0, 6; "the top of the sixth step")]
    #[test_case(16384.0, 7; "the seventh")]
    #[test_case(65536.0, 8; "the eighth")]
    #[test_case(256.0, 4; "the top of the fourth step")]
    #[test_case(257.0, 5; "just past it")]
    #[test_case(1024.0, 5; "the top of the fifth")]
    #[test_case(1025.0, 6; "just past that")]
    #[test_case(1e5, 9; "the ceiling")]
    fn the_step_is_the_least_power_of_four_that_holds_the_scale(scale: f64, step: i32) {
        assert_eq!(step_of(scale), step);
    }

    #[test_case(4, 0.001; "at the top of 256 the floor is finer than half a pixel")]
    #[test_case(5, 0.5 / 1024.0; "half a pixel at 1024 pixels per millimetre")]
    #[test_case(6, 0.5 / 4096.0; "and at 4096")]
    #[test_case(9, 0.5 / 262_144.0; "at the ceiling's step")]
    fn the_tolerance_is_half_a_pixel_at_the_top_of_the_step_never_coarser_than_the_floor(
        step: i32,
        tolerance: f64,
    ) {
        assert!((tolerance_of(step) - tolerance).abs() < 1e-15);
    }

    #[test]
    fn nothing_is_built_at_or_below_256_pixels_per_millimetre() {
        let mut refiner = Refiner::default();
        assert_eq!(refiner.check(view(0.0, 0.0, 10.0), 256.0, 0.0), None);
        assert!(!refiner.refining());
    }

    #[test]
    fn entering_a_step_builds_the_region_around_the_view_once() {
        let mut refiner = Refiner::default();
        let here = view(100.0, 100.0, 2.0);
        let plan = refiner
            .check(here, 300.0, 0.0)
            .expect("the first refinement");
        let fine = plan.fine.expect("a region");
        assert!((fine.tolerance - 0.5 / 1024.0).abs() < 1e-15);
        // One view on every side: three views wide, centred on the view.
        assert_eq!(fine.region, view(98.0, 98.0, 6.0));
        // Zooming within the step builds nothing, in or out of the view.
        refiner.input(1.0);
        assert_eq!(refiner.check(view(100.5, 100.5, 1.0), 600.0, 1.0), None);
        refiner.input(2.0);
        assert_eq!(refiner.check(view(100.0, 100.0, 2.0), 1024.0, 2.0), None);
    }

    #[test]
    fn zooming_into_the_next_step_builds_once_and_out_only_after_two_steps() {
        let mut refiner = Refiner::default();
        refiner
            .check(view(0.0, 0.0, 2.0), 300.0, 0.0)
            .expect("step 5");
        let plan = refiner
            .check(view(0.0, 0.0, 0.5), 1500.0, 1.0)
            .expect("step 6");
        let fine = plan.fine.expect("step 6's region");
        assert!((fine.tolerance - 0.5 / 4096.0).abs() < 1e-15);
        refiner.input(2.0);
        assert_eq!(refiner.check(view(0.0, 0.0, 0.5), 1500.0, 2.0), None);
        // One step out (step 5) keeps step 6's tolerance.
        refiner.input(3.0);
        assert_eq!(refiner.check(view(0.0, 0.0, 1.0), 800.0, 3.0), None);
        // Two steps out (step 4, the floor) coarsens, once.
        let plan = refiner
            .check(view(0.0, 0.0, 4.0), 200.0, 4.0)
            .expect("the floor");
        assert_eq!(plan.fine, None);
        assert_eq!(refiner.check(view(0.0, 0.0, 4.0), 150.0, 5.0), None);
        assert!(!refiner.refining());
    }

    #[test]
    fn step_5_coarsens_to_the_floor_two_steps_below_it() {
        let mut refiner = Refiner::default();
        refiner
            .check(view(0.0, 0.0, 2.0), 300.0, 0.0)
            .expect("step 5");
        // Step 4 is one below: step 5's tolerance stays.
        assert_eq!(refiner.check(view(0.0, 0.0, 2.0), 200.0, 1.0), None);
        // Step 3 (64 px/mm) is two below: the floor, once.
        let plan = refiner
            .check(view(0.0, 0.0, 8.0), 50.0, 2.0)
            .expect("the floor");
        assert_eq!(plan.fine, None);
        assert!(!refiner.refining());
        // Panning builds nothing more.
        refiner.input(3.0);
        assert_eq!(refiner.check(view(5.0, 0.0, 8.0), 50.0, 4.0), None);
    }

    #[test]
    fn zooming_out_within_a_step_past_the_region_builds_nothing_where_the_floor_shows_nothing() {
        let mut refiner = Refiner::default();
        refiner
            .check(view(0.0, 0.0, 2.0), 1000.0, 0.0)
            .expect("step 5");
        refiner.input(1.0);
        // 300 px/mm: the view is 3.3 times as big, past the region's 3,
        // and the floor is within half a pixel of the curve still.
        assert_eq!(refiner.check(view(-1.5, -1.5, 6.6), 300.0, 1.0), None);
        // At 600 px/mm the floor shows, and the view past the region builds.
        let plan = refiner.check(view(-3.0, -3.0, 3.3), 600.0, 2.0);
        assert!(plan.is_some());
    }

    #[test]
    fn the_view_leaving_the_region_builds_a_new_one_centred_on_it() {
        let mut refiner = Refiner::default();
        refiner
            .check(view(0.0, 0.0, 2.0), 1000.0, 0.0)
            .expect("step 5");
        refiner.input(1.0);
        // The region is x from -2 to 4: a pan to 1.9 still has the view in.
        assert_eq!(refiner.check(view(1.9, 0.0, 2.0), 1000.0, 1.0), None);
        let plan = refiner
            .check(view(2.5, 0.0, 2.0), 1000.0, 2.0)
            .expect("left");
        assert_eq!(
            plan.fine.map(|fine| fine.region),
            Some(view(0.5, -2.0, 6.0))
        );
    }

    #[test]
    fn a_pan_that_stops_off_centre_builds_after_250_ms_and_a_burst_builds_once() {
        let mut refiner = Refiner::default();
        refiner
            .check(view(0.0, 0.0, 2.0), 300.0, 0.0)
            .expect("step 5");
        // A burst of pans, each inside the region.
        for (i, x) in [0.25, 0.5, 0.75, 1.0].into_iter().enumerate() {
            #[expect(clippy::cast_precision_loss, reason = "at most 3")]
            let now = 100.0 + 20.0 * i as f64;
            refiner.input(now);
            assert_eq!(refiner.check(view(x, 0.0, 2.0), 300.0, now), None);
        }
        // 249 ms after the last input: not yet.
        assert_eq!(
            refiner.check(view(1.0, 0.0, 2.0), 300.0, 160.0 + 249.0),
            None
        );
        let plan = refiner
            .check(view(1.0, 0.0, 2.0), 300.0, 160.0 + 250.0)
            .expect("the pan stopped");
        assert_eq!(
            plan.fine.map(|fine| fine.region),
            Some(view(-1.0, -2.0, 6.0))
        );
        // Centred now: nothing more.
        assert_eq!(refiner.check(view(1.0, 0.0, 2.0), 300.0, 1000.0), None);
    }

    #[test]
    fn the_wait_names_the_time_left_to_a_pan_stop_only_when_one_is_pending() {
        let mut refiner = Refiner::default();
        refiner
            .check(view(0.0, 0.0, 2.0), 300.0, 0.0)
            .expect("step 5");
        refiner.input(100.0);
        let off = view(0.5, 0.0, 2.0);
        assert_eq!(refiner.wait(&off, 150.0), Some(200.0));
        assert_eq!(refiner.wait(&off, 350.0), None, "due: check builds it");
        let centred = view(0.0, 0.0, 2.0);
        assert_eq!(refiner.wait(&centred, 150.0), None, "nothing waits");
    }
}
