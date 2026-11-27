//! Translated from `fairy-cursor/tests/core/HeldPileTests.swift`.

use std::sync::atomic::{AtomicU64, Ordering};

use igorio_core::geom::CardId;
use igorio_core::pile::{HeldPile, HasCardId, LeaveStyle, ReleaseReason};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Card {
    id: CardId,
    name: String,
}

impl HasCardId for Card {
    fn card_id(&self) -> CardId {
        self.id
    }
}

/// Swift's `let id = UUID()`: every card gets its own id.
fn card(name: &str) -> Card {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    Card {
        id: CardId(NEXT.fetch_add(1, Ordering::Relaxed)),
        name: name.to_string(),
    }
}

fn pile(names: &[&str]) -> (HeldPile<Card>, Vec<Card>) {
    let mut p = HeldPile::default();
    let cards: Vec<Card> = names.iter().map(|name| card(name)).collect();
    for c in &cards {
        p.push(c.clone());
    }
    (p, cards)
}

#[test]
fn test_empty() {
    let p = HeldPile::<Card>::default();
    assert!(!p.is_holding());
    assert_eq!(p.top(), None);
    assert!(p.is_empty());
    assert!(!p.can_restore());
}

#[test]
fn test_top_is_newest() {
    let (p, c) = pile(&["a", "b", "c"]);
    assert_eq!(p.top(), Some(&c[2]));
    let active = p.active_top_first();
    let names: Vec<&str> = active.iter().map(|card| card.name.as_str()).collect();
    assert_eq!(names, ["c", "b", "a"]);
}

#[test]
fn test_leaving_card_is_not_top_and_not_active() {
    let (mut p, c) = pile(&["a", "b"]);
    p.begin_leaving(&[c[1].id], LeaveStyle::Dissolve, ReleaseReason::Shaken, 0.0);
    assert_eq!(p.top(), Some(&c[0]));
    assert_eq!(p.cards().len(), 2, "still drawn while animating");
    assert_eq!(p.style(c[1].id), Some(LeaveStyle::Dissolve));
    p.finish_leaving(c[1].id);
    assert_eq!(p.cards().to_vec(), vec![c[0].clone()]);
}

#[test]
fn test_capture_during_leave_animation_is_not_swept_away() {
    // Old bug: dropAll kept dropping "the top" until empty, taking a capture that landed
    // mid-delivery with it (never saved/pasted anywhere).
    let (mut p, c) = pile(&["a", "b"]);
    let ids: Vec<CardId> = p.active_top_first().iter().map(|card| card.id).collect();
    let delivered = p.begin_leaving(&ids, LeaveStyle::Queued, ReleaseReason::Delivered, 0.0);
    let mut delivered_sorted = delivered.clone();
    delivered_sorted.sort();
    let mut all_sorted: Vec<CardId> = c.iter().map(|card| card.id).collect();
    all_sorted.sort();
    assert_eq!(delivered_sorted, all_sorted);
    let fresh = card("fresh");
    p.push(fresh.clone());
    for id in &delivered {
        p.finish_leaving(*id);
    }
    assert_eq!(p.cards().to_vec(), vec![fresh.clone()]);
    assert_eq!(p.top(), Some(&fresh));
}

#[test]
fn test_overlapping_leaves_both_complete() {
    // Old bug: a second dissolve overwrote the single `leavingID`, so the first card's
    // finish was skipped and it came back.
    let (mut p, c) = pile(&["a", "b", "c"]);
    p.begin_leaving(&[c[2].id], LeaveStyle::Drop, ReleaseReason::Delivered, 0.0);
    p.begin_leaving(&[c[1].id], LeaveStyle::Dissolve, ReleaseReason::Shaken, 0.1);
    p.finish_leaving(c[2].id);
    p.finish_leaving(c[1].id);
    assert_eq!(p.cards().to_vec(), vec![c[0].clone()]);
}

#[test]
fn test_begin_leaving_ignores_unknown_and_already_leaving() {
    let (mut p, c) = pile(&["a"]);
    assert_eq!(
        p.begin_leaving(
            &[CardId(999_999)],
            LeaveStyle::Drop,
            ReleaseReason::Delivered,
            0.0
        ),
        Vec::<CardId>::new()
    );
    assert_eq!(
        p.begin_leaving(&[c[0].id], LeaveStyle::Drop, ReleaseReason::Delivered, 0.0),
        vec![c[0].id]
    );
    assert_eq!(
        p.begin_leaving(&[c[0].id], LeaveStyle::Drop, ReleaseReason::Delivered, 0.0),
        Vec::<CardId>::new()
    );
    assert_eq!(p.released().len(), 1);
}

#[test]
fn test_finish_leaving_of_active_card_is_no_op() {
    let (mut p, c) = pile(&["a"]);
    p.finish_leaving(c[0].id);
    assert_eq!(p.cards().to_vec(), c);
}

#[test]
fn test_restore_after_shake() {
    let (mut p, c) = pile(&["a", "b"]);
    p.begin_leaving(&[c[1].id], LeaveStyle::Dissolve, ReleaseReason::Shaken, 0.0);
    p.finish_leaving(c[1].id);
    assert!(p.can_restore());
    assert_eq!(p.restore_last(1.0), vec![c[1].clone()]);
    assert_eq!(p.cards().to_vec(), vec![c[0].clone(), c[1].clone()]);
    assert_eq!(p.top(), Some(&c[1]));
    assert!(!p.can_restore());
}

#[test]
fn test_restore_while_still_animating_cancels_the_leave() {
    let (mut p, c) = pile(&["a", "b"]);
    p.begin_leaving(&[c[1].id], LeaveStyle::Dissolve, ReleaseReason::Shaken, 0.0);
    p.restore_last(0.1);
    let names: Vec<&str> = p.cards().iter().map(|card| card.name.as_str()).collect();
    assert_eq!(names, ["a", "b"], "no duplicate");
    assert_eq!(p.style(c[1].id), None);
    // Late animation completion must not remove it.
    p.finish_leaving(c[1].id);
    assert_eq!(p.cards().len(), 2);
}

#[test]
fn test_restore_discard_all_keeps_order() {
    let (mut p, c) = pile(&["a", "b", "c"]);
    p.discard_all(ReleaseReason::DiscardedAll, 0.0);
    assert!(p.is_empty());
    p.restore_last(1.0);
    assert_eq!(p.cards().to_vec(), c);
    assert_eq!(p.top(), Some(&c[2]));
}

#[test]
fn test_restore_delivered_pile_keeps_order() {
    let (mut p, c) = pile(&["a", "b", "c"]);
    let ids: Vec<CardId> = p.active_top_first().iter().map(|card| card.id).collect();
    let ids = p.begin_leaving(&ids, LeaveStyle::Queued, ReleaseReason::Delivered, 0.0);
    for id in &ids {
        p.finish_leaving(*id);
    }
    p.restore_last(1.0);
    assert_eq!(p.cards().to_vec(), c);
}

#[test]
fn test_restore_goes_on_top_of_newer_cards() {
    let (mut p, c) = pile(&["a"]);
    p.begin_leaving(&[c[0].id], LeaveStyle::Dissolve, ReleaseReason::Shaken, 0.0);
    p.finish_leaving(c[0].id);
    let b = card("b");
    p.push(b);
    p.restore_last(1.0);
    let names: Vec<&str> = p.cards().iter().map(|card| card.name.as_str()).collect();
    assert_eq!(names, ["b", "a"]);
}

#[test]
fn test_released_memory_is_bounded_by_batches() {
    let mut p: HeldPile<Card> = HeldPile::new(2, 100, 15.0 * 60.0);
    for i in 0..5 {
        let c = card(&i.to_string());
        p.push(c.clone());
        p.begin_leaving(
            &[c.id],
            LeaveStyle::Dissolve,
            ReleaseReason::Shaken,
            i as f64,
        );
        p.finish_leaving(c.id);
    }
    assert_eq!(p.released().len(), 2);
    let restored = p.restore_last(5.0);
    let names: Vec<&str> = restored.iter().map(|card| card.name.as_str()).collect();
    assert_eq!(names, ["4"]);
    let restored = p.restore_last(5.0);
    let names: Vec<&str> = restored.iter().map(|card| card.name.as_str()).collect();
    assert_eq!(names, ["3"]);
    assert_eq!(p.restore_last(5.0), Vec::<Card>::new());
}

#[test]
fn test_released_memory_is_bounded_by_cards_but_keeps_newest_batch() {
    let mut p: HeldPile<Card> = HeldPile::new(5, 3, 15.0 * 60.0);
    let many: Vec<Card> = (0..5).map(|i| card(&format!("m{i}"))).collect();
    for c in &many {
        p.push(c.clone());
    }
    p.discard_all(ReleaseReason::DiscardedAll, 0.0);
    assert_eq!(
        p.released().len(),
        1,
        "newest batch is kept even when over the card budget"
    );
    let one = card("one");
    p.push(one.clone());
    p.begin_leaving(
        &[one.id],
        LeaveStyle::Dissolve,
        ReleaseReason::Shaken,
        1.0,
    );
    assert_eq!(p.released().len(), 1, "older over-budget batch dropped");
    assert_eq!(
        p.released().last().map(|batch| batch.cards.clone()),
        Some(vec![one])
    );
}

#[test]
fn test_released_expires() {
    let mut p: HeldPile<Card> = HeldPile::new(3, 8, 60.0);
    let c = card("a");
    p.push(c);
    p.discard_all(ReleaseReason::DiscardedAll, 0.0);
    assert_eq!(p.restore_last(61.0), Vec::<Card>::new());
}

#[test]
fn test_stash_only_goes_to_memory() {
    let mut p = HeldPile::<Card>::default();
    let ghost = card("ghost");
    p.stash(ghost.clone(), ReleaseReason::Spurious, 0.0);
    assert!(p.is_empty());
    assert_eq!(
        p.released().last().map(|batch| batch.reason),
        Some(ReleaseReason::Spurious)
    );
    assert_eq!(p.restore_last(1.0), vec![ghost.clone()]);
    assert_eq!(p.top(), Some(&ghost));
}

#[test]
fn test_discard_all_with_nothing_active_remembers_nothing() {
    let mut p = HeldPile::<Card>::default();
    p.discard_all(ReleaseReason::DiscardedAll, 0.0);
    assert!(!p.can_restore());
}

#[test]
fn test_restyle_queued_to_drop() {
    let (mut p, c) = pile(&["a"]);
    p.begin_leaving(
        &[c[0].id],
        LeaveStyle::Queued,
        ReleaseReason::Delivered,
        0.0,
    );
    p.restyle(c[0].id, LeaveStyle::Drop);
    assert_eq!(p.style(c[0].id), Some(LeaveStyle::Drop));
    // Unknown: no-op.
    p.restyle(CardId(999_999), LeaveStyle::Drop);
}
