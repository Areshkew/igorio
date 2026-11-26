//! Geometry shared by the overlay window, the follower and the drawing code.
//! From `FairyLayout` in `FairyView.swift`. All values are logical pixels (96 DPI);
//! multiply by the monitor scale to get physical pixels.

use igorio_core::geom::Size;

/// Room for the stack plus its soft shadow and tilt.
pub const PANEL_SIZE: Size = Size::new(200.0, 170.0);
/// Inset from the panel's top-left to the content's top-left (shadow and tilt bleed).
pub const INSET: f64 = 18.0;
pub const THUMB_MAX: Size = Size::new(112.0, 84.0);
/// How many sheets of the stack are drawn (older ones hide underneath).
pub const VISIBLE_CARDS: usize = 4;
