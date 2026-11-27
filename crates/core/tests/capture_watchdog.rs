//! Translated from `fairy-cursor/tests/core/CaptureWatchdogTests.swift`.

use igorio_core::capture_watchdog::CaptureWatchdog;

#[test]
fn test_overlay_seen_then_gone_turns_stale() {
    let mut w = CaptureWatchdog::new();
    w.session_started(true);
    assert!(!w.observe(true, 0.0));
    assert!(!w.observe(false, 1.0));
    assert!(!w.observe(false, 2.5));
    assert!(w.observe(false, 4.0));
    assert!(w.is_stale());
    assert!(!w.observe(false, 5.0), "fires once");
}

#[test]
fn test_overlay_never_seen_never_fires() {
    let mut w = CaptureWatchdog::new();
    w.session_started(true);
    for i in 0..100 {
        assert!(!w.observe(false, i as f64));
    }
}

#[test]
fn test_flicker_does_not_fire() {
    let mut w = CaptureWatchdog::new();
    w.session_started(true);
    let _ = w.observe(true, 0.0);
    assert!(!w.observe(false, 2.0));
    assert!(!w.observe(true, 2.9));
    assert!(!w.observe(false, 3.2));
    assert!(!w.observe(false, 6.0));
    assert!(w.observe(false, 6.2));
}

#[test]
fn test_slow_real_capture_after_selection_is_not_declared_stale() {
    // Mouse-up at 5.0 removes the overlay; the monitor's ScreenCaptureKit capture arrives
    // 1.5 s later and ends the session.
    let mut w = CaptureWatchdog::new();
    w.session_started(true);
    for k in 0..20 {
        let _ = w.observe(true, k as f64 * 0.25);
    }
    for k in 0..6 {
        assert!(!w.observe(false, 5.0 + k as f64 * 0.25));
    }
    assert!(!w.is_stale());
    w.session_ended();
}

#[test]
fn test_non_interactive_sessions_are_not_watched() {
    let mut w = CaptureWatchdog::new();
    w.session_started(false);
    let _ = w.observe(true, 0.0);
    assert!(!w.observe(false, 5.0));
}

#[test]
fn test_end_resets_and_new_session_starts_clean() {
    let mut w = CaptureWatchdog::new();
    w.session_started(true);
    let _ = w.observe(true, 0.0);
    assert!(!w.observe(false, 1.0));
    assert!(w.observe(false, 4.5));
    w.session_ended();
    assert!(!w.is_stale());
    assert!(!w.is_watching());
    w.session_started(true);
    assert!(!w.observe(false, 10.0), "previous sighting does not carry over");
}

#[test]
fn test_not_watching_without_session() {
    let mut w = CaptureWatchdog::new();
    assert!(!w.observe(true, 0.0));
    assert!(!w.observe(false, 5.0));
}
