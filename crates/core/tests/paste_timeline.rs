use igorio_core::paste_policy::{Outcome, OutcomeRules, RestoreRules, RestoreStep};

/// Runs the outcome and restore rules together over a simulated timeline, like `PasteJob` does
/// (polling every 50 ms / 100 ms after Ctrl+V).
#[derive(Default)]
struct Timeline {
    has_probe: bool,
    /// Times (after Ctrl+V) at which the target reads the promised data.
    reads: Vec<f64>,
    /// Time at which the probe first sees a change.
    probe_changes_at: Option<f64>,
    /// Time at which someone else writes the clipboard.
    foreign_write_at: Option<f64>,
}

struct Result {
    outcome: Outcome,
    decided_at: f64,
    restore_at: Option<f64>,
    restored: bool,
}

impl Timeline {
    fn run(&self) -> Result {
        let outcome_rules = OutcomeRules::new(self.has_probe, false);
        let restore_rules = RestoreRules::default();
        let mut outcome = Outcome::Pending;
        let mut decided_at = 0.0;
        let mut restore_at = None;
        let mut restored = false;
        let mut t = 0.0;
        while t < 10.0 && (outcome == Outcome::Pending || restore_at.is_none()) {
            let reads_so_far: Vec<f64> = self.reads.iter().copied().filter(|r| *r <= t).collect();
            if outcome == Outcome::Pending {
                let o = outcome_rules.evaluate(
                    t,
                    reads_so_far.len() as u32,
                    self.probe_changes_at.is_some_and(|at| at <= t),
                );
                if o != Outcome::Pending {
                    outcome = o;
                    decided_at = t;
                }
            }
            if restore_at.is_none() {
                let unchanged = self.foreign_write_at.is_none_or(|at| t < at);
                match restore_rules.step(t, reads_so_far.last().map(|at| t - at), unchanged) {
                    RestoreStep::Wait => {}
                    RestoreStep::Restore => {
                        restore_at = Some(t);
                        restored = true;
                    }
                    RestoreStep::Abandon => {
                        restore_at = Some(t);
                        restored = false;
                    }
                }
            }
            t += 0.05;
        }
        Result {
            outcome,
            decided_at,
            restore_at,
            restored,
        }
    }
}

#[test]
fn test_browser_composer_reads_immediately() {
    let r = Timeline {
        reads: vec![0.03, 0.04],
        ..Default::default()
    }
    .run();
    assert_eq!(r.outcome, Outcome::Accepted);
    assert!(r.decided_at < 0.3);
    assert!(r.restored);
    assert!(r.restore_at.unwrap_or(0.0) >= 2.0);
}

#[test]
fn test_app_ignoring_ctrl_v_keeps_the_screenshot_and_restores_clipboard() {
    // The ChatGPT failure: Ctrl+V never reached a paste handler.
    let r = Timeline::default().run();
    assert_eq!(r.outcome, Outcome::Failed);
    assert!(r.restored);
}

#[test]
fn test_slow_native_app_reading_late_is_not_cut_off() {
    // Reads after an async hop, 1.4 s in, then again at 1.8 s (preview, then upload).
    let r = Timeline {
        reads: vec![1.4, 1.8],
        ..Default::default()
    }
    .run();
    assert_eq!(r.outcome, Outcome::Accepted);
    assert!(
        r.restore_at.unwrap_or(0.0) >= 2.4 - 0.001,
        "restore waits until reads go quiet"
    );
}

#[test]
fn test_busy_app_handling_ctrl_v_late_still_gets_the_screenshot() {
    // A Chromium page frozen for 3 s: the app reports "couldn't paste" at 1.6 s, but the
    // clipboard must still hold the screenshot when the page finally reads it at 3 s.
    let r = Timeline {
        reads: vec![3.0],
        ..Default::default()
    }
    .run();
    assert_eq!(r.outcome, Outcome::Failed);
    assert!(r.restored);
    assert!(r.restore_at.unwrap_or(0.0) > 3.0);
}

#[test]
fn test_search_field_refuses_image() {
    // Single-line field: looked at the clipboard types, never read image data.
    let r = Timeline {
        has_probe: true,
        ..Default::default()
    }
    .run();
    assert_eq!(r.outcome, Outcome::Failed);
}

#[test]
fn test_address_bar_reads_but_stays_unchanged() {
    let r = Timeline {
        has_probe: true,
        reads: vec![0.05],
        ..Default::default()
    }
    .run();
    assert_eq!(r.outcome, Outcome::Rejected);
}

#[test]
fn test_single_line_field_that_takes_the_image() {
    let r = Timeline {
        has_probe: true,
        reads: vec![0.05],
        probe_changes_at: Some(0.4),
        ..Default::default()
    }
    .run();
    assert_eq!(r.outcome, Outcome::Accepted);
}

#[test]
fn test_user_copies_something_right_after_the_paste() {
    let r = Timeline {
        reads: vec![0.05],
        foreign_write_at: Some(0.8),
        ..Default::default()
    }
    .run();
    assert_eq!(r.outcome, Outcome::Accepted);
    assert!(!r.restored, "never overwrite what the user copied");
}

#[test]
fn test_next_paste_of_a_pile_supersedes_the_restore() {
    // The second card is written ~0.75 s later; the first job must not restore over it.
    let r = Timeline {
        reads: vec![0.05],
        foreign_write_at: Some(0.75),
        ..Default::default()
    }
    .run();
    assert!(!r.restored);
}
