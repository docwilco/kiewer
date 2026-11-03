//! Which nets a frame lights (docs/v1-scope.md, Net interaction), for
//! hits of any kind, so a board (`crate::highlight`) and a sheet
//! (`crate::sheet_highlight`) keep one model. [`Pick`] holds what the
//! pointer is over, the pinned nets with their colour slots, the undo
//! history of the pins, and what those ask of a frame: the strength of
//! each lit net and the dim of everything else.
//!
//! Nothing here knows about geometry, a colour or a clock. A change
//! retargets the lit nets; [`Pick::advance`] walks them towards their
//! targets by the time the page hands in. A sheet and a board under
//! `prefers-reduced-motion` reach the targets at once.

use std::collections::VecDeque;

/// What everything off the lit nets draws at while a net is pinned, and
/// on a sheet while one is hovered: this fraction of its opacity.
pub const DIM: f32 = 0.3;
/// What everything off a hovered net draws at on a board with no pins.
pub const HOVER_DIM: f32 = 0.6;
/// A net's zones while it is hovered and not pinned: this fraction of
/// their strength.
pub const HOVER_ZONE: f32 = 0.35;
/// How long a change takes on a board, in milliseconds: an entry's whole
/// strength, and the dim from 1 to [`DIM`].
pub const FADE_MS: f64 = 150.0;
/// The most one frame moves a fade, in milliseconds, so a long frame
/// does not jump over it.
const LONGEST_STEP_MS: f64 = 50.0;
/// What the first frame of a fade moves it by: there is no frame before
/// it to measure from.
const FIRST_STEP_MS: f64 = 16.0;
/// How many pin changes undo can step back through.
pub const UNDO_STEPS: usize = 50;

/// What a hit is on: its net. The board's hits and the sheet's both
/// keep their state in a [`Pick`].
pub trait OnNet: Copy {
    /// The net's id.
    type Net: Copy + PartialEq + std::fmt::Debug;
    /// The net the hit is on.
    fn net(self) -> Self::Net;
}

/// A press on the canvas, as the page tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// A plain mouse click: leaves the net as the one pin.
    Click,
    /// A mouse click with Ctrl or Cmd: adds or removes one net.
    ModifiedClick,
    /// A touch or a pen: adds or removes one net, with no modifier to
    /// hold.
    Tap,
}

/// A pinned net and the colour it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin<N> {
    /// The net.
    pub net: N,
    /// Its colour slot: 0 is the layer's own colour, or each item's on a
    /// sheet; the others are generated (`crate::highlight::Ramp`).
    pub slot: usize,
}

/// Where a lit net is heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Aim {
    /// Pinned: solid, zones and all.
    Pinned,
    /// Hovered and not pinned: solid, its zones at [`HOVER_ZONE`].
    Hovered,
    /// Neither any more: fading out, then gone.
    Leaving,
}

/// One net a frame draws over the dimmed scene.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lit<N> {
    /// The net.
    pub net: N,
    /// Its colour slot.
    pub slot: usize,
    aim: Aim,
    /// How far in it has faded, 0 to 1.
    strength: f32,
    /// How far from hovered to pinned it stands, 0 to 1: what its zones
    /// follow.
    pinned: f32,
}

impl<N> Lit<N> {
    fn new(net: N, slot: usize, aim: Aim, pinned: f32) -> Self {
        Self {
            net,
            slot,
            aim,
            strength: 0.0,
            pinned,
        }
    }

    /// The opacity its pads, vias and tracks draw at.
    #[must_use]
    pub fn strength(&self) -> f32 {
        self.strength
    }

    /// The opacity its zones draw at.
    #[must_use]
    pub fn zone_strength(&self) -> f32 {
        self.strength * (HOVER_ZONE + (1.0 - HOVER_ZONE) * self.pinned)
    }

    /// The strength it is heading for.
    fn strength_aim(&self) -> f32 {
        match self.aim {
            Aim::Pinned | Aim::Hovered => 1.0,
            Aim::Leaving => 0.0,
        }
    }

    /// The pinned level it is heading for; one that is leaving keeps
    /// the level it left with, so a pin fades out solid.
    fn pinned_aim(&self) -> f32 {
        match self.aim {
            Aim::Pinned => 1.0,
            Aim::Hovered => 0.0,
            Aim::Leaving => self.pinned,
        }
    }

    /// Whether it stands where it is heading. [`toward`] lands on its
    /// target exactly, so the comparison is exact too.
    #[expect(
        clippy::float_cmp,
        reason = "toward lands on its target exactly, never near it"
    )]
    fn settled(&self) -> bool {
        self.strength == self.strength_aim() && self.pinned == self.pinned_aim()
    }

    /// Whether it is a hovered net on its way out, which only one of may
    /// be at a time; a pin on its way out is not.
    fn outgoing_hover(&self) -> bool {
        self.aim == Aim::Leaving && self.pinned <= 0.0
    }
}

/// `value` moved `stride` towards `target`, landing on it exactly.
fn toward(value: f32, target: f32, stride: f32) -> f32 {
    if value < target {
        (value + stride).min(target)
    } else {
        (value - stride).max(target)
    }
}

/// The net under the pointer, the pinned nets, and what a frame lights:
/// the interaction model of docs/v1-scope.md, Net interaction.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick<H: OnNet> {
    hovered: Option<H>,
    pins: Vec<Pin<H::Net>>,
    /// The pins before each change, oldest first.
    history: VecDeque<Vec<Pin<H::Net>>>,
    /// The pins in pin order, then the hovered net, then what is fading
    /// out: the order a frame draws them in.
    lit: Vec<Lit<H::Net>>,
    dim: f32,
    /// What a hovered net dims the rest to with nothing pinned.
    hover_dim: f32,
    /// Whether a change lands in one step, with no fade.
    instant: bool,
    /// When the last step of a fade in progress was, in the page's
    /// milliseconds.
    clock: Option<f64>,
}

impl<H: OnNet> Default for Pick<H> {
    fn default() -> Self {
        Self::soft()
    }
}

impl<H: OnNet> Pick<H> {
    /// A board's: hover dims to [`HOVER_DIM`] and every change eases.
    #[must_use]
    pub fn soft() -> Self {
        Self {
            hovered: None,
            pins: Vec::new(),
            history: VecDeque::new(),
            lit: Vec::new(),
            dim: 1.0,
            hover_dim: HOVER_DIM,
            instant: false,
            clock: None,
        }
    }

    /// A sheet's: hover dims to [`DIM`] like a pin, and nothing eases.
    #[must_use]
    pub fn flat() -> Self {
        Self {
            hover_dim: DIM,
            instant: true,
            ..Self::soft()
        }
    }

    /// `prefers-reduced-motion`: the same levels, reached at once.
    pub fn set_instant(&mut self, instant: bool) {
        self.instant = instant;
        self.land();
    }

    /// What the pointer is over.
    #[must_use]
    pub fn hovered(&self) -> Option<H> {
        self.hovered
    }

    /// The pinned nets, in pin order.
    #[must_use]
    pub fn pins(&self) -> &[Pin<H::Net>] {
        &self.pins
    }

    /// What a frame draws over the scene, in draw order.
    #[must_use]
    pub fn lit(&self) -> &[Lit<H::Net>] {
        &self.lit
    }

    /// What everything off the lit nets draws at this frame.
    #[must_use]
    pub fn dim(&self) -> f32 {
        self.dim
    }

    /// Whether a fade is in progress, so another frame is due.
    #[must_use]
    pub fn fading(&self) -> bool {
        !self.dim_settled() || self.lit.iter().any(|lit| !lit.settled())
    }

    /// The pointer is over `hit` (or over nothing). Whether a frame is
    /// due.
    pub fn hover(&mut self, hit: Option<H>) -> bool {
        self.hovered = hit;
        self.retarget()
    }

    /// `gesture` on `hit`, or on empty space. Whether a frame is due.
    pub fn press(&mut self, gesture: Gesture, hit: Option<H>) -> bool {
        let net = hit.map(OnNet::net);
        let pins = match (gesture, net) {
            (Gesture::Click, Some(net)) => vec![Pin { net, slot: 0 }],
            (Gesture::Click | Gesture::Tap, None) => Vec::new(),
            (Gesture::ModifiedClick, None) => return false,
            (Gesture::ModifiedClick | Gesture::Tap, Some(net)) => self.toggled(net),
        };
        self.change_pins(pins)
    }

    /// Escape: nothing pinned, nothing hovered. Whether a frame is due.
    pub fn clear(&mut self) -> bool {
        self.hovered = None;
        let changed = self.change_pins(Vec::new());
        self.retarget() || changed
    }

    /// Step back one pin change. Whether there was one to step back.
    pub fn undo(&mut self) -> bool {
        let Some(pins) = self.history.pop_back() else {
            return false;
        };
        self.pins = pins;
        self.retarget();
        true
    }

    /// Move the fade in progress to `now`, in the page's milliseconds.
    pub fn advance(&mut self, now: f64) {
        if !self.fading() {
            self.clock = None;
            return;
        }
        let ms = self.clock.map_or(FIRST_STEP_MS, |last| {
            (now - last).clamp(0.0, LONGEST_STEP_MS)
        });
        self.clock = Some(now);
        self.step(ms);
        if !self.fading() {
            self.clock = None;
        }
    }

    /// The pins with `net` added in the lowest free slot, or removed.
    fn toggled(&self, net: H::Net) -> Vec<Pin<H::Net>> {
        let mut pins = self.pins.clone();
        match pins.iter().position(|pin| pin.net == net) {
            Some(at) => {
                pins.remove(at);
            }
            None => pins.push(Pin {
                net,
                slot: self.free_slot(),
            }),
        }
        pins
    }

    /// The lowest slot no pin holds; `n` pins leave one free among the
    /// first `n + 1`.
    fn free_slot(&self) -> usize {
        (0..=self.pins.len())
            .find(|slot| !self.pins.iter().any(|pin| pin.slot == *slot))
            .unwrap_or(0)
    }

    /// Make `pins` the pins, as one undo step when they differ.
    fn change_pins(&mut self, pins: Vec<Pin<H::Net>>) -> bool {
        if pins == self.pins {
            return false;
        }
        if self.history.len() == UNDO_STEPS {
            self.history.pop_front();
        }
        self.history
            .push_back(std::mem::replace(&mut self.pins, pins));
        self.retarget();
        true
    }

    /// The hovered net, unless a pin already lights it.
    fn preview(&self) -> Option<H::Net> {
        self.hovered
            .map(OnNet::net)
            .filter(|net| !self.pins.iter().any(|pin| pin.net == *net))
    }

    /// Where the dim is heading.
    fn dim_aim(&self) -> f32 {
        if !self.pins.is_empty() {
            DIM
        } else if self.hovered.is_some() {
            self.hover_dim
        } else {
            1.0
        }
    }

    /// Whether the dim stands where it is heading.
    #[expect(
        clippy::float_cmp,
        reason = "toward lands on its target exactly, never near it"
    )]
    fn dim_settled(&self) -> bool {
        self.dim == self.dim_aim()
    }

    /// Rebuild the lit nets from the pins and the hover, keeping how far
    /// each has faded: the pins, the hovered net, then the rest on their
    /// way out, of which only the strongest hovered net stays. Whether
    /// a frame is due.
    fn retarget(&mut self) -> bool {
        let aims = self.aims();
        let mut rest = std::mem::take(&mut self.lit);
        let mut take = |net: H::Net| {
            rest.iter()
                .position(|lit| lit.net == net)
                .map(|at| rest.remove(at))
        };
        let mut lit = Vec::with_capacity(self.pins.len() + 2);
        for pin in &self.pins {
            let mut entry =
                take(pin.net).unwrap_or_else(|| Lit::new(pin.net, pin.slot, Aim::Pinned, 1.0));
            entry.slot = pin.slot;
            entry.aim = Aim::Pinned;
            lit.push(entry);
        }
        if let Some(net) = self.preview() {
            let slot = self.free_slot();
            let mut entry = take(net).unwrap_or_else(|| Lit::new(net, slot, Aim::Hovered, 0.0));
            entry.slot = slot;
            entry.aim = Aim::Hovered;
            lit.push(entry);
        }
        for entry in &mut rest {
            entry.aim = Aim::Leaving;
        }
        let strongest = rest
            .iter()
            .filter(|lit| lit.outgoing_hover())
            .map(Lit::strength)
            .fold(0.0, f32::max);
        rest.retain(|lit| !lit.outgoing_hover() || lit.strength >= strongest);
        lit.append(&mut rest);
        self.lit = lit;
        self.land();
        aims != self.aims()
    }

    /// What each lit net is heading for and where the dim is, to tell
    /// whether a change changed anything a frame shows.
    fn aims(&self) -> (Vec<(H::Net, usize, Aim)>, f32) {
        let nets = self
            .lit
            .iter()
            .map(|lit| (lit.net, lit.slot, lit.aim))
            .collect();
        (nets, self.dim_aim())
    }

    /// With no fade, everything is where it is heading.
    fn land(&mut self) {
        if self.instant {
            self.step(2.0 * FADE_MS);
        }
    }

    /// Move every strength and the dim on by `ms` of the fade, and let
    /// go of the nets that have faded out.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a stride is a fraction of a fade, at most 2, which f32 holds"
    )]
    fn step(&mut self, ms: f64) {
        let stride = (ms / FADE_MS) as f32;
        for lit in &mut self.lit {
            lit.strength = toward(lit.strength, lit.strength_aim(), stride);
            lit.pinned = toward(lit.pinned, lit.pinned_aim(), stride);
        }
        self.lit
            .retain(|lit| lit.aim != Aim::Leaving || lit.strength > 0.0);
        self.dim = toward(self.dim, self.dim_aim(), stride * (1.0 - DIM));
    }
}

#[cfg(test)]
mod tests {
    use kiewer_assert::assert_is_empty;
    use test_case::test_case;

    use super::*;

    /// A hit is its net here: the model cares for nothing else.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Net(u32);

    impl OnNet for Net {
        type Net = u32;

        fn net(self) -> u32 {
            self.0
        }
    }

    const FIRST: Net = Net(1);
    const SECOND: Net = Net(2);
    const THIRD: Net = Net(3);

    /// A board whose fades land at once, for the tests about pins.
    fn landed() -> Pick<Net> {
        let mut pick = Pick::soft();
        pick.set_instant(true);
        pick
    }

    fn pinned(pick: &Pick<Net>) -> Vec<(u32, usize)> {
        pick.pins().iter().map(|pin| (pin.net, pin.slot)).collect()
    }

    fn with_pins(nets: &[Net]) -> Pick<Net> {
        let mut pick = landed();
        for net in nets {
            pick.press(Gesture::ModifiedClick, Some(*net));
        }
        pick
    }

    /// Run the fade to its end, a frame at a time.
    fn settle(pick: &mut Pick<Net>, from: f64) {
        let mut now = from;
        while pick.fading() {
            pick.advance(now);
            now += 16.0;
        }
    }

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-5
    }

    #[test_case(Gesture::Click, Some(THIRD), &[(3, 0)] ; "a click on an unpinned net leaves it as the one pin")]
    #[test_case(Gesture::Click, Some(SECOND), &[(2, 0)] ; "a click on a pinned net leaves it as the one pin in slot 0")]
    #[test_case(Gesture::Click, None, &[] ; "a click on empty space clears all")]
    #[test_case(Gesture::ModifiedClick, Some(THIRD), &[(1, 0), (2, 1), (3, 2)] ; "ctrl click on an unpinned net adds it")]
    #[test_case(Gesture::ModifiedClick, Some(SECOND), &[(1, 0)] ; "ctrl click on a pinned net unpins it")]
    #[test_case(Gesture::ModifiedClick, None, &[(1, 0), (2, 1)] ; "ctrl click on empty space does nothing")]
    #[test_case(Gesture::Tap, Some(THIRD), &[(1, 0), (2, 1), (3, 2)] ; "a tap on an unpinned net adds it")]
    #[test_case(Gesture::Tap, Some(SECOND), &[(1, 0)] ; "a tap on a pinned net unpins it")]
    #[test_case(Gesture::Tap, None, &[] ; "a tap on empty space clears all")]
    fn the_gesture_table(gesture: Gesture, hit: Option<Net>, expected: &[(u32, usize)]) {
        let mut pick = with_pins(&[FIRST, SECOND]);
        pick.press(gesture, hit);
        assert_eq!(pinned(&pick), expected);
    }

    #[test_case(Some(THIRD) ; "over an unpinned net")]
    #[test_case(Some(SECOND) ; "over a pinned net")]
    #[test_case(None ; "over empty space")]
    fn escape_clears_all_wherever_the_pointer_is(hovered: Option<Net>) {
        let mut pick = with_pins(&[FIRST, SECOND]);
        pick.hover(hovered);
        assert!(pick.clear());
        assert_eq!(pinned(&pick), []);
        assert_eq!(pick.hovered(), None);
        assert!(!pick.clear(), "nothing left to clear");
    }

    #[test]
    fn a_freed_colour_goes_to_the_next_pin() {
        let mut pick = with_pins(&[FIRST, SECOND, THIRD]);
        pick.press(Gesture::ModifiedClick, Some(FIRST));
        assert_eq!(pinned(&pick), [(2, 1), (3, 2)]);
        pick.press(Gesture::ModifiedClick, Some(Net(4)));
        assert_eq!(pinned(&pick), [(2, 1), (3, 2), (4, 0)]);
    }

    #[test]
    fn undo_steps_back_through_pin_changes_and_a_no_change_is_no_step() {
        let mut pick = with_pins(&[FIRST, SECOND]);
        pick.press(Gesture::ModifiedClick, None);
        pick.press(Gesture::ModifiedClick, Some(THIRD));
        pick.press(Gesture::Click, Some(THIRD));
        pick.press(Gesture::Click, Some(THIRD));
        assert_eq!(pinned(&pick), [(3, 0)]);
        assert!(pick.undo());
        assert_eq!(pinned(&pick), [(1, 0), (2, 1), (3, 2)]);
        assert!(pick.undo());
        assert_eq!(pinned(&pick), [(1, 0), (2, 1)]);
        assert!(pick.undo());
        assert_eq!(pinned(&pick), [(1, 0)]);
        assert!(pick.undo());
        assert_eq!(pinned(&pick), []);
        assert!(!pick.undo(), "no redo, and nothing before the first");
    }

    #[test]
    fn a_click_that_only_moves_a_pin_to_slot_0_is_a_step() {
        let mut pick = with_pins(&[FIRST, SECOND]);
        pick.press(Gesture::ModifiedClick, Some(FIRST));
        assert_eq!(pinned(&pick), [(2, 1)]);
        assert!(pick.press(Gesture::Click, Some(SECOND)));
        assert_eq!(pinned(&pick), [(2, 0)]);
        pick.undo();
        assert_eq!(pinned(&pick), [(2, 1)]);
    }

    #[test]
    fn escape_is_one_step_and_undo_brings_the_set_back() {
        let mut pick = with_pins(&[FIRST, SECOND]);
        pick.clear();
        pick.undo();
        assert_eq!(pinned(&pick), [(1, 0), (2, 1)]);
    }

    #[test]
    fn undo_goes_back_50_changes_and_no_further() {
        let mut pick = landed();
        for index in 0..60 {
            pick.press(Gesture::Click, Some(Net(index)));
        }
        let mut steps = 0;
        while pick.undo() {
            steps += 1;
        }
        assert_eq!(steps, 50);
        assert_eq!(pinned(&pick), [(9, 0)]);
    }

    #[test]
    fn a_hover_eases_the_dim_and_the_net_in_over_150_ms() {
        let mut pick: Pick<Net> = Pick::soft();
        assert!(pick.hover(Some(FIRST)));
        assert!(pick.fading());
        pick.advance(1000.0);
        let lit = pick.lit()[0];
        assert!(close(lit.strength(), 16.0 / 150.0), "{lit:?}");
        // The first frame's 16 ms, then the 50 and 25 between frames.
        pick.advance(1050.0);
        pick.advance(1075.0);
        let lit = pick.lit()[0];
        assert!(close(lit.strength(), 91.0 / 150.0), "{lit:?}");
        assert!(close(pick.dim(), 0.6), "the dim moves at a constant rate");
        settle(&mut pick, 1100.0);
        let lit = pick.lit()[0];
        assert!(close(lit.strength(), 1.0), "{lit:?}");
        assert!(close(lit.zone_strength(), 0.35), "{lit:?}");
        assert!(close(pick.dim(), 0.6));
    }

    #[test]
    fn the_dim_takes_the_full_fade_from_nothing_to_a_pin() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.press(Gesture::Click, Some(FIRST));
        pick.advance(0.0);
        for frame in 1..=8 {
            pick.advance(f64::from(frame) * 16.0);
        }
        assert!(pick.fading(), "144 ms in, the last frame is still to come");
        pick.advance(9.0 * 16.0);
        pick.advance(10.0 * 16.0);
        assert!(!pick.fading());
        assert!(close(pick.dim(), 0.3));
        let lit = pick.lit()[0];
        assert!(close(lit.zone_strength(), 1.0), "{lit:?}");
    }

    #[test]
    fn a_long_frame_moves_a_fade_at_most_a_third() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.hover(Some(FIRST));
        pick.advance(0.0);
        pick.advance(5000.0);
        let lit = pick.lit()[0];
        assert!(close(lit.strength(), 66.0 / 150.0), "{lit:?}");
    }

    #[test]
    fn a_second_fade_starts_from_its_own_first_frame() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.hover(Some(FIRST));
        settle(&mut pick, 0.0);
        pick.hover(None);
        pick.advance(90_000.0);
        let lit = pick.lit()[0];
        assert!(close(lit.strength(), 1.0 - 16.0 / 150.0), "{lit:?}");
    }

    #[test]
    fn hover_keeps_the_incoming_net_and_the_stronger_outgoing_one() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.hover(Some(FIRST));
        settle(&mut pick, 0.0);
        pick.hover(Some(SECOND));
        pick.advance(1000.0);
        pick.advance(1030.0);
        assert_eq!(pick.lit().len(), 2);
        pick.hover(Some(THIRD));
        let nets: Vec<u32> = pick.lit().iter().map(|lit| lit.net).collect();
        assert_eq!(nets, [3, 1], "the fainter of the fading ones goes at once");
    }

    #[test]
    fn a_hover_back_to_an_outgoing_net_turns_it_round_where_it_stands() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.hover(Some(FIRST));
        settle(&mut pick, 0.0);
        pick.hover(Some(SECOND));
        pick.advance(1000.0);
        pick.hover(Some(FIRST));
        let first = pick.lit()[0];
        assert_eq!(first.net, 1);
        assert!(close(first.strength(), 1.0 - 16.0 / 150.0), "{first:?}");
    }

    #[test]
    fn a_net_that_faded_out_is_let_go_and_the_frames_stop() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.hover(Some(FIRST));
        settle(&mut pick, 0.0);
        pick.hover(None);
        settle(&mut pick, 1000.0);
        assert_is_empty!(pick.lit());
        assert!(close(pick.dim(), 1.0));
        assert!(!pick.fading());
    }

    #[test]
    fn with_a_pin_the_board_stays_at_30_and_a_hover_previews_in_the_next_colour() {
        let mut pick = with_pins(&[FIRST]);
        assert!(pick.hover(Some(SECOND)));
        assert!(close(pick.dim(), 0.3));
        let [pin, preview] = pick.lit() else {
            panic!("{:?}", pick.lit());
        };
        assert_eq!((pin.net, pin.slot), (1, 0));
        assert_eq!((preview.net, preview.slot), (2, 1));
        assert!(close(pin.zone_strength(), 1.0));
        assert!(close(preview.strength(), 1.0));
        assert!(close(preview.zone_strength(), 0.35));
    }

    #[test]
    fn hovering_a_pinned_net_changes_nothing_a_frame_shows() {
        let mut pick = with_pins(&[FIRST]);
        assert!(!pick.hover(Some(FIRST)));
        assert_eq!(pick.lit().len(), 1);
        assert_eq!(pick.hovered(), Some(FIRST));
    }

    #[test]
    fn moving_within_a_net_asks_for_no_frame() {
        let mut pick: Pick<Net> = Pick::soft();
        assert!(pick.hover(Some(FIRST)));
        assert!(!pick.hover(Some(FIRST)));
    }

    #[test]
    fn a_pin_of_the_hovered_net_eases_its_zones_up_without_a_dip() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.hover(Some(FIRST));
        settle(&mut pick, 0.0);
        pick.press(Gesture::Click, Some(FIRST));
        pick.advance(1000.0);
        pick.advance(1059.0);
        let lit = pick.lit()[0];
        assert!(close(lit.strength(), 1.0), "{lit:?}");
        assert!(
            lit.zone_strength() > 0.35 && lit.zone_strength() < 1.0,
            "{lit:?}"
        );
        settle(&mut pick, 1100.0);
        assert!(close(pick.lit()[0].zone_strength(), 1.0));
    }

    #[test]
    fn clearing_many_pins_fades_them_all_out() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.press(Gesture::ModifiedClick, Some(FIRST));
        pick.press(Gesture::ModifiedClick, Some(SECOND));
        pick.press(Gesture::ModifiedClick, Some(THIRD));
        settle(&mut pick, 0.0);
        pick.clear();
        pick.advance(1000.0);
        assert_eq!(pick.lit().len(), 3, "a pin on its way out is no hover");
        settle(&mut pick, 1016.0);
        assert_is_empty!(pick.lit());
    }

    #[test]
    fn a_pin_comes_first_whatever_was_lit_before_it() {
        let mut pick = with_pins(&[FIRST]);
        pick.hover(Some(SECOND));
        pick.press(Gesture::ModifiedClick, Some(THIRD));
        let nets: Vec<(u32, usize)> = pick.lit().iter().map(|lit| (lit.net, lit.slot)).collect();
        assert_eq!(
            nets,
            [(1, 0), (3, 1), (2, 2)],
            "the preview takes the next slot"
        );
    }

    #[test]
    fn a_sheet_dims_to_30_on_hover_and_lands_at_once() {
        let mut pick: Pick<Net> = Pick::flat();
        pick.hover(Some(FIRST));
        assert!(close(pick.dim(), 0.3));
        assert!(close(pick.lit()[0].strength(), 1.0));
        assert!(!pick.fading());
        pick.hover(None);
        assert_is_empty!(pick.lit());
        assert!(close(pick.dim(), 1.0));
    }

    #[test]
    fn reduced_motion_keeps_the_levels_and_drops_the_fade() {
        let mut pick: Pick<Net> = Pick::soft();
        pick.set_instant(true);
        pick.hover(Some(FIRST));
        assert!(close(pick.dim(), 0.6), "the board's hover level");
        assert!(close(pick.lit()[0].zone_strength(), 0.35));
        assert!(!pick.fading());
    }
}
