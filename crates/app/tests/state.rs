//! Tests for `igorio_app::state::FairyState`, translated from the Swift behaviour.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use igorio_app::scene::{Leaving, SceneCard};
use igorio_app::state::FairyState;
use igorio_core::geom::CardId;
use igorio_core::pile::LeaveStyle;
use igorio_kit::Image;

fn image() -> Arc<Image> {
    Arc::new(Image::new(1, 1, vec![0, 0, 0, 255]).unwrap())
}

fn hold(state: &mut FairyState, now: f64) {
    state.hold(image(), 1.0, SystemTime::UNIX_EPOCH, false, None, None, now);
}

fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{actual} is not {expected}"
    );
}

fn ids(state: &FairyState) -> Vec<CardId> {
    state.stack().iter().map(|card| card.id).collect()
}

fn top_card(scene: &igorio_app::scene::Scene) -> &SceneCard {
    scene.cards.last().expect("a card")
}

#[test]
fn hold_shows_shake_hint_then_it_clears() {
    let mut state = FairyState::new(1);
    hold(&mut state, 0.0);
    assert_eq!(state.hint(), Some("shake to discard"));
    assert_eq!(
        state.scene(0.0, 1.0).hint.as_deref(),
        Some("shake to discard")
    );

    state.tick(2.39);
    assert_eq!(state.hint(), Some("shake to discard"));
    state.tick(2.4);
    assert_eq!(state.hint(), None);
}

#[test]
fn kept_card_shows_saved_hint() {
    let mut state = FairyState::new(1);
    state.hold(image(), 1.0, SystemTime::UNIX_EPOCH, true, None, None, 0.0);
    assert_eq!(state.hint(), Some("saved · shake to let go"));
}

#[test]
fn two_cards_count_them_in_the_hint() {
    let mut state = FairyState::new(1);
    hold(&mut state, 0.0);
    hold(&mut state, 0.0);
    assert_eq!(state.hint(), Some("2 held · shake to discard"));
}

#[test]
fn a_lone_recording_shows_the_recording_hint() {
    let mut state = FairyState::new(1);
    state.hold(
        image(),
        1.0,
        SystemTime::UNIX_EPOCH,
        true,
        Some(PathBuf::from("recording.mp4")),
        Some(4.0),
        0.0,
    );
    assert_eq!(
        state.hint(),
        Some("recording · double-click a text box to share")
    );
    state.tick(2.7);
    assert_eq!(
        state.hint(),
        Some("recording · double-click a text box to share")
    );
    state.tick(2.8);
    assert_eq!(state.hint(), None);
}

#[test]
fn tilts_alternate_sign_and_stay_between_8_and_15_degrees() {
    let mut state = FairyState::new(0xDEAD_BEEF);
    for _ in 0..4 {
        hold(&mut state, 0.0);
    }
    let tilts: Vec<f64> = state.stack().iter().map(|card| card.tilt).collect();
    assert_eq!(tilts.len(), 4);
    for (index, tilt) in tilts.iter().enumerate() {
        let expected_sign = if index % 2 == 0 { 1.0 } else { -1.0 };
        assert!(
            tilt.signum() == expected_sign,
            "tilt {index} is {tilt}, wanted sign {expected_sign}"
        );
        let size = tilt.abs();
        assert!((8.0..=15.0).contains(&size), "tilt {index} has size {size}");
    }
}

#[test]
fn dissolve_removes_the_top_card_after_0_26_s_and_restores() {
    let mut state = FairyState::new(2);
    hold(&mut state, 1.0);
    let id = state.top().unwrap().id;
    state.dissolve(1.0);
    assert_eq!(state.leave_style(id), Some(LeaveStyle::Dissolve));

    state.tick(1.25);
    assert_eq!(state.stack().len(), 1, "still animating out");
    state.tick(1.26);
    assert_eq!(state.stack().len(), 0);
    assert!(state.can_restore());

    state.restore_last(1.3);
    assert_eq!(ids(&state), vec![id]);
    assert_eq!(state.hint(), Some("restored"));
}

#[test]
fn deliver_drops_cards_one_by_one_top_first_and_sets_the_hint() {
    let mut state = FairyState::new(3);
    hold(&mut state, 0.0);
    hold(&mut state, 0.0);
    hold(&mut state, 0.0);
    let order: Vec<CardId> = state.deliverable().iter().map(|card| card.id).collect();
    assert_eq!(order.len(), 3);

    state.deliver(&order, "saved", 0.0);
    assert_eq!(state.hint(), Some("saved"));
    assert_eq!(state.leave_style(order[0]), Some(LeaveStyle::Drop));
    assert_eq!(state.leave_style(order[1]), Some(LeaveStyle::Queued));
    assert_eq!(state.leave_style(order[2]), Some(LeaveStyle::Queued));
    assert!(matches!(
        top_card(&state.scene(0.0, 1.0)).leaving,
        Some(Leaving::Drop(p)) if p == 0.0
    ));

    state.tick(0.35);
    assert!(
        state.card(order[0]).is_none(),
        "the first card finished dropping"
    );
    assert_eq!(state.leave_style(order[1]), Some(LeaveStyle::Queued));

    state.tick(0.36);
    assert_eq!(state.leave_style(order[1]), Some(LeaveStyle::Drop));
    assert_eq!(state.leave_style(order[2]), Some(LeaveStyle::Queued));

    state.tick(0.72);
    assert_eq!(state.leave_style(order[2]), Some(LeaveStyle::Drop));

    state.tick(2.0);
    assert!(state.stack().is_empty());
}

#[test]
fn a_card_held_during_a_deliver_stays() {
    let mut state = FairyState::new(4);
    hold(&mut state, 0.0);
    hold(&mut state, 0.0);
    let order: Vec<CardId> = state.deliverable().iter().map(|card| card.id).collect();
    state.deliver(&order, "saved", 0.0);

    hold(&mut state, 0.1);
    let fresh = state.top().unwrap().id;

    state.tick(2.0);
    assert_eq!(ids(&state), vec![fresh]);
}

#[test]
fn reject_bumps_the_tick_and_does_nothing_when_not_holding() {
    let mut state = FairyState::new(5);
    state.reject("no", 0.0);
    assert_eq!(state.reject_tick(), 0);
    assert_eq!(state.hint(), None);

    hold(&mut state, 0.0);
    state.reject("no", 1.0);
    assert_eq!(state.reject_tick(), 1);
    assert_eq!(state.hint(), Some("no"));

    let scene = state.scene(1.1, 1.0);
    approx(top_card(&scene).wobble, 0.1 / 0.42);
    assert!(
        scene.cards[..scene.cards.len() - 1]
            .iter()
            .all(|card| card.wobble == 0.0)
    );
}

#[test]
fn discard_all_then_restore_brings_everything_back() {
    let mut state = FairyState::new(6);
    hold(&mut state, 0.0);
    hold(&mut state, 0.0);
    hold(&mut state, 0.0);
    let before = ids(&state);

    state.discard_all(0.0);
    assert!(state.stack().is_empty());
    assert!(state.can_restore());

    state.restore_last(0.1);
    assert_eq!(ids(&state), before);
    assert_eq!(state.hint(), Some("restored 3"));
}

#[test]
fn hover_hint_none_keeps_a_shake_hint_and_clears_others() {
    let mut state = FairyState::new(7);
    hold(&mut state, 0.0);
    assert_eq!(state.hint(), Some("shake to discard"));

    state.set_hover_hint(None, 0.1);
    assert_eq!(state.hint(), Some("shake to discard"));

    state.set_hover_hint(Some("drop to paste"), 0.2);
    assert_eq!(state.hint(), Some("drop to paste"));

    state.set_hover_hint(None, 0.3);
    assert_eq!(state.hint(), None);
}

#[test]
fn one_late_tick_matches_many_small_ticks() {
    let mut late = FairyState::new(8);
    let mut steady = FairyState::new(8);
    for state in [&mut late, &mut steady] {
        hold(state, 0.0);
        hold(state, 0.5);
        state.dissolve(0.7);
    }

    late.tick(10.0);
    let mut now = 0.0;
    while now < 10.0 {
        now += 0.01;
        steady.tick(now);
    }

    assert_eq!(ids(&late), ids(&steady));
    let tilts =
        |state: &FairyState| -> Vec<f64> { state.stack().iter().map(|card| card.tilt).collect() };
    assert_eq!(tilts(&late), tilts(&steady));
    assert_eq!(late.hint(), steady.hint());
    assert_eq!(late.reject_tick(), steady.reject_tick());
    assert_eq!(late.can_restore(), steady.can_restore());
    assert_eq!(
        late.is_animating(10.0),
        steady.is_animating(10.0),
        "both settled"
    );
    assert_eq!(late.next_deadline(), steady.next_deadline());
    let late_scene = late.scene(10.0, 1.0);
    let steady_scene = steady.scene(10.0, 1.0);
    assert_eq!(late_scene.cards.len(), steady_scene.cards.len());
    assert_eq!(late_scene.hint, steady_scene.hint);
}

#[test]
fn a_lone_card_has_no_tilt_and_depths_go_down_from_the_bottom() {
    let mut state = FairyState::new(9);
    hold(&mut state, 0.0);
    let scene = state.scene(0.0, 2.0);
    assert_eq!(scene.cards.len(), 1);
    assert_eq!(scene.cards[0].tilt, 0.0);
    assert_eq!(scene.cards[0].depth, 0);
    assert_eq!(scene.scale, 2.0);

    let mut state = FairyState::new(9);
    for _ in 0..5 {
        hold(&mut state, 0.0);
    }
    let scene = state.scene(0.0, 1.0);
    let depths: Vec<usize> = scene.cards.iter().map(|card| card.depth).collect();
    assert_eq!(depths, vec![3, 2, 1, 0], "bottom first, four visible");
}

#[test]
fn leaving_progress_is_half_way_half_way_through() {
    let mut state = FairyState::new(10);
    hold(&mut state, 0.0);
    state.dissolve(0.0);
    match state.scene(0.11, 1.0).cards[0].leaving {
        Some(Leaving::Dissolve(p)) => approx(p, 0.5),
        other => panic!("expected a dissolve, got {other:?}"),
    }

    let mut state = FairyState::new(10);
    hold(&mut state, 0.0);
    let id = state.top().unwrap().id;
    state.deliver(&[id], "saved", 0.0);
    match state.scene(0.13, 1.0).cards[0].leaving {
        Some(Leaving::Drop(p)) => approx(p, 0.5),
        other => panic!("expected a drop, got {other:?}"),
    }
}

#[test]
fn hint_and_capture_opacity_fade_over_their_durations() {
    let mut state = FairyState::new(11);
    hold(&mut state, 0.0);
    approx(state.scene(0.0, 1.0).hint_opacity, 0.0);
    approx(state.scene(0.09, 1.0).hint_opacity, 0.5);
    approx(state.scene(0.18, 1.0).hint_opacity, 1.0);

    state.tick(2.4);
    assert_eq!(state.hint(), None, "cleared at once");
    assert_eq!(
        state.scene(2.4, 1.0).hint.as_deref(),
        Some("shake to discard"),
        "old text stays while fading"
    );
    approx(state.scene(2.49, 1.0).hint_opacity, 0.5);
    approx(state.scene(2.58, 1.0).hint_opacity, 0.0);
    assert_eq!(state.scene(2.58, 1.0).hint, None, "gone after the fall");

    approx(state.scene(0.0, 1.0).opacity, 1.0);
    state.set_hidden_for_capture(true, 3.0);
    approx(state.scene(3.075, 1.0).opacity, 0.5);
    approx(state.scene(3.15, 1.0).opacity, 0.0);
    state.set_hidden_for_capture(false, 4.0);
    approx(state.scene(4.075, 1.0).opacity, 0.5);
    approx(state.scene(4.15, 1.0).opacity, 1.0);
}

#[test]
fn is_animating_goes_false_once_everything_settles() {
    let mut state = FairyState::new(12);
    assert!(!state.is_animating(0.0));
    assert_eq!(state.next_deadline(), None);

    hold(&mut state, 0.0);
    assert!(state.is_animating(0.0));
    let next = state.next_deadline().expect("a fade-in deadline");
    approx(next, 0.18);

    state.tick(2.4);
    assert!(state.is_animating(2.4), "the hint is still fading out");
    let next = state.next_deadline().expect("a fade-out deadline");
    approx(next, 2.58);

    state.tick(2.58);
    assert!(!state.is_animating(2.58));
    assert_eq!(state.next_deadline(), None);
}
