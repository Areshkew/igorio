//! Translation of `fairy-cursor/tests/core/RecordingViewportTests.swift`.

use igorio_core::geom::{Point, Rect};
use igorio_core::viewport::RecordingViewport;

fn display() -> Rect {
    Rect::new(0.0, 0.0, 1600.0, 1000.0)
}

fn settle(v: &mut RecordingViewport, cursor: Point, following: bool) {
    for _ in 0..120 {
        v.step(cursor, following, 1.0 / 60.0);
    }
}

/// `CGRect.contains(_ rect: CGRect)`: the inner rect is entirely inside the outer one.
fn contains_rect(outer: Rect, inner: Rect) -> bool {
    inner.min_x() >= outer.min_x()
        && inner.max_x() <= outer.max_x()
        && inner.min_y() >= outer.min_y()
        && inner.max_y() <= outer.max_y()
}

#[track_caller]
fn assert_close(actual: f64, expected: f64, accuracy: f64) {
    assert!(
        (actual - expected).abs() <= accuracy,
        "expected {actual} to be within {accuracy} of {expected}"
    );
}

#[test]
fn test_starts_showing_the_whole_display() {
    let v = RecordingViewport::new(display());
    assert_eq!(v.area(), display());
}

#[test]
fn test_zoom_eases_toward_target() {
    let mut v = RecordingViewport::new(display());
    v.zoom_by(2.0_f64.ln());
    assert_close(v.target_zoom(), 2.0, 0.0001);
    v.step(Point::new(800.0, 500.0), false, 1.0 / 60.0);
    assert!(v.zoom() > 1.0);
    assert!(v.zoom() < 2.0, "one frame is not the whole way");
    settle(&mut v, Point::new(800.0, 500.0), true);
    assert_close(v.zoom(), 2.0, 0.001);
    assert_close(v.area().width, 800.0, 1.0);
    assert_close(v.area().height, 500.0, 1.0);
}

#[test]
fn test_zoom_is_clamped() {
    let mut v = RecordingViewport::new(display());
    v.zoom_by(-5.0);
    assert_eq!(v.target_zoom(), 1.0);
    v.zoom_by(50.0);
    assert_eq!(v.target_zoom(), 4.0);
}

#[test]
fn test_area_stays_on_the_display() {
    let mut v = RecordingViewport::new(display());
    v.zoom_by(3.0_f64.ln());
    settle(&mut v, Point::new(1.0, 1.0), true);
    assert!(v.area().min_x() >= display().min_x() - 0.001);
    assert!(v.area().min_y() >= display().min_y() - 0.001);
    settle(&mut v, Point::new(1599.0, 999.0), true);
    assert!(v.area().max_x() <= display().max_x() + 0.001);
    assert!(v.area().max_y() <= display().max_y() + 0.001);
}

#[test]
fn test_area_stays_put_without_option() {
    let mut v = RecordingViewport::new(display());
    v.zoom_by(2.0_f64.ln());
    settle(&mut v, Point::new(800.0, 500.0), true);
    let before = v.area();
    settle(&mut v, Point::new(1500.0, 900.0), false);
    assert_close(v.area().mid_x(), before.mid_x(), 0.001);
    assert_close(v.area().mid_y(), before.mid_y(), 0.001);
}

#[test]
fn test_area_centres_on_the_cursor_while_option_is_held() {
    let mut v = RecordingViewport::new(display());
    v.zoom_by(2.0_f64.ln());
    settle(&mut v, Point::new(800.0, 500.0), true);
    settle(&mut v, Point::new(700.0, 450.0), true);
    assert_close(v.area().mid_x(), 700.0, 1.0);
    assert_close(v.area().mid_y(), 450.0, 1.0);
}

#[test]
fn test_letting_go_of_option_stops_the_area_at_once() {
    let mut v = RecordingViewport::new(display());
    v.zoom_by(2.0_f64.ln());
    settle(&mut v, Point::new(800.0, 500.0), true);
    for _ in 0..6 {
        v.step(Point::new(1300.0, 800.0), true, 1.0 / 60.0);
    }
    let at_release = v.area();
    assert!(at_release.mid_x() > 800.0, "it started moving");
    settle(&mut v, Point::new(1300.0, 800.0), false);
    assert_close(v.area().mid_x(), at_release.mid_x(), 0.001);
    assert_close(v.area().mid_y(), at_release.mid_y(), 0.001);
}

#[test]
fn test_zoom_still_eases_while_pinned() {
    let mut v = RecordingViewport::new(display());
    v.zoom_by(2.0_f64.ln());
    settle(&mut v, Point::new(400.0, 300.0), false);
    assert_close(v.zoom(), 2.0, 0.001);
    assert_close(v.area().mid_x(), 800.0, 1.0);
}

#[test]
fn test_reset_eases_back_to_the_whole_display() {
    let mut v = RecordingViewport::new(display());
    v.zoom_by(3.0_f64.ln());
    settle(&mut v, Point::new(1300.0, 800.0), true);
    v.reset();
    assert_eq!(v.target_zoom(), 1.0);
    v.step(Point::new(1300.0, 800.0), false, 1.0 / 60.0);
    assert!(v.zoom() > 1.0, "one frame is not the whole way");
    assert!(contains_rect(display(), v.area()), "stays on the display on the way out");
    settle(&mut v, Point::new(1300.0, 800.0), false);
    assert_eq!(v.area(), display());
}

#[test]
fn test_display_with_offset_origin() {
    let left = Rect::new(-1920.0, -300.0, 1920.0, 1080.0);
    let mut v = RecordingViewport::new(left);
    v.zoom_by(2.0_f64.ln());
    settle(&mut v, Point::new(-1900.0, -280.0), true);
    assert_close(v.area().min_x(), left.min_x(), 0.5);
    assert_close(v.area().min_y(), left.min_y(), 0.5);
}
