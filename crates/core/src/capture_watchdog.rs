//! Stale screenshot-session detection.

/// The pile hides while the system screenshot UI is up. The screenshot detector ends an
/// interactive (Win+Shift+S) session on capture, Esc or a 3-minute timeout — but a selection
/// session cancelled any other way (a click without a drag, switching away) stays "open", so the
/// pile and its screenshots stayed invisible for up to three minutes, and the stale session
/// would turn the next ordinary drag into a phantom capture.
///
/// This watches whether the screenshot UI's overlay is actually on screen. Only once it has been
/// seen *and* then gone for `gone_grace` is the session declared stale. If the overlay is never
/// seen (unknown window layout) nothing changes, so the worst case is the old behaviour.
#[derive(Debug, Clone)]
pub struct CaptureWatchdog {
    /// The overlay also disappears the moment a real selection is made, and the monitor's own
    /// capture (ScreenCaptureKit) can take a second or more to arrive after that, so the grace
    /// must comfortably exceed a slow capture.
    pub gone_grace: f64,

    active: bool,
    interactive: bool,
    saw_overlay: bool,
    gone_since: Option<f64>,
    is_stale: bool,
}

impl Default for CaptureWatchdog {
    fn default() -> Self {
        Self {
            gone_grace: 3.0,
            active: false,
            interactive: false,
            saw_overlay: false,
            gone_since: None,
            is_stale: false,
        }
    }
}

impl CaptureWatchdog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_watching(&self) -> bool {
        self.active && self.interactive && !self.is_stale
    }

    pub fn is_stale(&self) -> bool {
        self.is_stale
    }

    pub fn session_started(&mut self, interactive: bool) {
        self.active = true;
        self.interactive = interactive;
        self.saw_overlay = false;
        self.gone_since = None;
        self.is_stale = false;
    }

    pub fn session_ended(&mut self) {
        self.active = false;
        self.interactive = false;
        self.saw_overlay = false;
        self.gone_since = None;
        self.is_stale = false;
    }

    /// Feed an observation; returns true exactly once, when the session turns stale.
    pub fn observe(&mut self, overlay_visible: bool, t: f64) -> bool {
        if !self.is_watching() {
            return false;
        }
        if overlay_visible {
            self.saw_overlay = true;
            self.gone_since = None;
            return false;
        }
        if !self.saw_overlay {
            return false;
        }
        let since = self.gone_since.unwrap_or(t);
        self.gone_since = Some(since);
        if t - since >= self.gone_grace {
            self.is_stale = true;
            return true;
        }
        false
    }
}
