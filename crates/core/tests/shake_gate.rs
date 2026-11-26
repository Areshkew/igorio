//! Translated from `reference/fairy-cursor/tests/core/ShakeGateTests.swift`.

use std::f64::consts::PI;

use igorio_core::shake::{ShakeGate, ShakeResult};

#[derive(Clone, Copy)]
struct Sample {
    t: f64,
    x: f64,
    y: f64,
}

type Path = Vec<Sample>;

/// `Motion.shake` from `ShakeDetectorTests.swift`, with its documented defaults.
fn shake(amplitude: f64, hz: f64, duration: f64) -> Path {
    shake_starting_at(amplitude, hz, duration, 0.0)
}

fn shake_starting_at(amplitude: f64, hz: f64, duration: f64, start: f64) -> Path {
    let rate = 120.0;
    let mut path: Path = Vec::new();
    let n = (duration * rate) as i64;
    for i in 0..=n {
        let t = i as f64 / rate;
        let s = amplitude * (2.0 * PI * hz * t).sin();
        path.push(Sample {
            t: start + t,
            x: 800.0 + s,
            y: 500.0,
        });
    }
    path
}

fn run(gate: &mut ShakeGate, path: &Path, holding: bool) -> Vec<ShakeResult> {
    path.iter()
        .map(|sample| gate.move_(sample.x, sample.y, sample.t, holding))
        .filter(|result| *result != ShakeResult::None)
        .collect()
}

#[test]
fn test_shake_while_holding_discards() {
    let mut gate = ShakeGate::new();
    assert_eq!(
        run(&mut gate, &shake(60.0, 3.0, 1.0), true),
        vec![ShakeResult::DiscardTop]
    );
}

#[test]
fn test_nothing_held_never_discards() {
    let mut gate = ShakeGate::new();
    assert_eq!(
        run(&mut gate, &shake(60.0, 3.0, 1.0), false),
        Vec::<ShakeResult>::new()
    );
}

#[test]
fn test_picking_a_window_during_cmd_shift_4_never_discards() {
    // ⌘⇧4, Space, then sweeping back and forth across windows to choose one.
    let mut gate = ShakeGate::new();
    gate.screenshot_ui_shown();
    assert_eq!(
        run(&mut gate, &shake(300.0, 2.5, 3.0), true),
        Vec::<ShakeResult>::new()
    );
    assert_eq!(
        run(&mut gate, &shake_starting_at(60.0, 4.0, 2.0, 3.1), true),
        Vec::<ShakeResult>::new()
    );
}

#[test]
fn test_movement_during_capture_does_not_carry_over_afterwards() {
    let mut gate = ShakeGate::new();
    gate.screenshot_ui_shown();
    let _ = run(&mut gate, &shake(60.0, 3.0, 0.6), true);
    gate.screenshot_ui_gone();
    gate.captured();
    // Half a shake right after the capture is not enough.
    assert_eq!(
        run(&mut gate, &shake_starting_at(60.0, 3.0, 0.4, 0.62), true),
        Vec::<ShakeResult>::new()
    );
}

#[test]
fn test_partial_shake_before_capture_does_not_combine_with_after() {
    let mut gate = ShakeGate::new();
    let _ = run(&mut gate, &shake(60.0, 3.0, 0.55), true);
    gate.captured();
    assert_eq!(
        run(&mut gate, &shake_starting_at(60.0, 3.0, 0.35, 0.56), true),
        Vec::<ShakeResult>::new()
    );
}

#[test]
fn test_shake_works_again_once_the_capture_ui_is_gone() {
    let mut gate = ShakeGate::new();
    gate.screenshot_ui_shown();
    gate.screenshot_ui_gone();
    assert_eq!(
        run(&mut gate, &shake(60.0, 3.0, 1.0), true),
        vec![ShakeResult::DiscardTop]
    );
}
