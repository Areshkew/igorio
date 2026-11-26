//! Geometry shared by the overlay window, the follower and the drawing code.
//! From `FairyLayout` in `FairyView.swift`. All values are logical pixels (96 DPI);
//! multiply by the monitor scale to get physical pixels.

use igorio_core::geom::Size;
use serde::{Deserialize, Serialize};

/// Room for the stack plus its soft shadow and tilt, and for the hint centred under it.
pub const PANEL_SIZE: Size = Size::new(300.0, 170.0);
/// Vertical inset from the panel top to the content top (shadow and tilt bleed).
pub const INSET: f64 = 18.0;
/// How many sheets of the stack are drawn (older ones hide underneath).
pub const VISIBLE_CARDS: usize = 4;

/// The card size the user chooses in settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardSize {
    /// 80 x 60 logical px.
    #[default]
    Small,
    /// 96 x 72 logical px.
    Medium,
    /// 112 x 84 logical px, the macOS original.
    Large,
}

impl CardSize {
    /// The box a card is fitted into, logical px: 80 x 60, 96 x 72, 112 x 84 (the macOS size).
    pub const fn thumb_max(self) -> Size {
        match self {
            CardSize::Small => Size::new(80.0, 60.0),
            CardSize::Medium => Size::new(96.0, 72.0),
            CardSize::Large => Size::new(112.0, 84.0),
        }
    }

    /// Horizontal inset that centres a full-width card in the panel.
    pub fn inset_x(self) -> f64 {
        (PANEL_SIZE.width - self.thumb_max().width) / 2.0
    }
}
