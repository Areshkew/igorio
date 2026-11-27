//! Timing and decision rules for pasting a screenshot into another app: write the clipboard,
//! press Ctrl+V, learn whether it was taken, put the user's clipboard back. Pure; the drop router
//! in the app runs it against the real clipboard and keyboard input.

// MARK: Sequencing

/// The double-click event arrives on the second mouse-*down*. Wait for the release (so focus
/// and the word selection are final) but never forever.
pub const MOUSE_UP_POLL_INTERVAL: f64 = 0.02;
pub const MOUSE_UP_TIMEOUT: f64 = 1.0;
/// Settle after the release before typing into the field.
pub const AFTER_MOUSE_UP_DELAY: f64 = 0.08;
/// After asking the target app to activate, how long to wait for it to become frontmost.
pub const ACTIVATION_TIMEOUT: f64 = 0.6;
/// Between collapsing the selection (→) and pressing Ctrl+V.
pub const COLLAPSE_TO_PASTE_DELAY: f64 = 0.06;
/// Between closing a whiteboard's fresh text box (esc) and pressing Ctrl+V.
pub const DISMISS_TO_PASTE_DELAY: f64 = 0.18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseUpStep {
    Wait,
    Proceed,
}

// MARK: Terminals: wait out copy-on-select

/// Terminals with copy-on-select (and TUIs like Claude Code in them) copy the selection to the
/// clipboard on every mouse-up: the double-clicked word, or the whole wrapped line when a
/// third click follows. A copy that lands after our write replaces the screenshot, and the
/// Ctrl+V then pastes that text. So for terminals the keystroke waits until the clicking is over
/// and the clipboard has been quiet for a moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalSettle {
    /// Since the last mouse-up (a third click restarts it).
    pub after_mouse_up: f64,
    /// Since the last time anyone wrote the clipboard.
    pub clipboard_quiet: f64,
    /// Never hold the paste longer than this.
    pub timeout: f64,
}

impl Default for TerminalSettle {
    fn default() -> Self {
        Self {
            after_mouse_up: 0.14,
            clipboard_quiet: 0.09,
            timeout: 1.5,
        }
    }
}

impl TerminalSettle {
    pub fn step(
        &self,
        elapsed: f64,
        buttons_down: bool,
        since_mouse_up: f64,
        since_clipboard_change: Option<f64>,
    ) -> MouseUpStep {
        if elapsed >= self.timeout {
            return MouseUpStep::Proceed;
        }
        if buttons_down || since_mouse_up < self.after_mouse_up {
            return MouseUpStep::Wait;
        }
        if since_clipboard_change.is_some_and(|quiet| quiet < self.clipboard_quiet) {
            return MouseUpStep::Wait;
        }
        MouseUpStep::Proceed
    }
}

// MARK: Someone else wrote the clipboard after us

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteStep {
    /// Put the screenshot back: the Ctrl+V has not been served yet and would paste their text.
    Rewrite,
    /// The paste was already served; treat the write as selection noise and still restore
    /// the user's clipboard afterwards.
    Adopt,
    /// A real copy by the user: leave it alone and never restore over it.
    Leave,
}

/// How long after Ctrl+V a foreign clipboard write is taken for the click's own side effect
/// rather than the user copying something.
pub const OVERWRITE_WINDOW: f64 = 1.0;
pub const MAX_REWRITES: u32 = 3;

pub fn overwrite_step(
    since_send: f64,
    reads: u32,
    rewrites: u32,
    is_terminal: bool,
) -> OverwriteStep {
    if since_send >= OVERWRITE_WINDOW {
        return OverwriteStep::Leave;
    }
    if reads == 0 {
        return if rewrites < MAX_REWRITES {
            OverwriteStep::Rewrite
        } else {
            OverwriteStep::Leave
        };
    }
    if is_terminal {
        OverwriteStep::Adopt
    } else {
        OverwriteStep::Leave
    }
}

// MARK: The user's clipboard, as it was before the click

/// Which copy of the clipboard to put back after the paste. `remembered` was taken at the
/// first mouse-down of the double-click; if the clipboard changed between then and now, that
/// change is the terminal's copy-on-select, not something the user copied.
pub fn use_remembered_clipboard(
    remembered_count: Option<u32>,
    current_count: u32,
    age: Option<f64>,
) -> bool {
    let (Some(remembered), Some(age)) = (remembered_count, age) else {
        return false;
    };
    if remembered == current_count {
        return true;
    }
    age <= 2.0
}

pub fn mouse_up_step(elapsed: f64, buttons_down: bool) -> MouseUpStep {
    if !buttons_down || elapsed >= MOUSE_UP_TIMEOUT {
        return MouseUpStep::Proceed;
    }
    MouseUpStep::Wait
}

// MARK: Key delivery

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRoute {
    /// Send as system input: the foreground app receives it exactly like real typing
    /// (accelerators, modifier state and all). Preferred.
    Session,
    /// Post straight to a process. Only a fallback: many apps read the global modifier state
    /// and ignore a Ctrl+V delivered this way, which is why pasting into e.g. the ChatGPT app
    /// silently did nothing.
    Process(i32),
}

pub fn needs_activation(target: Option<i32>, frontmost: Option<i32>) -> bool {
    let Some(target) = target else {
        return false;
    };
    if target <= 0 {
        return false;
    }
    Some(target) != frontmost
}

pub fn key_route(target: Option<i32>, frontmost: Option<i32>) -> KeyRoute {
    match (target, frontmost) {
        (Some(target), Some(frontmost)) if target > 0 && target != frontmost => {
            KeyRoute::Process(target)
        }
        _ => KeyRoute::Session,
    }
}

// MARK: Outcome

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Accepted,
    Rejected,
    Failed,
}

/// Decides whether the paste was taken from two signals:
///  - `reads`: how many times another process asked for the clipboard data we promised.
///    No read at all means the Ctrl+V never reached a paste handler (or the field only takes
///    text and saw no text) — the screenshot must stay on the pile.
///  - `probe_changed`: the accessibility fingerprint of a single-line field changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutcomeRules {
    /// Single-line fields (search/address bars) are verified with the probe.
    pub has_probe: bool,
    /// With no read by then, give up: the paste did not happen.
    pub no_read_timeout: f64,
    /// A drop onto a canvas is a guess (nothing says it takes images until it reads one),
    /// so give up sooner.
    pub is_canvas: bool,
    /// Probe-verified fields: unchanged by then → the field refused the image.
    pub probe_timeout: f64,
    /// A read counts once the reader had a moment to finish.
    pub read_settle: f64,
}

impl Default for OutcomeRules {
    fn default() -> Self {
        Self {
            has_probe: false,
            no_read_timeout: 1.6,
            is_canvas: false,
            probe_timeout: 3.0,
            read_settle: 0.1,
        }
    }
}

impl OutcomeRules {
    pub fn new(has_probe: bool, is_canvas: bool) -> Self {
        let mut rules = Self {
            has_probe,
            is_canvas,
            ..Self::default()
        };
        if is_canvas {
            rules.no_read_timeout = 1.0;
        }
        rules
    }

    pub fn evaluate(&self, elapsed: f64, reads: u32, probe_changed: bool) -> Outcome {
        if probe_changed {
            return Outcome::Accepted;
        }
        if self.has_probe {
            if reads == 0 && elapsed >= self.no_read_timeout {
                return Outcome::Failed;
            }
            if elapsed >= self.probe_timeout {
                return if reads > 0 {
                    Outcome::Rejected
                } else {
                    Outcome::Failed
                };
            }
            return Outcome::Pending;
        }
        if reads > 0 && elapsed >= self.read_settle {
            return Outcome::Accepted;
        }
        if elapsed >= self.no_read_timeout {
            return Outcome::Failed;
        }
        Outcome::Pending
    }
}

// MARK: Clipboard restore

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreStep {
    Wait,
    Restore,
    Abandon,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RestoreRules {
    /// Never restore earlier than this after Ctrl+V (apps may read late, e.g. after an await).
    pub min_delay: f64,
    /// Restore once the last read is this old…
    pub quiet_after_read: f64,
    /// …and in any case by this time.
    pub max_delay: f64,
    /// Nobody has read the screenshot yet: the Ctrl+V may still be waiting in a busy app's queue
    /// (Chromium apps can be frozen for seconds). Putting the user's clipboard back now would
    /// make that late Ctrl+V paste their text instead, so hold the screenshot there much longer.
    pub no_read_delay: f64,
}

impl Default for RestoreRules {
    fn default() -> Self {
        Self {
            min_delay: 2.0,
            quiet_after_read: 0.6,
            max_delay: 6.0,
            no_read_delay: 8.0,
        }
    }
}

impl RestoreRules {
    /// - clipboard_unchanged: nobody wrote the clipboard since our write (if the user copied
    ///   something meanwhile, or a later paste of the same pile wrote it, leave it alone).
    pub fn step(
        &self,
        since_send: f64,
        since_last_read: Option<f64>,
        clipboard_unchanged: bool,
    ) -> RestoreStep {
        if !clipboard_unchanged {
            return RestoreStep::Abandon;
        }
        let Some(quiet) = since_last_read else {
            return if since_send >= self.no_read_delay {
                RestoreStep::Restore
            } else {
                RestoreStep::Wait
            };
        };
        if since_send >= self.max_delay {
            return RestoreStep::Restore;
        }
        if since_send < self.min_delay {
            return RestoreStep::Wait;
        }
        if quiet < self.quiet_after_read {
            RestoreStep::Wait
        } else {
            RestoreStep::Restore
        }
    }
}

/// The clipboard types written for an image paste. Image types are promised lazily (so
/// reads can be observed); terminals also get a shell-quoted path as text.
pub fn image_types(for_terminal: bool) -> Vec<&'static str> {
    let types = vec!["public.png", "public.tiff"];
    if for_terminal {
        return ["public.utf8-plain-text", "public.file-url"]
            .into_iter()
            .chain(types)
            .collect();
    }
    types
}

/// Markers that ask clipboard managers (nspasteboard.org convention) not to record the
/// screenshot as the user's clipboard — and not to read it, which would look like a paste.
pub const TRANSIENT_MARKER_TYPES: &[&str] = &[
    "org.nspasteboard.TransientType",
    "org.nspasteboard.AutoGeneratedType",
];

/// Shell-safe path followed by a space, so the command line can continue.
pub fn shell_quoted(path: &str) -> String {
    let needs_quotes = path.chars().any(|c| " '\"\\$`!&;|<>()*?[]{}#~".contains(c));
    if !needs_quotes {
        return format!("{path} ");
    }
    format!("'{}' ", path.replace('\'', "'\\''"))
}
