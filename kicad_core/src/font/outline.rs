//! KiCad's layout of a text item in an outline face (a system font the
//! viewer draws through a glyph atlas), with the glyphs themselves left
//! to whoever has the font: the browser measures each character's
//! advance and the face's ascent and descent, and [`layout`] places one
//! cell per character on the board.
//!
//! The anchor rules are the ones [`super::shape`] follows for Newstroke
//! (KiCad's `FONT::getLinePositions`: the block's first line counts
//! 1.17 heights, the lines are spaced by the interline, the block hangs
//! from the anchor as the justification says), so a text drawn in either
//! font sits where KiCad put it. Two things differ from the stroke font,
//! as they do in KiCad's `OUTLINE_FONT`: the em of the face is 1.4 times
//! the text height ([`EM_PER_HEIGHT`], KiCad's outline font size
//! compensation), and the interline is the face's line height times the
//! 1.68 pitch, without the stroke font's legacy factor. The hand-off
//! board's `render_cache` polygons confirm both: the Consolas "J1" at
//! size 1 has its baseline 0.415 mm below its centre anchor and its
//! capitals 0.9 mm tall.
//!
//! A cell is the character's advance wide, from the face's ascent above
//! the baseline to its descent below, in the text's frame; its ink is
//! whatever the browser rasterised into the matching atlas rectangle. A
//! knockout text is its box with the glyphs cut out; here the box is the
//! cells' bounds grown by KiCad's margin, and [`OutlineText::strips`]
//! are the parts of that box no cell covers, so the viewer fills the
//! strips flat and the cells with their coverage inverted.

use super::{Placement, cursors, first_baseline, knockout_grow};
use crate::geometry::{Bounds, Transform};
use crate::model::{Horizontal, Point};

/// The em of an outline face per unit of text height: KiCad's
/// `OUTLINE_FONT::m_outlineFontSizeCompensation`, which makes a TrueType
/// capital about as tall as a Newstroke one at the same size.
pub const EM_PER_HEIGHT: f64 = 1.4;

/// Baseline to baseline between lines, in text heights, before the
/// face's own line height scales it: KiCad's default interline pitch.
pub const INTERLINE_PITCH: f64 = 1.68;

/// What the browser measured of a face, in em units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaceMetrics {
    /// From the baseline up to the top of the line box.
    pub ascent: f64,
    /// From the baseline down to the bottom of the line box.
    pub descent: f64,
}

impl FaceMetrics {
    /// The line box's height, in em units.
    #[must_use]
    pub fn line_height(self) -> f64 {
        self.ascent + self.descent
    }
}

/// One character's cell on the board.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    /// The character.
    pub c: char,
    /// The cell's corners in board millimetres, in reading order: the
    /// top left of the glyph, its top right, bottom right, bottom left.
    /// Mirroring and rotation are applied, so for a mirrored text the
    /// "top left" is on the right.
    pub corners: [Point; 4],
}

/// A text laid out in an outline face.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutlineText {
    /// One cell per character drawn; spaces and tabs advance the pen
    /// and have no cell.
    pub cells: Vec<Cell>,
    /// For a knockout text, the parts of its box no cell covers, each
    /// a quadrilateral in board millimetres; empty otherwise.
    pub strips: Vec<[Point; 4]>,
}

/// An axis-aligned box in the text's unrotated frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    min: Point,
    max: Point,
}

impl Rect {
    fn corners(self) -> [Point; 4] {
        [
            self.min,
            Point {
                x: self.max.x,
                y: self.min.y,
            },
            self.max,
            Point {
                x: self.min.x,
                y: self.max.y,
            },
        ]
    }

    fn is_empty(self) -> bool {
        self.max.x <= self.min.x || self.max.y <= self.min.y
    }
}

/// Lays `text` out as `placement` says in a face of `metrics`, each
/// character `advance` em wide (the browser's measurement; a space is
/// `advance(' ')`, a tab reaches the next stop as the stroke font places
/// it). With `knockout`, the strips of the box around the cells are
/// computed too.
#[must_use]
pub fn layout(
    text: &str,
    placement: &Placement,
    metrics: FaceMetrics,
    advance: impl Fn(char) -> f64,
    knockout: bool,
) -> OutlineText {
    let size = placement.size;
    let em = Point {
        x: size.width.abs() * EM_PER_HEIGHT,
        y: size.height.abs() * EM_PER_HEIGHT,
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let interline = size.height * INTERLINE_PITCH * metrics.line_height();
    let offset_y = first_baseline(
        size.height,
        lines.len(),
        interline,
        placement.justify.vertical,
    );
    let origin = placement.position;
    let mut cells: Vec<(char, Rect)> = Vec::new();
    let mut blanks: Vec<Rect> = Vec::new();
    let mut line_boxes: Vec<Rect> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a text has far fewer lines than 2^53"
        )]
        let baseline = origin.y + offset_y + index as f64 * interline;
        let line_box = Rect {
            min: Point {
                x: origin.x,
                y: baseline - metrics.ascent * em.y,
            },
            max: Point {
                x: origin.x,
                y: baseline + metrics.descent * em.y,
            },
        };
        line_boxes.push(line_cells(
            line,
            line_box,
            placement.justify.horizontal,
            em.x,
            &advance,
            &mut cells,
            knockout.then_some(&mut blanks),
        ));
    }
    if cells.is_empty() {
        // Nothing to draw: no glyph, so no box to cut one out of.
        return OutlineText::default();
    }
    let strips = if knockout {
        let mut strips = knockout_strips(&line_boxes, placement.pen, size.height);
        strips.append(&mut blanks);
        strips
    } else {
        Vec::new()
    };
    let mirror = placement.justify.mirror;
    let turn = Transform::new(origin, placement.angle);
    let place = |p: Point| {
        let x = if mirror {
            origin.x - (p.x - origin.x)
        } else {
            p.x
        };
        turn.apply(Point {
            x: x - origin.x,
            y: p.y - origin.y,
        })
    };
    OutlineText {
        cells: cells
            .into_iter()
            .map(|(c, rect)| Cell {
                c,
                corners: rect.corners().map(place),
            })
            .collect(),
        strips: strips
            .into_iter()
            .map(|rect| rect.corners().map(place))
            .collect(),
    }
}

/// The cells of one line: `line_box` gives the anchor's x and the line's
/// top and bottom; the line is justified about the anchor as
/// `horizontal` says, each glyph's cell pushed to `cells` and, when
/// `blanks` is given (a knockout), each space's or tab's to it. Returns
/// the line's box, anchor to advance.
fn line_cells(
    line: &str,
    line_box: Rect,
    horizontal: Horizontal,
    em: f64,
    advance: &impl Fn(char) -> f64,
    cells: &mut Vec<(char, Rect)>,
    mut blanks: Option<&mut Vec<Rect>>,
) -> Rect {
    let advances = cursors(
        line,
        em,
        advance(' ') * em,
        |c| advance(c) * em,
        Point::default(),
    );
    let width = advances.last().map_or(0.0, |(_, end)| end.x);
    let start_x = line_box.min.x
        + match horizontal {
            Horizontal::Left => 0.0,
            Horizontal::Center => -width / 2.0,
            Horizontal::Right => -width,
        };
    for pair in advances.windows(2) {
        let (Some((c, from)), Some((_, to))) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let rect = Rect {
            min: Point {
                x: start_x + from.x,
                y: line_box.min.y,
            },
            max: Point {
                x: start_x + to.x,
                y: line_box.max.y,
            },
        };
        if matches!(c, ' ' | '\t') {
            // A blank has no glyph; in a knockout it is part of the
            // filled box.
            if let Some(blanks) = blanks.as_deref_mut() {
                blanks.push(rect);
            }
            continue;
        }
        cells.push((*c, rect));
    }
    Rect {
        min: Point {
            x: start_x,
            y: line_box.min.y,
        },
        max: Point {
            x: start_x + width,
            y: line_box.max.y,
        },
    }
}

/// The knockout box less the lines: the box is the line boxes' bounds
/// grown by half the pen and KiCad's margin (the larger of half the pen
/// and a ninth of the height); the strips are the bands above the first
/// line, between lines and below the last, full box width, and each
/// line's left and right bands. A band a line's box swallows is left
/// out. A line's cells and blanks tile its box, so the box is exactly
/// the cells, the blanks and these strips.
fn knockout_strips(lines: &[Rect], pen: f64, height: f64) -> Vec<Rect> {
    let Some(bounds) = Bounds::around(lines.iter().flat_map(|rect| [rect.min, rect.max])) else {
        return Vec::new();
    };
    let grow = knockout_grow(pen, height);
    let outer = Rect {
        min: Point {
            x: bounds.min.x - grow,
            y: bounds.min.y - grow,
        },
        max: Point {
            x: bounds.max.x + grow,
            y: bounds.max.y + grow,
        },
    };
    let mut strips = Vec::new();
    let mut band_top = outer.min.y;
    for line in lines {
        strips.push(Rect {
            min: Point {
                x: outer.min.x,
                y: band_top,
            },
            max: Point {
                x: outer.max.x,
                y: line.min.y,
            },
        });
        strips.push(Rect {
            min: Point {
                x: outer.min.x,
                y: line.min.y,
            },
            max: Point {
                x: line.min.x,
                y: line.max.y,
            },
        });
        strips.push(Rect {
            min: Point {
                x: line.max.x,
                y: line.min.y,
            },
            max: Point {
                x: outer.max.x,
                y: line.max.y,
            },
        });
        band_top = band_top.max(line.max.y);
    }
    strips.push(Rect {
        min: Point {
            x: outer.min.x,
            y: band_top,
        },
        max: outer.max,
    });
    strips.retain(|strip| !strip.is_empty());
    strips
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::model::{Justify, Size, Vertical};
    use kiewer_assert::{assert_is_empty, assert_is_not_empty};

    /// Consolas as a browser measures it: every glyph 0.55 em wide, the
    /// line box 0.74 em up and 0.26 em down.
    const CONSOLAS: FaceMetrics = FaceMetrics {
        ascent: 0.74,
        descent: 0.26,
    };
    const ADVANCE: f64 = 0.55;

    fn monospace(_: char) -> f64 {
        ADVANCE
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn assert_point(p: Point, x: f64, y: f64) {
        assert!(close(p.x, x) && close(p.y, y), "{p:?} is not ({x}, {y})");
    }

    fn placement(justify: Justify, angle: f64) -> Placement {
        Placement {
            size: Size {
                width: 1.0,
                height: 1.0,
            },
            pen: 0.15,
            italic: false,
            justify,
            position: Point { x: 10.0, y: 20.0 },
            angle,
        }
    }

    fn cell(text: &OutlineText, index: usize) -> &Cell {
        text.cells
            .get(index)
            .unwrap_or_else(|| panic!("no cell {index} in {text:?}"))
    }

    #[test]
    fn a_centred_glyph_hangs_from_the_anchor_as_kicads_j1_does() {
        // The hand-off's "J1": size 1, centred, baseline 0.415 below the
        // anchor (1 - 1.17 / 2), two cells of 0.55 em at 1.4 mm per em
        // centred on the anchor.
        let text = layout(
            "J1",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            false,
        );
        assert_eq!(text.cells.len(), 2);
        assert_eq!(cell(&text, 0).c, 'J');
        assert_eq!(cell(&text, 1).c, '1');
        let baseline = 20.0 + 1.0 - 1.17 / 2.0;
        let cell_width = ADVANCE * 1.4;
        let j = cell(&text, 0).corners;
        assert_point(j[0], 10.0 - cell_width, baseline - 0.74 * 1.4);
        assert_point(j[1], 10.0, baseline - 0.74 * 1.4);
        assert_point(j[2], 10.0, baseline + 0.26 * 1.4);
        assert_point(j[3], 10.0 - cell_width, baseline + 0.26 * 1.4);
        let one = cell(&text, 1).corners;
        assert_point(one[1], 10.0 + cell_width, baseline - 0.74 * 1.4);
        assert_is_empty!(text.strips);
    }

    #[test_case(Horizontal::Left, Vertical::Top, 0.0, 1.0; "left top: the block starts at the anchor and its baseline sits a height below")]
    #[test_case(Horizontal::Right, Vertical::Bottom, -0.77, 1.0 - 1.17; "right bottom: the line ends at the anchor and the block hangs above")]
    fn justification_moves_the_line_as_kicad_moves_it(
        horizontal: Horizontal,
        vertical: Vertical,
        start_dx: f64,
        baseline_dy: f64,
    ) {
        let text = layout(
            "I",
            &placement(
                Justify {
                    horizontal,
                    vertical,
                    mirror: false,
                },
                0.0,
            ),
            CONSOLAS,
            monospace,
            false,
        );
        let corners = cell(&text, 0).corners;
        assert!(close(corners[0].x, 10.0 + start_dx), "{corners:?}");
        let baseline = corners[0].y + 0.74 * 1.4;
        assert!(close(baseline, 20.0 + baseline_dy), "{corners:?}");
    }

    #[test]
    fn the_width_scales_the_cells_and_the_height_scales_the_lines() {
        let mut wide = placement(Justify::default(), 0.0);
        wide.size = Size {
            width: 2.0,
            height: 1.0,
        };
        let text = layout("I", &wide, CONSOLAS, monospace, false);
        let corners = cell(&text, 0).corners;
        assert!(close(corners[1].x - corners[0].x, ADVANCE * 1.4 * 2.0));
        assert!(close(corners[3].y - corners[0].y, 1.4));
    }

    #[test]
    fn a_second_line_sits_one_interline_lower_scaled_by_the_faces_line_height() {
        let tall = FaceMetrics {
            ascent: 0.9,
            descent: 0.3,
        };
        let text = layout(
            "I\nI",
            &placement(Justify::default(), 0.0),
            tall,
            monospace,
            false,
        );
        let dy = cell(&text, 1).corners[0].y - cell(&text, 0).corners[0].y;
        assert!(close(dy, 1.68 * 1.2), "{dy}");
        // The block is centred on the anchor: 1.17 for the first line
        // plus one interline.
        let baseline = cell(&text, 0).corners[0].y + 0.9 * 1.4;
        assert!(close(
            baseline,
            20.0 + 1.0 - f64::midpoint(1.17, 1.68 * 1.2)
        ));
    }

    #[test_case("I I", 2.0 * ADVANCE * 1.4; "a space advances the space width")]
    #[test_case("I\tI", 3.0 * 1.4 + ADVANCE * 1.4; "a tab after the first column reaches the stop at three ems plus a space, as in the stroke font")]
    fn a_blank_advances_the_pen_without_a_cell(text: &str, expected_dx: f64) {
        let left = Justify {
            horizontal: Horizontal::Left,
            vertical: Vertical::Center,
            mirror: false,
        };
        let laid = layout(text, &placement(left, 0.0), CONSOLAS, monospace, false);
        assert_eq!(laid.cells.len(), 2);
        let dx = cell(&laid, 1).corners[0].x - cell(&laid, 0).corners[0].x;
        assert!(close(dx, expected_dx), "{dx}");
    }

    #[test]
    fn a_character_the_browser_measured_wider_gets_a_wider_cell() {
        let text = layout(
            "iW",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            |c| if c == 'W' { 0.9 } else { 0.3 },
            false,
        );
        let i = cell(&text, 0).corners;
        let w = cell(&text, 1).corners;
        assert!(close(i[1].x - i[0].x, 0.3 * 1.4));
        assert!(close(w[1].x - w[0].x, 0.9 * 1.4));
        assert!(close(i[1].x, w[0].x), "the cells abut");
    }

    #[test]
    fn mirrored_text_is_reflected_about_its_position_with_its_corners_following() {
        let plain = layout(
            "I",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            false,
        );
        let mirror = Justify {
            mirror: true,
            ..Justify::default()
        };
        let mirrored = layout("I", &placement(mirror, 0.0), CONSOLAS, monospace, false);
        let (a, b) = (cell(&plain, 0).corners, cell(&mirrored, 0).corners);
        // The glyph's top left lands where its top right was, so a quad
        // textured by corner order reads mirrored.
        assert!(close(a[0].x - 10.0, 10.0 - b[0].x) && close(a[0].y, b[0].y));
        assert!(b[0].x > b[1].x, "{b:?}");
    }

    #[test]
    fn a_text_turned_90_degrees_reads_upwards() {
        let flat = layout(
            "I",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            false,
        );
        let turned = layout(
            "I",
            &placement(Justify::default(), 90.0),
            CONSOLAS,
            monospace,
            false,
        );
        let (a, b) = (cell(&flat, 0).corners, cell(&turned, 0).corners);
        // Counter-clockwise on the screen: what was up (smaller y) is now
        // left (smaller x), as the stroke font's test has it.
        assert!(close(b[0].x - 10.0, a[0].y - 20.0) && close(b[0].y - 20.0, -(a[0].x - 10.0)));
    }

    #[test]
    fn an_empty_text_has_no_cells_and_no_strips() {
        let text = layout(
            "",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            true,
        );
        assert_eq!(text, OutlineText::default());
    }

    /// The area of an axis-aligned quad given by its corners.
    fn area(corners: &[Point; 4]) -> f64 {
        (corners[1].x - corners[0].x).abs() * (corners[3].y - corners[0].y).abs()
    }

    fn bounds_of(quads: impl Iterator<Item = [Point; 4]>) -> Bounds {
        Bounds::around(quads.flatten()).expect("some quads")
    }

    #[test]
    fn a_knockout_is_the_grown_box_tiled_by_the_cells_and_the_strips() {
        let text = layout(
            "I I\nII",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            true,
        );
        assert_eq!(text.cells.len(), 4);
        assert_is_not_empty!(text.strips);
        let cells = text.cells.iter().map(|cell| cell.corners);
        let outer = bounds_of(cells.clone().chain(text.strips.iter().copied()));
        // Pen 0.15: half is 0.075; the margin is a ninth of the height,
        // which beats half the pen. The box grows by both around the
        // cells (the first line's three cells span its box).
        let inner = bounds_of(cells.clone());
        let grow = 0.075 + 1.0 / 9.0;
        assert!(
            close(outer.min.x, inner.min.x - grow),
            "{outer:?} {inner:?}"
        );
        assert!(
            close(outer.max.y, inner.max.y + grow),
            "{outer:?} {inner:?}"
        );
        // The cells and the strips tile the box: their areas sum to it,
        // and no strip overlaps a cell.
        let tiled: f64 = cells.clone().map(|c| area(&c)).sum::<f64>()
            + text.strips.iter().map(area).sum::<f64>();
        assert!(
            close(tiled, outer.width() * outer.height()),
            "{tiled} vs {}",
            outer.width() * outer.height()
        );
        for strip in &text.strips {
            for cell in cells.clone() {
                let overlap_x = strip[1].x.min(cell[1].x) - strip[0].x.max(cell[0].x);
                let overlap_y = strip[3].y.min(cell[3].y) - strip[0].y.max(cell[0].y);
                assert!(
                    overlap_x <= 1e-9 || overlap_y <= 1e-9,
                    "{strip:?} overlaps {cell:?}"
                );
            }
        }
        // The gap between the lines is a strip too: the space between
        // "I I" and "II" is covered.
        let gap_y = f64::midpoint(cell(&text, 0).corners[3].y, cell(&text, 3).corners[0].y);
        assert!(text.strips.iter().any(|s| s[0].y < gap_y && s[3].y > gap_y));
        // And the space between the I's of the first line is filled: a
        // blank has no glyph to cut out of the box.
        let space_x = f64::midpoint(cell(&text, 0).corners[1].x, cell(&text, 1).corners[0].x);
        let space_y = f64::midpoint(cell(&text, 0).corners[0].y, cell(&text, 0).corners[3].y);
        assert!(text.strips.iter().any(|s| {
            s[0].x < space_x && s[1].x > space_x && s[0].y < space_y && s[3].y > space_y
        }));
    }
}
