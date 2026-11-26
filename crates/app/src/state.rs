//! The overlay's facts, translated from `reference/fairy-cursor/sources/app/FairyState.swift`.
//!
//! No clocks, timers or threads: every operation that depends on time takes `now` (seconds,
//! monotonic) and everything that would be a `Timer` or `DispatchQueue.main.asyncAfter` in
//! Swift becomes a stored deadline. `tick` runs whatever is due, `next_deadline` says when to
//! tick next. Deadlines cascade from the deadline that triggered them, so a late tick ends in
//! the same state as many small ones.
//!
//! Animations have no wake-up deadlines of their own: `is_animating` reads their progress from
//! `now`, so a caller never wakes up just to end one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use igorio_core::geom::CardId;
use igorio_core::pile::{HeldPile, LeaveStyle, ReleaseReason};
use igorio_kit::{HeldCard, Image};

use crate::layout::{self, CardSize};
use crate::scene::{Leaving, Scene, SceneCard};

/// How long a hint takes to fade in, and to fade out.
const HINT_FADE: f64 = 0.18;
/// How long the whole overlay takes to show or hide while a capture UI is up.
const CAPTURE_FADE: f64 = 0.15;
/// One "no" wobble of the top card.
const WOBBLE: f64 = 0.42;
/// A new (or restored) card growing in, and a card sliding to a new depth.
const STACK_ANIMATION: f64 = 0.22;
/// Leave animations (`FairyView.swift`); the card is dropped a little after the animation ends.
const DISSOLVE_ANIMATION: f64 = 0.22;
const DROP_ANIMATION: f64 = 0.26;
const DISSOLVE_FINISH: f64 = 0.26;
const DROP_FINISH: f64 = 0.30;
/// Delivered cards leave one after the other, this far apart.
const DELIVER_STEP: f64 = 0.36;

/// A deferred step: the Rust stand-in for one `Timer` or `asyncAfter` closure.
#[derive(Debug, Clone)]
enum Task {
    /// Take the hint down again if it still says `text`.
    ClearHint { text: String },
    /// A queued delivery reached its turn: start dropping this card.
    DeliverStep { id: CardId, text: String },
    /// Remove cards whose leave animation is over.
    FinishLeaving { ids: Vec<CardId> },
}

#[derive(Debug, Clone)]
struct Scheduled {
    deadline: f64,
    /// Insertion order, so equal deadlines run in the order they were scheduled.
    seq: u64,
    task: Task,
}

/// Where a card's depth is moving from, and when the move started.
#[derive(Debug, Clone, Copy)]
struct DepthMove {
    from: f64,
    to: f64,
    at: f64,
}

impl DepthMove {
    fn value_at(&self, now: f64) -> f64 {
        let t = ((now - self.at) / STACK_ANIMATION).clamp(0.0, 1.0);
        if t <= 0.0 {
            self.from
        } else if t >= 1.0 {
            self.to
        } else {
            self.from + (self.to - self.from) * ease_out(t)
        }
    }
}

/// Which appear value a card started from (0 for a brand-new card; the opacity it had for a
/// card restored in the middle of leaving), and when the entrance started.
#[derive(Debug, Clone, Copy)]
struct Appearance {
    from: f64,
    at: f64,
}

impl Appearance {
    fn value_at(&self, now: f64) -> f64 {
        let from = self.from.clamp(0.0, 1.0);
        let t = ((now - self.at) / STACK_ANIMATION).clamp(0.0, 1.0);
        if t <= 0.0 {
            from
        } else if t >= 1.0 {
            1.0
        } else {
            from + (1.0 - from) * ease_out(t)
        }
    }
}

/// The stacking ease (SwiftUI's spring is replaced with an ease-out cubic).
fn ease_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Everything the overlay shows and does, as a pure value: the pile, the hint, the last "no",
/// and the animations that are still running.
///
/// Caller contract: draw one frame after every mutating call and after every `tick`. While
/// `is_animating(now)` is true, draw every frame. Otherwise sleep until `next_deadline()`, or
/// until input arrives when it is `None`.
#[derive(Debug, Clone)]
pub struct FairyState {
    pile: HeldPile<HeldCard>,
    hint: Option<String>,
    /// The text that is fading out: still drawn, but `hint` is already `None`.
    hint_fade_text: Option<String>,
    hint_from: f64,
    hint_at: f64,
    hint_target: f64,
    reject_tick: u64,
    last_reject_at: Option<f64>,
    hidden_for_capture: bool,
    opacity_from: f64,
    opacity_at: f64,
    opacity_target: f64,
    /// When each leaving card got its current leave style (for the animation progress).
    leave_started: HashMap<CardId, f64>,
    /// The depth each card is moving from, and when that move started.
    depth_moves: HashMap<CardId, DepthMove>,
    /// When each card entered the pile, and the appear value it started from.
    appearances: HashMap<CardId, Appearance>,
    tasks: Vec<Scheduled>,
    next_seq: u64,
    next_card_id: u64,
    rng: u64,
    card_size: CardSize,
}

impl FairyState {
    /// `seed` drives the random tilts; the same seed and the same calls give the same pile.
    pub fn new(seed: u64) -> Self {
        Self {
            pile: HeldPile::default(),
            hint: None,
            hint_fade_text: None,
            hint_from: 0.0,
            hint_at: 0.0,
            hint_target: 0.0,
            reject_tick: 0,
            last_reject_at: None,
            hidden_for_capture: false,
            opacity_from: 1.0,
            opacity_at: 0.0,
            opacity_target: 1.0,
            leave_started: HashMap::new(),
            depth_moves: HashMap::new(),
            appearances: HashMap::new(),
            tasks: Vec::new(),
            next_seq: 0,
            next_card_id: 1,
            // xorshift is stuck at 0, so never start there.
            rng: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
            card_size: CardSize::default(),
        }
    }

    // MARK: Read accessors

    /// Every card on screen, oldest first (including ones animating out).
    pub fn stack(&self) -> &[HeldCard] {
        self.pile.cards()
    }

    /// The card gestures act on.
    pub fn top(&self) -> Option<&HeldCard> {
        self.pile.top()
    }

    pub fn is_holding(&self) -> bool {
        self.pile.is_holding()
    }

    pub fn can_restore(&self) -> bool {
        self.pile.can_restore()
    }

    /// Cards a delivery takes, top first.
    pub fn deliverable(&self) -> Vec<HeldCard> {
        self.pile.active_top_first()
    }

    pub fn leave_style(&self, id: CardId) -> Option<LeaveStyle> {
        self.pile.style(id)
    }

    pub fn card(&self, id: CardId) -> Option<&HeldCard> {
        if self.pile.is_active(id) {
            self.pile.card(id)
        } else {
            None
        }
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    pub fn reject_tick(&self) -> u64 {
        self.reject_tick
    }

    pub fn hidden_for_capture(&self) -> bool {
        self.hidden_for_capture
    }

    /// The card size; the next frame is drawn at it.
    pub fn card_size(&self) -> CardSize {
        self.card_size
    }

    /// Changes the card size. There is no animation: the next frame draws the new size.
    pub fn set_card_size(&mut self, size: CardSize, _now: f64) {
        self.card_size = size;
    }

    /// The fairy steps out of the way while the system capture UI is up.
    pub fn set_hidden_for_capture(&mut self, hidden: bool, now: f64) {
        let from = self.opacity_now(now);
        self.hidden_for_capture = hidden;
        self.opacity_from = from;
        self.opacity_at = now;
        self.opacity_target = if hidden { 0.0 } else { 1.0 };
    }

    // MARK: Actions

    /// `kept`: the screenshot was already saved, so shaking only lets go of it.
    /// `movie`: the card is a screen recording (already saved), `image` its poster frame.
    #[allow(clippy::too_many_arguments)]
    pub fn hold(
        &mut self,
        image: Arc<Image>,
        scale: f64,
        captured_at: SystemTime,
        kept: bool,
        movie: Option<PathBuf>,
        duration: Option<f64>,
        now: f64,
    ) {
        let is_movie = movie.is_some();
        let card = self.make_card(image, scale, captured_at, kept, movie, duration);
        let id = card.id;
        self.pile.push(card);
        self.appearances
            .insert(id, Appearance { from: 0.0, at: now });
        self.sync_depths(now);
        let count = self.pile.active().len();
        let verb = if kept {
            "saved · shake to let go"
        } else {
            "shake to discard"
        };
        let (text, seconds) = if is_movie && count == 1 {
            (
                "recording · double-click a text box to share".to_string(),
                2.8,
            )
        } else if count > 1 {
            (format!("{count} held · {verb}"), 2.4)
        } else {
            (verb.to_string(), 2.4)
        };
        self.flash_hint_at(text, seconds, now);
    }

    /// A capture we don't trust (from a screenshot session that had already ended on screen):
    /// keep it only in the restore memory instead of pushing it on the pile.
    pub fn stash(&mut self, image: Arc<Image>, scale: f64, captured_at: SystemTime, now: f64) {
        let card = self.make_card(image, scale, captured_at, false, None, None);
        self.pile.stash(card, ReleaseReason::Spurious, now);
    }

    /// Shake off the top card.
    pub fn dissolve(&mut self, now: f64) {
        let Some(id) = self.pile.top().map(|card| card.id) else {
            return;
        };
        let taken =
            self.pile
                .begin_leaving(&[id], LeaveStyle::Dissolve, ReleaseReason::Shaken, now);
        if taken.is_empty() {
            return;
        }
        for id in &taken {
            self.leave_started.insert(*id, now);
        }
        self.clear_hint(now);
        self.push_task(now + DISSOLVE_FINISH, Task::FinishLeaving { ids: taken });
    }

    /// The given cards were delivered (pasted, saved, put in a note). They leave one after the
    /// other, top first; cards not in `ids` — e.g. a capture that lands meanwhile — stay.
    pub fn deliver(&mut self, ids: &[CardId], hint: &str, now: f64) {
        let taken = self
            .pile
            .begin_leaving(ids, LeaveStyle::Queued, ReleaseReason::Delivered, now);
        // Keep the requested (top-first) order for the animation.
        let order: Vec<CardId> = ids
            .iter()
            .copied()
            .filter(|id| taken.contains(id))
            .collect();
        let mut order = order.into_iter().enumerate();
        // Swift schedules the first step with `asyncAfter(.now())`; run it now so the pile
        // already shows the drop and the hint the moment a delivery starts.
        if let Some((_, id)) = order.next() {
            self.start_drop(id, hint.to_string(), now);
        }
        for (k, id) in order {
            self.push_task(
                now + k as f64 * DELIVER_STEP,
                Task::DeliverStep {
                    id,
                    text: hint.to_string(),
                },
            );
        }
    }

    /// The target refused the screenshot: wobble, explain, keep holding.
    pub fn reject(&mut self, hint: &str, now: f64) {
        if !self.pile.is_holding() {
            return;
        }
        self.reject_tick += 1;
        self.last_reject_at = Some(now);
        self.flash_hint(hint, 2.4, now);
    }

    pub fn discard_all(&mut self, now: f64) {
        self.pile.discard_all(ReleaseReason::DiscardedAll, now);
        self.leave_started.clear();
        self.depth_moves.clear();
        self.appearances.clear();
        self.clear_hint(now);
    }

    /// Bring back what last left the pile (shaken off, discarded, or delivered).
    pub fn restore_last(&mut self, now: f64) {
        // A card restored while leaving carries on from the opacity it had, so it does not
        // blink out and back in. Cards that had already left start from fully transparent.
        let leaving_opacities: HashMap<CardId, f64> = self
            .pile
            .cards()
            .iter()
            .filter_map(|card| self.leave_opacity_now(card.id, now).map(|a| (card.id, a)))
            .collect();
        let restored = self.pile.restore_last(now);
        if restored.is_empty() {
            return;
        }
        let restored_ids: Vec<CardId> = restored.iter().map(|card| card.id).collect();
        for id in &restored_ids {
            let from = leaving_opacities.get(id).copied().unwrap_or(0.0);
            self.appearances.insert(*id, Appearance { from, at: now });
            self.leave_started.remove(id);
        }
        self.sync_depths(now);
        // A leave or delivery scheduled before the restore must not reach the card now that it
        // is back on the pile. Drop its deliver steps and its share of pending finishes.
        self.tasks
            .retain_mut(|scheduled| match &mut scheduled.task {
                Task::ClearHint { .. } => true,
                Task::DeliverStep { id, .. } => !restored_ids.contains(id),
                Task::FinishLeaving { ids } => {
                    ids.retain(|id| !restored_ids.contains(id));
                    !ids.is_empty()
                }
            });
        let text = if restored.len() == 1 {
            "restored".to_string()
        } else {
            format!("restored {}", restored.len())
        };
        self.flash_hint(&text, 1.6, now);
    }

    /// Show a hint that clears itself after `seconds`.
    pub fn flash_hint(&mut self, text: &str, seconds: f64, now: f64) {
        self.flash_hint_at(text.to_string(), seconds, now);
    }

    /// Context hint while hovering a target (cleared when `None`).
    pub fn set_hover_hint(&mut self, text: Option<&str>, now: f64) {
        if !self.pile.is_holding() {
            return;
        }
        match text {
            Some(text) => {
                // The hover hint stays until the pointer leaves, so it has no timer.
                self.cancel_clear_hint();
                self.show_hint(text.to_string(), now);
            }
            None => {
                if !self
                    .hint
                    .as_deref()
                    .is_some_and(|hint| hint.contains("shake"))
                {
                    self.clear_hint(now);
                }
            }
        }
    }

    // MARK: Time

    /// Run every deferred step whose deadline has passed, in deadline order, then forget
    /// released screenshots that have expired.
    pub fn tick(&mut self, now: f64) {
        if !now.is_finite() {
            return;
        }
        while let Some(index) = self.next_due(now) {
            let Scheduled { deadline, task, .. } = self.tasks.remove(index);
            self.run(task, deadline);
        }
        self.pile.prune(now);
        // The faded hint text is kept only while it is still visible.
        if self.hint.is_none() && self.hint_opacity_now(now) <= 0.0 {
            self.hint_fade_text = None;
        }
    }

    /// The earliest deadline still pending, if any: when to call `tick` next.
    pub fn next_deadline(&self) -> Option<f64> {
        let tasks = self
            .tasks
            .iter()
            .map(|scheduled| scheduled.deadline)
            .min_by(f64::total_cmp);
        match (tasks, self.prune_deadline()) {
            (Some(task), Some(prune)) => Some(task.min(prune)),
            (task, prune) => task.or(prune),
        }
    }

    /// Whether pixels still change: something is fading, wobbling, leaving, appearing or
    /// sliding to a new depth. Pending deadlines alone do not count, so a static hint does
    /// not drive a redraw loop.
    pub fn is_animating(&self, now: f64) -> bool {
        self.hint_fading(now)
            || self.opacity_fading(now)
            || self.wobble_active(now)
            || self.leaving_active(now)
            || self.appearing_active(now)
            || self.depth_active(now)
    }

    // MARK: Drawing

    /// The frame to draw at `now`.
    pub fn scene(&self, now: f64, scale: f64) -> Scene {
        let all = self.pile.cards();
        let top_id = all.last().map(|card| card.id);
        let wobble = self.wobble_now(now);
        let last_depth = layout::VISIBLE_CARDS as f64;

        let cards: Vec<SceneCard> = all
            .iter()
            .enumerate()
            .filter_map(|(index, card)| {
                let target = (all.len() - 1 - index) as f64;
                let depth = self.depth_at(card.id, target, now);
                // At rest only depths 0..VISIBLE_CARDS-1 are drawn. A card crossing the
                // boundary (3 <-> 4) stays in the scene while its depth is still inside.
                if depth >= last_depth {
                    return None;
                }
                let leaving = match self.pile.style(card.id) {
                    Some(LeaveStyle::Dissolve) => {
                        let at = self.leave_started.get(&card.id).copied().unwrap_or(now);
                        Some(Leaving::Dissolve(
                            ((now - at) / DISSOLVE_ANIMATION).clamp(0.0, 1.0),
                        ))
                    }
                    Some(LeaveStyle::Drop) => {
                        let at = self.leave_started.get(&card.id).copied().unwrap_or(now);
                        Some(Leaving::Drop(((now - at) / DROP_ANIMATION).clamp(0.0, 1.0)))
                    }
                    _ => None,
                };
                Some(SceneCard {
                    image: card.image.clone(),
                    // A lone screenshot sits straight; tilts only appear once there is a pile.
                    tilt: if all.len() > 1 { card.tilt } else { 0.0 },
                    depth,
                    appear: self.appear_at(card.id, now),
                    wobble: if Some(card.id) == top_id { wobble } else { 0.0 },
                    leaving,
                    movie: if card.is_movie() {
                        Some(card.duration)
                    } else {
                        None
                    },
                })
            })
            .collect();

        let hint_opacity = self.hint_opacity_now(now);
        let hint = if self.hint.is_some() {
            self.hint.clone()
        } else if hint_opacity > 0.0 {
            // Keep the old text on screen while it fades.
            self.hint_fade_text.clone()
        } else {
            None
        };

        Scene {
            cards,
            hint,
            hint_opacity,
            opacity: self.opacity_now(now),
            card_size: self.card_size,
            scale,
        }
    }

    // MARK: Internals

    fn make_card(
        &mut self,
        image: Arc<Image>,
        scale: f64,
        captured_at: SystemTime,
        kept: bool,
        movie: Option<PathBuf>,
        duration: Option<f64>,
    ) -> HeldCard {
        // Alternate the tilt direction so each new sheet peeks out from the previous one.
        let sign = if self.pile.cards().len().is_multiple_of(2) {
            1.0
        } else {
            -1.0
        };
        let id = CardId(self.next_card_id);
        self.next_card_id += 1;
        HeldCard {
            id,
            image,
            scale,
            captured_at,
            kept,
            tilt: sign * self.random_tilt(),
            movie,
            duration,
        }
    }

    /// A tilt of 8..15 degrees, from a small xorshift generator.
    fn random_tilt(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        8.0 + (x >> 11) as f64 / (1u64 << 53) as f64 * 7.0
    }

    fn push_task(&mut self, deadline: f64, task: Task) {
        // A deadline that is not a real number would never come due in a sane order.
        if !deadline.is_finite() {
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.tasks.push(Scheduled {
            deadline,
            seq,
            task,
        });
    }

    fn next_due(&self, now: f64) -> Option<usize> {
        self.tasks
            .iter()
            .enumerate()
            .filter(|(_, scheduled)| scheduled.deadline <= now)
            .min_by(|(_, a), (_, b)| a.deadline.total_cmp(&b.deadline).then(a.seq.cmp(&b.seq)))
            .map(|(index, _)| index)
    }

    /// Run one deferred step. `at` is the deadline it was due at: follow-up steps are
    /// scheduled from there, so a late tick catches up instead of stretching the animation.
    fn run(&mut self, task: Task, at: f64) {
        match task {
            Task::ClearHint { text } => {
                if self.hint.as_deref() == Some(text.as_str()) {
                    self.clear_hint(at);
                }
            }
            Task::DeliverStep { id, text } => {
                if self.pile.style(id) == Some(LeaveStyle::Queued) {
                    self.start_drop(id, text, at);
                }
            }
            Task::FinishLeaving { ids } => self.finish_leaving(&ids, at),
        }
    }

    fn start_drop(&mut self, id: CardId, text: String, at: f64) {
        self.pile.restyle(id, LeaveStyle::Drop);
        self.leave_started.insert(id, at);
        self.show_hint(text, at);
        self.push_task(at + DROP_FINISH, Task::FinishLeaving { ids: vec![id] });
    }

    fn finish_leaving(&mut self, ids: &[CardId], at: f64) {
        for id in ids {
            self.pile.finish_leaving(*id);
            self.leave_started.remove(id);
            if self.pile.card(*id).is_none() {
                self.depth_moves.remove(id);
                self.appearances.remove(id);
            }
        }
        self.sync_depths(at);
        // A restore while animating cancels the leave; only the hint is left to sort out.
        if self.pile.is_empty() {
            self.clear_hint(at);
        } else if let Some(text) = self.hint.clone() {
            self.flash_hint_at(text, 1.2, at);
        }
    }

    fn flash_hint_at(&mut self, text: String, seconds: f64, at: f64) {
        self.cancel_clear_hint();
        self.show_hint(text.clone(), at);
        self.push_task(at + seconds, Task::ClearHint { text });
    }

    fn show_hint(&mut self, text: String, at: f64) {
        let from = self.hint_opacity_now(at);
        self.hint = Some(text);
        self.hint_fade_text = None;
        self.hint_from = from;
        self.hint_at = at;
        self.hint_target = 1.0;
    }

    fn clear_hint(&mut self, at: f64) {
        let Some(text) = self.hint.take() else {
            return;
        };
        let from = self.hint_opacity_now(at);
        self.hint_fade_text = Some(text);
        self.hint_from = from;
        self.hint_at = at;
        self.hint_target = 0.0;
    }

    fn cancel_clear_hint(&mut self) {
        self.tasks
            .retain(|scheduled| !matches!(scheduled.task, Task::ClearHint { .. }));
    }

    fn hint_opacity_now(&self, now: f64) -> f64 {
        let t = ((now - self.hint_at) / HINT_FADE).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t) * (1.0 - t);
        (self.hint_from + (self.hint_target - self.hint_from) * eased).clamp(0.0, 1.0)
    }

    fn opacity_now(&self, now: f64) -> f64 {
        let t = ((now - self.opacity_at) / CAPTURE_FADE).clamp(0.0, 1.0);
        let eased = t * t * (3.0 - 2.0 * t);
        (self.opacity_from + (self.opacity_target - self.opacity_from) * eased).clamp(0.0, 1.0)
    }

    fn wobble_now(&self, now: f64) -> f64 {
        match self.last_reject_at {
            Some(at) => {
                let t = (now - at) / WOBBLE;
                if t < 1.0 { t.clamp(0.0, 1.0) } else { 0.0 }
            }
            None => 0.0,
        }
    }

    fn hint_fading(&self, now: f64) -> bool {
        self.hint_from != self.hint_target && now - self.hint_at < HINT_FADE
    }

    fn opacity_fading(&self, now: f64) -> bool {
        self.opacity_from != self.opacity_target && now - self.opacity_at < CAPTURE_FADE
    }

    fn wobble_active(&self, now: f64) -> bool {
        !self.pile.is_empty() && self.last_reject_at.is_some_and(|at| now - at < WOBBLE)
    }

    fn appearing_active(&self, now: f64) -> bool {
        self.pile.cards().iter().any(|card| {
            self.appearances
                .get(&card.id)
                .is_some_and(|appear| appear.from < 1.0 && now - appear.at < STACK_ANIMATION)
        })
    }

    fn depth_active(&self, now: f64) -> bool {
        self.pile.cards().iter().any(|card| {
            self.depth_moves
                .get(&card.id)
                .is_some_and(|move_| move_.from != move_.to && now - move_.at < STACK_ANIMATION)
        })
    }

    fn appear_at(&self, id: CardId, now: f64) -> f64 {
        self.appearances
            .get(&id)
            .map_or(1.0, |appear| appear.value_at(now))
    }

    fn depth_at(&self, id: CardId, target: f64, now: f64) -> f64 {
        self.depth_moves
            .get(&id)
            .map_or(target, |move_| move_.value_at(now))
    }

    /// Reconcile each card's depth with its position in the pile, starting a 0.22 s move
    /// wherever the target changed. Several changes at one instant collapse into the final
    /// depth: no time passed, so there is nothing to animate.
    fn sync_depths(&mut self, now: f64) {
        let count = self.pile.cards().len();
        let ids: Vec<CardId> = self.pile.cards().iter().map(|card| card.id).collect();
        for (index, id) in ids.iter().enumerate() {
            let target = (count - 1 - index) as f64;
            match self.depth_moves.get_mut(id) {
                Some(move_) if move_.to != target => {
                    if now <= move_.at {
                        *move_ = DepthMove {
                            from: target,
                            to: target,
                            at: now,
                        };
                    } else {
                        let from = move_.value_at(now);
                        *move_ = DepthMove {
                            from,
                            to: target,
                            at: now,
                        };
                    }
                }
                Some(_) => {}
                None => {
                    self.depth_moves.insert(
                        *id,
                        DepthMove {
                            from: target,
                            to: target,
                            at: now,
                        },
                    );
                }
            }
        }
        self.depth_moves.retain(|id, _| ids.contains(id));
    }

    /// The opacity a leaving card currently has, or `None` when it is not in the pile.
    fn leave_opacity_now(&self, id: CardId, now: f64) -> Option<f64> {
        let style = self.pile.style(id)?;
        let at = self.leave_started.get(&id).copied().unwrap_or(now);
        let progress = match style {
            // Still fully drawn, waiting for its turn.
            LeaveStyle::Queued => return Some(1.0),
            LeaveStyle::Dissolve => ((now - at) / DISSOLVE_ANIMATION).clamp(0.0, 1.0),
            LeaveStyle::Drop => ((now - at) / DROP_ANIMATION).clamp(0.0, 1.0),
        };
        Some(1.0 - progress.powi(2))
    }

    fn leaving_active(&self, now: f64) -> bool {
        self.pile.cards().iter().any(|card| {
            let Some(at) = self.leave_started.get(&card.id).copied() else {
                return false;
            };
            match self.pile.style(card.id) {
                Some(LeaveStyle::Dissolve) => now - at < DISSOLVE_ANIMATION,
                Some(LeaveStyle::Drop) => now - at < DROP_ANIMATION,
                _ => false,
            }
        })
    }

    /// When the oldest remembered release expires (the Swift prune timer).
    fn prune_deadline(&self) -> Option<f64> {
        self.pile
            .released()
            .iter()
            .map(|batch| batch.at)
            .filter(|at| at.is_finite())
            .min_by(f64::total_cmp)
            .map(|at| at + self.pile.release_ttl + 1.0)
            .filter(|at| at.is_finite())
    }
}
