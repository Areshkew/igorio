//! Translated from `fairy-cursor/tests/core/LegacyShakeTests.swift`.

use std::f64::consts::PI;

use igorio_core::shake::ShakeDetector;

/// The detector Fairy Cursor shipped with (verbatim logic): reversals counted per axis, 18 pt
/// swing, 4 reversals in 1 s. Kept here to document which ordinary movements it mistook for a
/// shake — each of these silently discarded a carried screenshot.
struct LegacyShakeDetector {
    required_reversals: i32,
    window: f64,
    min_swing: f64,
    cooldown: f64,
    samples: Vec<LegacySample>,
    last_fired: f64,
}

struct LegacySample {
    t: f64,
    x: f64,
    y: f64,
}

impl LegacyShakeDetector {
    fn new() -> Self {
        Self {
            required_reversals: 4,
            window: 1.0,
            min_swing: 18.0,
            cooldown: 1.0,
            samples: Vec::new(),
            last_fired: 0.0,
        }
    }

    fn feed(&mut self, x: f64, y: f64, t: f64) -> bool {
        self.samples.push(LegacySample { t, x, y });
        let cutoff = t - self.window;
        match self.samples.iter().position(|sample| sample.t >= cutoff) {
            Some(first) if first > 0 => {
                self.samples.drain(0..first);
            }
            _ => {}
        }
        let cooled_down = t - self.last_fired > self.cooldown;
        if !cooled_down || self.samples.len() < 4 {
            return false;
        }
        let xs: Vec<f64> = self.samples.iter().map(|sample| sample.x).collect();
        let ys: Vec<f64> = self.samples.iter().map(|sample| sample.y).collect();
        if self.reversals(&xs).max(self.reversals(&ys)) >= self.required_reversals {
            self.last_fired = t;
            self.samples.clear();
            return true;
        }
        false
    }

    fn reversals(&self, values: &[f64]) -> i32 {
        let mut count = 0;
        let mut direction = 0;
        let mut turning = values[0];
        for &v in values.iter().skip(1) {
            let d = v - turning;
            if direction == 0 {
                if d.abs() >= self.min_swing {
                    direction = if d > 0.0 { 1 } else { -1 };
                    turning = v;
                }
            } else if (d > 0.0) == (direction > 0) {
                turning = v;
            } else if d.abs() >= self.min_swing {
                count += 1;
                direction = -direction;
                turning = v;
            }
        }
        count
    }
}

#[derive(Clone, Copy)]
struct Sample {
    t: f64,
    x: f64,
    y: f64,
}

type Path = Vec<Sample>;

struct Motion;

impl Motion {
    fn shake(amplitude: f64, hz: f64, duration: f64) -> Path {
        let rate = 120.0;
        let mut path: Path = Vec::new();
        let n = (duration * rate) as i64;
        for i in 0..=n {
            let t = i as f64 / rate;
            let s = amplitude * (2.0 * PI * hz * t).sin();
            path.push(Sample {
                t,
                x: 800.0 + s,
                y: 500.0,
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
}

fn legacy_fires(path: &Path) -> bool {
    let mut d = LegacyShakeDetector::new();
    path.iter().any(|sample| d.feed(sample.x, sample.y, sample.t + 100.0))
}

fn new_fires(path: &Path) -> bool {
    let mut d = ShakeDetector::new();
    path.iter().any(|sample| d.feed(sample.x, sample.y, sample.t + 100.0))
}

#[test]
fn test_circling_was_a_false_shake() {
    let path = Motion::circle(40.0, 2.0, 1.5);
    assert!(legacy_fires(&path), "legacy detector discarded on circling");
    assert!(!new_fires(&path));
}

#[test]
fn test_slowish_comparing_was_a_false_shake() {
    // Glancing between two things 60 pt apart at ~2.2 Hz.
    let path = Motion::shake(30.0, 2.2, 1.5);
    assert!(legacy_fires(&path));
    // 60 pt strokes at 2.2 Hz is ~260 pt/s — still a "shake" by the new speed rule only if it
    // reverses 4 times within 1 s, which 2.2 Hz does not quite reach in a straight line.
    assert!(!new_fires(&Motion::shake(30.0, 1.6, 2.0)));
}

#[test]
fn test_small_circles_around_an_item_were_a_false_shake() {
    // Circling a word while talking someone through the screen.
    let path = Motion::circle(30.0, 2.5, 1.2);
    assert!(legacy_fires(&path));
    assert!(!new_fires(&path));
}

#[test]
fn test_diagonal_drift_with_vertical_jitter_was_a_false_shake() {
    // Moving steadily right while the hand bobs ±12 pt vertically (trackpad thumb drag):
    // the legacy y-axis counter saw reversals every bob.
    let mut path: Path = Vec::new();
    for i in 0..=150 {
        let t = i as f64 / 120.0;
        path.push(Sample {
            t,
            x: 200.0 + t * 600.0,
            y: 400.0 + 12.0 * (2.0 * PI * 3.0 * t).sin(),
        });
    }
    assert!(legacy_fires(&path));
    assert!(!new_fires(&path));
}

#[test]
fn test_warps_were_a_false_shake() {
    let mut path: Path = Vec::new();
    for i in 0..12 {
        path.push(Sample {
            t: i as f64 * 0.06,
            x: if i % 2 == 0 { 100.0 } else { 1900.0 },
            y: 500.0,
        });
    }
    assert!(legacy_fires(&path));
    assert!(!new_fires(&path));
}
