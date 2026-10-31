//! Plane geometry over the model's points: the frame a footprint places
//! its pads in, arcs given by three points, and bounding boxes. Pure
//! functions in f64; the tessellator and the viewer's camera build on them.
//!
//! Angles follow KiCad: a positive rotation turns counter-clockwise as
//! seen on the screen, where y points down. In the file's coordinates
//! that is a rotation by minus the angle.

use std::f64::consts::{PI, TAU};

use crate::model::Point;

/// A rotation about the origin followed by a translation: the frame of a
/// footprint (its `at` and `rotation`) or of a pad's shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    cos: f64,
    sin: f64,
    origin: Point,
}

impl Transform {
    /// The frame at `origin` turned by `degrees`, KiCad's sense.
    #[must_use]
    pub fn new(origin: Point, degrees: f64) -> Self {
        let (sin, cos) = (-degrees).to_radians().sin_cos();
        Self { cos, sin, origin }
    }

    /// The frame that changes nothing.
    #[must_use]
    pub fn identity() -> Self {
        Self::new(Point::default(), 0.0)
    }

    /// A point of the frame, on the board.
    #[must_use]
    pub fn apply(&self, point: Point) -> Point {
        let turned = self.rotate(point);
        Point {
            x: turned.x + self.origin.x,
            y: turned.y + self.origin.y,
        }
    }

    /// A vector of the frame, turned but not moved: an offset, a size's
    /// direction.
    #[must_use]
    pub fn rotate(&self, vector: Point) -> Point {
        Point {
            x: vector.x * self.cos - vector.y * self.sin,
            y: vector.x * self.sin + vector.y * self.cos,
        }
    }

    /// The frame's origin on the board.
    #[must_use]
    pub fn origin(&self) -> Point {
        self.origin
    }

    /// The frame `inner`, given in this frame, as a frame on the board: a
    /// pad's shape frame inside its footprint's.
    #[must_use]
    pub fn then(&self, inner: Transform) -> Transform {
        Transform {
            cos: self.cos * inner.cos - self.sin * inner.sin,
            sin: self.sin * inner.cos + self.cos * inner.sin,
            origin: self.apply(inner.origin),
        }
    }
}

/// A circular arc: the centre, the radius, where it starts and how far it
/// goes. Angles are radians in the file's frame (`atan2(dy, dx)`, so a
/// positive sweep turns clockwise on the screen).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Arc {
    /// The centre.
    pub center: Point,
    /// The radius.
    pub radius: f64,
    /// The angle of the start point.
    pub start_angle: f64,
    /// The signed angle from the start to the end, through the mid point.
    pub sweep: f64,
}

/// Below this, three points are taken as collinear and give no arc.
const COLLINEAR: f64 = 1e-12;

impl Arc {
    /// The arc from `start` through `mid` to `end`, or `None` when the
    /// three are collinear (a line, not an arc).
    #[must_use]
    pub fn through(start: Point, mid: Point, end: Point) -> Option<Self> {
        let d = 2.0
            * (start.x * (mid.y - end.y) + mid.x * (end.y - start.y) + end.x * (start.y - mid.y));
        if d.abs() < COLLINEAR {
            return None;
        }
        let sq = |p: Point| p.x * p.x + p.y * p.y;
        let center = Point {
            x: (sq(start) * (mid.y - end.y)
                + sq(mid) * (end.y - start.y)
                + sq(end) * (start.y - mid.y))
                / d,
            y: (sq(start) * (end.x - mid.x)
                + sq(mid) * (start.x - end.x)
                + sq(end) * (mid.x - start.x))
                / d,
        };
        let angle = |p: Point| (p.y - center.y).atan2(p.x - center.x);
        let start_angle = angle(start);
        let to_mid = (angle(mid) - start_angle).rem_euclid(TAU);
        let to_end = (angle(end) - start_angle).rem_euclid(TAU);
        let sweep = if to_mid <= to_end {
            to_end
        } else {
            to_end - TAU
        };
        Some(Self {
            center,
            radius: (start.x - center.x).hypot(start.y - center.y),
            start_angle,
            sweep,
        })
    }

    /// The point at `angle` on the circle.
    #[must_use]
    pub fn point_at(&self, angle: f64) -> Point {
        Point {
            x: self.center.x + self.radius * angle.cos(),
            y: self.center.y + self.radius * angle.sin(),
        }
    }

    /// The end point.
    #[must_use]
    pub fn end(&self) -> Point {
        self.point_at(self.start_angle + self.sweep)
    }

    /// The arc as a polyline from its start to its end, every point on the
    /// circle and no chord further than `tolerance` from it.
    #[must_use]
    pub fn flatten(&self, tolerance: f64) -> Vec<Point> {
        let segments = segments_for(self.radius, self.sweep.abs(), tolerance);
        (0..=segments)
            .map(|i| {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a segment count is small; well under 2^53"
                )]
                let t = i as f64 / segments as f64;
                self.point_at(self.start_angle + self.sweep * t)
            })
            .collect()
    }

    /// The box around the arc itself (not a stroke of it): its ends and
    /// whichever of the circle's four extremes it passes.
    #[must_use]
    pub fn bounds(&self) -> Bounds {
        let mut bounds = Bounds::of(self.point_at(self.start_angle)).including(self.end());
        let (from, to) = if self.sweep >= 0.0 {
            (self.start_angle, self.start_angle + self.sweep)
        } else {
            (self.start_angle + self.sweep, self.start_angle)
        };
        // The extremes at multiples of a quarter turn from the first one
        // at or after `from`.
        let mut extreme = (from / (PI / 2.0)).ceil() * (PI / 2.0);
        while extreme <= to {
            bounds.include(self.point_at(extreme));
            extreme += PI / 2.0;
        }
        bounds
    }
}

/// How many chords flatten an arc of `radius` over `sweep` radians so no
/// chord is further than `tolerance` from the circle; at least one.
#[must_use]
pub fn segments_for(radius: f64, sweep: f64, tolerance: f64) -> usize {
    if radius <= tolerance || sweep <= 0.0 {
        return 1;
    }
    // The chord of a step of `step` radians sits radius * (1 - cos(step /
    // 2)) inside the circle; solve for the step at which that is the
    // tolerance.
    let step = 2.0 * (1.0 - tolerance / radius).acos();
    if step <= 0.0 {
        return 1;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the quotient is positive and finite, and ceil leaves an integer"
    )]
    let segments = (sweep / step).ceil() as usize;
    segments.max(1)
}

/// An axis-aligned box: the least and the greatest x and y of what it
/// holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// The least x and y.
    pub min: Point,
    /// The greatest x and y.
    pub max: Point,
}

impl Bounds {
    /// The box around one point.
    #[must_use]
    pub fn of(point: Point) -> Self {
        Self {
            min: point,
            max: point,
        }
    }

    /// The box around every point, or `None` for no points.
    pub fn around(points: impl IntoIterator<Item = Point>) -> Option<Self> {
        let mut points = points.into_iter();
        let mut bounds = Self::of(points.next()?);
        for point in points {
            bounds.include(point);
        }
        Some(bounds)
    }

    /// Grow to hold `point`.
    pub fn include(&mut self, point: Point) {
        self.min.x = self.min.x.min(point.x);
        self.min.y = self.min.y.min(point.y);
        self.max.x = self.max.x.max(point.x);
        self.max.y = self.max.y.max(point.y);
    }

    /// This box grown to hold `point`.
    #[must_use]
    pub fn including(mut self, point: Point) -> Self {
        self.include(point);
        self
    }

    /// The box around both.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        self.including(other.min).including(other.max)
    }

    /// Horizontal extent.
    #[must_use]
    pub fn width(&self) -> f64 {
        self.max.x - self.min.x
    }

    /// Vertical extent.
    #[must_use]
    pub fn height(&self) -> f64 {
        self.max.y - self.min.y
    }

    /// The middle.
    #[must_use]
    pub fn center(&self) -> Point {
        Point {
            x: f64::midpoint(self.min.x, self.max.x),
            y: f64::midpoint(self.min.y, self.max.y),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9
    }

    #[test]
    fn a_positive_angle_turns_counter_clockwise_on_the_screen() {
        // Right turned by 90 degrees points up on the screen, which is
        // negative y in the file.
        let frame = Transform::new(Point { x: 10.0, y: 10.0 }, 90.0);
        let placed = frame.apply(Point { x: 5.0, y: 0.0 });
        assert!(close(placed, Point { x: 10.0, y: 5.0 }), "{placed:?}");
        assert!(close(
            frame.rotate(Point { x: 0.0, y: 1.0 }),
            Point { x: 1.0, y: 0.0 }
        ));
    }

    #[test]
    fn frames_compose() {
        let footprint = Transform::new(Point { x: 100.0, y: 100.0 }, 90.0);
        let pad = Transform::new(Point { x: 5.0, y: 0.0 }, 90.0);
        let composed = footprint.then(pad);
        let direct = footprint.apply(pad.apply(Point { x: 1.0, y: 0.0 }));
        assert!(close(composed.apply(Point { x: 1.0, y: 0.0 }), direct));
        // The pad's centre lands up the screen from the footprint; its own
        // 90 degrees on top of the footprint's make x point left.
        assert!(close(composed.origin(), Point { x: 100.0, y: 95.0 }));
        assert!(close(direct, Point { x: 99.0, y: 95.0 }), "{direct:?}");
    }

    #[test]
    fn an_arc_through_three_points_finds_its_circle() {
        let arc = Arc::through(
            Point { x: 10.0, y: 0.0 },
            Point { x: 0.0, y: 10.0 },
            Point { x: -10.0, y: 0.0 },
        )
        .expect("not collinear");
        assert!(close(arc.center, Point::default()), "{:?}", arc.center);
        assert!((arc.radius - 10.0).abs() < 1e-9);
        assert!((arc.sweep - PI).abs() < 1e-9, "sweep {}", arc.sweep);
        assert!(close(arc.end(), Point { x: -10.0, y: 0.0 }));
    }

    #[test]
    fn the_sweep_goes_the_way_of_the_mid_point() {
        let arc = Arc::through(
            Point { x: 10.0, y: 0.0 },
            Point { x: 0.0, y: -10.0 },
            Point { x: -10.0, y: 0.0 },
        )
        .expect("not collinear");
        assert!((arc.sweep + PI).abs() < 1e-9, "sweep {}", arc.sweep);
        // A short arc whose angles wrap through pi.
        let arc = Arc::through(
            Point { x: -1.0, y: -1.0 },
            Point {
                x: -2.0_f64.sqrt(),
                y: 0.0,
            },
            Point { x: -1.0, y: 1.0 },
        )
        .expect("not collinear");
        assert!((arc.sweep + PI / 2.0).abs() < 1e-9, "sweep {}", arc.sweep);
    }

    #[test]
    fn collinear_points_are_no_arc() {
        assert_eq!(
            Arc::through(
                Point { x: 0.0, y: 0.0 },
                Point { x: 1.0, y: 1.0 },
                Point { x: 2.0, y: 2.0 }
            ),
            None
        );
    }

    #[test]
    fn flattening_stays_within_the_tolerance() {
        let arc = Arc::through(
            Point { x: 1.0, y: 0.0 },
            Point { x: 0.0, y: 1.0 },
            Point { x: -1.0, y: 0.0 },
        )
        .expect("an arc");
        let points = arc.flatten(0.01);
        assert!(points.len() > 10, "{} points", points.len());
        let first = points.first().copied().expect("a first point");
        let last = points.last().copied().expect("a last point");
        assert!(close(first, Point { x: 1.0, y: 0.0 }));
        assert!(close(last, Point { x: -1.0, y: 0.0 }));
        for pair in points.windows(2) {
            let [a, b] = pair else {
                panic!("windows of two");
            };
            let mid = Point {
                x: f64::midpoint(a.x, b.x),
                y: f64::midpoint(a.y, b.y),
            };
            let inside = 1.0 - mid.x.hypot(mid.y);
            assert!(
                (0.0..=0.01 + 1e-12).contains(&inside),
                "chord {inside} inside"
            );
        }
    }

    #[test]
    fn arc_bounds_reach_the_extremes_it_passes() {
        // A quarter turn from (10, 0) to (0, 10) passes no axis crossing
        // beyond its ends; the half turn through (0, 10) to (-10, 0) does
        // not reach (0, -10).
        let quarter = Arc::through(
            Point { x: 10.0, y: 0.0 },
            Point {
                x: 7.071_067_8,
                y: 7.071_067_8,
            },
            Point { x: 0.0, y: 10.0 },
        )
        .expect("an arc")
        .bounds();
        assert!(close(quarter.min, Point { x: 0.0, y: 0.0 }), "{quarter:?}");
        assert!(
            close(quarter.max, Point { x: 10.0, y: 10.0 }),
            "{quarter:?}"
        );
        let half = Arc::through(
            Point { x: 10.0, y: 0.0 },
            Point { x: 0.0, y: 10.0 },
            Point { x: -10.0, y: 0.0 },
        )
        .expect("an arc")
        .bounds();
        assert!(close(half.min, Point { x: -10.0, y: 0.0 }), "{half:?}");
        assert!(close(half.max, Point { x: 10.0, y: 10.0 }), "{half:?}");
    }

    #[test]
    fn bounds_grow_and_measure() {
        let bounds = Bounds::around([
            Point { x: 1.0, y: 5.0 },
            Point { x: -2.0, y: 3.0 },
            Point { x: 4.0, y: 4.0 },
        ])
        .expect("three points");
        assert_eq!(bounds.min, Point { x: -2.0, y: 3.0 });
        assert_eq!(bounds.max, Point { x: 4.0, y: 5.0 });
        assert!((bounds.width() - 6.0).abs() < 1e-12);
        assert!((bounds.height() - 2.0).abs() < 1e-12);
        assert_eq!(bounds.center(), Point { x: 1.0, y: 4.0 });
        assert_eq!(Bounds::around([]), None);
    }
}
