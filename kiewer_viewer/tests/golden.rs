//! The golden images: the hand-off board rendered headless, page by page,
//! against the PNGs the hand-off's pipeline made of the same views
//! (`viewer-drop/exports/png`, kicad-cli and rsvg). The test is not
//! pixel equality, since the reference differs from the viewer in ways
//! that are not drift: it draws net labels in every pad and via, its
//! silkscreen text is Consolas where the viewer draws Newstroke, its
//! anti-aliasing is rsvg's coverage where the viewer's is 4x
//! multisampling, and a translucent layer is composited as a group
//! where the viewer blends shape by shape (#56). So the metric is a
//! count: inside each view's tile, a pixel is off when its colour is
//! more than [`CHANNEL_TOLERANCE`] away on some channel from the
//! reference's pixel there and from every reference pixel one step
//! around it (the step forgives an edge anti-aliased differently or
//! placed a fraction of a pixel apart); the page fails when the off
//! pixels exceed its budget in [`BUDGETS`], set about one point above
//! what the sample board measured (the floor is in each line there, the
//! measurement in the done-note of #7). Inside that margin a change of
//! colour on a layer that covers an area, copper, mask, board fill or a
//! hole rim, blows through on the pages that show it, as does a missing
//! layer, a shift or a mirror; a tint change on the silkscreen text or
//! the paste inside the pads does not, since those pixels are already
//! in the floor under the labels and the other face. Outside the tiles
//! (the margins and the header band, where the hand-off writes the
//! title) the pages are not compared.
//!
//! Needs a Vulkan driver: lavapipe on a box without a GPU. Without one
//! the tests skip with a line saying so, except on CI (`CI` set), where
//! the runner installs lavapipe and a skip would hide a broken install.
//! On a failure the render and a picture of where it differs land under
//! `target/tmp/golden/`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use kiewer_viewer::headless::hand_off::{self, Page, Rendered};
use kiewer_viewer::headless::{Frame, Headless};
use kiewer_viewer::render::offscreen::Image;
use kiewer_viewer::scene::Scene;
use kiewer_viewer::view::{Layer, ViewState};

const BOARD_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_pcb"
);
const REFERENCE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../viewer-drop/exports/png");

/// How far a channel may differ before the pixel counts as off: the
/// anti-aliasing of an edge lands on both sides of this, a layer in a
/// different colour does not (the closest two palette colours are 20
/// apart on their nearest channel).
const CHANNEL_TOLERANCE: u8 = 8;

/// The fraction of each page's tile pixels that may be off, by file,
/// each about one point over the floor the sample board measured on
/// lavapipe (Mesa 25.2.8), noted beside it. The floors differ by what
/// each view shows: the views with both coppers carry the pad labels of
/// both sides, the compounded translucent copper (#56) and the courtyard
/// lines, a thin line being off along its whole length.
const BUDGETS: [(&str, f64); 9] = [
    ("01-F_Cu_F_Paste.png", 0.065),                              // 5.54%
    ("02-In1_Cu.png", 0.030),                                    // 1.82%
    ("03-In2_Cu.png", 0.030),                                    // 1.83%
    ("04-B_Cu.png", 0.035),                                      // 2.59%
    ("05-F_Cu_F_Paste_F_Mask_F_SilkS.png", 0.085),               // 7.19%
    ("06-B_Cu_B_Paste_B_Mask_B_SilkS_mirrored.png", 0.030),      // 2.10%
    ("07-F_Cu_B_Cu_F_SilkS_B_SilkS_F_CrtYd_B_CrtYd.png", 0.130), // 11.69%
    ("08-B_Cu_F_Cu_B_SilkS_F_SilkS_B_CrtYd_F_CrtYd.png", 0.115), // 10.04%
    ("midi8knob-layers.png", 0.085),                             // 7.40%
];

/// The budget of the page named `name`.
fn budget(name: &str) -> f64 {
    BUDGETS
        .iter()
        .find(|(budgeted, _)| *budgeted == name)
        .map_or_else(
            || panic!("{name} has no budget in BUDGETS; measure it and add a line"),
            |(_, budget)| *budget,
        )
}

fn scene() -> &'static Scene {
    static SCENE: OnceLock<Scene> = OnceLock::new();
    SCENE.get_or_init(|| {
        let source =
            std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo");
        let board = kicad_core::parse_board(&source).expect("the hand-off board parses");
        Scene::build(&board).expect("a board with an outline")
    })
}

/// A headless renderer, or `None` with the reason printed when the box
/// has no Vulkan driver; on CI that is a failure, not a skip.
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

fn reference(name: &str) -> Image {
    let path = Path::new(REFERENCE_DIR).join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    Image::from_png(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Where a failed comparison writes what it rendered, for a look:
/// cargo's scratch directory.
fn dump(name: &str, image: &Image) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("golden");
    std::fs::create_dir_all(&dir).expect("cargo's scratch directory is writable");
    let path = dir.join(name);
    std::fs::write(&path, image.to_png().expect("encodes")).expect("writes");
    path
}

/// The largest difference over the colour channels of two pixels.
fn channel_difference(a: [u8; 4], e: [u8; 4]) -> u8 {
    a.iter()
        .zip(e.iter())
        .take(3)
        .map(|(a, e)| a.abs_diff(*e))
        .max()
        .unwrap_or(0)
}

/// Whether the pixel at (`x`, `y`) of `actual` is off: it differs by
/// more than the tolerance from the reference's pixel there and from
/// every reference pixel one step around it. The step forgives the
/// edge of a shape, which the two rasterisers anti-alias differently
/// and place a fraction of a pixel apart; it forgives nothing a pixel
/// wide or more, so a layer in another colour, a missing layer, a shift
/// or a mirror still count.
fn is_off(actual: &Image, expected: &Image, x: u32, y: u32) -> bool {
    let Some(a) = actual.pixel(x, y) else {
        return false;
    };
    let near = |dx: i64, dy: i64| {
        let x = u32::try_from(i64::from(x) + dx).ok()?;
        let y = u32::try_from(i64::from(y) + dy).ok()?;
        expected.pixel(x, y)
    };
    (-1..=1).all(|dy| {
        (-1..=1).all(|dx| near(dx, dy).is_none_or(|e| channel_difference(a, e) > CHANNEL_TOLERANCE))
    })
}

/// What a comparison found inside the tiles of one page.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Verdict {
    /// Pixels compared.
    compared: u64,
    /// Pixels off by more than the tolerance on some channel.
    off: u64,
    /// The mean over the compared pixels of the largest channel difference.
    mean: f64,
}

impl Verdict {
    fn off_fraction(&self) -> f64 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "pixel counts are in the millions, far below 2^52"
        )]
        let fraction = self.off as f64 / self.compared.max(1) as f64;
        fraction
    }
}

/// Compare `actual` with `expected` inside the tiles of `page`. The two
/// may differ by a pixel in size: the hand-off's page is laid out around
/// kicad-cli's box of the outline, which falls a hundredth of a
/// millimetre short of the outline's own (its arcs are polygonised), and
/// at 30 px/mm that is the last pixel of the width; the pixels the
/// smaller page lacks are not compared.
fn compare(page: &Page, actual: &Image, expected: &Image) -> Verdict {
    assert!(
        actual.width.abs_diff(expected.width) <= 1 && actual.height.abs_diff(expected.height) <= 1,
        "the page is {} by {}, the reference {} by {}",
        actual.width,
        actual.height,
        expected.width,
        expected.height
    );
    let mut verdict = Verdict {
        compared: 0,
        off: 0,
        mean: 0.0,
    };
    let mut total: u64 = 0;
    for tile in &page.tiles {
        let (left, top, right, bottom) = tile.pixel_box();
        for y in top..bottom {
            for x in left..right {
                let (Some(a), Some(e)) = (actual.pixel(x, y), expected.pixel(x, y)) else {
                    continue;
                };
                let difference = channel_difference(a, e);
                verdict.compared += 1;
                total += u64::from(difference);
                if difference > CHANNEL_TOLERANCE && is_off(actual, expected, x, y) {
                    verdict.off += 1;
                }
            }
        }
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "pixel counts are in the millions, far below 2^52"
    )]
    {
        verdict.mean = total as f64 / verdict.compared.max(1) as f64;
    }
    verdict
}

/// Where the two differ, as a picture: white where a pixel is off,
/// grey where it differs within the tolerance, black where it matches,
/// outside the tiles the page colour.
fn difference_image(page: &Page, actual: &Image, expected: &Image) -> Image {
    let (width, height) = (actual.width, actual.height);
    let mut rgba = [0x2b, 0x2b, 0x2b, 255].repeat(width as usize * height as usize);
    for tile in &page.tiles {
        let (left, top, right, bottom) = tile.pixel_box();
        for y in top..bottom.min(height) {
            for x in left..right.min(width) {
                let (Some(a), Some(e)) = (actual.pixel(x, y), expected.pixel(x, y)) else {
                    continue;
                };
                let difference = channel_difference(a, e);
                let shade = if difference > CHANNEL_TOLERANCE && is_off(actual, expected, x, y) {
                    255
                } else if difference > 0 {
                    96
                } else {
                    0
                };
                let at = (y as usize * width as usize + x as usize) * 4;
                rgba[at..at + 3].copy_from_slice(&[shade, shade, shade]);
            }
        }
    }
    Image {
        width,
        height,
        rgba,
    }
}

fn report(name: &str, verdict: &Verdict, budget: f64, elapsed: std::time::Duration) -> String {
    format!(
        "{name}: {:.3}% of {} pixels off by more than {CHANNEL_TOLERANCE} (budget {:.1}%), mean difference {:.2}, compared in {:.2}s",
        verdict.off_fraction() * 100.0,
        verdict.compared,
        budget * 100.0,
        verdict.mean,
        elapsed.as_secs_f64()
    )
}

#[test]
fn every_view_and_the_overview_match_the_hand_off_within_the_budget() {
    let Some(mut headless) = headless_or_skip() else {
        return;
    };
    let scene = scene();
    let started = Instant::now();
    let rendered = hand_off::render_all(&mut headless, scene, "midi8knob").expect("renders");
    let render_time = started.elapsed();
    assert_eq!(rendered.len(), 9, "eight views and the overview");
    let mut failures = Vec::new();
    for Rendered { name, page, image } in &rendered {
        let started = Instant::now();
        let expected = reference(name);
        let verdict = compare(page, image, &expected);
        let budget = budget(name);
        let line = report(name, &verdict, budget, started.elapsed());
        eprintln!("{line}");
        if verdict.off_fraction() > budget {
            let dumped = dump(name, image);
            let difference = dump(
                &format!("difference-{name}"),
                &difference_image(page, image, &expected),
            );
            failures.push(format!(
                "{line}; the render is at {} and where it differs at {}",
                dumped.display(),
                difference.display()
            ));
        }
    }
    eprintln!(
        "rendered the nine pages in {:.2}s on {} ({:?})",
        render_time.as_secs_f64(),
        headless.adapter().name,
        headless.adapter().device_type
    );
    assert!(
        failures.is_empty(),
        "over the budget:\n{}",
        failures.join("\n")
    );
}

#[test]
fn a_layer_in_another_colour_fails_the_comparison() {
    let Some(mut headless) = headless_or_skip() else {
        return;
    };
    let scene = scene();
    // View 5, the front as built, with the copper drawn in the KiCad
    // theme's red instead of copper yellow: the kind of palette slip
    // the comparison exists to catch.
    let page = hand_off::page(&scene.bounds, &[5], hand_off::VIEW_DENSITY);
    let tile = &page.tiles[0];
    let frame = Frame {
        width: page.width,
        height: page.height,
        center: kicad_core::model::Point {
            x: scene.bounds.center().x
                + (f64::from(page.width) / 2.0 - (tile.x + tile.width / 2.0)) / page.scale,
            y: scene.bounds.center().y
                + (f64::from(page.height) / 2.0 - (tile.y + tile.height / 2.0)) / page.scale,
        },
        scale: page.scale,
        mirrored: false,
    };
    let mut draws = scene.draw_list(
        &ViewState::new(5),
        kiewer_viewer::render::offscreen::Offscreen::FORMAT,
    );
    let copper = scene
        .slices
        .iter()
        .find(|slice| slice.layer == Layer::Copper("F.Cu".to_owned()))
        .expect("front copper")
        .range
        .clone();
    let mut recoloured = 0;
    for draw in &mut draws {
        if draw.range == copper {
            draw.color = kiewer_viewer::view::palette::F_CU
                .for_format(kiewer_viewer::render::offscreen::Offscreen::FORMAT);
            recoloured += 1;
        }
    }
    assert_eq!(recoloured, 1);
    let image = headless
        .render_draws(scene, &draws, &frame)
        .expect("renders");
    let name = "05-F_Cu_F_Paste_F_Mask_F_SilkS.png";
    let verdict = compare(&page, &image, &reference(name));
    let budget = budget(name);
    eprintln!(
        "{}",
        report(
            "recoloured copper",
            &verdict,
            budget,
            std::time::Duration::ZERO
        )
    );
    assert!(
        verdict.off_fraction() > budget,
        "the comparison let a recoloured copper layer through: {verdict:?}"
    );
}
