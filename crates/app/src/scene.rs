//! What one frame of the overlay shows. `state` holds the facts, `main` turns them into
//! a `Scene` with animation progress, `draw` paints it. Plain data, no behaviour.

use std::sync::Arc;

use igorio_kit::Image;

/// How a card is leaving, with progress from 0.0 (just started) to 1.0 (gone).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Leaving {
    /// Shaken off: shrinks to 0.72 around its centre, blurs and fades.
    Dissolve(f64),
    /// Delivered: shrinks to 0.35 towards its top-left corner and fades.
    Drop(f64),
}

#[derive(Debug, Clone)]
pub struct SceneCard {
    pub image: Arc<Image>,
    /// Degrees, clockwise. Already 0 when the pile has one card.
    pub tilt: f64,
    /// 0 is the top card, 1 the one under it, and so on.
    pub depth: usize,
    /// 0.0 to 1.0 within one "no" wobble; 0.0 when not wobbling.
    pub wobble: f64,
    pub leaving: Option<Leaving>,
    /// `Some` for a recording; the inner value is its length in seconds, if known.
    pub movie: Option<Option<f64>>,
}

#[derive(Debug, Clone)]
pub struct Scene {
    /// Bottom card first, top card last. At most `layout::VISIBLE_CARDS`.
    pub cards: Vec<SceneCard>,
    pub hint: Option<String>,
    /// 0.0 to 1.0, for the hint fading in and out.
    pub hint_opacity: f64,
    /// 0.0 to 1.0 for the whole overlay (0 while a system capture UI is up).
    pub opacity: f64,
    /// Physical pixels per logical pixel (1.0 at 96 DPI, 1.25 at 120 DPI).
    pub scale: f64,
}
