//! The layers a board is drawn as, the views that stack them, and the
//! state a link carries.
//!
//! A [`Layer`] is one thing the viewer can show or hide: a KiCad layer
//! (copper, mask, paste, silkscreen, courtyard, the outline) or one of the
//! two the composed views add, the board fill and the holes. A [`View`]
//! is a named stack of them with an opacity each, the hand-off's eight
//! (viewer-drop/README.md) built for whatever copper a board has:
//! [`Stack::views`]. A [`ViewState`] is a view plus the toggles and
//! opacities set on top of it, and it is what the URL fragment carries
//! ([`ViewState::fragment`], [`ViewState::from_fragment`]), so a link
//! opens on the same picture.

use std::collections::BTreeMap;
use std::fmt;

use kicad_core::model::Board;

use crate::render::Rgb;

/// The colours the views use: the hand-off's for the composed views, the
/// KiCad default theme's for the layer views (viewer-drop/README.md).
pub mod palette {
    use super::Rgb;

    /// The page behind the board.
    pub const PAGE: Rgb = Rgb(0x2b, 0x2b, 0x2b);
    /// Bare FR4, the board fill of the composed views.
    pub const BOARD: Rgb = Rgb(0x7a, 0x5a, 0x32);
    /// Copper as it looks: the composed views' copper and every hole's rim.
    pub const COPPER: Rgb = Rgb(0xf0, 0xc0, 0x30);
    /// Solder paste.
    pub const PASTE: Rgb = Rgb(0xc9, 0xc9, 0xc9);
    /// Solder mask, drawn at [`MASK_OPACITY`] so the copper shows through.
    pub const MASK: Rgb = Rgb(0x1f, 0x8a, 0x3f);
    /// The mask overlay's opacity in the composed views.
    pub const MASK_OPACITY: f32 = 0.7;
    /// The board outline, `Edge.Cuts`.
    pub const OUTLINE: Rgb = Rgb(0xd0, 0xd2, 0xcd);
    /// The front copper in the KiCad default theme.
    pub const F_CU: Rgb = Rgb(0xc8, 0x34, 0x34);
    /// The back copper in the KiCad default theme.
    pub const B_CU: Rgb = Rgb(0x4d, 0x7f, 0xc4);
    /// The inner copper layers in the KiCad default theme, `In1.Cu` first;
    /// a deeper layer takes the colour of its index modulo these. The
    /// hand-off README vouches for the first two.
    pub const INNER_CU: [Rgb; 4] = [
        Rgb(0x7f, 0xc8, 0x7f),
        Rgb(0xce, 0x7d, 0x2c),
        Rgb(0x4f, 0xcb, 0xcb),
        Rgb(0xdb, 0x62, 0x8b),
    ];
    /// The front silkscreen in the KiCad default theme.
    pub const F_SILKS: Rgb = Rgb(0xf2, 0xed, 0xa1);
    /// The back silkscreen in the KiCad default theme.
    pub const B_SILKS: Rgb = Rgb(0xe8, 0xb2, 0xa7);
    /// The front courtyard in the KiCad default theme.
    pub const F_CRTYD: Rgb = Rgb(0xff, 0x26, 0xe2);
    /// The back courtyard in the KiCad default theme.
    pub const B_CRTYD: Rgb = Rgb(0x26, 0xe9, 0xff);
}

/// A side of the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Front,
    Back,
}

impl Side {
    /// The other side.
    #[must_use]
    pub fn other(self) -> Side {
        match self {
            Side::Front => Side::Back,
            Side::Back => Side::Front,
        }
    }

    /// The letter KiCad's layer names start with: `F` or `B`.
    fn letter(self) -> char {
        match self {
            Side::Front => 'F',
            Side::Back => 'B',
        }
    }

    fn of_letter(letter: &str) -> Option<Side> {
        match letter {
            "F" => Some(Side::Front),
            "B" => Some(Side::Back),
            _ => None,
        }
    }
}

/// One thing the viewer shows or hides. Its [`fmt::Display`] is its key in
/// the fragment and the panel: the KiCad canonical layer name for a
/// KiCad layer, `board` and `holes` for the two of the viewer's own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Layer {
    /// The board itself, filled: the composed views' FR4.
    Board,
    /// The `Edge.Cuts` strokes: the layer views' bare outline.
    Outline,
    /// A copper layer, by its canonical name (`F.Cu`, `In1.Cu`, `B.Cu`).
    Copper(String),
    /// The solder mask overlay of a side: the board in mask colour with
    /// the openings cut out.
    Mask(Side),
    /// The solder paste of a side.
    Paste(Side),
    /// The silkscreen of a side, graphics and text.
    Silk(Side),
    /// The courtyards of a side.
    Courtyard(Side),
    /// Every drilled hole in the page colour, a copper rim around the
    /// plated ones.
    Holes,
}

impl Layer {
    /// The layer a fragment or panel key names, or `None` for a key that
    /// is none of them. Any `*.Cu` is taken as copper; whether the board
    /// has it is the stack's business.
    #[must_use]
    pub fn parse(key: &str) -> Option<Layer> {
        match key {
            "board" => return Some(Layer::Board),
            "holes" => return Some(Layer::Holes),
            "Edge.Cuts" => return Some(Layer::Outline),
            _ => {}
        }
        let (side, kind) = key.split_once('.')?;
        if kind == "Cu" {
            return Some(Layer::Copper(key.to_owned()));
        }
        let side = Side::of_letter(side)?;
        match kind {
            "Mask" => Some(Layer::Mask(side)),
            "Paste" => Some(Layer::Paste(side)),
            "SilkS" => Some(Layer::Silk(side)),
            "CrtYd" => Some(Layer::Courtyard(side)),
            _ => None,
        }
    }

    /// The side a layer belongs to; `None` for the board, the outline,
    /// the holes and an inner copper layer.
    #[must_use]
    pub fn side(&self) -> Option<Side> {
        match self {
            Layer::Board | Layer::Outline | Layer::Holes => None,
            Layer::Copper(name) => Side::of_letter(name.split_once('.')?.0),
            Layer::Mask(side) | Layer::Paste(side) | Layer::Silk(side) | Layer::Courtyard(side) => {
                Some(*side)
            }
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Layer::Board => f.write_str("board"),
            Layer::Outline => f.write_str("Edge.Cuts"),
            Layer::Copper(name) => f.write_str(name),
            Layer::Mask(side) => write!(f, "{}.Mask", side.letter()),
            Layer::Paste(side) => write!(f, "{}.Paste", side.letter()),
            Layer::Silk(side) => write!(f, "{}.SilkS", side.letter()),
            Layer::Courtyard(side) => write!(f, "{}.CrtYd", side.letter()),
            Layer::Holes => f.write_str("holes"),
        }
    }
}

/// A board's layers as the viewer stacks them: the copper from front to
/// back, which is what differs from board to board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// The copper layers' canonical names in stack order, `F.Cu` first
    /// and `B.Cu` last when the board has them.
    pub copper: Vec<String>,
}

impl Stack {
    /// The stack of `board`: its copper layers in file order, which KiCad
    /// writes front to back.
    #[must_use]
    pub fn of(board: &Board) -> Self {
        Self {
            copper: board
                .copper_layers()
                .map(|(_, layer)| layer.name.clone())
                .collect(),
        }
    }

    /// Every layer in draw order, bottom first, with `top`'s layers over
    /// the other side's: the board, the outline, the copper from the far
    /// side to the near one, then mask, paste, silkscreen and courtyard,
    /// far side before near, and the holes last. By kind rather than by
    /// physical depth, as the hand-off composes its views: every
    /// silkscreen over every copper.
    #[must_use]
    pub fn order(&self, top: Side) -> Vec<Layer> {
        let far = top.other();
        let mut layers = vec![Layer::Board, Layer::Outline];
        let copper = self.copper.iter().cloned().map(Layer::Copper);
        match top {
            Side::Front => layers.extend(copper.rev()),
            Side::Back => layers.extend(copper),
        }
        for kind in [Layer::Mask, Layer::Paste, Layer::Silk, Layer::Courtyard] {
            layers.push(kind(far));
            layers.push(kind(top));
        }
        layers.push(Layer::Holes);
        layers
    }

    /// The copper layer on `side`, by name, when the board has one.
    fn copper_on(&self, side: Side) -> Option<Layer> {
        let name = format!("{}.Cu", side.letter());
        self.copper.contains(&name).then_some(Layer::Copper(name))
    }

    /// The views of this stack, the hand-off's eight for a four-layer
    /// board: one per copper layer over the bare outline (the front with
    /// its paste), the front and the back as built (board, copper under
    /// the mask, paste, silkscreen, holes; the back mirrored), and both
    /// coppers with both silkscreens and courtyards, front on top and
    /// then back on top.
    #[must_use]
    pub fn views(&self) -> Vec<View> {
        let mut views = Vec::new();
        for name in &self.copper {
            let copper = Layer::Copper(name.clone());
            let side = copper.side();
            let mut layers = vec![(Layer::Outline, 1.0), (copper, 1.0)];
            let title = match side {
                Some(Side::Front) => {
                    layers.push((Layer::Paste(Side::Front), 1.0));
                    format!("{name} + F.Paste")
                }
                _ => name.clone(),
            };
            views.push(View {
                title,
                top: side.unwrap_or(Side::Front),
                mirrored: false,
                as_built: false,
                layers: layers.into_iter().collect(),
            });
        }
        for side in [Side::Front, Side::Back] {
            let letter = side.letter();
            let mut layers = vec![
                (Layer::Board, 1.0),
                (Layer::Mask(side), palette::MASK_OPACITY),
                (Layer::Paste(side), 1.0),
                (Layer::Silk(side), 1.0),
                (Layer::Holes, 1.0),
            ];
            layers.extend(self.copper_on(side).map(|copper| (copper, 1.0)));
            let mirrored = side == Side::Back;
            views.push(View {
                title: format!(
                    "{letter}.Cu + {letter}.Paste + {letter}.Mask + {letter}.SilkS{}",
                    if mirrored { ", mirrored" } else { "" }
                ),
                top: side,
                mirrored,
                as_built: true,
                layers: layers.into_iter().collect(),
            });
        }
        for top in [Side::Front, Side::Back] {
            let far = top.other();
            let (t, f) = (top.letter(), far.letter());
            let mut layers = vec![
                (Layer::Outline, 1.0),
                (Layer::Silk(far), 1.0),
                (Layer::Silk(top), 1.0),
                (Layer::Courtyard(far), 1.0),
                (Layer::Courtyard(top), 1.0),
                (Layer::Holes, 1.0),
            ];
            layers.extend(self.copper_on(far).map(|copper| (copper, 1.0)));
            // The near copper is translucent so the far one shows through.
            layers.extend(self.copper_on(top).map(|copper| (copper, 0.8)));
            views.push(View {
                title: format!("{t}.Cu + {f}.Cu + {t}.SilkS + {f}.SilkS + {t}.CrtYd + {f}.CrtYd"),
                top,
                mirrored: false,
                as_built: false,
                layers: layers.into_iter().collect(),
            });
        }
        views
    }

    /// The view a page opens on when the link names none: the front as
    /// built, the first of the composed views after one per copper layer.
    #[must_use]
    pub fn default_view(&self) -> usize {
        self.copper.len() + 1
    }

    /// The theme colour of the copper layer named `name`: the front's,
    /// the back's, or an inner layer's by its position.
    fn copper_theme(&self, name: &str) -> Rgb {
        match name {
            "F.Cu" => palette::F_CU,
            "B.Cu" => palette::B_CU,
            _ => {
                let inner = self
                    .copper
                    .iter()
                    .filter(|layer| layer.as_str() != "F.Cu")
                    .position(|layer| layer == name)
                    .unwrap_or(0);
                palette::INNER_CU[inner % palette::INNER_CU.len()]
            }
        }
    }
}

/// A named stack of layers with an opacity each: one of the hand-off's
/// views, for one board.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// The hand-off's header: `F.Cu + F.Paste`.
    pub title: String,
    /// The side whose layers draw over the other side's.
    pub top: Side,
    /// Flipped left to right: seen from the back, so the back's text
    /// reads normally.
    pub mirrored: bool,
    /// Copper on the top side drawn as it looks under a mask (copper
    /// colour) rather than in the theme's colour for the layer.
    pub as_built: bool,
    /// The layers shown and their opacity; a layer not here is hidden.
    pub layers: BTreeMap<Layer, f32>,
}

impl View {
    /// The colour `layer` draws in under this view, in `stack`.
    #[must_use]
    pub fn color(&self, stack: &Stack, layer: &Layer) -> Rgb {
        match layer {
            Layer::Board => palette::BOARD,
            Layer::Outline => palette::OUTLINE,
            Layer::Copper(name) => {
                if self.as_built && layer.side() == Some(self.top) {
                    palette::COPPER
                } else {
                    stack.copper_theme(name)
                }
            }
            Layer::Mask(_) => palette::MASK,
            Layer::Paste(_) => palette::PASTE,
            Layer::Silk(Side::Front) => palette::F_SILKS,
            Layer::Silk(Side::Back) => palette::B_SILKS,
            Layer::Courtyard(Side::Front) => palette::F_CRTYD,
            Layer::Courtyard(Side::Back) => palette::B_CRTYD,
            Layer::Holes => palette::COPPER,
        }
    }
}

/// The key of the view number in the fragment.
const VIEW_KEY: &str = "view";

/// A view with toggles and opacities set on top of it: what the panel
/// edits and the fragment carries.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewState {
    /// The view, numbered from 1 as the hand-off numbers them.
    pub view: usize,
    /// What differs from the view: a layer shown at an opacity, or
    /// hidden (`None`). A layer set back to the view's own value leaves
    /// the map.
    pub overrides: BTreeMap<Layer, Option<f32>>,
}

impl ViewState {
    /// View `view` as it is.
    #[must_use]
    pub fn new(view: usize) -> Self {
        Self {
            view,
            overrides: BTreeMap::new(),
        }
    }

    /// The opacity `layer` draws at under `view` (the one this state
    /// names) with the overrides applied, or `None` when hidden.
    #[must_use]
    pub fn opacity(&self, view: &View, layer: &Layer) -> Option<f32> {
        match self.overrides.get(layer) {
            Some(opacity) => *opacity,
            None => view.layers.get(layer).copied(),
        }
    }

    /// Show `layer` at `opacity` (rounded to a hundredth and clamped to
    /// 0..=1, what the fragment can carry), or hide it with `None`. A
    /// setting equal to `view`'s own is no override.
    pub fn set(&mut self, view: &View, layer: Layer, opacity: Option<f32>) {
        let opacity = opacity.map(round_opacity);
        if view.layers.get(&layer).copied() == opacity {
            self.overrides.remove(&layer);
        } else {
            self.overrides.insert(layer, opacity);
        }
    }

    /// The fragment for this state, without the `#`: `view=5`, then one
    /// `layer=opacity` or `layer=off` per override, in [`Layer`] order.
    #[must_use]
    pub fn fragment(&self) -> String {
        let mut out = format!("{VIEW_KEY}={}", self.view);
        for (layer, opacity) in &self.overrides {
            out.push('&');
            out.push_str(&layer.to_string());
            out.push('=');
            match opacity {
                Some(opacity) => out.push_str(&format_opacity(*opacity)),
                None => out.push_str("off"),
            }
        }
        out
    }

    /// The state a fragment names (with or without its `#`) for `stack`:
    /// the view number when the stack has it, else the stack's default
    /// view; every `layer=value` with a layer [`Layer::parse`] knows and
    /// a value that is `off` or a number, clamped to 0..=1; anything
    /// else ignored.
    #[must_use]
    pub fn from_fragment(fragment: &str, stack: &Stack) -> Self {
        let mut state = Self::new(stack.default_view());
        let view_count = stack.views().len();
        for pair in fragment.trim_start_matches('#').split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if key == VIEW_KEY {
                if let Ok(view) = value.parse::<usize>()
                    && (1..=view_count).contains(&view)
                {
                    state.view = view;
                }
                continue;
            }
            let Some(layer) = Layer::parse(key) else {
                continue;
            };
            if value == "off" {
                state.overrides.insert(layer, None);
            } else if let Ok(opacity) = value.parse::<f32>()
                && opacity.is_finite()
            {
                state.overrides.insert(layer, Some(round_opacity(opacity)));
            }
        }
        state
    }
}

/// An opacity as the fragment carries it: to a hundredth, in 0..=1.
fn round_opacity(opacity: f32) -> f32 {
    (opacity.clamp(0.0, 1.0) * 100.0).round() / 100.0
}

/// An opacity as the fragment writes it: `1`, `0.7`, `0.85`.
fn format_opacity(opacity: f32) -> String {
    let text = format!("{opacity:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn four_layers() -> Stack {
        Stack {
            copper: ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]
                .map(str::to_owned)
                .to_vec(),
        }
    }

    fn copper(name: &str) -> Layer {
        Layer::Copper(name.to_owned())
    }

    #[test]
    fn a_four_layer_board_has_the_hand_off_eight_views_in_its_order() {
        let titles: Vec<String> = four_layers()
            .views()
            .into_iter()
            .map(|view| view.title)
            .collect();
        assert_eq!(
            titles,
            [
                "F.Cu + F.Paste",
                "In1.Cu",
                "In2.Cu",
                "B.Cu",
                "F.Cu + F.Paste + F.Mask + F.SilkS",
                "B.Cu + B.Paste + B.Mask + B.SilkS, mirrored",
                "F.Cu + B.Cu + F.SilkS + B.SilkS + F.CrtYd + B.CrtYd",
                "B.Cu + F.Cu + B.SilkS + F.SilkS + B.CrtYd + F.CrtYd",
            ]
        );
        assert_eq!(four_layers().default_view(), 5);
    }

    #[test]
    fn the_front_as_built_is_board_copper_mask_paste_silk_and_holes() {
        let stack = four_layers();
        let views = stack.views();
        let front = &views[4];
        assert_eq!(front.top, Side::Front);
        assert!(!front.mirrored && front.as_built);
        let shown: Vec<(String, f32)> = front
            .layers
            .iter()
            .map(|(layer, opacity)| (layer.to_string(), *opacity))
            .collect();
        assert_eq!(
            shown,
            [
                ("board", 1.0),
                ("F.Cu", 1.0),
                ("F.Mask", 0.7),
                ("F.Paste", 1.0),
                ("F.SilkS", 1.0),
                ("holes", 1.0),
            ]
            .map(|(name, opacity)| (name.to_owned(), opacity))
        );
        // The copper under the mask is copper-coloured, the mask green.
        assert_eq!(front.color(&stack, &copper("F.Cu")), palette::COPPER);
        assert_eq!(front.color(&stack, &copper("B.Cu")), palette::B_CU);
        assert_eq!(
            front.color(&stack, &Layer::Mask(Side::Front)),
            palette::MASK
        );
    }

    #[test]
    fn the_back_as_built_is_mirrored_and_the_other_composed_views_are_not() {
        let views = four_layers().views();
        assert!(views[5].mirrored && views[5].top == Side::Back);
        assert_eq!(
            views[5].color(&four_layers(), &copper("B.Cu")),
            palette::COPPER
        );
        assert!(!views[6].mirrored && views[6].top == Side::Front);
        assert!(!views[7].mirrored && views[7].top == Side::Back);
        // Both coppers, the near one translucent, in the theme's colours.
        assert_eq!(views[7].layers.get(&copper("B.Cu")), Some(&0.8));
        assert_eq!(views[7].layers.get(&copper("F.Cu")), Some(&1.0));
        assert_eq!(
            views[7].color(&four_layers(), &copper("B.Cu")),
            palette::B_CU
        );
        assert_eq!(views[6].layers.get(&copper("F.Cu")), Some(&0.8));
    }

    #[test]
    fn the_draw_order_puts_the_top_side_last_by_kind() {
        let stack = four_layers();
        let front: Vec<String> = stack
            .order(Side::Front)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            front,
            [
                "board",
                "Edge.Cuts",
                "B.Cu",
                "In2.Cu",
                "In1.Cu",
                "F.Cu",
                "B.Mask",
                "F.Mask",
                "B.Paste",
                "F.Paste",
                "B.SilkS",
                "F.SilkS",
                "B.CrtYd",
                "F.CrtYd",
                "holes",
            ]
        );
        let back: Vec<String> = stack
            .order(Side::Back)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(&back[2..6], ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]);
        assert_eq!(&back[6..8], ["F.Mask", "B.Mask"]);
    }

    #[test]
    fn inner_copper_takes_the_theme_colour_by_position() {
        let stack = four_layers();
        assert_eq!(stack.copper_theme("In1.Cu"), palette::INNER_CU[0]);
        assert_eq!(stack.copper_theme("In2.Cu"), palette::INNER_CU[1]);
    }

    #[test_case("board", Some(Layer::Board))]
    #[test_case("holes", Some(Layer::Holes))]
    #[test_case("Edge.Cuts", Some(Layer::Outline))]
    #[test_case("In3.Cu", Some(Layer::Copper("In3.Cu".to_owned())))]
    #[test_case("B.Mask", Some(Layer::Mask(Side::Back)))]
    #[test_case("F.CrtYd", Some(Layer::Courtyard(Side::Front)))]
    #[test_case("F.Fab", None)]
    #[test_case("view", None)]
    #[test_case("", None)]
    fn a_layer_key_parses_and_prints_back(key: &str, layer: Option<Layer>) {
        assert_eq!(Layer::parse(key), layer);
        if let Some(layer) = layer {
            assert_eq!(layer.to_string(), key);
        }
    }

    #[test]
    fn a_fragment_round_trips_with_its_overrides() {
        let views = four_layers().views();
        let mut state = ViewState::new(5);
        state.set(&views[4], copper("F.Cu"), Some(0.5));
        state.set(&views[4], Layer::Silk(Side::Front), None);
        state.set(&views[4], Layer::Mask(Side::Back), Some(1.0));
        // Back to the view's own value: no override.
        state.set(&views[4], Layer::Paste(Side::Front), Some(1.0));
        assert_eq!(state.fragment(), "view=5&F.Cu=0.5&B.Mask=1&F.SilkS=off");
        let parsed = ViewState::from_fragment(&format!("#{}", state.fragment()), &four_layers());
        assert_eq!(parsed, state);
        assert_eq!(parsed.opacity(&views[4], &copper("F.Cu")), Some(0.5));
        assert_eq!(parsed.opacity(&views[4], &Layer::Silk(Side::Front)), None);
        assert_eq!(
            parsed.opacity(&views[4], &Layer::Mask(Side::Front)),
            Some(0.7)
        );
        assert_eq!(
            parsed.opacity(&views[4], &Layer::Courtyard(Side::Front)),
            None
        );
    }

    #[test]
    fn a_view_alone_is_a_bare_fragment_and_an_empty_fragment_is_the_default() {
        assert_eq!(ViewState::new(6).fragment(), "view=6");
        assert_eq!(
            ViewState::from_fragment("", &four_layers()),
            ViewState::new(5)
        );
        assert_eq!(
            ViewState::from_fragment("#", &four_layers()),
            ViewState::new(5)
        );
    }

    #[test_case("view=9", 5; "a view past the last is the default")]
    #[test_case("view=0", 5; "view zero is the default")]
    #[test_case("view=two", 5; "a view that is not a number is the default")]
    #[test_case("view=3&F.Fab=1&junk&=1", 3; "unknown keys and loose pairs are skipped")]
    fn a_bad_fragment_keeps_what_it_can(fragment: &str, view: usize) {
        let state = ViewState::from_fragment(fragment, &four_layers());
        assert_eq!(state.view, view);
        assert!(state.overrides.is_empty());
    }

    #[test]
    fn an_opacity_is_clamped_and_rounded_to_a_hundredth() {
        let state =
            ViewState::from_fragment("view=1&F.Cu=1.7&B.Cu=0.123&In1.Cu=nan", &four_layers());
        assert_eq!(state.overrides.get(&copper("F.Cu")), Some(&Some(1.0)));
        assert_eq!(state.overrides.get(&copper("B.Cu")), Some(&Some(0.12)));
        assert_eq!(state.overrides.get(&copper("In1.Cu")), None);
        assert_eq!(state.fragment(), "view=1&B.Cu=0.12&F.Cu=1");
    }
}
