//! KiCad's layout of a text item in an outline face (a system font the
//! viewer draws through a glyph atlas), with the glyphs themselves left
//! to whoever has the font: the browser measures each character's
//! advance and ink and the face's ascent and descent, and [`layout`]
//! places one cell per character on the board.
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
//! The pen moves by each character's advance, and between two
//! characters by the pair's kern as the browser measured it, as KiCad's
//! HarfBuzz shaping sets a kerned pair. A glyph's quad runs from the
//! face's ascent above the baseline to its descent below, in the text's
//! frame, and across its advance grown by its ink's [`Overhang`] on
//! either side, so ink that reaches past the advance (an italic f, a j's
//! tail) is drawn whole; the atlas rectangle it samples is the same
//! size. Neighbouring quads overlap where a glyph overhangs or a pair is
//! kerned, which a plain text draws as it is: a glyph is transparent
//! around its ink.
//!
//! A knockout text is its knockout box with the glyphs cut out. The box
//! is KiCad 10's (`PCB_TEXT::buildBoundingHull`): the bounds of the
//! glyph ink with the text unturned, grown on every side by KiCad's
//! margin and no pen, since an outline glyph has none. The viewer draws
//! a knockout's cells with their coverage inverted, so two of them over
//! the same spot would fill each other's ink: a knockout's line is cut
//! at every quad's edge instead, each piece is one cell that names the
//! glyph over it and the neighbour whose quad overlaps it, and the
//! viewer cuts both glyphs' ink out of that piece. A quad is taller than
//! the box, so the cells are cropped to it, and [`OutlineText::strips`]
//! are the parts of the box no cropped cell covers: the viewer fills the
//! strips flat.

use std::ops::Range;

use super::{Placement, cursors, first_baseline, knockout_margin};
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

/// Where a character's ink reaches from its pen position on the
/// baseline, in em units, as the browser's `measureText` reports it
/// (`actualBoundingBoxLeft` and its siblings): each distance is positive
/// in its own direction, so ink that starts right of the pen has a
/// negative `left`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Ink {
    /// How far the ink reaches left of the pen.
    pub left: f64,
    /// How far it reaches right of the pen.
    pub right: f64,
    /// How far it reaches above the baseline.
    pub ascent: f64,
    /// How far it reaches below the baseline.
    pub descent: f64,
}

impl Ink {
    /// Ink that fills a whole cell `advance` em wide in a face of
    /// `metrics`: from the pen to the advance, ascent to descent.
    #[must_use]
    pub fn whole_cell(advance: f64, metrics: FaceMetrics) -> Self {
        Self {
            left: 0.0,
            right: advance,
            ascent: metrics.ascent,
            descent: metrics.descent,
        }
    }

    /// How far this ink reaches past a cell `advance` em wide; nothing
    /// for no ink.
    #[must_use]
    pub fn overhang(self, advance: f64) -> Overhang {
        if self.is_empty() {
            return Overhang::default();
        }
        Overhang {
            left: self.left.max(0.0),
            right: (self.right - advance).max(0.0),
        }
    }

    /// Whether there is no ink: no width or no height, as for a space,
    /// or a measurement that is not a number.
    fn is_empty(self) -> bool {
        !(self.left + self.right > 0.0 && self.ascent + self.descent > 0.0)
    }
}

/// How far a glyph's ink reaches past its advance, in em: left of the
/// pen and right of the advance, each 0 where the ink stays inside. The
/// glyph's quad on the board and its rectangle in the atlas are both the
/// advance plus the two, with the pen `left` in from their left edge.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Overhang {
    /// How far the ink reaches left of the pen.
    pub left: f64,
    /// How far it reaches right of the advance.
    pub right: f64,
}

impl Overhang {
    /// The width of the quad of a glyph `advance` em wide, in em.
    #[must_use]
    pub fn quad_width(self, advance: f64) -> f64 {
        self.left + advance + self.right
    }
}

/// One cell on the board: the quad of a glyph, or a piece of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    /// The character.
    pub c: char,
    /// The corners of the part of the glyph's quad drawn, in board
    /// millimetres, in reading order: the top left of the glyph, its top
    /// right, bottom right, bottom left. Mirroring and rotation are
    /// applied, so for a mirrored text the "top left" is on the right.
    pub corners: [Point; 4],
    /// Which part of the glyph's quad `corners` covers, as fractions of
    /// the quad's width and height from its top left in reading order:
    /// 0 to 1 both ways for a whole quad, less for a knockout's piece of
    /// its line, cropped to its box. The atlas samples the same part of
    /// the glyph's rectangle.
    pub crop: Bounds,
    /// In a knockout, the neighbouring glyph whose quad covers this same
    /// piece of the line (a kerned pair, or ink overhanging the
    /// neighbour), with the part of its quad the piece is: the viewer
    /// cuts both glyphs' ink out here. `None` otherwise.
    pub neighbour: Option<GlyphPart>,
}

/// A part of a glyph's quad: the character, and which part as
/// [`Cell::crop`] has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphPart {
    /// The character.
    pub c: char,
    /// Which part of its quad, as fractions from its top left.
    pub crop: Bounds,
}

/// A text laid out in an outline face.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutlineText {
    /// One cell per character drawn, or for a knockout one per piece of
    /// a line a glyph's quad covers; spaces and tabs advance the pen and
    /// have no cell.
    pub cells: Vec<Cell>,
    /// For a knockout text, the parts of its knockout box no cell
    /// covers, each a quadrilateral in board millimetres; empty
    /// otherwise.
    pub strips: Vec<[Point; 4]>,
    /// For a knockout text with ink, its knockout box: the corners in
    /// the order of a cell's, mirrored and turned with the text. `None`
    /// otherwise.
    pub knockout_box: Option<[Point; 4]>,
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

    /// The part of this box inside `other`, empty when they do not meet.
    fn crop(self, other: Rect) -> Rect {
        Rect {
            min: Point {
                x: self.min.x.max(other.min.x),
                y: self.min.y.max(other.min.y),
            },
            max: Point {
                x: self.max.x.min(other.max.x),
                y: self.max.y.min(other.max.y),
            },
        }
    }

    fn union(self, other: Rect) -> Rect {
        Rect {
            min: Point {
                x: self.min.x.min(other.min.x),
                y: self.min.y.min(other.min.y),
            },
            max: Point {
                x: self.max.x.max(other.max.x),
                y: self.max.y.max(other.max.y),
            },
        }
    }

    /// This box grown by `by` on every side.
    fn grown(self, by: f64) -> Rect {
        Rect {
            min: Point {
                x: self.min.x - by,
                y: self.min.y - by,
            },
            max: Point {
                x: self.max.x + by,
                y: self.max.y + by,
            },
        }
    }
}

/// The whole of a glyph's quad, as [`Cell::crop`] gives it for a quad
/// nothing crops.
pub const WHOLE_CELL: Bounds = Bounds {
    min: Point { x: 0.0, y: 0.0 },
    max: Point { x: 1.0, y: 1.0 },
};

/// A glyph's quad and its ink, in the text's unrotated frame.
struct PlacedGlyph {
    c: char,
    quad: Rect,
    ink: Option<Rect>,
}

/// One line of a text: its glyphs (a range of the text's) and its box,
/// from the pen's start to its end and from the face's ascent to its
/// descent.
struct Line {
    glyphs: Range<usize>,
    rect: Rect,
}

/// Lays `text` out as `placement` says in a face of `metrics`, each
/// character `advance` em wide, each character after another moved by
/// the pair's `kern` em, and each glyph's ink where `ink` says (the
/// browser's measurements; a space is `advance(' ')`, a tab reaches the
/// next stop as the stroke font places it). With `knockout`, the
/// knockout box is the ink's bounds grown by KiCad's margin, the lines
/// are cut into cells that do not overlap, cropped to it, and the strips
/// of it around them are computed too.
#[must_use]
pub fn layout(
    text: &str,
    placement: &Placement,
    metrics: FaceMetrics,
    advance: impl Fn(char) -> f64,
    kern: impl Fn(char, char) -> f64,
    ink: impl Fn(char) -> Ink,
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
    let mut glyphs: Vec<PlacedGlyph> = Vec::new();
    let mut placed_lines: Vec<Line> = Vec::new();
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
        let first = glyphs.len();
        let rect = line_quads(
            line,
            line_box,
            placement.justify.horizontal,
            em.x,
            &advance,
            &kern,
            &mut glyphs,
        );
        reach_ink(
            glyphs.get_mut(first..).unwrap_or_default(),
            baseline,
            em,
            &advance,
            &ink,
        );
        placed_lines.push(Line {
            glyphs: first..glyphs.len(),
            rect,
        });
    }
    if glyphs.is_empty() {
        // Nothing to draw: no glyph, so no box to cut one out of.
        return OutlineText::default();
    }
    let unplaced = if knockout {
        let margin = knockout_margin(placement.pen, size.height);
        let Some(unplaced) = knockout_parts(&glyphs, &placed_lines, margin) else {
            // No ink anywhere: no box, and nothing in a cell to cut out.
            return OutlineText::default();
        };
        unplaced
    } else {
        whole_quads(&glyphs)
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
        cells: unplaced
            .cells
            .into_iter()
            .map(|cell| Cell {
                c: cell.c,
                corners: cell.rect.corners().map(place),
                crop: cell.crop,
                neighbour: cell.neighbour,
            })
            .collect(),
        strips: unplaced
            .strips
            .into_iter()
            .map(|rect| rect.corners().map(place))
            .collect(),
        knockout_box: unplaced.knockout_box.map(|rect| rect.corners().map(place)),
    }
}

/// A text's cells (each with the part of it drawn), strips and knockout
/// box in its frame, before mirroring and turning.
struct Unplaced {
    cells: Vec<UnplacedCell>,
    strips: Vec<Rect>,
    knockout_box: Option<Rect>,
}

/// A cell in the text's frame: the part of its glyph's quad drawn,
/// which part that is and the neighbour over it, as [`Cell`] has them.
struct UnplacedCell {
    c: char,
    rect: Rect,
    crop: Bounds,
    neighbour: Option<GlyphPart>,
}

/// A plain text's parts: every glyph's quad whole, and no strips or box.
fn whole_quads(glyphs: &[PlacedGlyph]) -> Unplaced {
    Unplaced {
        cells: glyphs
            .iter()
            .map(|glyph| UnplacedCell {
                c: glyph.c,
                rect: glyph.quad,
                crop: WHOLE_CELL,
                neighbour: None,
            })
            .collect(),
        strips: Vec::new(),
        knockout_box: None,
    }
}

/// Fills in the ink of `glyphs`, one line's, whose pens are on
/// `baseline` in a face whose em is `em`, and grows each quad (so far its
/// cell, the pen to the advance) by its ink's overhang.
fn reach_ink(
    glyphs: &mut [PlacedGlyph],
    baseline: f64,
    em: Point,
    advance: &impl Fn(char) -> f64,
    ink: &impl Fn(char) -> Ink,
) {
    for glyph in glyphs {
        let measured = ink(glyph.c);
        let overhang = measured.overhang(advance(glyph.c));
        glyph.ink = ink_at(measured, glyph.quad.min.x, baseline, em);
        glyph.quad.min.x -= overhang.left * em.x;
        glyph.quad.max.x += overhang.right * em.x;
    }
}

/// Where `measured` ink sits for a glyph whose pen is at `pen_x` on
/// `baseline`, in a face whose em is `em`; `None` for no ink.
fn ink_at(measured: Ink, pen_x: f64, baseline: f64, em: Point) -> Option<Rect> {
    (!measured.is_empty()).then_some(Rect {
        min: Point {
            x: pen_x - measured.left * em.x,
            y: baseline - measured.ascent * em.y,
        },
        max: Point {
            x: pen_x + measured.right * em.x,
            y: baseline + measured.descent * em.y,
        },
    })
}

/// A knockout's parts: the box is the union of the glyphs' ink grown by
/// `margin`; each line, from the first quad's or the pen's start to the
/// last's end, is cut into pieces at every quad's edge, and each piece
/// cropped to the box is a cell (of the glyph whose quad covers it, and
/// of the neighbour whose quad covers it too) or, covered by none, a
/// blank. The strips are the box less the cropped lines, plus the
/// blanks. `None` when no glyph has ink.
fn knockout_parts(glyphs: &[PlacedGlyph], lines: &[Line], margin: f64) -> Option<Unplaced> {
    let knockout_box = glyphs
        .iter()
        .filter_map(|glyph| glyph.ink)
        .reduce(Rect::union)?
        .grown(margin);
    let mut cells = Vec::new();
    let mut blanks = Vec::new();
    let mut cropped_lines = Vec::new();
    for line in lines {
        let glyphs = glyphs.get(line.glyphs.clone()).unwrap_or_default();
        let extent = glyphs
            .iter()
            .fold(line.rect, |extent, glyph| extent.union(glyph.quad));
        for (piece, over) in pieces(extent, glyphs) {
            let part = piece.crop(knockout_box);
            if part.is_empty() {
                continue;
            }
            // A third quad over the same piece, which only an overhang
            // reaching past a whole neighbour makes, is left out: its ink
            // there is filled (#111).
            let mut over = over.into_iter();
            match over.next() {
                None => blanks.push(part),
                Some(glyph) => cells.push(UnplacedCell {
                    c: glyph.c,
                    rect: part,
                    crop: part_of(glyph.quad, part),
                    neighbour: over.next().map(|neighbour| GlyphPart {
                        c: neighbour.c,
                        crop: part_of(neighbour.quad, part),
                    }),
                }),
            }
        }
        let cropped = extent.crop(knockout_box);
        if !cropped.is_empty() {
            cropped_lines.push(cropped);
        }
    }
    let mut strips = knockout_strips(&cropped_lines, knockout_box);
    strips.extend(blanks);
    Some(Unplaced {
        cells,
        strips,
        knockout_box: Some(knockout_box),
    })
}

/// The line `extent` cut at every edge of `glyphs`' quads, left to
/// right, each piece with the glyphs whose quads cover it in reading
/// order: none for a blank, two where neighbouring quads overlap.
fn pieces(extent: Rect, glyphs: &[PlacedGlyph]) -> Vec<(Rect, Vec<&PlacedGlyph>)> {
    let mut edges: Vec<f64> = glyphs
        .iter()
        .flat_map(|glyph| [glyph.quad.min.x, glyph.quad.max.x])
        .chain([extent.min.x, extent.max.x])
        .collect();
    edges.sort_by(f64::total_cmp);
    edges.dedup();
    edges
        .windows(2)
        .filter_map(|pair| {
            let (&from, &to) = (pair.first()?, pair.get(1)?);
            let piece = Rect {
                min: Point {
                    x: from,
                    y: extent.min.y,
                },
                max: Point {
                    x: to,
                    y: extent.max.y,
                },
            };
            let over = glyphs
                .iter()
                .filter(|glyph| glyph.quad.min.x <= from && glyph.quad.max.x >= to)
                .collect();
            Some((piece, over))
        })
        .collect()
}

/// Which part of `quad` the `part` inside it is, as fractions of its
/// width and height from its top left. A part that is not empty lies in
/// a quad that is not, so neither extent is zero.
fn part_of(quad: Rect, part: Rect) -> Bounds {
    let (width, height) = (quad.max.x - quad.min.x, quad.max.y - quad.min.y);
    let fraction = |p: Point| Point {
        x: (p.x - quad.min.x) / width,
        y: (p.y - quad.min.y) / height,
    };
    Bounds {
        min: fraction(part.min),
        max: fraction(part.max),
    }
}

/// The glyphs' quads of one line: `line_box` gives the anchor's x and
/// the line's top and bottom; the line is justified about the anchor as
/// `horizontal` says, the pen moving by `advance` and between two
/// characters by their `kern`, and each glyph's cell, pen to advance,
/// pushed to `glyphs` as its quad (the caller widens it by the ink's
/// overhang and fills the ink in). Spaces and tabs push nothing. Returns
/// the line's box, from the pen's start to its end.
fn line_quads(
    line: &str,
    line_box: Rect,
    horizontal: Horizontal,
    em: f64,
    advance: &impl Fn(char) -> f64,
    kern: &impl Fn(char, char) -> f64,
    glyphs: &mut Vec<PlacedGlyph>,
) -> Rect {
    let advances = cursors(
        line,
        em,
        advance(' ') * em,
        |c| advance(c) * em,
        |previous, c| kern(previous, c) * em,
        Point::default(),
    );
    let width = advances.last().map_or(0.0, |(_, end)| end.x);
    let start_x = line_box.min.x
        + match horizontal {
            Horizontal::Left => 0.0,
            Horizontal::Center => -width / 2.0,
            Horizontal::Right => -width,
        };
    // The last entry is the pen at the end of the line, no character.
    let characters = advances.split_last().map_or(&[][..], |(_, rest)| rest);
    for &(c, from) in characters {
        if matches!(c, ' ' | '\t') {
            // A blank has no glyph; in a knockout it is part of the
            // filled box.
            continue;
        }
        glyphs.push(PlacedGlyph {
            c,
            quad: Rect {
                min: Point {
                    x: start_x + from.x,
                    y: line_box.min.y,
                },
                max: Point {
                    x: start_x + (from.x + advance(c) * em),
                    y: line_box.max.y,
                },
            },
            ink: None,
        });
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

/// The knockout box `knockout_box` less the `lines` (each already cropped to
/// it, top to bottom): the strips are the bands above the first line,
/// between lines and below the last, full box width, and each line's
/// left and right bands. A band a line swallows is left out. A line's
/// cropped cells and blanks tile it, so the box is exactly the cells,
/// the blanks and these strips.
fn knockout_strips(lines: &[Rect], knockout_box: Rect) -> Vec<Rect> {
    let mut strips = Vec::new();
    let mut band_top = knockout_box.min.y;
    for line in lines {
        strips.push(Rect {
            min: Point {
                x: knockout_box.min.x,
                y: band_top,
            },
            max: Point {
                x: knockout_box.max.x,
                y: line.min.y,
            },
        });
        strips.push(Rect {
            min: Point {
                x: knockout_box.min.x,
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
                x: knockout_box.max.x,
                y: line.max.y,
            },
        });
        band_top = band_top.max(line.max.y);
    }
    strips.push(Rect {
        min: Point {
            x: knockout_box.min.x,
            y: band_top,
        },
        max: knockout_box.max,
    });
    strips.retain(|strip| !strip.is_empty());
    strips
}

#[cfg(test)]
mod tests {
    use kiewer_assert::{assert_is_empty, assert_is_not_empty};
    use test_case::test_case;

    use super::*;
    use crate::model::{Justify, Size, Vertical};

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

    /// A monospace face kerns no pair.
    fn no_kern(_: char, _: char) -> f64 {
        0.0
    }

    /// Ink that fills the whole of a [`CONSOLAS`] cell.
    fn whole_cell(_: char) -> Ink {
        Ink::whole_cell(ADVANCE, CONSOLAS)
    }

    /// Ink as a capital has it: 0.05 em in from each side of the cell,
    /// from the baseline up to 0.64 em, so the cell stands 0.1 em above
    /// it and 0.26 em below.
    fn capital(_: char) -> Ink {
        Ink {
            left: -0.05,
            right: ADVANCE - 0.05,
            ascent: 0.64,
            descent: 0.0,
        }
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
            no_kern,
            whole_cell,
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
            no_kern,
            whole_cell,
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
        let text = layout("I", &wide, CONSOLAS, monospace, no_kern, whole_cell, false);
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
            no_kern,
            whole_cell,
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
        let laid = layout(
            text,
            &placement(left, 0.0),
            CONSOLAS,
            monospace,
            no_kern,
            whole_cell,
            false,
        );
        assert_eq!(laid.cells.len(), 2);
        let dx = cell(&laid, 1).corners[0].x - cell(&laid, 0).corners[0].x;
        assert!(close(dx, expected_dx), "{dx}");
    }

    #[test]
    fn a_character_the_browser_measured_wider_gets_a_wider_cell() {
        let advance = |c: char| if c == 'W' { 0.9 } else { 0.3 };
        // Each glyph's ink fills its own cell, so neither overhangs.
        let text = layout(
            "iW",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            advance,
            no_kern,
            |c| Ink::whole_cell(advance(c), CONSOLAS),
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
            no_kern,
            whole_cell,
            false,
        );
        let mirror = Justify {
            mirror: true,
            ..Justify::default()
        };
        let mirrored = layout(
            "I",
            &placement(mirror, 0.0),
            CONSOLAS,
            monospace,
            no_kern,
            whole_cell,
            false,
        );
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
            no_kern,
            whole_cell,
            false,
        );
        let turned = layout(
            "I",
            &placement(Justify::default(), 90.0),
            CONSOLAS,
            monospace,
            no_kern,
            whole_cell,
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
            no_kern,
            whole_cell,
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

    fn knockout_box(text: &OutlineText) -> [Point; 4] {
        text.knockout_box
            .unwrap_or_else(|| panic!("no knockout box in {text:?}"))
    }

    #[test]
    fn a_knockout_is_the_ink_grown_by_the_margin_tiled_by_the_cropped_cells_and_the_strips() {
        let text = layout(
            "I I\nII",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            no_kern,
            capital,
            true,
        );
        assert_eq!(text.cells.len(), 4);
        assert_is_not_empty!(text.strips);
        // Pen 0.15: the margin is a ninth of the height, which beats half
        // the pen, and the box grows by the margin alone around the ink.
        // The lines' baselines are 19.575 and 21.255 (the block of 1.17
        // and one interline of 1.68 centred on 20); the first line's
        // three cells start at 8.845, its I's ink 0.07 in from that.
        let margin = 1.0 / 9.0;
        let outer = knockout_box(&text);
        assert_point(outer[0], 8.845 + 0.07 - margin, 19.575 - 0.896 - margin);
        assert_point(outer[2], 11.155 - 0.07 + margin, 21.255 + margin);
        // No cell reaches outside the box: the cells are cropped to it.
        // The first line's cells lose their tops, the second line's their
        // bottoms, and each samples the same part of its glyph.
        for cell in &text.cells {
            for corner in cell.corners {
                assert!(
                    corner.x >= outer[0].x - 1e-9
                        && corner.x <= outer[2].x + 1e-9
                        && corner.y >= outer[0].y - 1e-9
                        && corner.y <= outer[2].y + 1e-9,
                    "{cell:?} reaches outside {outer:?}"
                );
            }
        }
        let first = cell(&text, 0);
        assert!(close(first.corners[0].y, outer[0].y), "{first:?}");
        assert!(close(first.crop.min.y, 0.1 - margin / 1.4), "{first:?}");
        assert!(close(first.crop.max.y, 1.0) && close(first.crop.min.x, 0.0));
        let last = cell(&text, 3);
        assert!(close(last.corners[3].y, outer[2].y), "{last:?}");
        assert!(close(last.crop.max.y, 0.74 + margin / 1.4), "{last:?}");
        // The cells and the strips tile the box: their areas sum to it,
        // and no strip overlaps a cell.
        let cells = text.cells.iter().map(|cell| cell.corners);
        let box_area = area(&outer);
        let tiled: f64 = cells.clone().map(|c| area(&c)).sum::<f64>()
            + text.strips.iter().map(area).sum::<f64>();
        assert!(close(tiled, box_area), "{tiled} vs {box_area}");
        assert!(close(
            bounds_of(cells.clone().chain(text.strips.iter().copied())).width(),
            outer[1].x - outer[0].x
        ));
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

    #[test_case("I"; "no spaces")]
    #[test_case(" I"; "a leading space")]
    #[test_case("I \t"; "a trailing space and tab")]
    #[test_case("  \nI"; "a line of spaces alone above")]
    #[test_case("I\n   "; "a line of spaces alone below")]
    fn spaces_at_the_ends_of_a_knockout_line_do_not_widen_its_box(text: &str) {
        let laid = layout(
            text,
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            no_kern,
            capital,
            true,
        );
        // The I's ink, 0.45 em by 0.64 em, and the margin around it.
        let outer = knockout_box(&laid);
        let margin = 2.0 / 9.0;
        let (width, height) = (outer[1].x - outer[0].x, outer[3].y - outer[0].y);
        assert!(close(width, 0.45 * 1.4 + margin), "{width}");
        assert!(close(height, 0.64 * 1.4 + margin), "{height}");
        let tiled: f64 = laid
            .cells
            .iter()
            .map(|cell| area(&cell.corners))
            .sum::<f64>()
            + laid.strips.iter().map(area).sum::<f64>();
        assert!(close(tiled, width * height), "{tiled}");
    }

    #[test_case("   "; "spaces alone")]
    #[test_case("I"; "a glyph without ink")]
    fn a_knockout_without_ink_has_no_box(text: &str) {
        let laid = layout(
            text,
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            no_kern,
            |_| Ink::default(),
            true,
        );
        assert_eq!(laid, OutlineText::default());
    }

    #[test]
    fn a_plain_texts_cells_are_whole() {
        let laid = layout(
            "I",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            no_kern,
            capital,
            false,
        );
        assert_eq!(cell(&laid, 0).crop, WHOLE_CELL);
        assert_eq!(laid.knockout_box, None);
    }

    #[test]
    fn a_mirrored_knockouts_box_is_the_plain_ones_reflected_about_the_position() {
        let left = Justify {
            horizontal: Horizontal::Left,
            vertical: Vertical::Center,
            mirror: false,
        };
        let mirrored = Justify {
            mirror: true,
            ..left
        };
        let plain = layout(
            "IW",
            &placement(left, 0.0),
            CONSOLAS,
            monospace,
            no_kern,
            capital,
            true,
        );
        let flipped = layout(
            "IW",
            &placement(mirrored, 0.0),
            CONSOLAS,
            monospace,
            no_kern,
            capital,
            true,
        );
        // Corner for corner in the text's reading order, x reflected about
        // the position's 10 and y kept: the box's top left lands on the
        // right, as a mirrored cell's does.
        for (a, b) in knockout_box(&plain).iter().zip(knockout_box(&flipped)) {
            assert_point(b, 20.0 - a.x, a.y);
        }
        assert!(knockout_box(&flipped)[0].x > 10.0, "{flipped:?}");
    }

    /// A proportional face: an f 0.3 em wide, everything else
    /// [`ADVANCE`].
    fn proportional(c: char) -> f64 {
        if c == 'f' { 0.3 } else { ADVANCE }
    }

    /// An italic f's ink, reaching 0.1 em left of its pen and 0.4 em
    /// past its 0.3 em advance; every other glyph a [`capital`].
    fn italic_f(c: char) -> Ink {
        if c == 'f' {
            Ink {
                left: 0.1,
                right: 0.7,
                ascent: 0.7,
                descent: 0.2,
            }
        } else {
            capital(c)
        }
    }

    /// A face that kerns every pair by -0.1 em.
    fn kern_all(_: char, _: char) -> f64 {
        -0.1
    }

    fn left_justified() -> Justify {
        Justify {
            horizontal: Horizontal::Left,
            vertical: Vertical::Center,
            mirror: false,
        }
    }

    #[test_case(capital('I'), ADVANCE, 0.0, 0.0; "ink inside the advance")]
    #[test_case(italic_f('f'), 0.3, 0.1, 0.4; "ink past both sides")]
    #[test_case(Ink::default(), 0.3, 0.0, 0.0; "no ink")]
    fn the_overhang_is_how_far_the_ink_reaches_past_the_advance(
        ink: Ink,
        advance: f64,
        left: f64,
        right: f64,
    ) {
        let overhang = ink.overhang(advance);
        assert!(
            close(overhang.left, left) && close(overhang.right, right),
            "{overhang:?}"
        );
        assert!(close(overhang.quad_width(advance), left + advance + right));
    }

    #[test]
    fn a_glyph_whose_ink_overhangs_its_advance_has_a_quad_that_reaches_it_and_the_pen_moves_by_the_advance()
     {
        let text = layout(
            "fI",
            &placement(left_justified(), 0.0),
            CONSOLAS,
            proportional,
            no_kern,
            italic_f,
            false,
        );
        // The pen starts at 10: the f's quad reaches 0.1 em left of it
        // and 0.7 em right, at 1.4 mm to the em, and is drawn whole.
        let f = cell(&text, 0);
        assert!(close(f.corners[0].x, 10.0 - 0.14), "{f:?}");
        assert!(close(f.corners[1].x, 10.0 + 0.98), "{f:?}");
        assert_eq!((f.crop, f.neighbour), (WHOLE_CELL, None));
        // The I's pen is the f's advance on, so the quads overlap.
        let i = cell(&text, 1).corners;
        assert!(close(i[0].x, 10.0 + 0.42), "{i:?}");
        assert!(close(i[1].x, 10.0 + 0.42 + ADVANCE * 1.4), "{i:?}");
    }

    #[test_case("AV", (ADVANCE - 0.1) * 1.4; "a kerned pair sits closer by the kern")]
    #[test_case("A\tV", 3.0 * 1.4 + ADVANCE * 1.4; "a tab is kerned on neither side")]
    fn the_kern_moves_the_second_glyph_of_a_pair(text: &str, expected_dx: f64) {
        let laid = layout(
            text,
            &placement(left_justified(), 0.0),
            CONSOLAS,
            monospace,
            kern_all,
            capital,
            false,
        );
        let dx = cell(&laid, 1).corners[0].x - cell(&laid, 0).corners[0].x;
        assert!(close(dx, expected_dx), "{dx}");
    }

    #[test]
    fn a_centred_kerned_pair_is_centred_on_its_kerned_width() {
        let text = layout(
            "AV",
            &placement(Justify::default(), 0.0),
            CONSOLAS,
            monospace,
            kern_all,
            capital,
            false,
        );
        // Two advances less the kern, 1.0 em or 1.4 mm, centred on 10.
        let (a, v) = (cell(&text, 0).corners, cell(&text, 1).corners);
        assert!(
            close(a[0].x, 10.0 - 0.7) && close(v[1].x, 10.0 + 0.7),
            "{text:?}"
        );
    }

    /// Whether two upright quads share more than an edge.
    fn overlap(a: &[Point; 4], b: &[Point; 4]) -> bool {
        let overlap_x = a[1].x.min(b[1].x) - a[0].x.max(b[0].x);
        let overlap_y = a[3].y.min(b[3].y) - a[0].y.max(b[0].y);
        overlap_x > 1e-9 && overlap_y > 1e-9
    }

    #[test_case("II", monospace, no_kern, capital, &[('I', None), ('I', None)]; "a monospace pair, one cell each")]
    #[test_case("fI", proportional, no_kern, italic_f, &[('f', None), ('f', Some('I')), ('I', None)]; "an f overhanging the I after it")]
    #[test_case("AV", monospace, kern_all, capital, &[('A', None), ('A', Some('V')), ('V', None)]; "a kerned pair")]
    fn a_knockouts_cells_do_not_overlap_and_name_each_glyph_whose_quad_covers_them(
        text: &str,
        advance: fn(char) -> f64,
        kern: fn(char, char) -> f64,
        ink: fn(char) -> Ink,
        expected: &[(char, Option<char>)],
    ) {
        let laid = layout(
            text,
            &placement(left_justified(), 0.0),
            CONSOLAS,
            advance,
            kern,
            ink,
            true,
        );
        let named: Vec<(char, Option<char>)> = laid
            .cells
            .iter()
            .map(|cell| (cell.c, cell.neighbour.map(|n| n.c)))
            .collect();
        assert_eq!(named, expected);
        // A shared piece runs from where the neighbour's quad starts to
        // where the glyph's ends.
        for cell in &laid.cells {
            if let Some(neighbour) = cell.neighbour {
                assert!(close(cell.crop.max.x, 1.0), "{cell:?}");
                assert!(close(neighbour.crop.min.x, 0.0), "{cell:?}");
            }
        }
        // No inverted cell over another, none outside the box, and the
        // cells and strips tile the box.
        for (index, a) in laid.cells.iter().enumerate() {
            for b in laid.cells.iter().skip(index + 1) {
                assert!(!overlap(&a.corners, &b.corners), "{a:?} overlaps {b:?}");
            }
            for strip in &laid.strips {
                assert!(!overlap(&a.corners, strip), "{a:?} overlaps {strip:?}");
            }
        }
        let outer = knockout_box(&laid);
        for corner in laid.cells.iter().flat_map(|cell| cell.corners) {
            assert!(
                corner.x >= outer[0].x - 1e-9
                    && corner.x <= outer[2].x + 1e-9
                    && corner.y >= outer[0].y - 1e-9
                    && corner.y <= outer[2].y + 1e-9,
                "{corner:?} is outside {outer:?}"
            );
        }
        let tiled: f64 = laid
            .cells
            .iter()
            .map(|cell| area(&cell.corners))
            .sum::<f64>()
            + laid.strips.iter().map(area).sum::<f64>();
        assert!(close(tiled, area(&outer)), "{tiled} vs {}", area(&outer));
    }

    #[test]
    fn a_knockouts_box_is_the_ink_grown_by_the_margin_whatever_the_overhang() {
        // The f's ink reaches 0.1 em left of the pen at 10 and 0.7 em
        // right; the I's ends 0.05 em short of its cell, 0.3 + 0.5 em on.
        let laid = layout(
            "fI",
            &placement(left_justified(), 0.0),
            CONSOLAS,
            proportional,
            no_kern,
            italic_f,
            true,
        );
        let margin = 1.0 / 9.0;
        let outer = knockout_box(&laid);
        assert!(close(outer[0].x, 10.0 - 0.14 - margin), "{outer:?}");
        assert!(close(outer[1].x, 10.0 + 0.8 * 1.4 + margin), "{outer:?}");
    }

    const BOARD_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_pcb"
    );

    /// The points of each `render_cache` polygon of the footprint
    /// reference `reference` in the hand-off board's source.
    fn render_cache(source: &str, reference: &str) -> Vec<Vec<Point>> {
        let root = crate::sexpr::read(source).expect("reads");
        let property = root
            .children("footprint")
            .flat_map(|footprint| footprint.children("property"))
            .find(|property| {
                property.atom_opt(0) == Some("Reference") && property.atom_opt(1) == Some(reference)
            })
            .unwrap_or_else(|| panic!("no reference {reference}"));
        property
            .require("render_cache")
            .expect("a render cache")
            .children("polygon")
            .map(|polygon| {
                polygon
                    .require("pts")
                    .expect("points")
                    .children("xy")
                    .map(|xy| Point {
                        x: xy.number(0).expect("x"),
                        y: xy.number(1).expect("y"),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_turned_knockouts_box_is_where_kicads_plot_draws_it() {
        // C7 on the hand-off board: Consolas, size 1, pen 0.15, centred,
        // turned 45 degrees, and the one turned knockout whose plotted
        // box touches no other silkscreen.
        let source = std::fs::read_to_string(BOARD_PATH).expect("the hand-off board");
        let board = crate::parse_board(&source).expect("parses");
        let silk = board.layer_named("F.SilkS").expect("a silkscreen");
        let (content, placement, _) = super::super::placed_texts(&board, silk)
            .find(|(content, _, text)| content == "C7" && text.knockout)
            .expect("C7's reference");
        assert!(close(placement.angle, 45.0), "{placement:?}");
        // Each glyph's ink is its render_cache polygon, turned back into
        // the text's frame, from its pen position on the baseline: two
        // cells of ADVANCE em centred on the anchor, the baseline 0.415
        // below it. The layout puts each ink back where its polygon was,
        // so the advance used here cancels out.
        let polygons = render_cache(&source, "C7");
        assert_eq!(polygons.len(), 2, "one polygon per glyph");
        let unturn = Transform::new(Point::default(), -placement.angle);
        let em = EM_PER_HEIGHT;
        let baseline = 1.0 - 1.17 / 2.0;
        let inks: Vec<Ink> = [-1.0, 0.0]
            .into_iter()
            .zip(&polygons)
            .map(|(cells_from_anchor, polygon)| {
                let local = Bounds::around(polygon.iter().map(|p| {
                    unturn.rotate(Point {
                        x: p.x - placement.position.x,
                        y: p.y - placement.position.y,
                    })
                }))
                .expect("points");
                let pen = cells_from_anchor * ADVANCE * em;
                Ink {
                    left: (pen - local.min.x) / em,
                    right: (local.max.x - pen) / em,
                    ascent: (baseline - local.min.y) / em,
                    descent: (local.max.y - baseline) / em,
                }
            })
            .collect();
        let ink = |c: char| match c {
            'C' => inks.first().copied().unwrap_or_default(),
            '7' => inks.get(1).copied().unwrap_or_default(),
            _ => Ink::default(),
        };
        let laid = layout(
            &content, &placement, CONSOLAS, monospace, no_kern, ink, true,
        );
        // KiCad's plot viewer-drop/exports/midi8knob-F_Silkscreen.svg
        // draws the box with these corners, 50 mm short of the board's
        // in both axes and rounded to 0.1 um: the text's top left, top
        // right, bottom right and bottom left.
        let plotted = [
            (49.4237, 26.0044),
            (50.5637, 24.8645),
            (51.3674, 25.6682),
            (50.2275, 26.8081),
        ];
        let outer = knockout_box(&laid);
        for (corner, (x, y)) in outer.iter().zip(plotted) {
            assert!(
                (corner.x - 50.0 - x).abs() <= 0.5e-4 && (corner.y - 50.0 - y).abs() <= 0.5e-4,
                "{outer:?} is not the plot's {plotted:?}"
            );
        }
    }
}
