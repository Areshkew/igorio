use igorio_core::paste_policy::{
    KeyRoute, MAX_REWRITES, MOUSE_UP_TIMEOUT, MouseUpStep, Outcome, OutcomeRules, OverwriteStep,
    RestoreRules, RestoreStep, TerminalSettle, image_types, key_route, mouse_up_step,
    needs_activation, overwrite_step, shell_quoted, use_remembered_clipboard,
};

// MARK: Key routing

#[test]
fn test_frontmost_target_uses_session_stream() {
    assert_eq!(key_route(Some(42), Some(42)), KeyRoute::Session);
    assert!(!needs_activation(Some(42), Some(42)));
}

#[test]
fn test_unknown_target_uses_session_stream() {
    assert_eq!(key_route(None, Some(42)), KeyRoute::Session);
    assert_eq!(key_route(Some(0), Some(42)), KeyRoute::Session);
    assert_eq!(key_route(Some(-1), None), KeyRoute::Session);
    assert!(!needs_activation(None, Some(42)));
}

#[test]
fn test_background_target_is_activated_then_falls_back_to_pid() {
    assert!(needs_activation(Some(42), Some(7)));
    assert_eq!(key_route(Some(42), Some(7)), KeyRoute::Process(42));
    assert!(needs_activation(Some(42), None));
}

// MARK: Mouse-up wait

#[test]
fn test_waits_for_release() {
    assert_eq!(mouse_up_step(0.05, true), MouseUpStep::Wait);
    assert_eq!(mouse_up_step(0.05, false), MouseUpStep::Proceed);
}

#[test]
fn test_does_not_wait_forever_on_a_held_button() {
    assert_eq!(mouse_up_step(MOUSE_UP_TIMEOUT, true), MouseUpStep::Proceed);
}

// MARK: Outcome

#[test]
fn test_multiline_read_means_accepted() {
    let r = OutcomeRules::new(false, false);
    assert_eq!(r.evaluate(0.05, 1, false), Outcome::Pending);
    assert_eq!(r.evaluate(0.2, 1, false), Outcome::Accepted);
}

#[test]
fn test_multiline_no_read_means_failed_not_silently_dropped() {
    // The old code assumed every multi-line paste was accepted, so a Ctrl+V the app ignored
    // (ChatGPT) dropped the screenshot — and the clipboard was restored, so it was gone.
    let r = OutcomeRules::new(false, false);
    assert_eq!(r.evaluate(1.0, 0, false), Outcome::Pending);
    assert_eq!(r.evaluate(1.6, 0, false), Outcome::Failed);
}

#[test]
fn test_probe_change_accepts_immediately() {
    let r = OutcomeRules::new(true, false);
    assert_eq!(r.evaluate(0.15, 0, true), Outcome::Accepted);
}

#[test]
fn test_single_line_read_but_unchanged_is_rejected() {
    let r = OutcomeRules::new(true, false);
    assert_eq!(r.evaluate(2.0, 2, false), Outcome::Pending);
    assert_eq!(r.evaluate(3.0, 2, false), Outcome::Rejected);
}

#[test]
fn test_single_line_no_read_fails_early() {
    let r = OutcomeRules::new(true, false);
    assert_eq!(r.evaluate(1.6, 0, false), Outcome::Failed);
}

#[test]
fn test_outcome_is_monotonic_over_time() {
    // Once decided at time t, the same signals at a later time give the same decision.
    for has_probe in [false, true] {
        let r = OutcomeRules::new(has_probe, false);
        for reads in [0u32, 1] {
            let mut decided: Option<Outcome> = None;
            for step in 0..=80 {
                let o = r.evaluate(step as f64 * 0.05, reads, false);
                if let Some(d) = decided {
                    assert!(
                        o == d || d == Outcome::Pending,
                        "probe={has_probe} reads={reads}: {d:?} → {o:?}"
                    );
                }
                if o != Outcome::Pending && decided.is_none() {
                    decided = Some(o);
                }
            }
            assert!(decided.is_some(), "always decides eventually");
        }
    }
}

// MARK: Clipboard restore

#[test]
fn test_never_restores_before_min_delay() {
    let r = RestoreRules::default();
    assert_eq!(r.step(1.2, Some(1.0), true), RestoreStep::Wait);
}

#[test]
fn test_restores_after_quiet_read() {
    let r = RestoreRules::default();
    assert_eq!(r.step(2.1, Some(0.9), true), RestoreStep::Restore);
}

#[test]
fn test_waits_while_the_app_is_still_reading() {
    let r = RestoreRules::default();
    assert_eq!(r.step(2.5, Some(0.2), true), RestoreStep::Wait);
}

#[test]
fn test_without_any_read_holds_the_screenshot_longer() {
    // The Ctrl+V may still be queued in a busy app: restoring now would paste the user's text.
    let r = RestoreRules::default();
    assert_eq!(r.step(2.0, None, true), RestoreStep::Wait);
    assert_eq!(r.step(6.0, None, true), RestoreStep::Wait);
    assert_eq!(r.step(8.0, None, true), RestoreStep::Restore);
}

#[test]
fn test_restores_by_max_delay_even_if_reads_continue() {
    let r = RestoreRules::default();
    assert_eq!(r.step(6.0, Some(0.01), true), RestoreStep::Restore);
}

#[test]
fn test_abandons_when_someone_else_wrote_the_clipboard() {
    let r = RestoreRules::default();
    assert_eq!(r.step(0.1, None, false), RestoreStep::Abandon);
    assert_eq!(r.step(9.0, None, false), RestoreStep::Abandon);
}

// MARK: Clipboard contents

#[test]
fn test_image_types() {
    assert_eq!(image_types(false), ["public.png", "public.tiff"]);
    let t = image_types(true);
    assert!(t.contains(&"public.utf8-plain-text"));
    assert!(t.contains(&"public.png"));
}

#[test]
fn test_shell_quoting() {
    assert_eq!(
        shell_quoted("/Users/a/Pictures/x.png"),
        "/Users/a/Pictures/x.png "
    );
    assert_eq!(
        shell_quoted("/Users/a/My Pictures/Screenshot 1.png"),
        "'/Users/a/My Pictures/Screenshot 1.png' "
    );
    assert_eq!(shell_quoted("/tmp/it's.png"), "'/tmp/it'\\''s.png' ");
    assert_eq!(shell_quoted("/tmp/a(1).png"), "'/tmp/a(1).png' ");
}

// MARK: Terminals

#[test]
fn test_waits_while_a_third_click_is_held() {
    let settle = TerminalSettle::default();
    assert_eq!(settle.step(0.3, true, 0.3, None), MouseUpStep::Wait);
}

#[test]
fn test_waits_for_the_copy_on_select_after_the_last_mouse_up() {
    // A copy-on-select terminal copies the selection ~10 ms after each mouse-up.
    let settle = TerminalSettle::default();
    assert_eq!(settle.step(0.05, false, 0.05, None), MouseUpStep::Wait);
    assert_eq!(settle.step(0.2, false, 0.2, None), MouseUpStep::Proceed);
}

#[test]
fn test_a_third_click_restarts_the_wait() {
    let settle = TerminalSettle::default();
    assert_eq!(settle.step(0.5, false, 0.02, None), MouseUpStep::Wait);
}

#[test]
fn test_waits_while_the_clipboard_is_still_changing() {
    let settle = TerminalSettle::default();
    assert_eq!(settle.step(0.3, false, 0.3, Some(0.02)), MouseUpStep::Wait);
    assert_eq!(
        settle.step(0.4, false, 0.4, Some(0.12)),
        MouseUpStep::Proceed
    );
}

#[test]
fn test_never_waits_forever() {
    let settle = TerminalSettle::default();
    assert_eq!(settle.step(1.5, true, 0.0, Some(0.0)), MouseUpStep::Proceed);
}

#[test]
fn test_an_overwrite_before_the_paste_is_read_is_undone() {
    assert_eq!(overwrite_step(0.02, 0, 0, true), OverwriteStep::Rewrite);
    assert_eq!(overwrite_step(0.02, 0, 0, false), OverwriteStep::Rewrite);
    assert_eq!(
        overwrite_step(0.02, 0, MAX_REWRITES, true),
        OverwriteStep::Leave,
        "don't fight another writer forever"
    );
}

#[test]
fn test_a_selection_copied_right_after_a_terminal_paste_does_not_become_the_clipboard() {
    assert_eq!(overwrite_step(0.2, 2, 0, true), OverwriteStep::Adopt);
    assert_eq!(overwrite_step(0.2, 2, 0, false), OverwriteStep::Leave);
}

#[test]
fn test_a_later_copy_is_the_users_own() {
    assert_eq!(overwrite_step(1.5, 2, 0, true), OverwriteStep::Leave);
    assert_eq!(overwrite_step(1.5, 0, 0, true), OverwriteStep::Leave);
}

#[test]
fn test_the_clipboard_from_before_the_click_is_the_one_restored() {
    // Unchanged since the first mouse-down: same thing either way.
    assert!(use_remembered_clipboard(Some(7), 7, Some(30.0)));
    // Changed during the double-click: that was copy-on-select.
    assert!(use_remembered_clipboard(Some(7), 8, Some(0.3)));
    // Changed long after the last click we saw: the user copied something.
    assert!(!use_remembered_clipboard(Some(7), 8, Some(10.0)));
    assert!(!use_remembered_clipboard(None, 8, None));
}

#[test]
fn test_a_canvas_drop_gives_up_sooner() {
    let rules = OutcomeRules::new(false, true);
    assert_eq!(rules.evaluate(0.5, 0, false), Outcome::Pending);
    assert_eq!(rules.evaluate(1.0, 0, false), Outcome::Failed);
    assert_eq!(rules.evaluate(0.2, 1, false), Outcome::Accepted);
}
