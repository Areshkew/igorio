//! Translated from `fairy-cursor/tests/core/ShakeDetectorTests.swift`.

use std::f64::consts::PI;

use igorio_core::geom::Point;
use igorio_core::shake::{ShakeDetector, ShakeGesture, ShakeResult};

/// Synthetic cursor paths, sampled like a real mouse/trackpad stream.
struct Motion;

#[derive(Clone, Copy)]
struct Sample {
    t: f64,
    x: f64,
    y: f64,
}

type Path = Vec<Sample>;

/// Named-argument stand-in for `Motion.shake`'s optional parameters.
#[derive(Clone, Copy)]
struct ShakeSpec {
    amplitude: f64,
    hz: f64,
    duration: f64,
    angle: f64,
    rate: f64,
    start: f64,
    origin: Point,
    jitter: f64,
    seed: u64,
}

impl Default for ShakeSpec {
    fn default() -> Self {
        Self {
            amplitude: 0.0,
            hz: 0.0,
            duration: 0.0,
            angle: 0.0,
            rate: 120.0,
            start: 0.0,
            origin: Point::new(800.0, 500.0),
            jitter: 0.0,
            seed: 1,
        }
    }
}

impl Motion {
    /// Back-and-forth along `angle` (radians) with the given amplitude (points, half-stroke)
    /// and full cycles per second.
    fn shake(spec: &ShakeSpec) -> Path {
        let mut rng = SeededRandom::new(spec.seed);
        let mut path: Path = Vec::new();
        let n = (spec.duration * spec.rate) as i64;
        for i in 0..=n {
            let t = i as f64 / spec.rate;
            let s = spec.amplitude * (2.0 * PI * spec.hz * t).sin();
            let jx = if spec.jitter > 0.0 {
                rng.next_in(-1.0, 1.0) * spec.jitter
            } else {
                0.0
            };
            let jy = if spec.jitter > 0.0 {
                rng.next_in(-1.0, 1.0) * spec.jitter
            } else {
                0.0
            };
            path.push(Sample {
                t: spec.start + t,
                x: spec.origin.x + s * spec.angle.cos() + jx,
                y: spec.origin.y + s * spec.angle.sin() + jy,
            });
        }
        path
    }

    fn circle(radius: f64, hz: f64, duration: f64) -> Path {
        let rate = 120.0;
        let mut path: Path = Vec::new();
        let n = (duration * rate) as i64;
        for i in 0..=n {
            let t = i as f64 / rate;
            let a = 2.0 * PI * hz * t;
            path.push(Sample {
                t,
                x: 800.0 + radius * a.cos(),
                y: 500.0 + radius * a.sin(),
            });
        }
        path
    }

    /// Straight moves between waypoints at a given speed (points/s), pausing `dwell` at each.
    fn waypoints(points: &[Point], speed: f64, dwell: f64, start: f64) -> Path {
        let rate = 120.0;
        let mut path: Path = Vec::new();
        let mut t = start;
        let mut current = match points.first() {
            Some(current) => *current,
            None => return Vec::new(),
        };
        path.push(Sample {
            t,
            x: current.x,
            y: current.y,
        });
        for next in points.iter().skip(1) {
            let d = (next.x - current.x).hypot(next.y - current.y);
            let steps = ((d / speed) * rate) as i64;
            let steps = steps.max(1);
            for k in 1..=steps {
                t += 1.0 / rate;
                let f = k as f64 / steps as f64;
                path.push(Sample {
                    t,
                    x: current.x + (next.x - current.x) * f,
                    y: current.y + (next.y - current.y) * f,
                });
            }
            if dwell > 0.0 {
                let dwell_steps = (dwell * rate) as i64;
                for _ in 0..dwell_steps {
                    t += 1.0 / rate;
                    path.push(Sample {
                        t,
                        x: next.x,
                        y: next.y,
                    });
                }
            }
            current = *next;
        }
        path
    }

    fn feed(detector: &mut ShakeDetector, path: &Path) -> Vec<f64> {
        let mut fired: Vec<f64> = Vec::new();
        for sample in path {
            if detector.feed(sample.x, sample.y, sample.t) {
                fired.push(sample.t);
            }
        }
        fired
    }
}

struct SeededRandom {
    state: u64,
}

impl SeededRandom {
    fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    fn next_uint(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn next_in(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * (self.next_uint() % 1_000_000) as f64 / 1_000_000.0
    }
}

fn shake(amplitude: f64, hz: f64, duration: f64) -> Path {
    Motion::shake(&ShakeSpec {
        amplitude,
        hz,
        duration,
        ..ShakeSpec::default()
    })
}

fn shake_starting_at(amplitude: f64, hz: f64, duration: f64, start: f64) -> Path {
    Motion::shake(&ShakeSpec {
        amplitude,
        hz,
        duration,
        start,
        ..ShakeSpec::default()
    })
}

// MARK: Deliberate shakes are detected

#[test]
fn test_horizontal_shake_fires() {
    let mut d = ShakeDetector::new();
    assert!(!Motion::feed(&mut d, &shake(60.0, 3.0, 1.2)).is_empty());
}

#[test]
fn test_vertical_and_diagonal_shakes_fire() {
    for angle in [PI / 2.0, PI / 4.0, -PI / 3.0, 2.5] {
        let mut d = ShakeDetector::new();
        let path = Motion::shake(&ShakeSpec {
            amplitude: 50.0,
            hz: 3.5,
            duration: 1.2,
            angle,
            ..ShakeSpec::default()
        });
        assert!(!Motion::feed(&mut d, &path).is_empty(), "angle {angle}");
    }
}

#[test]
fn test_small_quick_trackpad_wiggle_fires() {
    // ±22 pt (44 pt strokes) at 4 Hz.
    let mut d = ShakeDetector::new();
    assert!(!Motion::feed(&mut d, &shake(22.0, 4.0, 1.0)).is_empty());
}

#[test]
fn test_shake_with_hand_jitter_fires() {
    let mut d = ShakeDetector::new();
    let path = Motion::shake(&ShakeSpec {
        amplitude: 50.0,
        hz: 3.0,
        duration: 1.3,
        jitter: 3.0,
        seed: 7,
        ..ShakeSpec::default()
    });
    assert!(!Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_shake_at_high_polling_rate_fires() {
    // 1000 Hz gaming mouse.
    let mut d = ShakeDetector::new();
    let path = Motion::shake(&ShakeSpec {
        amplitude: 40.0,
        hz: 3.0,
        duration: 1.2,
        rate: 1000.0,
        ..ShakeSpec::default()
    });
    assert!(!Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_shake_at_low_event_rate_fires() {
    // Coalesced 30 Hz stream (busy main thread).
    let mut d = ShakeDetector::new();
    let path = Motion::shake(&ShakeSpec {
        amplitude: 60.0,
        hz: 3.0,
        duration: 1.3,
        rate: 30.0,
        ..ShakeSpec::default()
    });
    assert!(!Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_fires_only_once_per_cooldown() {
    let mut d = ShakeDetector::new();
    let fired = Motion::feed(&mut d, &shake(60.0, 4.0, 1.3));
    assert_eq!(fired.len(), 1, "{fired:?}");
}

#[test]
fn test_fires_again_after_cooldown() {
    let mut d = ShakeDetector::new();
    let fired = Motion::feed(&mut d, &shake(60.0, 4.0, 3.5));
    assert!(fired.len() >= 2);
    if fired.len() >= 2 {
        assert!(fired[1] - fired[0] > d.cooldown);
    }
}

// MARK: Ordinary movement never fires

#[test]
fn test_slow_back_and_forth_does_not_fire() {
    // Reading / comparing two things: 100 pt strokes at 0.8 Hz.
    let mut d = ShakeDetector::new();
    assert!(Motion::feed(&mut d, &shake(100.0, 0.8, 5.0)).is_empty());
}

#[test]
fn test_two_fast_flicks_do_not_fire() {
    // Flick to a far corner and back, twice — only 3 reversals.
    let mut d = ShakeDetector::new();
    let a = Point::new(200.0, 300.0);
    let b = Point::new(1400.0, 900.0);
    let path = Motion::waypoints(&[a, b, a, b, a], 6000.0, 0.0, 0.0);
    assert!(Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_circling_does_not_fire() {
    for radius in [30.0, 45.0, 80.0, 150.0] {
        for hz in [1.0, 2.0, 3.0] {
            let mut d = ShakeDetector::new();
            let path = Motion::circle(radius, hz, 3.0);
            assert!(
                Motion::feed(&mut d, &path).is_empty(),
                "r={radius} hz={hz}"
            );
        }
    }
}

#[test]
fn test_staircase_zig_zag_does_not_fire() {
    // Moving down a list: right, down, left?, … 90° turns only.
    let mut pts: Vec<Point> = Vec::new();
    let mut p = Point::new(400.0, 200.0);
    for i in 0..12 {
        pts.push(p);
        p = if i % 2 == 0 {
            Point::new(p.x + 80.0, p.y)
        } else {
            Point::new(p.x, p.y + 60.0)
        };
    }
    let mut d = ShakeDetector::new();
    assert!(Motion::feed(&mut d, &Motion::waypoints(&pts, 1500.0, 0.0, 0.0)).is_empty());
}

#[test]
fn test_hunting_between_buttons_with_pauses_does_not_fire() {
    // Fast moves between toolbar buttons, but a short stop on each.
    let pts: Vec<Point> = (0..10)
        .map(|i| {
            Point::new(
                if i % 2 == 0 { 300.0 } else { 420.0 },
                40.0,
            )
        })
        .collect();
    let mut d = ShakeDetector::new();
    assert!(Motion::feed(&mut d, &Motion::waypoints(&pts, 2000.0, 0.35, 0.0)).is_empty());
}

#[test]
fn test_overshoot_and_correct_does_not_fire() {
    let pts = [
        Point::new(100.0, 100.0),
        Point::new(900.0, 500.0),
        Point::new(860.0, 480.0),
        Point::new(880.0, 490.0),
        Point::new(870.0, 486.0),
    ];
    let mut d = ShakeDetector::new();
    assert!(Motion::feed(&mut d, &Motion::waypoints(&pts, 3000.0, 0.0, 0.0)).is_empty());
}

#[test]
fn test_tiny_jitter_does_not_fire() {
    // A resting hand on a trackpad: ±6 pt noise at high frequency.
    let mut d = ShakeDetector::new();
    let path = Motion::shake(&ShakeSpec {
        amplitude: 6.0,
        hz: 8.0,
        duration: 4.0,
        jitter: 4.0,
        seed: 3,
        ..ShakeSpec::default()
    });
    assert!(Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_small_fast_shake_below_swing_does_not_fire() {
    // ±12 pt (24 pt strokes) — below min_swing.
    let mut d = ShakeDetector::new();
    assert!(Motion::feed(&mut d, &shake(12.0, 5.0, 2.0)).is_empty());
}

// MARK: Warps, display jumps, pauses

#[test]
fn test_cursor_warps_back_and_forth_do_not_fire() {
    // An app (or display rearrangement) teleporting the pointer between two spots.
    let mut path: Path = Vec::new();
    for i in 0..40 {
        let t = i as f64 * 0.03;
        path.push(Sample {
            t,
            x: if i % 2 == 0 { 100.0 } else { 2400.0 },
            y: 500.0,
        });
    }
    let mut d = ShakeDetector::new();
    assert!(Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_warp_mid_shake_resets_count() {
    let mut d = ShakeDetector::new();
    // ~3 reversals.
    let mut path = shake(50.0, 3.0, 0.55);
    let t_warp = path.last().unwrap().t + 0.008;
    // Warp.
    path.push(Sample {
        t: t_warp,
        x: 3000.0,
        y: 1200.0,
    });
    path.extend(Motion::shake(&ShakeSpec {
        amplitude: 50.0,
        hz: 3.0,
        duration: 0.3,
        start: t_warp + 0.008,
        origin: Point::new(3000.0, 1200.0),
        ..ShakeSpec::default()
    }));
    assert!(Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_crossing_displays_continuously_still_detects_shake() {
    // Shaking across a display boundary at x = 1512 (global coordinates are continuous).
    let mut d = ShakeDetector::new();
    let path = Motion::shake(&ShakeSpec {
        amplitude: 60.0,
        hz: 3.0,
        duration: 1.3,
        origin: Point::new(1512.0, 400.0),
        ..ShakeSpec::default()
    });
    assert!(!Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_negative_coordinates_on_left_display() {
    let mut d = ShakeDetector::new();
    let path = Motion::shake(&ShakeSpec {
        amplitude: 60.0,
        hz: 3.0,
        duration: 1.3,
        origin: Point::new(-900.0, -200.0),
        ..ShakeSpec::default()
    });
    assert!(!Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_pause_splits_reversals_across_the_window() {
    // Two half-shakes separated by a pause are not one shake.
    let mut d = ShakeDetector::new();
    let mut path = shake(60.0, 3.0, 0.45);
    let start = path.last().unwrap().t + 0.6;
    path.extend(Motion::shake(&ShakeSpec {
        amplitude: 60.0,
        hz: 3.0,
        duration: 0.45,
        start,
        ..ShakeSpec::default()
    }));
    assert!(Motion::feed(&mut d, &path).is_empty());
}

#[test]
fn test_timestamp_going_backwards_resets() {
    let mut d = ShakeDetector::new();
    assert!(!d.feed(0.0, 0.0, 10.0));
    // Must not crash or count.
    assert!(!d.feed(50.0, 0.0, 9.0));
    assert_eq!(d.pending_reversals(), 0);
}

#[test]
fn test_reset_clears_partial_shake() {
    let mut d = ShakeDetector::new();
    let _ = Motion::feed(&mut d, &shake(60.0, 3.0, 0.55));
    d.reset();
    assert_eq!(d.pending_reversals(), 0);
    assert!(Motion::feed(&mut d, &shake_starting_at(60.0, 3.0, 0.3, 0.6)).is_empty());
}

#[test]
fn test_repeated_same_point_is_harmless() {
    let mut d = ShakeDetector::new();
    for i in 0..500 {
        assert!(!d.feed(10.0, 10.0, i as f64 * 0.008));
    }
}

// MARK: Two-stage gesture

#[test]
fn test_single_shake_discards_top_only() {
    let mut g = ShakeGesture::new();
    let mut results: Vec<ShakeResult> = Vec::new();
    for s in shake(60.0, 3.0, 1.0) {
        let r = g.feed(s.x, s.y, s.t);
        if r != ShakeResult::None {
            results.push(r);
        }
    }
    assert_eq!(results, vec![ShakeResult::DiscardTop]);
}

#[test]
fn test_shaking_on_past_first_discard_discards_all() {
    let mut g = ShakeGesture::new();
    let mut results: Vec<ShakeResult> = Vec::new();
    for s in shake(60.0, 3.5, 2.6) {
        let r = g.feed(s.x, s.y, s.t);
        if r != ShakeResult::None {
            results.push(r);
        }
    }
    assert_eq!(results.first().copied(), Some(ShakeResult::DiscardTop));
    assert!(
        results.contains(&ShakeResult::DiscardAll),
        "{results:?}"
    );
}

#[test]
fn test_discard_all_not_listened_for_without_a_first_discard() {
    let mut g = ShakeGesture::new();
    // Feed the long detector's worth of slow reversals: never a discardAll first.
    let mut first: Option<ShakeResult> = None;
    for s in shake(60.0, 3.5, 2.6) {
        let r = g.feed(s.x, s.y, s.t);
        if r != ShakeResult::None && first.is_none() {
            first = Some(r);
        }
    }
    assert_eq!(first, Some(ShakeResult::DiscardTop));
}

#[test]
fn test_stop_after_first_discard_keeps_the_rest() {
    let mut g = ShakeGesture::new();
    let mut results: Vec<ShakeResult> = Vec::new();
    let mut path = shake(60.0, 3.0, 0.95);
    // Then ordinary movement.
    let start = path.last().unwrap().t + 0.01;
    path.extend(Motion::waypoints(
        &[
            Point::new(800.0, 500.0),
            Point::new(1000.0, 700.0),
            Point::new(400.0, 300.0),
        ],
        900.0,
        0.0,
        start,
    ));
    for s in &path {
        let r = g.feed(s.x, s.y, s.t);
        if r != ShakeResult::None {
            results.push(r);
        }
    }
    assert_eq!(results, vec![ShakeResult::DiscardTop]);
}
