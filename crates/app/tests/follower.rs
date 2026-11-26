use igorio_app::follower::Follower;
use igorio_app::layout::THUMB_MAX;
use igorio_core::geom::{Point, Rect};

const SCREEN: Rect = Rect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 };
const MIDDLE: Point = Point { x: 960.0, y: 540.0 };
/// The box the follower keeps on screen, logical px (see `target_origin`).
const CONTENT_W: f64 = THUMB_MAX.width + 14.0;
const CONTENT_H: f64 = THUMB_MAX.height + 40.0;
/// Flipped window origin for a mouse at (1900, 1000) at scale 1.0.
const FLIPPED_X: f64 = 1900.0 - 14.0 - CONTENT_W - 18.0;
const FLIPPED_Y: f64 = 1000.0 - 18.0 - CONTENT_H - 18.0;

fn distance(a: Point, b: Point) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

#[test]
fn defaults_match_swift() {
    let follower = Follower::new();
    assert_eq!(follower.offset.dx, 14.0);
    assert_eq!(follower.offset.dy, 18.0);
    assert_eq!(follower.stiffness, 16.0);
}

#[test]
fn middle_of_the_screen_sits_below_right() {
    let follower = Follower::new();
    let origin = follower.target_origin(MIDDLE, SCREEN, 1.0);
    // mouse + (offset - inset) on both axes in top-left coordinates.
    assert_eq!(origin, Point::new(956.0, 540.0));
    assert_eq!(origin, Point::new(MIDDLE.x + 14.0 - 18.0, MIDDLE.y + 18.0 - 18.0));
}

#[test]
fn near_the_right_edge_flips_to_the_left() {
    let follower = Follower::new();
    let mouse = Point::new(1900.0, 540.0);
    let origin = follower.target_origin(mouse, SCREEN, 1.0);
    // content top-left x = 1900 - 14 - content width, then minus the inset.
    assert_eq!(origin.x, FLIPPED_X);
    assert_eq!(origin.y, 540.0);
    assert!(origin.x < mouse.x);
    // The whole content box now fits inside the work area.
    assert!(origin.x + 18.0 + CONTENT_W <= SCREEN.max_x());
}

#[test]
fn near_the_bottom_edge_flips_above() {
    let follower = Follower::new();
    let mouse = Point::new(960.0, 1000.0);
    let origin = follower.target_origin(mouse, SCREEN, 1.0);
    // content top-left y = 1000 - 18 - content height, then minus the inset.
    assert_eq!(origin.x, 956.0);
    assert_eq!(origin.y, FLIPPED_Y);
    assert!(origin.y < mouse.y);
    assert!(origin.y + 18.0 + CONTENT_H <= SCREEN.max_y());
}

#[test]
fn in_the_bottom_right_corner_flips_both_ways() {
    let follower = Follower::new();
    let mouse = Point::new(1900.0, 1000.0);
    let origin = follower.target_origin(mouse, SCREEN, 1.0);
    assert_eq!(origin, Point::new(FLIPPED_X, FLIPPED_Y));
}

#[test]
fn scale_multiplies_offsets_and_sizes() {
    let follower = Follower::new();
    let origin = follower.target_origin(MIDDLE, SCREEN, 1.5);
    // (14, 18) * 1.5 = (21, 27); inset 18 * 1.5 = 27.
    assert_eq!(origin, Point::new(960.0 - 6.0, 540.0));

    let mouse = Point::new(1900.0, 1000.0);
    let origin = follower.target_origin(mouse, SCREEN, 1.5);
    // content is 1.5x in physical px; flipped top-left is mouse - offset - size.
    assert_eq!(origin, Point::new(1900.0 - 21.0 - CONTENT_W * 1.5 - 27.0,
            1000.0 - 27.0 - CONTENT_H * 1.5 - 27.0,));
}

#[test]
fn work_area_with_a_non_zero_origin() {
    let follower = Follower::new();

    let second_monitor = Rect { x: 1920.0, y: 0.0, width: 1920.0, height: 1080.0 };
    let mouse = Point::new(1920.0 + 960.0, 540.0);
    assert_eq!(follower.target_origin(mouse, second_monitor, 1.0), Point::new(2876.0, 540.0));

    let left_monitor = Rect { x: -1920.0, y: 0.0, width: 1920.0, height: 1080.0 };
    let mouse = Point::new(-960.0, 540.0);
    assert_eq!(follower.target_origin(mouse, left_monitor, 1.0), Point::new(-964.0, 540.0));

    // The right edge of the left monitor still flips the content.
    let mouse = Point::new(-20.0, 540.0);
    let origin = follower.target_origin(mouse, left_monitor, 1.0);
    assert!(origin.x < mouse.x);
    assert!(origin.x + 18.0 + CONTENT_W <= left_monitor.max_x());
}

#[test]
fn first_step_snaps_and_reports_the_target() {
    let mut follower = Follower::new();
    let target = follower.target_origin(MIDDLE, SCREEN, 1.0);
    assert_eq!(follower.step(MIDDLE, SCREEN, 1.0, 1.0 / 120.0), Some(target));
}

#[test]
fn steps_converge_to_a_moved_target() {
    let mut follower = Follower::new();
    let start = Point::new(500.0, 500.0);
    let first = follower.step(start, SCREEN, 1.0, 0.016).unwrap();
    assert_eq!(first, follower.target_origin(start, SCREEN, 1.0));

    let moved = Point::new(700.0, 600.0);
    let target = follower.target_origin(moved, SCREEN, 1.0);
    let mut last = distance(first, target);
    for _ in 0..6 {
        let origin = follower.step(moved, SCREEN, 1.0, 0.016).expect("still moving");
        let now = distance(origin, target);
        assert!(now < last, "distance {now} did not shrink from {last}");
        last = now;
    }
    assert!(last < distance(first, target) / 2.0);
}

#[test]
fn dt_above_the_cap_behaves_like_the_cap() {
    let mut capped = Follower::new();
    let mut huge = Follower::new();
    capped.step(MIDDLE, SCREEN, 1.0, 0.016);
    huge.step(MIDDLE, SCREEN, 1.0, 0.016);

    let moved = Point::new(1200.0, 700.0);
    assert_eq!(
        capped.step(moved, SCREEN, 1.0, 0.05),
        huge.step(moved, SCREEN, 1.0, 5.0)
    );
}

#[test]
fn nan_dt_counts_as_zero() {
    let mut follower = Follower::new();
    follower.step(MIDDLE, SCREEN, 1.0, 0.016);
    assert_eq!(follower.step(Point::new(1200.0, 700.0), SCREEN, 1.0, f64::NAN), None);
}

#[test]
fn step_returns_none_when_nothing_moved() {
    let mut follower = Follower::new();
    follower.step(MIDDLE, SCREEN, 1.0, 0.016);
    assert_eq!(follower.step(MIDDLE, SCREEN, 1.0, 0.016), None);
    assert_eq!(follower.step(MIDDLE, SCREEN, 1.0, 0.0), None);
    // A mouse move smaller than 0.05 px does not produce a new origin.
    let nearby = Point::new(MIDDLE.x + 0.01, MIDDLE.y);
    assert_eq!(follower.step(nearby, SCREEN, 1.0, 0.05), None);
}

#[test]
fn reset_snaps_to_the_current_target() {
    let mut follower = Follower::new();
    follower.step(MIDDLE, SCREEN, 1.0, 0.016);
    follower.step(Point::new(1200.0, 700.0), SCREEN, 1.0, 0.016);
    follower.reset();
    let target = follower.target_origin(Point::new(1200.0, 700.0), SCREEN, 1.0);
    assert_eq!(follower.step(Point::new(1200.0, 700.0), SCREEN, 1.0, 0.016), Some(target));
}

#[test]
fn is_settled_tracks_the_target() {
    let mut follower = Follower::new();
    // (4, 0) maps to the initial (0, 0) position, but nothing has started yet.
    assert!(!follower.is_settled(Point::new(4.0, 0.0), SCREEN, 1.0));
    assert!(!follower.is_settled(MIDDLE, SCREEN, 1.0));
    follower.step(MIDDLE, SCREEN, 1.0, 0.016);
    assert!(follower.is_settled(MIDDLE, SCREEN, 1.0));

    let moved = Point::new(700.0, 600.0);
    assert!(!follower.is_settled(moved, SCREEN, 1.0));
    for _ in 0..200 {
        follower.step(moved, SCREEN, 1.0, 0.05);
    }
    assert!(follower.is_settled(moved, SCREEN, 1.0));
}

#[test]
fn non_finite_scale_or_mouse_is_ignored() {
    let mut follower = Follower::new();
    let snapped = follower.step(MIDDLE, SCREEN, 1.0, 0.016).unwrap();
    assert_eq!(snapped, Point::new(956.0, 540.0));

    let moved = Point::new(1200.0, 700.0);
    // A bad scale changes nothing, so the follower is still settled at the old target.
    assert_eq!(follower.step(moved, SCREEN, f64::NAN, 0.016), None);
    assert!(follower.is_settled(MIDDLE, SCREEN, 1.0));
    // A bad mouse makes the target non-finite, which is ignored too.
    assert_eq!(
        follower.step(Point::new(f64::NAN, f64::NAN), SCREEN, 1.0, 0.016),
        None
    );
    assert!(follower.is_settled(MIDDLE, SCREEN, 1.0));

    // The follower still works afterwards.
    assert!(follower.step(moved, SCREEN, 1.0, 0.016).is_some());
}

#[test]
fn non_finite_or_non_positive_scale_is_ignored() {
    let mut follower = Follower::new();
    for scale in [0.0, -1.0, f64::NEG_INFINITY, f64::INFINITY, f64::NAN] {
        assert_eq!(follower.step(MIDDLE, SCREEN, scale, 0.016), None, "scale {scale}");
        assert!(!follower.is_settled(MIDDLE, SCREEN, scale), "scale {scale}");
    }
    // The ignored calls did not start the follower, so this one snaps.
    assert_eq!(follower.step(MIDDLE, SCREEN, 1.0, 0.016), Some(Point::new(956.0, 540.0)));
}

#[test]
fn a_non_finite_position_heals_by_snapping() {
    let mut follower = Follower::new();
    follower.step(MIDDLE, SCREEN, 1.0, 0.016);
    // `stiffness` is public; a bad value drives the stored position non-finite.
    follower.stiffness = f64::NAN;
    let moved = Point::new(1200.0, 700.0);
    assert_eq!(follower.step(moved, SCREEN, 1.0, 0.016), None);
    follower.stiffness = 16.0;
    let target = follower.target_origin(moved, SCREEN, 1.0);
    assert_eq!(follower.step(moved, SCREEN, 1.0, 0.016), Some(target));
    assert!(follower.is_settled(moved, SCREEN, 1.0));
}
