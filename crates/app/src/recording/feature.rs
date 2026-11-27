//! The screen recording capability for the app (`RecordingCapability.swift`): one recording
//! at a time, with the recorder ([`Recorder`]), what it shows on screen ([`RecordingUi`]) and
//! the zoomed area ([`RecordingViewport`]). `main.rs` drives it from the UI loop like the
//! notes: it asks for a toggle, feeds the mouse events, runs one [`RecordingFeature::pass`]
//! per loop pass and carries out what the pass returns.
//!
//! # Who owns the viewport
//!
//! The app does, here. The recorder only takes a crop (`Recorder::set_area`, monitor-local
//! physical pixels) and never waits for it; the recording UI only shows an outline
//! (`RecordingUi::set_area`, screen pixels). This module steps the viewport on the UI loop at
//! the app's frame rate while it moves, with the cursor in monitor-local pixels and the real
//! time step, and sends its area to both. (The UI demo's comment "as the recorder will" means
//! the same loop; the recorder's module comment, "the caller owns the viewport", settles it.)
//!
//! # States
//!
//! [`advance`] is the whole state machine (`RecordingCapability.State`): idle, starting (the
//! recorder sets up and waits for its first frame), recording, stopping (the file is being
//! finished). The rules derived from the state are pure too: [`exclusive`] (the note gesture
//! and the tool menu are off, as with `host.setExclusive`), [`pen_keys`] (the Pen and
//! Highlighter hotkeys are registered) and [`drawing`] (the fairy steps aside while a tool is
//! in hand, as with `host.setCaptureUIVisible`).
//!
//! # Stop order
//!
//! `ui.stop()` first: the wheel and drawing-key gates are released at once and a pen in hand
//! is put down. Then `recorder.stop()`. The note gesture and the pen keys come back in the
//! same pass (the state is no longer starting or recording). The UI is shut down after its
//! `Hidden` event, or [`UI_FADE_BOUND`] later. A new recording waits until the old UI thread
//! has ended: the input gates are process-wide.
//!
//! # Session end
//!
//! While a recording runs (from the start until its file is finished) the tray window holds a
//! shutdown block reason ([`BLOCK_REASON`], `ShutdownBlockReasonCreate`): this is the case it
//! exists for, an operation whose interruption loses the user's work. If Windows ends the
//! session meanwhile it shows the reason, and the quit path gets the time to finish the file
//! (bounded by its own wait). A recording that still cannot be finished leaves its
//! `.recording-*.partial.mp4` in the captures folder.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use igorio_core::geom::{Point, Rect};
use igorio_core::ink::InkTool;
use igorio_core::viewport::RecordingViewport;
use igorio_kit::Image;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::core::{BOOL, PCWSTR, w};

use super::frame::Labels;
use super::ink_overlay::{RecordingUi, RecordingUiConfig, UiEvent};
pub use super::input::RecordingState;
use super::recorder::{CursorMode, MonitorId, Recorder, RecorderEvent, RecordingConfig};
use crate::drop_router::DebugLog;
use crate::hook::MouseEvent;
use crate::overlay::{cursor_pos, monitor_bounds_at};
use crate::settings::{DefaultFolders, Settings};
use crate::shortcuts::{Action, Shortcuts, display_for_layout};

/// The capability's id in `settings.json` (`RecordingCapability.manifest.id`).
pub const CAPABILITY: &str = "fairy.screen-recording";
/// The drawn cursor's size: `CursorScale.enlarge` turns the usual size 1 into
/// `min(4, max(1 × 1.75, 2))` = 2.
pub const CURSOR_SCALE: f64 = 2.0;
/// How long the recording UI may fade out after `stop` before it is shut down anyway.
pub const UI_FADE_BOUND: f64 = 1.0;
/// How long the recording UI thread may take to end once it is asked to.
const UI_SHUTDOWN: Duration = Duration::from_millis(500);
/// How long joining the recorder's worker may take after its last event (it has returned).
const RECORDER_JOIN: Duration = Duration::from_millis(200);
/// What Windows shows if the session ends while a recording runs.
pub const BLOCK_REASON: PCWSTR = w!("Saving a screen recording");
/// The area changed by less than this (physical pixels): the viewport has settled.
const SETTLED: f64 = 0.01;

// ---------------------------------------------------------------------------------------
// The pure rules
// ---------------------------------------------------------------------------------------

/// What happened, for [`advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// The ToggleRecording shortcut, the tray item or the tool menu's Record screen.
    Toggle,
    /// The recorder wrote its first frame.
    Started,
    /// The recording UI could not start.
    UiFailed,
    /// The recorder finished the file.
    Finished,
    /// The recorder failed, before or after its first frame.
    Failed,
    /// The capability was switched off in the settings.
    SwitchedOff,
}

/// What to do for an input, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Pick the monitor under the cursor and start the recorder.
    Launch,
    /// Make the viewport and start the recording UI.
    ShowUi,
    /// `ui.stop()`: the input gates are released, a pen in hand is put down.
    StopUi,
    /// `recorder.stop()`: the file is finished.
    StopRecorder,
    /// The finished file goes on the pile.
    Carry,
    /// A hint that names the problem.
    Explain,
}

/// The state machine (`RecordingCapability.toggle`, `begin`, `finish`). A toggle while
/// starting or stopping waits, as in Swift; switching off saves a recording in progress
/// first ("finish the file rather than lose it") and it still goes on the pile.
pub fn advance(state: RecordingState, input: Input) -> (RecordingState, &'static [Step]) {
    use Input as I;
    use RecordingState::{Idle, Recording, Starting, Stopping};
    match (state, input) {
        (Idle, I::Toggle) => (Starting, &[Step::Launch]),
        (Starting, I::Started) => (Recording, &[Step::ShowUi]),
        (Starting, I::Failed) => (Idle, &[Step::Explain]),
        (Starting, I::SwitchedOff) => (Stopping, &[Step::StopRecorder]),
        // Not expected (the recorder starts before it finishes), but a file is a file.
        (Starting, I::Finished) => (Idle, &[Step::Carry]),
        (Recording, I::Toggle | I::SwitchedOff) => {
            (Stopping, &[Step::StopUi, Step::StopRecorder])
        }
        (Recording, I::UiFailed) => (Stopping, &[Step::StopRecorder, Step::Explain]),
        // The recorder ended by itself (the system closed the capture).
        (Recording, I::Finished) => (Idle, &[Step::StopUi, Step::Carry]),
        (Recording, I::Failed) => (Idle, &[Step::StopUi, Step::Explain]),
        (Stopping, I::Finished) => (Idle, &[Step::Carry]),
        (Stopping, I::Failed) => (Idle, &[Step::Explain]),
        (state, _) => (state, &[]),
    }
}

/// The rest of the app steps back (`host.setExclusive`): no note gesture, no tool menu. From
/// the press until the recorder is told to stop.
pub fn exclusive(state: RecordingState) -> bool {
    matches!(state, RecordingState::Starting | RecordingState::Recording)
}

/// The Pen and Highlighter hotkeys are registered (`Hotkeys::set_recording`).
pub fn pen_keys(state: RecordingState) -> bool {
    state == RecordingState::Recording
}

/// The fairy is hidden for drawing (`setCaptureUIVisible(tool != nil, owner: inkOwner)`):
/// it must not follow the pen around the video, and scribbling is not a shake.
pub fn drawing(state: RecordingState, tool: Option<InkTool>) -> bool {
    state == RecordingState::Recording && tool.is_some()
}

/// The state shown to the user: a stopped UI that has not ended yet still counts as stopping,
/// so a new recording waits for it.
pub fn shown_state(state: RecordingState, ui_closing: bool) -> RecordingState {
    if state == RecordingState::Idle && ui_closing { RecordingState::Stopping } else { state }
}

/// The tray item (`RecordingCapability.menuItems`): its label and whether it can be chosen.
pub fn menu_label(state: RecordingState) -> (&'static str, bool) {
    match state {
        RecordingState::Idle => ("Record Screen", true),
        RecordingState::Starting => ("Starting Recording…", false),
        RecordingState::Recording => ("Stop Recording", true),
        RecordingState::Stopping => ("Saving Recording…", false),
    }
}

/// The hint for a failure, in a few words. `started`: it failed after the first frame.
pub fn failure_hint(reason: &str, started: bool, partial: bool) -> String {
    let lower = reason.to_lowercase();
    if lower.contains("already active") {
        "the last recording is still saving".into()
    } else if lower.contains("monitor") {
        "recording stopped · the screen changed".into()
    } else if !started {
        "could not start recording".into()
    } else if partial {
        "recording failed · part of it was kept".into()
    } else {
        "recording failed".into()
    }
}

/// The pill's shortcut labels, from the user's shortcuts.
pub fn labels(shortcuts: &Shortcuts) -> Labels {
    let label = |action| shortcuts.get(action).map(|chord| display_for_layout(&chord));
    Labels {
        toggle: label(Action::ToggleRecording),
        pen: label(Action::Pen),
        highlighter: label(Action::Highlighter),
    }
}

/// The area moved by more than [`SETTLED`] on some side.
fn moved(a: Rect, b: Rect) -> bool {
    [(a.x, b.x), (a.y, b.y), (a.width, b.width), (a.height, b.height)]
        .iter()
        .any(|(a, b)| (a - b).abs() > SETTLED)
}

/// The viewport needs another step at the next frame: it follows the cursor, the zoom is
/// still easing, or the last step moved it.
pub fn viewport_moving(viewport: &RecordingViewport, following: bool, before: Rect) -> bool {
    following || viewport.zoom() != viewport.target_zoom() || moved(before, viewport.area())
}

/// `rect` moved by `by`.
fn offset(rect: Rect, by: Point) -> Rect {
    Rect::new(rect.x + by.x, rect.y + by.y, rect.width, rect.height)
}

// ---------------------------------------------------------------------------------------
// The feature
// ---------------------------------------------------------------------------------------

/// A finished recording for the pile (`host.carry(recording:poster:scale:duration:)`).
#[derive(Debug)]
pub struct Finished {
    pub file: PathBuf,
    pub poster: Arc<Image>,
    pub scale: f64,
    pub duration: f64,
    /// When the recording was started (wall clock).
    pub captured_at: SystemTime,
}

/// What one pass asks of the app.
#[derive(Debug, Default)]
pub struct Pass {
    /// Put this on the pile.
    pub carry: Option<Finished>,
    /// Show this hint.
    pub hint: Option<String>,
    /// When the loop must run again at the latest (the viewport moves, a thread is in
    /// flight, a fade bound).
    pub due: Option<f64>,
}

/// The viewport while a recording runs.
struct Live {
    viewport: RecordingViewport,
    /// The monitor's top-left corner on the virtual screen (physical pixels).
    origin: Point,
    following: bool,
    /// The last area sent to the recorder and the UI (monitor-local).
    area: Rect,
    last_step: f64,
    /// The last step moved the area: the next one uses the real time step. After a rest it
    /// starts from 0, so a zoom after a pause eases from where it is instead of jumping.
    moving: bool,
}

type Launched = Result<(Recorder, PathBuf), String>;

/// One recording at a time: see the module comment.
pub struct RecordingFeature {
    state: RecordingState,
    launch: Option<JoinHandle<Launched>>,
    recorder: Option<Recorder>,
    /// The captures folder of the current recording.
    folder: Option<PathBuf>,
    ui: Option<RecordingUi>,
    /// A stopped UI fading out, and when it is shut down at the latest.
    closing: Option<(RecordingUi, f64)>,
    /// A UI being shut down off the UI loop.
    ending: Option<JoinHandle<bool>>,
    live: Option<Live>,
    tool: Option<InkTool>,
    started_at: SystemTime,
    /// The recorder reported its first frame (for the failure hint).
    had_frames: bool,
    /// A toggle asked for by the tray, the tool menu or the shortcut, for the next pass.
    toggle: bool,
    /// The switch and the shortcuts of the last pass, for the tray menu.
    enabled: bool,
    shortcuts: Shortcuts,
    /// The tray window, for the shutdown block reason (made on this thread).
    window: isize,
    blocking: bool,
    log: DebugLog,
    frame: f64,
    recorder_events: Vec<RecorderEvent>,
    ui_events: Vec<UiEvent>,
}

impl RecordingFeature {
    /// `frame`: the app's frame interval in seconds.
    pub fn new(frame: f64) -> RecordingFeature {
        RecordingFeature {
            state: RecordingState::Idle,
            launch: None,
            recorder: None,
            folder: None,
            ui: None,
            closing: None,
            ending: None,
            live: None,
            tool: None,
            started_at: SystemTime::now(),
            had_frames: false,
            toggle: false,
            enabled: true,
            shortcuts: Shortcuts::defaults(),
            window: 0,
            blocking: false,
            log: DebugLog::from_env(),
            frame,
            recorder_events: Vec::new(),
            ui_events: Vec::new(),
        }
    }

    fn line(&mut self, now: f64, text: &str) {
        self.log.line(now, &format!("recording: {text}"));
    }

    /// A problem: in the debug log, or on stderr when there is none.
    fn problem(&mut self, now: f64, text: &str) {
        if !self.log.enabled() {
            eprintln!("recording: {text}");
        }
        self.line(now, text);
    }

    /// A top-level window of this thread (the tray's), for the shutdown block reason.
    pub fn set_window(&mut self, window: HWND) {
        self.window = window.0 as isize;
    }

    /// A line in the debug log, for the app's own recording decisions.
    pub fn log_line(&mut self, now: f64, text: &str) {
        self.line(now, text);
    }

    /// The capability's switch, as of the last pass.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The shortcuts, as of the last pass (for the tray menu's key column).
    pub fn shortcuts(&self) -> &Shortcuts {
        &self.shortcuts
    }

    pub fn state(&self) -> RecordingState {
        shown_state(self.state, self.closing.is_some() || self.ending.is_some())
    }

    /// The tool in hand.
    pub fn tool(&self) -> Option<InkTool> {
        self.tool
    }

    /// The note gesture and the tool menu are off.
    pub fn exclusive(&self) -> bool {
        exclusive(self.state)
    }

    /// The Pen and Highlighter hotkeys are registered.
    pub fn pen_keys(&self) -> bool {
        pen_keys(self.state)
    }

    /// The fairy is hidden for drawing.
    pub fn drawing(&self) -> bool {
        drawing(self.state, self.tool)
    }

    /// The shortcut, the tray item or the tool menu: start or stop at the next pass.
    pub fn request_toggle(&mut self) {
        self.toggle = true;
    }

    /// The Pen or Highlighter shortcut, or its tray item.
    pub fn tool_shortcut(&mut self, tool: InkTool, now: f64) {
        match &self.ui {
            Some(ui) => {
                ui.tool_shortcut(tool);
                self.line(now, &format!("{tool:?} shortcut"));
            }
            None => self.line(now, &format!("{tool:?} shortcut, but no recording runs")),
        }
    }

    /// The mouse events of this pass (the app has one mouse hook): the cursor of the next
    /// `Follow`.
    pub fn feed(&self, events: &[MouseEvent]) {
        if let Some(ui) = &self.ui {
            ui.feed(events);
        }
    }

    pub fn wakes(&self, out: &mut Vec<HANDLE>) {
        if let Some(recorder) = &self.recorder {
            out.push(recorder.wake_handle());
        }
        if let Some(ui) = &self.ui {
            out.push(ui.wake_handle());
        }
        if let Some((ui, _)) = &self.closing {
            out.push(ui.wake_handle());
        }
    }

    /// One pass of the UI loop. `enabled`: the capability's switch.
    pub fn pass(&mut self, enabled: bool, settings: &Settings, now: f64) -> Pass {
        let mut out = Pass::default();
        self.enabled = enabled;
        if self.shortcuts != settings.shortcuts {
            self.shortcuts = settings.shortcuts.clone();
        }
        self.finish_launch(settings, now, &mut out);
        if !enabled && exclusive(self.state) {
            self.line(now, "switched off: the recording is saved first");
            self.input(Input::SwitchedOff, settings, now, &mut out);
        }
        if std::mem::take(&mut self.toggle) {
            if !enabled {
                self.line(now, "toggle, but screen recording is off");
            } else if self.state() == RecordingState::Idle {
                self.line(now, "toggle: start");
                self.input(Input::Toggle, settings, now, &mut out);
            } else if self.state == RecordingState::Recording {
                self.line(now, "toggle: stop");
                self.input(Input::Toggle, settings, now, &mut out);
            } else {
                self.line(now, &format!("toggle while {:?}: wait", self.state()));
            }
        }
        self.recorder_pass(settings, now, &mut out);
        self.ui_pass(now);
        self.step_viewport(now, &mut out);
        self.closing_pass(now, &mut out);
        let block = self.state != RecordingState::Idle;
        if block != self.blocking {
            self.set_block_reason(block, now);
        }
        if self.launch.is_some() || self.ending.is_some() {
            out.due = earlier(out.due, Some(now + self.frame));
        }
        out
    }

    fn input(&mut self, input: Input, settings: &Settings, now: f64, out: &mut Pass) {
        let (next, steps) = advance(self.state, input);
        if next != self.state {
            self.line(now, &format!("{:?} -> {next:?} ({input:?})", self.state));
        }
        self.state = next;
        for step in steps {
            match step {
                Step::Launch => self.launch(settings, now),
                Step::ShowUi => {
                    if !self.show_ui(settings, now) {
                        self.input(Input::UiFailed, settings, now, out);
                        return;
                    }
                }
                Step::StopUi => self.stop_ui(now),
                Step::StopRecorder => {
                    if let Some(recorder) = &self.recorder {
                        recorder.stop();
                        self.line(now, "recorder: stop");
                    }
                }
                // Carried with the event's data in `recorder_pass`.
                Step::Carry => {}
                Step::Explain => {
                    if input == Input::UiFailed {
                        out.hint = Some("recording controls could not start".into());
                    }
                }
            }
        }
        if self.state == RecordingState::Idle {
            self.end_recorder(now);
        }
    }

    fn launch(&mut self, settings: &Settings, now: f64) {
        self.started_at = SystemTime::now();
        self.had_frames = false;
        let settings = settings.clone();
        // The folder check and the monitor lookup touch the disk and the display driver.
        self.launch = Some(thread::spawn(move || {
            let monitor = MonitorId::under_cursor().map_err(|e| format!("monitor: {e}"))?;
            let (folder, fallback) = settings.captures_dir(&DefaultFolders::from_system());
            if fallback {
                eprintln!("recording: the captures folder is unavailable, using {}", folder.display());
            }
            let mut config = RecordingConfig::new(monitor, folder.clone());
            config.cursor = CursorMode::Drawn { scale: CURSOR_SCALE };
            Recorder::start(config).map(|recorder| (recorder, folder)).map_err(|e| e.to_string())
        }));
        self.line(now, "recorder: starting");
    }

    /// The launch thread is done: keep the recorder, or report its failure.
    fn finish_launch(&mut self, settings: &Settings, now: f64, out: &mut Pass) {
        if !self.launch.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        let launched = self.launch.take().map(JoinHandle::join);
        match launched {
            Some(Ok(Ok((recorder, folder)))) => {
                self.line(now, &format!("recorder: started, folder {}", folder.display()));
                if self.state == RecordingState::Stopping {
                    recorder.stop();
                }
                self.recorder = Some(recorder);
                self.folder = Some(folder);
            }
            Some(Ok(Err(reason))) => self.fail(&reason, None, settings, now, out),
            _ => self.fail("the start thread panicked", None, settings, now, out),
        }
    }

    fn fail(
        &mut self,
        reason: &str,
        partial: Option<PathBuf>,
        settings: &Settings,
        now: f64,
        out: &mut Pass,
    ) {
        self.problem(now, &format!("failed: {reason}"));
        if let Some(partial) = &partial {
            self.problem(now, &format!("a partial file was kept: {}", partial.display()));
        }
        // Switched off before the first frame: nothing was lost, nothing to say.
        if self.state != RecordingState::Stopping || self.had_frames {
            out.hint = Some(failure_hint(reason, self.had_frames, partial.is_some()));
        }
        self.input(Input::Failed, settings, now, out);
    }

    fn recorder_pass(&mut self, settings: &Settings, now: f64, out: &mut Pass) {
        let Some(recorder) = &self.recorder else {
            return;
        };
        let mut events = std::mem::take(&mut self.recorder_events);
        recorder.drain(&mut events);
        for event in events.drain(..) {
            match event {
                RecorderEvent::Started { monitor_rect, output_size, scale, time } => {
                    self.had_frames = true;
                    let text = format!(
                        "recorder: first frame at {time:.3}, monitor {:.0}x{:.0} at ({:.0}, {:.0}), \
                         video {}x{}, card scale {scale:.2}",
                        monitor_rect.width,
                        monitor_rect.height,
                        monitor_rect.x,
                        monitor_rect.y,
                        output_size.0,
                        output_size.1
                    );
                    self.line(now, &text);
                    let display = Rect::new(0.0, 0.0, monitor_rect.width, monitor_rect.height);
                    self.live = Some(Live {
                        viewport: RecordingViewport::new(display),
                        origin: Point::new(monitor_rect.x, monitor_rect.y),
                        following: false,
                        area: display,
                        last_step: now,
                        moving: false,
                    });
                    self.input(Input::Started, settings, now, out);
                }
                RecorderEvent::Progress { elapsed, frames, dropped } => {
                    if let Some(ui) = &self.ui {
                        ui.set_elapsed(elapsed);
                    }
                    if dropped > 0 && frames > 0 && dropped * 10 > frames {
                        self.line(now, &format!("recorder: {dropped} of {frames} frames dropped"));
                    }
                }
                RecorderEvent::Finished { file, poster, scale, duration } => {
                    self.line(now, &format!("saved {} ({duration:.1} s)", file.display()));
                    out.carry = Some(Finished {
                        file,
                        poster,
                        scale,
                        duration,
                        captured_at: self.started_at,
                    });
                    self.input(Input::Finished, settings, now, out);
                }
                RecorderEvent::Failed { reason, partial_file } => {
                    self.fail(&reason, partial_file, settings, now, out);
                }
            }
        }
        self.recorder_events = events;
    }

    /// Idle again: join the recorder's worker (it already sent its last event).
    fn end_recorder(&mut self, now: f64) {
        self.live = None;
        self.tool = None;
        if let Some(mut recorder) = self.recorder.take()
            && !recorder.shutdown(RECORDER_JOIN)
        {
            self.problem(now, "the recorder's worker did not end; it ends with the process");
        }
    }

    fn show_ui(&mut self, settings: &Settings, now: f64) -> bool {
        let Some(live) = &self.live else {
            return false;
        };
        let screen = offset(live.viewport.display(), live.origin);
        let mut monitor = monitor_bounds_at(Point::new(screen.mid_x(), screen.mid_y()));
        if monitor.bounds != screen {
            self.line(now, &format!("monitor bounds differ: {:?} vs {screen:?}", monitor.bounds));
            monitor.bounds = screen;
        }
        let config = RecordingUiConfig {
            labels: labels(&settings.shortcuts),
            capturable: std::env::var_os("FAIRY_CAPTURABLE").is_some_and(|v| v == "1"),
            debug: self.log.enabled(),
            ..RecordingUiConfig::default()
        };
        match RecordingUi::start(monitor, config) {
            Ok(ui) => {
                ui.set_elapsed(0.0);
                ui.set_area(screen);
                self.ui = Some(ui);
                self.line(now, "ui: shown");
                true
            }
            Err(error) => {
                self.problem(now, &format!("ui: could not start: {error}"));
                false
            }
        }
    }

    fn stop_ui(&mut self, now: f64) {
        self.live = None;
        self.tool = None;
        if let Some(mut ui) = self.ui.take() {
            ui.stop();
            self.closing = Some((ui, now + UI_FADE_BOUND));
            self.line(now, "ui: stop (gates released)");
        }
    }

    fn ui_pass(&mut self, now: f64) {
        let mut events = std::mem::take(&mut self.ui_events);
        if let Some(ui) = &self.ui {
            ui.drain(&mut events);
        }
        for event in events.drain(..) {
            match event {
                UiEvent::Zoom(amount) => {
                    if let Some(live) = &mut self.live {
                        live.viewport.zoom_by(amount);
                        let target = live.viewport.target_zoom();
                        self.line(now, &format!("zoom {amount:+.2} -> target {target:.2}x"));
                    }
                }
                UiEvent::Follow { on, cursor } => {
                    if let Some(live) = &mut self.live {
                        live.following = on;
                        let area = live.area;
                        let (x, y) = cursor;
                        self.line(now, &format!("follow {on} at ({x}, {y}), area {area:?}"));
                    }
                }
                UiEvent::ResetZoom => {
                    if let Some(live) = &mut self.live {
                        live.viewport.reset();
                        self.line(now, "reset zoom: the whole screen");
                    }
                }
                UiEvent::ToolChanged(tool) => {
                    self.tool = tool;
                    self.line(now, &format!("tool {tool:?}"));
                }
                UiEvent::Hidden => {}
                UiEvent::Log(line) => self.ui_log(now, &line),
            }
        }
        self.ui_events = events;
    }

    /// A UI line starts with its own time on the same clock ("  1.234 recording: …").
    fn ui_log(&mut self, now: f64, line: &str) {
        let timed = line
            .trim_start()
            .split_once(' ')
            .and_then(|(at, text)| at.parse::<f64>().ok().map(|at| (at, text)));
        match timed {
            Some((at, text)) => self.log.line(at, text),
            None => self.log.line(now, line),
        }
    }

    fn step_viewport(&mut self, now: f64, out: &mut Pass) {
        let (Some(live), Some(recorder), Some(ui)) = (&mut self.live, &self.recorder, &self.ui)
        else {
            return;
        };
        let dt = if live.moving { now - live.last_step } else { 0.0 };
        live.last_step = now;
        let cursor = cursor_pos();
        let local = Point::new(cursor.x - live.origin.x, cursor.y - live.origin.y);
        let before = live.area;
        live.viewport.step(local, live.following, dt);
        let area = live.viewport.area();
        if area != live.area {
            live.area = area;
            recorder.set_area(area);
            ui.set_area(offset(area, live.origin));
        }
        live.moving = viewport_moving(&live.viewport, live.following, before);
        if live.moving {
            out.due = earlier(out.due, Some(now + self.frame));
        }
    }

    /// A stopped UI: shut down after `Hidden` or the fade bound, off the UI loop.
    fn closing_pass(&mut self, now: f64, out: &mut Pass) {
        if let Some((ui, until)) = &self.closing {
            let mut events = std::mem::take(&mut self.ui_events);
            ui.drain(&mut events);
            let hidden = events.contains(&UiEvent::Hidden);
            let late = now >= *until;
            let until = *until;
            for event in events.drain(..) {
                if let UiEvent::Log(line) = event {
                    self.ui_log(now, &line);
                }
            }
            self.ui_events = events;
            if (hidden || late) && self.ending.is_none() {
                let (ui, _) = self.closing.take().expect("checked above");
                let why = if hidden { "hidden" } else { "fade bound reached" };
                self.line(now, &format!("ui: {why}, shutting down"));
                self.ending = Some(thread::spawn(move || ui.shutdown(UI_SHUTDOWN)));
            } else {
                out.due = earlier(out.due, Some(until));
            }
        }
        if self.ending.as_ref().is_some_and(JoinHandle::is_finished) {
            let ended = self.ending.take().and_then(|thread| thread.join().ok());
            if ended == Some(true) {
                self.line(now, "ui: ended");
            } else {
                self.problem(now, "the recording UI thread did not end within its budget");
            }
        }
    }

    fn set_block_reason(&mut self, block: bool, now: f64) {
        windows::core::link!("user32.dll" "system" fn ShutdownBlockReasonCreate(hwnd: HWND, reason: PCWSTR) -> BOOL);
        windows::core::link!("user32.dll" "system" fn ShutdownBlockReasonDestroy(hwnd: HWND) -> BOOL);
        let window = HWND(self.window as *mut _);
        if window.is_invalid() {
            return;
        }
        // SAFETY: `window` is the tray's window, made on this thread (the only thread these
        // calls accept); `BLOCK_REASON` is a static NUL-terminated string.
        let done = unsafe {
            if block {
                ShutdownBlockReasonCreate(window, BLOCK_REASON)
            } else {
                ShutdownBlockReasonDestroy(window)
            }
        };
        self.blocking = block;
        let what = if block { "set" } else { "cleared" };
        if done.as_bool() {
            self.line(now, &format!("shutdown block reason {what}"));
        } else {
            let error = windows::core::Error::from_thread();
            self.problem(now, &format!("shutdown block reason not {what}: {error}"));
        }
    }

    /// At quit and session end: the UI first (gates released), then the recorder finishes
    /// the file within `within`. The file is not carried: the app is ending. Returns false
    /// when it did not finish in time (its `.partial.mp4` stays in the captures folder).
    pub fn finish_now(&mut self, within: Duration, now: f64) -> bool {
        let until = Instant::now() + within;
        let left = || until.saturating_duration_since(Instant::now());
        let busy = self.state != RecordingState::Idle;
        if busy {
            self.line(now, &format!("quit while {:?}: finishing the file", self.state));
        }
        if let Some(launch) = self.launch.take() {
            while !launch.is_finished() && Instant::now() < until {
                thread::sleep(Duration::from_millis(2));
            }
            if launch.is_finished() {
                match launch.join() {
                    Ok(Ok((recorder, folder))) => {
                        self.recorder = Some(recorder);
                        self.folder = Some(folder);
                    }
                    Ok(Err(reason)) => self.problem(now, &format!("failed: {reason}")),
                    Err(_) => self.problem(now, "the start thread panicked"),
                }
            } else {
                self.problem(now, "the recorder was still starting at quit");
            }
        }
        let mut uis: Vec<RecordingUi> = self.closing.take().map(|(ui, _)| ui).into_iter().collect();
        if let Some(mut ui) = self.ui.take() {
            ui.stop();
            uis.push(ui);
        }
        let mut finished = true;
        if let Some(mut recorder) = self.recorder.take() {
            finished = recorder.shutdown(left());
            let mut events = Vec::new();
            recorder.drain(&mut events);
            for event in events {
                match event {
                    RecorderEvent::Finished { file, duration, .. } => {
                        self.line(now, &format!("saved at quit {} ({duration:.1} s)", file.display()));
                    }
                    RecorderEvent::Failed { reason, partial_file } => {
                        self.problem(now, &format!("failed at quit: {reason}"));
                        if let Some(partial) = partial_file {
                            self.problem(now, &format!("a partial file was kept: {}", partial.display()));
                        }
                    }
                    _ => {}
                }
            }
            if !finished {
                let folder = self.folder.clone().unwrap_or_default();
                self.problem(
                    now,
                    &format!(
                        "not finished within {:.1} s: the .partial.mp4 stays in {}",
                        within.as_secs_f64(),
                        folder.display()
                    ),
                );
            }
        }
        for ui in uis {
            if !ui.shutdown(left().min(UI_SHUTDOWN)) {
                self.problem(now, "the recording UI thread did not end within its budget");
            }
        }
        if let Some(ending) = self.ending.take() {
            while !ending.is_finished() && Instant::now() < until {
                thread::sleep(Duration::from_millis(2));
            }
        }
        self.state = RecordingState::Idle;
        self.live = None;
        self.tool = None;
        if self.blocking && finished {
            self.set_block_reason(false, now);
        }
        if busy {
            self.line(now, "stopped at quit");
        }
        finished
    }
}

fn earlier(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RecordingState::{Idle, Recording, Starting, Stopping};

    const STATES: [RecordingState; 4] = [Idle, Starting, Recording, Stopping];
    const INPUTS: [Input; 6] = [
        Input::Toggle,
        Input::Started,
        Input::UiFailed,
        Input::Finished,
        Input::Failed,
        Input::SwitchedOff,
    ];

    #[test]
    fn every_input_in_every_state() {
        use Step::*;
        let table: [(RecordingState, Input, RecordingState, &[Step]); 24] = [
            (Idle, Input::Toggle, Starting, &[Launch]),
            (Idle, Input::Started, Idle, &[]),
            (Idle, Input::UiFailed, Idle, &[]),
            (Idle, Input::Finished, Idle, &[]),
            (Idle, Input::Failed, Idle, &[]),
            (Idle, Input::SwitchedOff, Idle, &[]),
            // Swift ignores the toggle while starting; switching off saves what there is.
            (Starting, Input::Toggle, Starting, &[]),
            (Starting, Input::Started, Recording, &[ShowUi]),
            (Starting, Input::UiFailed, Starting, &[]),
            (Starting, Input::Finished, Idle, &[Carry]),
            (Starting, Input::Failed, Idle, &[Explain]),
            (Starting, Input::SwitchedOff, Stopping, &[StopRecorder]),
            // The UI stops before the recorder (the gates are released first).
            (Recording, Input::Toggle, Stopping, &[StopUi, StopRecorder]),
            (Recording, Input::Started, Recording, &[]),
            (Recording, Input::UiFailed, Stopping, &[StopRecorder, Explain]),
            (Recording, Input::Finished, Idle, &[StopUi, Carry]),
            (Recording, Input::Failed, Idle, &[StopUi, Explain]),
            (Recording, Input::SwitchedOff, Stopping, &[StopUi, StopRecorder]),
            (Stopping, Input::Toggle, Stopping, &[]),
            (Stopping, Input::Started, Stopping, &[]),
            (Stopping, Input::UiFailed, Stopping, &[]),
            (Stopping, Input::Finished, Idle, &[Carry]),
            (Stopping, Input::Failed, Idle, &[Explain]),
            (Stopping, Input::SwitchedOff, Stopping, &[]),
        ];
        for (state, input, next, steps) in table {
            assert_eq!(advance(state, input), (next, steps), "{state:?} + {input:?}");
        }
        // The table covers every pair.
        for state in STATES {
            for input in INPUTS {
                assert!(table.iter().any(|row| row.0 == state && row.1 == input));
            }
        }
    }

    #[test]
    fn the_toggle_follows_input_toggle() {
        use super::super::input::{Toggle, toggle};
        for state in STATES {
            let (next, steps) = advance(state, Input::Toggle);
            match toggle(state) {
                Toggle::Start => assert_eq!(steps, &[Step::Launch]),
                Toggle::Stop => assert!(steps.contains(&Step::StopRecorder)),
                Toggle::Ignore => assert_eq!((next, steps), (state, &[][..])),
            }
        }
    }

    #[test]
    fn exclusive_state_rules() {
        // The note gesture and tool menu are off from the press until the stop.
        assert_eq!(STATES.map(exclusive), [false, true, true, false]);
        // The pen keys only while recording.
        assert_eq!(STATES.map(pen_keys), [false, false, true, false]);
        // Hidden for drawing only with a tool in hand while recording.
        for state in STATES {
            assert!(!drawing(state, None));
            assert_eq!(drawing(state, Some(InkTool::Pen)), state == Recording);
            assert_eq!(drawing(state, Some(InkTool::Highlighter)), state == Recording);
        }
        // A UI still fading out keeps a new recording waiting.
        assert_eq!(shown_state(Idle, true), Stopping);
        assert_eq!(shown_state(Idle, false), Idle);
        assert_eq!(shown_state(Recording, true), Recording);
        // The tray item.
        assert_eq!(STATES.map(|s| menu_label(s).1), [true, false, true, false]);
        assert_eq!(menu_label(Recording).0, "Stop Recording");
    }

    #[test]
    fn failure_hints_are_short() {
        assert_eq!(failure_hint("a recording is already active", false, false), "the last recording is still saving");
        assert_eq!(
            failure_hint("monitor bounds, resolution or DPI changed during recording", true, true),
            "recording stopped · the screen changed"
        );
        assert_eq!(failure_hint("Windows.Graphics.Capture is unavailable", false, false), "could not start recording");
        assert_eq!(failure_hint("write H.264 sample: disk full", true, true), "recording failed · part of it was kept");
        assert_eq!(failure_hint("write H.264 sample: disk full", true, false), "recording failed");
    }

    #[test]
    fn the_viewport_ticks_only_while_it_moves() {
        let display = Rect::new(0.0, 0.0, 2000.0, 1000.0);
        let mut viewport = RecordingViewport::new(display);
        let before = viewport.area();
        viewport.step(Point::new(100.0, 100.0), false, 0.0);
        assert!(!viewport_moving(&viewport, false, before), "at rest");
        assert!(viewport_moving(&viewport, true, before), "following");
        viewport.zoom_by(1.0);
        let mut steps = 0;
        loop {
            let before = viewport.area();
            viewport.step(Point::new(100.0, 100.0), false, 0.008);
            steps += 1;
            if !viewport_moving(&viewport, false, before) {
                break;
            }
            assert!(steps < 2000, "the zoom settles");
        }
        assert_eq!(viewport.zoom(), viewport.target_zoom());
        assert!((0.5..=3.0).contains(&(steps as f64 * 0.008)), "{steps} steps");
    }

    #[test]
    fn areas_move_to_screen_coordinates() {
        let area = Rect::new(10.0, 20.0, 300.0, 200.0);
        assert_eq!(offset(area, Point::new(-1920.0, 0.0)), Rect::new(-1910.0, 20.0, 300.0, 200.0));
        assert!(!moved(area, Rect::new(10.005, 20.0, 300.0, 200.0)));
        assert!(moved(area, Rect::new(10.5, 20.0, 300.0, 200.0)));
    }

    #[test]
    fn pill_labels_follow_the_shortcuts() {
        let mut shortcuts = Shortcuts::defaults();
        let defaults = labels(&shortcuts);
        assert!(defaults.toggle.is_some() && defaults.pen.is_some() && defaults.highlighter.is_some());
        shortcuts.set(Action::Pen, None).unwrap();
        assert_eq!(labels(&shortcuts).pen, None);
    }
}
