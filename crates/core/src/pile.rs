//! The pile of held cards, translated from
//! `fairy-cursor/sources/core/HeldPile.swift`.

use std::collections::HashMap;

use crate::geom::CardId;

/// Stands in for `Card: Identifiable where Card.ID == UUID`: the pile only needs the id.
pub trait HasCardId {
    fn card_id(&self) -> CardId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveStyle {
    /// Delivered, waiting for its turn to animate out (still drawn normally).
    Queued,
    /// Shaken off: blur + fade.
    Dissolve,
    /// Delivered: shrink towards the target.
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseReason {
    Shaken,
    DiscardedAll,
    Delivered,
    /// A capture that arrived from a screenshot session that had already ended on screen.
    Spurious,
}

/// One remember()ed release. Swift nests this in `HeldPile`; Rust keeps it at module level.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleasedBatch<Card> {
    /// In pile order (oldest first).
    pub cards: Vec<Card>,
    pub reason: ReleaseReason,
    pub at: f64,
}

/// The pile of screenshots the fairy carries, as a pure value: which cards are held, which are
/// on their way out (animating), and a short memory of what recently left so it can be brought
/// back ("Restore Last Screenshot").
///
/// Every operation names the cards it acts on by id, so a capture that lands while another
/// delivery is still animating can never be swept away with it.
#[derive(Debug, Clone)]
pub struct HeldPile<Card: HasCardId + Clone> {
    /// Oldest first; the last element is the top card.
    cards: Vec<Card>,
    leaving: HashMap<CardId, LeaveStyle>,
    /// Most recent last.
    released: Vec<ReleasedBatch<Card>>,

    pub max_released_batches: usize,
    pub max_released_cards: usize,
    /// Released cards older than this are forgotten (seconds).
    pub release_ttl: f64,
}

impl<Card: HasCardId + Clone> Default for HeldPile<Card> {
    fn default() -> Self {
        Self::new(3, 8, 15.0 * 60.0)
    }
}

impl<Card: HasCardId + Clone> HeldPile<Card> {
    pub fn new(max_released_batches: usize, max_released_cards: usize, release_ttl: f64) -> Self {
        Self {
            cards: Vec::new(),
            leaving: HashMap::new(),
            released: Vec::new(),
            max_released_batches,
            max_released_cards,
            release_ttl,
        }
    }

    // MARK: Queries

    /// Cards in pile order (oldest first), including those on their way out.
    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    /// Released batches, most recent last.
    pub fn released(&self) -> &[ReleasedBatch<Card>] {
        &self.released
    }

    /// Cards that gestures can still act on, oldest first.
    pub fn active(&self) -> Vec<Card> {
        self.cards
            .iter()
            .filter(|card| !self.leaving.contains_key(&card.card_id()))
            .cloned()
            .collect()
    }

    /// Active cards, top (newest) first: the order a pile is delivered in.
    pub fn active_top_first(&self) -> Vec<Card> {
        let mut cards = self.active();
        cards.reverse();
        cards
    }

    pub fn top(&self) -> Option<&Card> {
        self.cards
            .iter()
            .rev()
            .find(|card| !self.leaving.contains_key(&card.card_id()))
    }

    pub fn is_holding(&self) -> bool {
        self.top().is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    pub fn can_restore(&self) -> bool {
        !self.released.is_empty()
    }

    pub fn style(&self, id: CardId) -> Option<LeaveStyle> {
        self.leaving.get(&id).copied()
    }

    pub fn card(&self, id: CardId) -> Option<&Card> {
        self.cards.iter().find(|card| card.card_id() == id)
    }

    pub fn is_active(&self, id: CardId) -> bool {
        self.cards.iter().any(|card| card.card_id() == id) && !self.leaving.contains_key(&id)
    }

    // MARK: Mutations

    pub fn push(&mut self, card: Card) {
        self.cards.push(card);
    }

    /// Mark the given (active) cards as leaving and remember them as one released batch.
    /// Returns the ids that were actually active.
    pub fn begin_leaving(
        &mut self,
        ids: &[CardId],
        style: LeaveStyle,
        reason: ReleaseReason,
        now: f64,
    ) -> Vec<CardId> {
        let taken: Vec<CardId> = ids
            .iter()
            .copied()
            .filter(|id| self.is_active(*id))
            .collect();
        if taken.is_empty() {
            return Vec::new();
        }
        for id in &taken {
            self.leaving.insert(*id, style);
        }
        // Pile order.
        let gone: Vec<Card> = self
            .cards
            .iter()
            .filter(|card| taken.contains(&card.card_id()))
            .cloned()
            .collect();
        self.remember(gone, reason, now);
        taken
    }

    /// Change the animation style of a card that is already leaving (queued → drop).
    pub fn restyle(&mut self, id: CardId, to: LeaveStyle) {
        if !self.leaving.contains_key(&id) {
            return;
        }
        self.leaving.insert(id, to);
    }

    /// The leave animation finished: the card is gone from the pile (but still restorable).
    pub fn finish_leaving(&mut self, id: CardId) {
        if self.leaving.remove(&id).is_none() {
            return;
        }
        self.cards.retain(|card| card.card_id() != id);
    }

    /// Drop everything at once (no animation), remembering the active cards.
    pub fn discard_all(&mut self, reason: ReleaseReason, now: f64) {
        let gone = self.active();
        if !gone.is_empty() {
            self.remember(gone, reason, now);
        }
        self.cards.clear();
        self.leaving.clear();
    }

    /// Keep a card only in the released memory (never shown in the pile).
    pub fn stash(&mut self, card: Card, reason: ReleaseReason, now: f64) {
        self.remember(vec![card], reason, now);
    }

    /// Put the most recently released batch back on top of the pile. Cards still animating
    /// out are simply kept. Returns the restored cards.
    pub fn restore_last(&mut self, now: f64) -> Vec<Card> {
        self.prune(now);
        let batch = match self.released.pop() {
            Some(batch) => batch,
            None => return Vec::new(),
        };
        // Batches are kept in pile order (oldest first), so the old top ends up on top again.
        for card in &batch.cards {
            let id = card.card_id();
            if self.cards.iter().any(|held| held.card_id() == id) {
                self.leaving.remove(&id);
                // Move it to the top so it is the card gestures act on next.
                self.cards.retain(|held| held.card_id() != id);
            }
            self.cards.push(card.clone());
        }
        batch.cards
    }

    pub fn prune(&mut self, now: f64) {
        self.released.retain(|batch| now - batch.at <= self.release_ttl);
    }

    fn remember(&mut self, gone: Vec<Card>, reason: ReleaseReason, now: f64) {
        self.prune(now);
        self.released.push(ReleasedBatch {
            cards: gone,
            reason,
            at: now,
        });
        while self.released.len() > self.max_released_batches {
            self.released.remove(0);
        }
        // Bound memory: screenshots are big. Trim the oldest batches first, never the newest.
        while self.released.len() > 1
            && self
                .released
                .iter()
                .map(|batch| batch.cards.len())
                .sum::<usize>()
                > self.max_released_cards
        {
            self.released.remove(0);
        }
    }
}
