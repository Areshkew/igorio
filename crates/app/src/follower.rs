//! Moves the panel after the cursor with exponential smoothing.
//! Ported from `Follower.swift`, math only (no AppKit timers or windows).
//!
//! Coordinates here are Windows-style: physical pixels, origin at the top-left of the
//! virtual desktop, y grows DOWN. Swift uses points with y growing UP. The conversion is
//! a sign flip on the y offsets only:
//!
//! - Swift's "below the cursor" is `mouse.y - offset.y`; below the cursor in top-left
//!   coordinates is `mouse.y + offset.y`.
//! - Swift's "above the cursor" is `mouse.y + offset.y + contentH` (top edge); above the
//!   cursor in top-left coordinates is `mouse.y - offset.y - contentH` because the content
//!   grows downward from its top edge.
//! - Swift's panel origin is the bottom-left, so it adds the inset and subtracts the panel
//!   height. The Windows window origin is the top-left, and the content top-left is inset
//!   from it on both axes: `origin = contentTopLeft - (inset_x, INSET)`.
//!
//! All logical values (offset, the card size, `INSET`) are multiplied by `scale`, the
//! monitor's physical pixels per logical pixel, before they meet physical mouse/work-area
//! coordinates.

use igorio_core::geom::{Point, Rect, Vector};

use crate::layout::{CardSize, INSET};

/// Default offset of the content's top-left from the cursor hotspot, logical px.
const DEFAULT_OFFSET: Vector = Vector::new(14.0, 18.0);
/// Default response speed (1/s).
const DEFAULT_STIFFNESS: f64 = 16.0;
/// Extra room around the thumbnail used to decide whether the card fits on screen.
const CONTENT_PAD_X: f64 = 14.0;
const CONTENT_PAD_Y: f64 = 40.0;

pub struct Follower {
    /// Offset of the content's top-left from the cursor hotspot, logical px. Default (14, 18).
    pub offset: Vector,
    /// Response speed in 1/s; higher snaps faster. Default 16.
    pub stiffness: f64,
    /// Card size used for the flip box (the tallest/widest card plus padding).
    pub card_size: CardSize,
    /// Window origin (top-left, physical px); the smoothed position.
    position: Point,
    /// Last origin handed to the caller, so tiny moves are not reported twice.
    reported: Point,
    started: bool,
}

impl Follower {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next `step` snaps to the target instead of easing towards it.
    pub fn reset(&mut self) {
        self.started = false;
    }

    /// Window origin (top-left, physical px) so the content sits below-right of the cursor,
    /// flipping to the left and/or above when it would leave `work_area`.
    pub fn target_origin(&self, mouse: Point, work_area: Rect, scale: f64) -> Point {
        // Sizes and offsets are logical; the cursor and work area are physical.
        let thumb_max = self.card_size.thumb_max();
        let content_w = (thumb_max.width + CONTENT_PAD_X) * scale;
        let content_h = (thumb_max.height + CONTENT_PAD_Y) * scale;
        let offset_x = self.offset.dx * scale;
        let offset_y = self.offset.dy * scale;
        let inset = INSET * scale;

        // Content top-left, below-right of the cursor (y grows down, so +offset is below).
        let mut x = mouse.x + offset_x;
        let mut y = mouse.y + offset_y;

        if x + content_w > work_area.max_x() {
            x = mouse.x - offset_x - content_w;
        }
        // Swift tests `contentTopLeft.y - contentH < f.minY` (content bottom below the
        // screen bottom in y-up coordinates). In top-left coordinates the same situation
        // is `contentTopLeft.y + contentH > f.maxY`, and the flipped top edge is above the
        // cursor: `mouse.y - offset.y - contentH`.
        if y + content_h > work_area.max_y() {
            y = mouse.y - offset_y - content_h;
        }

        // The window top-left is the content top-left minus the insets.
        Point::new(x - self.card_size.inset_x() * scale, y - inset)
    }

    /// Advances by `dt` seconds (clamped to 0.0..=0.05; NaN counts as 0).
    /// The first call after new/reset snaps to the target. Returns the new origin when it
    /// moved more than 0.05 px on either axis since the last returned value, else None.
    /// A call whose target is not finite (NaN/infinite mouse, scale or work area) is ignored.
    pub fn step(&mut self, mouse: Point, work_area: Rect, scale: f64, dt: f64) -> Option<Point> {
        let target = self.finite_target(mouse, work_area, scale)?;
        if !self.started || !self.position.x.is_finite() || !self.position.y.is_finite() {
            // A non-finite position (for example from a bad `stiffness`) heals by snapping.
            self.started = true;
            self.position = target;
            self.reported = target;
            return Some(target);
        }

        let dt = if dt.is_nan() { 0.0 } else { dt.clamp(0.0, 0.05) };
        let alpha = 1.0 - (-dt * self.stiffness).exp();
        self.position.x += (target.x - self.position.x) * alpha;
        self.position.y += (target.y - self.position.y) * alpha;

        if (self.position.x - self.reported.x).abs() > 0.05
            || (self.position.y - self.reported.y).abs() > 0.05
        {
            self.reported = self.position;
            Some(self.position)
        } else {
            None
        }
    }

    /// True when the position is within 0.5 px of the target (the caller can stop ticking).
    /// False before the first `step` and for a non-finite or non-positive scale.
    pub fn is_settled(&self, mouse: Point, work_area: Rect, scale: f64) -> bool {
        if !self.started {
            return false;
        }
        let Some(target) = self.finite_target(mouse, work_area, scale) else {
            return false;
        };
        (self.position.x - target.x).abs() <= 0.5 && (self.position.y - target.y).abs() <= 0.5
    }

    /// The target when the inputs and the result are finite, so callers can ignore bad
    /// values without touching the stored position.
    fn finite_target(&self, mouse: Point, work_area: Rect, scale: f64) -> Option<Point> {
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let target = self.target_origin(mouse, work_area, scale);
        if !target.x.is_finite() || !target.y.is_finite() {
            return None;
        }
        Some(target)
    }
}

impl Default for Follower {
    fn default() -> Self {
        Self {
            offset: DEFAULT_OFFSET,
            stiffness: DEFAULT_STIFFNESS,
            card_size: CardSize::default(),
            position: Point::ZERO,
            reported: Point::ZERO,
            started: false,
        }
    }
}
