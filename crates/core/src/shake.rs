//! Shake detection, translated from `reference/fairy-cursor/sources/core/ShakeDetector.swift`.
//! All times are seconds and all distances are points, taken as parameters so the code stays
//! pure.

use crate::geom::{Point, Vector};

/// Detects a deliberate "shake" — fast back-and-forth strokes along one line — from a stream of
/// cursor positions (global points). Any direction works (horizontal, vertical, diagonal).
///
/// A shake discards a screenshot, so this is deliberately strict about what counts:
///  - a stroke must travel at least `min_swing` points and move at `min_stroke_speed` or faster;
///  - a reversal is a turn of at least ~145° (`reversal_cosine`): circling, zig-zagging along a
///    staircase or wandering between targets turn by ~90° and never count;
///  - `required_reversals` reversals must fall within `window` seconds;
///  - a pause longer than `max_pause` or a cursor warp (a jump larger than `max_jump` between two
///    consecutive samples, e.g. an app warping the pointer or a display reconfiguration) starts
///    over.
///
/// Not thread-safe; feed it from one thread.
#[derive(Debug, Clone)]
pub struct ShakeDetector {
    pub required_reversals: usize,
    pub window: f64,
    /// Minimum stroke length (points). 30 pt is still an easy trackpad wiggle.
    pub min_swing: f64,
    /// Minimum average speed of the stroke that ends in a reversal (points/second).
    pub min_stroke_speed: f64,
    /// cos of the smallest turn that counts as a reversal (-0.82 ≈ 145°).
    pub reversal_cosine: f64,
    pub max_pause: f64,
    pub max_jump: f64,
    pub cooldown: f64,

    last: Option<Sample>,
    /// Where the current stroke started (last turning point).
    anchor: Option<Sample>,
    /// Farthest point reached along the current stroke.
    extreme: Option<Sample>,
    /// Unit direction of the current stroke.
    direction: Option<Vector>,
    reversals: Vec<f64>,
    last_fired: f64,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    p: Point,
    t: f64,
}

impl Default for ShakeDetector {
    fn default() -> Self {
        Self {
            required_reversals: 4,
            window: 1.0,
            min_swing: 30.0,
            min_stroke_speed: 220.0,
            reversal_cosine: -0.82,
            max_pause: 0.3,
            max_jump: 500.0,
            cooldown: 1.0,
            last: None,
            anchor: None,
            extreme: None,
            direction: None,
            reversals: Vec::new(),
            last_fired: f64::NEG_INFINITY,
        }
    }
}

impl ShakeDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the current motion (keeps the cooldown).
    pub fn reset(&mut self) {
        self.last = None;
        self.anchor = None;
        self.extreme = None;
        self.direction = None;
        self.reversals.clear();
    }

    /// Number of reversals currently counted inside the window (for diagnostics/tests).
    pub fn pending_reversals(&self) -> usize {
        self.reversals.len()
    }

    /// Feed a cursor position; returns true once per detected shake.
    pub fn feed(&mut self, x: f64, y: f64, t: f64) -> bool {
        let sample = Sample { p: Point::new(x, y), t };
        if let Some(previous) = self.last {
            let gap = t - previous.t;
            if gap < 0.0 || gap > self.max_pause || distance(previous.p, sample.p) > self.max_jump {
                self.reset();
            }
        }
        self.last = Some(sample);

        let start = match self.anchor {
            Some(start) => start,
            None => {
                self.anchor = Some(sample);
                return false;
            }
        };
        let (dir, far) = match (self.direction, self.extreme) {
            (Some(dir), Some(far)) => (dir, far),
            _ => {
                // No stroke yet. A wiggle can start anywhere, so measure from the starting point
                // and from the farthest point reached so far (a small shake centred on the first
                // sample never gets `min_swing` away from it).
                let from_start = vector(start.p, sample.p);
                let provisional = self.extreme.and_then(|provisional| {
                    let to_sample = vector(provisional.p, sample.p);
                    if length(to_sample) >= self.min_swing && length(to_sample) > length(from_start) {
                        Some((provisional, to_sample))
                    } else {
                        None
                    }
                });
                if let Some((provisional, to_sample)) = provisional {
                    self.anchor = Some(provisional);
                    self.direction = Some(unit(to_sample));
                    self.extreme = Some(sample);
                } else if length(from_start) >= self.min_swing {
                    self.direction = Some(unit(from_start));
                    self.extreme = Some(sample);
                } else {
                    let farther = match self.extreme {
                        None => true,
                        Some(extreme) => length(from_start) > length(vector(start.p, extreme.p)),
                    };
                    if farther {
                        // Provisional: farthest point from the start so far.
                        self.extreme = Some(sample);
                    }
                }
                return false;
            }
        };

        let reached = dot(vector(start.p, far.p), dir);
        let now = dot(vector(start.p, sample.p), dir);
        if now >= reached {
            // Still travelling the same way.
            self.extreme = Some(sample);
            return false;
        }
        let back = vector(far.p, sample.p);
        let back_length = length(back);
        if back_length < self.min_swing {
            return false;
        }

        let cosine = dot(back, dir) / back_length;
        let stroke_length = length(vector(start.p, far.p));
        let stroke_time = (far.t - start.t).max(0.001);
        let fast_enough = stroke_length / stroke_time >= self.min_stroke_speed;
        if cosine <= self.reversal_cosine && fast_enough {
            self.reversals.push(t);
        } else if cosine > self.reversal_cosine {
            // A sideways turn (circle, staircase): not a shake; start counting again.
            self.reversals.clear();
        }
        self.anchor = Some(far);
        self.direction = Some(unit(back));
        self.extreme = Some(sample);

        self.reversals.retain(|reversal| t - reversal <= self.window);
        let cooled_down = t - self.last_fired > self.cooldown;
        if !cooled_down || self.reversals.len() < self.required_reversals {
            return false;
        }
        self.last_fired = t;
        self.reset();
        true
    }
}

/// What one cursor sample meant for the two-stage gesture.
///
/// Swift nests this as `ShakeGesture.Result`; Rust has no nested types, so the enum lives at
/// module level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShakeResult {
    None,
    DiscardTop,
    DiscardAll,
}

/// The two-stage shake: a shake discards the top screenshot; shaking on past it (within
/// `arm_duration`) discards the rest of the pile. The second stage is only listened for right
/// after a first discard, so one enthusiastic shake can't silently empty a whole pile.
#[derive(Debug, Clone)]
pub struct ShakeGesture {
    pub single: ShakeDetector,
    pub long: ShakeDetector,
    /// How long after a first discard further shaking counts towards "discard all".
    pub arm_duration: f64,
    armed_until: f64,
}

impl Default for ShakeGesture {
    fn default() -> Self {
        Self {
            single: ShakeDetector::new(),
            long: ShakeDetector {
                required_reversals: 4,
                window: 1.4,
                cooldown: 1.5,
                ..ShakeDetector::default()
            },
            arm_duration: 1.6,
            armed_until: f64::NEG_INFINITY,
        }
    }
}

impl ShakeGesture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.single.reset();
        self.long.reset();
        self.armed_until = f64::NEG_INFINITY;
    }

    pub fn feed(&mut self, x: f64, y: f64, t: f64) -> ShakeResult {
        if t <= self.armed_until && self.long.feed(x, y, t) {
            self.armed_until = f64::NEG_INFINITY;
            self.single.reset();
            return ShakeResult::DiscardAll;
        }
        if self.single.feed(x, y, t) {
            self.long.reset();
            self.armed_until = t + self.arm_duration;
            return ShakeResult::DiscardTop;
        }
        ShakeResult::None
    }
}

/// Feeds cursor movement to the shake gesture only when a shake can mean "discard": something
/// is held and the system screenshot UI is not up. While the user aims the ⌘⇧4 crosshair or
/// sweeps across windows in window-pick mode, the pile is invisible — that movement used to
/// count, and quietly shook screenshots off.
#[derive(Debug, Clone)]
pub struct ShakeGate {
    pub gesture: ShakeGesture,
    capture_ui_visible: bool,
}

impl Default for ShakeGate {
    fn default() -> Self {
        Self {
            gesture: ShakeGesture::new(),
            capture_ui_visible: false,
        }
    }
}

impl ShakeGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn capture_ui_visible(&self) -> bool {
        self.capture_ui_visible
    }

    pub fn screenshot_ui_shown(&mut self) {
        self.capture_ui_visible = true;
        self.gesture.reset();
    }

    pub fn screenshot_ui_gone(&mut self) {
        self.capture_ui_visible = false;
        self.gesture.reset();
    }

    /// A new capture landed on the pile: start from a clean slate.
    pub fn captured(&mut self) {
        self.gesture.reset();
    }

    /// Swift's `move`; renamed because `move` is a Rust keyword.
    pub fn move_(&mut self, x: f64, y: f64, t: f64, holding: bool) -> ShakeResult {
        if !holding || self.capture_ui_visible {
            return ShakeResult::None;
        }
        self.gesture.feed(x, y, t)
    }
}

// MARK: - Vector helpers

fn vector(a: Point, b: Point) -> Vector {
    Vector::new(b.x - a.x, b.y - a.y)
}

fn length(v: Vector) -> f64 {
    (v.dx * v.dx + v.dy * v.dy).sqrt()
}

fn distance(a: Point, b: Point) -> f64 {
    length(vector(a, b))
}

fn dot(a: Vector, b: Vector) -> f64 {
    a.dx * b.dx + a.dy * b.dy
}

fn unit(v: Vector) -> Vector {
    let l = length(v);
    if l > 0.0 {
        Vector::new(v.dx / l, v.dy / l)
    } else {
        Vector::new(1.0, 0.0)
    }
}
