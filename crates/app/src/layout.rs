//! Geometry shared by the overlay window, the follower and the drawing code.
//! From `FairyLayout` in `FairyView.swift`. All values are logical pixels (96 DPI);
//! multiply by the monitor scale to get physical pixels.

use igorio_core::geom::Size;

/// Room for the stack plus its soft shadow and tilt, and for the hint centred under it.
pub const PANEL_SIZE: Size = Size::new(300.0, 170.0);
/// Vertical inset from the panel top to the content top (shadow and tilt bleed).
pub const INSET: f64 = 18.0;
/// Horizontal inset from the panel left to the content left. It puts the middle of a
/// full-width card at the middle of the panel, so a hint wider than the card has the
/// same room on both sides.
pub const INSET_X: f64 = (PANEL_SIZE.width - THUMB_MAX.width) / 2.0;
/// The macOS original uses 112 x 84; smaller here so the pile covers less of the screen.
pub const THUMB_MAX: Size = Size::new(80.0, 60.0);
/// How many sheets of the stack are drawn (older ones hide underneath).
pub const VISIBLE_CARDS: usize = 4;
