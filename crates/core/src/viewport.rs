//! The part of the display a screen recording shows: the output video is this area scaled to the
//! full frame, so a smaller area reads as zooming in.
//!
//! ⌥ + scroll sets a target zoom; the actual zoom eases toward it, so a flick of the wheel becomes
//! one smooth push in rather than a jump. The area moves only while ⌥ is held: then it glides to
//! centre on the cursor. Let go of ⌥ and it stays where it is, wherever the cursor wanders.
//!
//! Geometry is in global points with a top-left origin (CoreGraphics / CGEvent space), the same
//! space as `display`. Pure and value-typed; the recorder steps it once per output frame.

use crate::geom::{Point, Rect, Size};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordingViewport {
    /// The recorded display, in global points.
    pub display: Rect,
    pub min_zoom: f64,
    pub max_zoom: f64,
    /// How quickly the zoom catches up with its target (per second; higher is snappier).
    pub zoom_response: f64,
    /// How quickly the area glides to the cursor while following (per second).
    pub follow_response: f64,

    pub target_zoom: f64,
    /// The zoom in use right now. Swift spells the setter `zoom(by:)`; in Rust the method is
    /// `zoom_by` because a field and a method cannot share the name `zoom`.
    pub zoom: f64,
    pub center: Point,
    target_center: Point,
}

impl RecordingViewport {
    pub fn new(display: Rect) -> Self {
        let center = Point::new(display.mid_x(), display.mid_y());
        Self {
            display,
            min_zoom: 1.0,
            max_zoom: 4.0,
            zoom_response: 9.0,
            follow_response: 7.0,
            target_zoom: 1.0,
            zoom: 1.0,
            center,
            target_center: center,
        }
    }

    /// Change the target zoom by a factor of e^amount (positive zooms in).
    pub fn zoom_by(&mut self, amount: f64) {
        self.target_zoom = self.max_zoom.min(self.min_zoom.max(self.target_zoom * amount.exp()));
    }

    /// Back to the whole display, with the usual ease (double-tap ⌥).
    pub fn reset(&mut self) {
        self.target_zoom = self.min_zoom;
    }

    /// The area shown right now.
    pub fn area(&self) -> Rect {
        self.area_at(self.zoom, self.center)
    }

    /// Advance by `dt` seconds with the cursor at `cursor`. `following` (⌥ held) lets the area
    /// move to the cursor; otherwise it stays put and only the zoom keeps easing.
    pub fn step(&mut self, cursor: Point, following: bool, dt: f64) {
        // `max(0, min(dt, 0.25))`: NaN counts as 0, like Swift's `max`/`min`.
        let dt = if dt > 0.25 { 0.25 } else if dt > 0.0 { dt } else { 0.0 };
        self.zoom = self.approach(self.zoom, self.target_zoom, self.zoom_response, dt);
        if (self.zoom - self.target_zoom).abs() < 0.0005 {
            self.zoom = self.target_zoom;
        }

        let size = self.area_size(self.zoom);
        // Following: glide toward the cursor. Not following: stop right where it is (no drift
        // toward wherever the cursor was when ⌥ was let go).
        self.target_center = if following { cursor } else { self.center };
        self.target_center = self.clamped_center(self.target_center, size);
        self.center = Point::new(
            self.approach(self.center.x, self.target_center.x, self.follow_response, dt),
            self.approach(self.center.y, self.target_center.y, self.follow_response, dt),
        );
        self.center = self.clamped_center(self.center, size);
    }

    // MARK: - Geometry

    fn area_size(&self, zoom: f64) -> Size {
        Size::new(self.display.width / zoom, self.display.height / zoom)
    }

    fn area_at(&self, zoom: f64, center: Point) -> Rect {
        let size = self.area_size(zoom);
        let c = self.clamped_center(center, size);
        Rect::new(c.x - size.width / 2.0, c.y - size.height / 2.0, size.width, size.height)
    }

    /// Keep an area of `size` around `point` entirely on the display.
    fn clamped_center(&self, point: Point, size: Size) -> Point {
        Point::new(
            point.x.max(self.display.min_x() + size.width / 2.0).min(self.display.max_x() - size.width / 2.0),
            point.y.max(self.display.min_y() + size.height / 2.0).min(self.display.max_y() - size.height / 2.0),
        )
    }

    /// Exponential approach: frame-rate independent smoothing.
    fn approach(&self, value: f64, target: f64, response: f64, dt: f64) -> f64 {
        value + (target - value) * (1.0 - (-response * dt).exp())
    }
}
