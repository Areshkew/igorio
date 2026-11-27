#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Wires the modules together: one UI thread owns the overlay, the tray, the state and the
//! drop router. `igorio --demo N` starts with N generated screenshots on the pile.
//! `IGORIO_DEBUG=1` logs every drop decision (see `drop_router::DebugLog`).

use std::cell::{Cell, RefCell, RefMut};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use igorio_app::draw::Renderer;
use igorio_app::drop_router::{DebugLog, Drops};
use igorio_app::follower::Follower;
use igorio_app::guide::{GuideWindow, Tried};
use igorio_app::hook::{self, MouseEvent, MouseHook};
use igorio_app::layout::CardSize;
use igorio_app::notes::panel::menu_notes;
use igorio_app::notes::store::{NoteId, RecycleBin};
use igorio_app::notes::summoner::{Block, SummonConfig, Summoner, SummonerEvent, ToolContext, ToolId};
use igorio_app::notes::window::{Event as NotesEvent, Notes, NotesConfig, RecentNote, Theme};
use igorio_app::overlay::{self, Overlay};
use igorio_app::recording::feature::{self as rec, RecordingFeature, RecordingState};
use igorio_app::saver;
use igorio_app::settings::{self, DefaultFolders, Settings};
use igorio_app::settings_window::{LiveSettings, SettingsWindow, ShortcutService};
use igorio_app::shortcuts::{Action, Hotkeys, Registration, display_for_layout};
use igorio_app::snip::{Snip, SnipEvent, Source};
use igorio_app::state::{IgorioState, Hidden};
use igorio_app::tray::{EndRequest, MenuItem, Tray};
use igorio_app::uia::Requester;
use igorio_core::geom::Point;
use igorio_core::ink::InkTool;
use igorio_core::shake::{ShakeGate, ShakeResult};
use igorio_kit::Image;
use tiny_skia::Pixmap;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HANDLE, LPARAM, SetLastError,
    WAIT_FAILED, WPARAM,
};
use windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateMutexExW, CreateWaitableTimerExW,
    GetCurrentThreadId, INFINITE, SYNCHRONIZATION_SYNCHRONIZE, SetWaitableTimer, TIMER_MODIFY_STATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, PostQuitMessage, PostThreadMessageW, QS_ALLINPUT, TranslateMessage, WM_CLOSE,
    WM_QUIT,
};
use windows::core::{BOOL, w};

const MENU_RESTORE: u32 = 1;
const MENU_ADD_DEMO: u32 = 2;
const MENU_QUIT: u32 = 3;
const MENU_SETTINGS: u32 = 4;
const MENU_OPEN_NOTES_FOLDER: u32 = 5;
const MENU_RECORD: u32 = 6;
const MENU_PEN: u32 = 7;
const MENU_HIGHLIGHTER: u32 = 8;
const MENU_NOTES_PANEL: u32 = 9;
const MENU_GUIDE: u32 = 10;
/// A recent note in the tray menu: this id plus its place in the list.
const MENU_NOTE_BASE: u32 = 100;
/// How long stopping the notes may take: saving the text and closing the windows.
const NOTES_STOP: Duration = Duration::from_secs(2);
/// How long the normal quit waits for a recording in progress to be finished. Bounded by the recorder's own shutdown wait.
const RECORDING_STOP: Duration = Duration::from_secs(3);
/// At session end Windows may end the process a few seconds after WM_ENDSESSION, so the quit
/// does only what matters, most important first, within 4.0 s in all: the clipboard restore
/// (this), the notes' text and window positions ([`END_NOTES`]), then the clipboard thread's
/// stop, which renders what we still own (its own bound, 2 s, in `Clipboard::drop`). A
/// recording in progress comes last ([`END_RECORDING`]): while one runs the tray window holds
/// a shutdown block reason, so Windows shows "Saving a screen recording" and waits for it
/// instead of ending the process after the usual few seconds.
const END_RESTORE: Duration = Duration::from_millis(800);
const END_NOTES: Duration = Duration::from_millis(1200);
const END_RECORDING: Duration = Duration::from_secs(5);
/// How long a console handler waits for the quit path. Windows ends the process about 5 s
/// after the console window is closed; Ctrl+C and Ctrl+Break run the normal quit, whose steps
/// add up to about 18.3 s at worst. If the quit has not finished by then, the default
/// handler ends the process.
const CONSOLE_CLOSE_WAIT: Duration = Duration::from_millis(4500);
const CONSOLE_BREAK_WAIT: Duration = Duration::from_secs(19);

/// Frame interval while something moves, in seconds.
const FRAME: f64 = 0.008;
/// The longest sleep. The loop wakes at least this often to check that the mouse hook is
/// still installed (Windows removes a slow hook without notice).
const HEALTH_CHECK: f64 = 1.0;

struct App {
    state: IgorioState,
    shake: ShakeGate,
    follower: Follower,
    renderer: Renderer,
    overlay: Overlay,
    pixmap: Option<Pixmap>,
    origin: (i32, i32),
    scale: f64,
    last_frame: f64,
    /// The scene changed since the last draw.
    dirty: bool,
    /// The last frame was drawn mid-animation, so one more frame shows the end state.
    was_animating: bool,
    demo_count: usize,
    /// What the user did this pass that the guide's "Try it now" lines wait for.
    tried: Vec<Tried>,
}

impl App {
    fn apply_card_size(&mut self, size: CardSize, now: f64) {
        self.state.set_card_size(size, now);
        self.follower.card_size = size;
        self.dirty = true;
    }

    fn add_demo(&mut self) {
        let image = Arc::new(demo_image(self.demo_count));
        self.demo_count += 1;
        self.state.hold(image, 1.0, SystemTime::now(), false, None, None, hook::now());
        self.shake.captured();
        self.dirty = true;
    }

    fn restore_last(&mut self) {
        self.state.restore_last(hook::now());
        self.dirty = true;
    }

    /// Runs a global shortcut. `OpenSettings` is handled by the loop, like the tray item.
    fn shortcut(&mut self, action: Action, now: f64) {
        match action {
            Action::RestoreLast => self.restore_last(),
            Action::DiscardTop | Action::DiscardAll if self.state.is_holding() => {
                if action == Action::DiscardTop {
                    self.state.dissolve(now);
                } else {
                    self.state.discard_all(now);
                }
                // As after a shake: the gesture starts over, so the motion before the key
                // press counts toward nothing. Unlike a shake, no "discard all" is armed.
                self.shake.gesture.reset();
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn capture_ui(&mut self, shown: bool, time: f64) {
        self.hide(Hidden::CaptureUi, shown, time);
    }

    /// One reason to hide the pile comes or goes: the
    /// screenshot UI, or a pen in hand while recording. It is hidden while any reason holds;
    /// the shake gate follows the overall edge.
    fn hide(&mut self, reason: Hidden, hidden: bool, time: f64) {
        if self.state.set_hidden(reason, hidden, time) {
            if hidden {
                self.shake.screenshot_ui_shown();
            } else {
                self.shake.screenshot_ui_gone();
            }
            self.dirty = true;
        }
    }

    /// A finished recording, carried like a screenshot (`carry(recording:poster:scale:duration:)`):
    /// already saved, so a shake only lets go of it.
    fn carry_recording(&mut self, finished: rec::Finished, now: f64) {
        let rec::Finished { file, poster, scale, duration, captured_at } = finished;
        self.state.hold(poster, scale, captured_at, true, Some(file), Some(duration), now);
        self.shake.captured();
        self.dirty = true;
    }

    /// Returns true if a visibility transition may overlap this batch of mouse events.
    fn screenshots(&mut self, events: &mut Vec<SnipEvent>, settings: &Settings) -> bool {
        let mut transitioned = false;
        for event in events.drain(..) {
            match event {
                SnipEvent::CaptureUiShown { time } => {
                    self.capture_ui(true, time);
                    transitioned = true;
                }
                SnipEvent::CaptureUiGone { time } => {
                    self.capture_ui(false, time);
                    transitioned = true;
                }
                SnipEvent::Captured {
                    image,
                    scale,
                    captured_at,
                    source,
                    saved_file,
                } => {
                    let kept = saved_file.is_some();
                    // A copied image is not a screenshot, so it is not kept.
                    if !kept && settings.keep_every_screenshot && source != Source::Copied {
                        let image = Arc::clone(&image);
                        let settings = settings.clone();
                        // Folder selection can itself hit the filesystem and the Shell.
                        std::thread::spawn(move || {
                            let (folder, _) = settings.captures_dir(&DefaultFolders::from_system());
                            saver::save_screenshot_in_background(
                                image,
                                scale,
                                captured_at,
                                folder,
                                |result| {
                                    if let Err(error) = result {
                                        eprintln!("could not keep screenshot: {error}");
                                    }
                                },
                            );
                        });
                    }
                    self.shake.captured();
                    self.state
                        .hold(image, scale, captured_at, kept, None, None, hook::now());
                    self.tried.push(Tried::CardLanded);
                    self.dirty = true;
                }
                // The detector already wrote the reason to its debug log.
                SnipEvent::Ignored { .. } => {}
            }
        }
        transitioned
    }

    /// Feeds mouse events to the shake gesture. Returns true when the cursor moved.
    fn mouse(&mut self, events: &[MouseEvent]) -> bool {
        let mut moved = false;
        // The shake thresholds are in logical pixels.
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        for event in events {
            let MouseEvent::Move { x, y, time, injected: false } = *event else {
                continue;
            };
            moved = true;
            let holding = self.state.is_holding();
            match self.shake.move_(x as f64 / scale, y as f64 / scale, time, holding) {
                ShakeResult::DiscardTop => {
                    self.state.dissolve(time);
                    self.tried.push(Tried::Shaken);
                    self.dirty = true;
                }
                ShakeResult::DiscardAll => {
                    self.state.discard_all(time);
                    self.tried.push(Tried::Shaken);
                    self.dirty = true;
                }
                ShakeResult::None => {}
            }
        }
        moved
    }

    /// True when a frame must run now, whatever the timer says.
    fn needs_frame(&mut self) -> bool {
        if self.overlay.take_display_changed() {
            self.overlay.reassert();
            self.dirty = true;
        }
        self.dirty
    }

    fn pile_visible(&self) -> bool {
        !self.state.stack().is_empty()
    }

    /// Runs one frame and returns the time of the next one; `None` means "when something
    /// happens" (a mouse move, a menu action).
    fn frame(&mut self, now: f64) -> windows::core::Result<Option<f64>> {
        let dt = now - self.last_frame;
        self.last_frame = now;

        let deadline_before = self.state.next_deadline();
        self.state.tick(now);
        if deadline_before.is_some_and(|at| at <= now) {
            self.dirty = true;
        }

        let animating = self.state.is_animating(now);
        if self.state.stack().is_empty() && !animating {
            if self.overlay.is_visible() {
                self.overlay.hide();
            }
            self.follower.reset();
            self.dirty = false;
            self.was_animating = false;
            return Ok(self.state.next_deadline());
        }

        let mouse = overlay::cursor_pos();
        let monitor = overlay::monitor_at(mouse);
        let moved = self
            .follower
            .step(mouse, monitor.work_area, monitor.scale, dt);
        if let Some(origin) = moved {
            self.origin = (origin.x.round() as i32, origin.y.round() as i32);
        }

        let scale_changed = monitor.scale != self.scale;
        self.scale = monitor.scale;
        if self.dirty || animating || self.was_animating || scale_changed {
            let (width, height) = Renderer::pixmap_size(self.scale);
            if self
                .pixmap
                .as_ref()
                .is_none_or(|p| p.width() != width || p.height() != height)
            {
                self.pixmap = Pixmap::new(width, height);
            }
            if let Some(pixmap) = self.pixmap.as_mut() {
                self.renderer
                    .render(&self.state.scene(now, self.scale), pixmap);
                self.overlay.present(pixmap, self.origin)?;
            }
            if !self.overlay.is_visible() {
                self.overlay.show();
            }
            self.dirty = false;
        } else if moved.is_some() {
            self.overlay.move_to(self.origin)?;
        }
        self.was_animating = animating;

        let settled = self
            .follower
            .is_settled(mouse, monitor.work_area, monitor.scale);
        let next_frame = (animating || !settled).then_some(now + FRAME);
        Ok(earlier(next_frame, self.state.next_deadline()))
    }
}

fn earlier(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A generated stand-in for a screenshot: a gradient with a few blocks, a new colour each time.
fn demo_image(index: usize) -> Image {
    const W: u32 = 1600;
    const H: u32 = 1000;
    const PALETTE: [[u8; 3]; 5] = [
        [59, 130, 246],
        [16, 185, 129],
        [245, 158, 11],
        [139, 92, 246],
        [244, 63, 94],
    ];
    let base = PALETTE[index % PALETTE.len()];
    let mut rgba = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let shade = 0.55 + 0.45 * (x + y) as f64 / (W + H) as f64;
            let block = (300..700).contains(&x) && (200..450).contains(&y)
                || (900..1300).contains(&x) && (550..800).contains(&y);
            let lift = if block { 90.0 } else { 0.0 };
            for channel in base {
                rgba.push((channel as f64 * shade + lift).min(255.0) as u8);
            }
            rgba.push(255);
        }
    }
    Image::new(W, H, rgba).expect("the buffer has the declared size")
}

/// A one-shot high-resolution timer, so an 8 ms sleep is not rounded up to the 15.6 ms
/// system tick.
struct Sleeper(HANDLE);

impl Sleeper {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: no pointers are passed; the returned handle is owned by `Sleeper`.
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                (TIMER_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE).0,
            )?
        };
        Ok(Self(handle))
    }

    /// Returns when `wait` has passed, a message is in the queue or one of `wakes` is
    /// signalled, whichever is first.
    fn wait(&self, wait: Duration, wakes: &[HANDLE]) -> windows::core::Result<()> {
        // Negative means relative, in 100 ns units. At least one unit, so the timer fires.
        let due = -((wait.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64);
        let mut handles = Vec::with_capacity(1 + wakes.len());
        handles.push(self.0);
        handles.extend_from_slice(wakes);
        // SAFETY: `self.0` and every handle in `wakes` are live handles, and `due` and
        // `handles` outlive the call.
        unsafe {
            SetWaitableTimer(self.0, &due, 0, None, None, false)?;
            let result = MsgWaitForMultipleObjectsEx(
                Some(&handles),
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            // A failed wait returns at once: end the loop rather than spin.
            if result == WAIT_FAILED {
                return Err(windows::core::Error::from_thread());
            }
        }
        Ok(())
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        // SAFETY: the handle was created in `new` and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// True for a queued message that ends the loop: WM_QUIT (the tray's Quit, a console event,
/// the end of a session) and WM_CLOSE posted to any window of this thread (`taskkill` without
/// /F posts it to the app's top-level windows). A WM_CLOSE seen here never reaches a window
/// procedure, so it destroys no window of this thread.
fn ends_loop(message: u32) -> bool {
    message == WM_QUIT || message == WM_CLOSE
}

/// Dispatches every queued message. Returns false when a message that ends the loop was seen.
fn pump_messages() -> bool {
    let mut message = MSG::default();
    // SAFETY: `message` is a valid MSG for the calls to fill and read; this thread owns
    // the windows whose messages are dispatched.
    unsafe {
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            if ends_loop(message.message) {
                return false;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    true
}

fn demo_count_from_args() -> usize {
    let args: Vec<String> = std::env::args().collect();
    match args.iter().position(|arg| arg == "--demo") {
        Some(i) => args.get(i + 1).and_then(|n| n.parse().ok()).unwrap_or(1),
        None => 0,
    }
}

/// Serializes detector startup and shutdown off the overlay's frame loop.
#[derive(Default)]
struct Detector {
    active: Option<Snip>,
    transition: Option<JoinHandle<Option<Snip>>>,
    retry_at: f64,
}

impl Detector {
    fn stop(&mut self) {
        if let Some(snip) = self.active.take() {
            self.transition = Some(thread::spawn(move || {
                if !snip.shutdown() {
                    eprintln!("screenshot workers did not stop within their shutdown budget");
                }
                None
            }));
        }
        self.retry_at = 0.0;
    }

    fn update(&mut self, enabled: bool, drops: Option<&Drops>, now: f64) {
        if self.transition.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(transition) = self.transition.take()
        {
            self.active = transition.join().ok().flatten();
        }
        if !enabled {
            self.stop();
        } else if self.active.is_none() && self.transition.is_none() && now >= self.retry_at
            && let Some(clipboard) = drops.and_then(Drops::clipboard)
        {
            self.retry_at = now + HEALTH_CHECK;
            let start = Snip::starter(clipboard);
            self.transition = Some(thread::spawn(move || match start() {
                Ok(snip) => Some(snip),
                Err(error) => {
                    eprintln!("screenshot detection is off: {error}");
                    None
                }
            }));
        }
    }
}

impl Drop for Detector {
    fn drop(&mut self) {
        // Both Snip startup and shutdown have bounded waits and need the clipboard alive.
        drop(self.transition.take().and_then(|thread| thread.join().ok().flatten()));
    }
}

/// The Notes capability while it runs. `stop` ends the gesture first, then the notes.
struct NotesRunning {
    summoner: Option<Summoner>,
    notes: Notes,
    /// The folder the notes thread uses, and the setting it came from.
    folder: PathBuf,
    chosen: Option<PathBuf>,
}

impl NotesRunning {
    /// Runs off the UI thread: the folder check touches the disk and both starts wait (bounded)
    /// for their threads. Returns the problems to log.
    fn start(
        settings: &Settings,
        windows_file: PathBuf,
        requester: Result<Requester, String>,
        summon: SummonConfig,
    ) -> (Option<NotesRunning>, Vec<String>) {
        let mut problems = Vec::new();
        let (folder, fallback) = settings.notes_dir(&DefaultFolders::from_system());
        if fallback {
            problems.push(format!("notes: the chosen folder is unavailable, using {}", folder.display()));
        }
        let notes = match Notes::start(NotesConfig {
            folder: folder.clone(),
            windows_file,
            trash: Box::new(RecycleBin),
            theme: Theme::System,
            restore: true,
            test: None,
        }) {
            Ok(notes) => notes,
            Err(error) => {
                problems.push(format!("notes are off: {error}"));
                return (None, problems);
            }
        };
        let summoner = match requester.and_then(|requester| Summoner::start(summon, requester)) {
            Ok(summoner) => Some(summoner),
            Err(error) => {
                problems.push(format!("notes: the note gesture is off: {error}"));
                None
            }
        };
        let running = NotesRunning { summoner, notes, folder, chosen: settings.notes_folder.clone() };
        (Some(running), problems)
    }

    /// The gesture first (it would ask for notes), then the notes: their text and window
    /// states are written before the thread ends. Returns the problems to log.
    fn stop(self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(summoner) = self.summoner
            && !summoner.shutdown()
        {
            problems.push("notes: the note gesture did not stop within its budget".into());
        }
        if !self.notes.shutdown(NOTES_STOP) {
            problems.push("notes: the notes thread did not stop within its budget".into());
        }
        problems
    }
}

/// Starts and stops the Notes capability off the UI loop (like [`Detector`]) and connects it
/// to the pile, the drop router and the tray.
struct NotesFeature {
    active: Option<NotesRunning>,
    transition: Option<JoinHandle<(Option<NotesRunning>, Vec<String>)>>,
    /// The transition is a start (not a stop).
    starting: bool,
    /// The last start failed: it is tried again after the switch goes off and on.
    failed: bool,
    windows_file: PathBuf,
    log: DebugLog,
    /// The last list from the notes thread, most recently edited first.
    recent: Vec<RecentNote>,
    /// The notes the tray menu shows, in menu order.
    shown: Vec<NoteId>,
    /// "Notes…" was chosen: the loop opens the panel by the tray icon.
    panel_asked: bool,
    summoner_events: Vec<SummonerEvent>,
    /// The tool menu's Record screen was chosen: the loop hands it to the recording.
    record: bool,
    /// The tool menu opened: the loop tells the guide.
    menu_opened: bool,
}

/// What the tool menu and the note gesture need to know about screen recording.
#[derive(Debug, Clone, Copy, Default)]
struct RecordingFacts {
    /// The capability is switched on.
    enabled: bool,
    /// A recording is starting, running or being saved.
    running: bool,
    /// The note gesture and the tool menu are off ([`rec::exclusive`]).
    exclusive: bool,
}

impl NotesFeature {
    fn new(windows_file: PathBuf) -> NotesFeature {
        NotesFeature {
            active: None,
            transition: None,
            starting: false,
            failed: false,
            windows_file,
            log: DebugLog::from_env(),
            recent: Vec::new(),
            panel_asked: false,
            shown: Vec::new(),
            summoner_events: Vec::new(),
            record: false,
            menu_opened: false,
        }
    }

    fn line(&mut self, now: f64, text: &str) {
        if !self.log.enabled() {
            eprintln!("{text}");
        }
        self.log.line(now, text);
    }

    fn summoner(&self) -> Option<&Summoner> {
        self.active.as_ref()?.summoner.as_ref()
    }

    /// Follows the settings switch and the notes folder.
    fn update(&mut self, enabled: bool, settings: &Settings, drops: Option<&mut Drops>, now: f64) {
        let mut drops = drops;
        if self.transition.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(transition) = self.transition.take()
        {
            let (running, problems) = transition.join().unwrap_or_else(|_| {
                (None, vec!["notes: the start or stop thread panicked".into()])
            });
            for problem in problems {
                self.line(now, &problem);
            }
            match running {
                Some(running) => {
                    let text = format!("notes: on, folder {}", running.folder.display());
                    self.log.line(now, &text);
                    if let Some(drops) = drops.as_deref_mut() {
                        drops.set_note_zones(Some(running.notes.drop_zones()));
                    }
                    self.active = Some(running);
                }
                None if self.starting => self.failed = true,
                None => {}
            }
            self.starting = false;
        }
        if !enabled {
            self.failed = false;
            self.stop(drops);
            return;
        }
        if let Some(running) = &mut self.active {
            if let Some(summoner) = &mut running.summoner {
                summoner.set_shortcuts(&settings.shortcuts);
                summoner.set_tool_menu(settings.tool_menu);
            }
            if running.chosen != settings.notes_folder {
                // Rare and asked for: the folder check runs here.
                let (folder, _) = settings.notes_dir(&DefaultFolders::from_system());
                running.chosen = settings.notes_folder.clone();
                if folder != running.folder {
                    running.notes.set_folder(folder.clone());
                    self.log.line(now, &format!("notes: folder {}", folder.display()));
                    running.folder = folder;
                }
            }
        } else if self.transition.is_none() && !self.failed {
            let requester = drops
                .as_deref()
                .map_or_else(|| Err("no UI Automation".to_string()), Drops::requester);
            let summon = SummonConfig {
                debug: self.log.enabled(),
                capturable: std::env::var_os("IGORIO_CAPTURABLE").is_some_and(|v| v == "1"),
                ..SummonConfig::default()
            };
            let settings = settings.clone();
            let windows_file = self.windows_file.clone();
            self.log.line(now, "notes: starting");
            self.starting = true;
            self.transition = Some(thread::spawn(move || {
                NotesRunning::start(&settings, windows_file, requester, summon)
            }));
        }
    }

    /// Switch off: the gesture stops, open notes are put away with their text saved and come
    /// back when the switch goes on again.
    fn stop(&mut self, drops: Option<&mut Drops>) {
        if let Some(running) = self.active.take() {
            if let Some(drops) = drops {
                drops.set_note_zones(None);
            }
            self.recent.clear();
            self.shown.clear();
            self.log.line(hook::now(), "notes: off");
            self.transition = Some(thread::spawn(move || (None, running.stop())));
        }
    }

    /// At quit, on every way out: a start or stop in flight finishes, then the gesture stops,
    /// then the notes (pending text written). Bounded by the starts' and stops' own waits.
    fn stop_now(&mut self) {
        if let Some(transition) = self.transition.take()
            && let Ok((running, problems)) = transition.join()
        {
            for problem in problems {
                self.line(hook::now(), &problem);
            }
            if running.is_some() {
                self.active = running;
            }
        }
        if let Some(running) = self.active.take() {
            for problem in running.stop() {
                self.line(hook::now(), &problem);
            }
        }
    }

    /// At session end: only the notes thread (pending text, then the window positions), all
    /// within `within`, a start or stop in flight included. Returns the gesture, which is
    /// stopped later or ends with the process.
    fn stop_notes_within(&mut self, within: Duration) -> Option<Summoner> {
        let until = Instant::now() + within;
        if let Some(transition) = self.transition.take() {
            while !transition.is_finished() && Instant::now() < until {
                thread::sleep(Duration::from_millis(2));
            }
            if !transition.is_finished() {
                // Dropping the handle detaches the thread; it ends with the process.
                self.line(hook::now(), "notes: a start or stop is still running at session end");
            } else if let Ok((running, problems)) = transition.join() {
                for problem in problems {
                    self.line(hook::now(), &problem);
                }
                if running.is_some() {
                    self.active = running;
                }
            }
        }
        let running = self.active.take()?;
        if !running.notes.shutdown(until.saturating_duration_since(Instant::now())) {
            self.line(hook::now(), "notes: the notes thread did not stop within its budget");
        }
        running.summoner
    }

    /// The mouse events of this pass, for the gesture (the app has one mouse hook).
    fn feed(&self, events: &[MouseEvent]) {
        if let Some(summoner) = self.summoner() {
            summoner.feed(events);
        }
    }

    /// One pass of the UI loop: cards for a note, the notes' events, summons, and what blocks
    /// the gesture. Returns true when the pile changed.
    fn pass(
        &mut self,
        state: &mut IgorioState,
        drops: Option<&mut Drops>,
        recording: RecordingFacts,
        now: f64,
    ) -> bool {
        let mut dirty = false;
        let mut drops = drops;
        let gifts = drops.as_deref_mut().map(Drops::take_note_gifts).unwrap_or_default();
        let Some(running) = self.active.as_mut() else {
            return false;
        };
        for ((x, y), cards) in gifts {
            running.notes.give_cards(Point::new(f64::from(x), f64::from(y)), cards);
        }
        let events = running.notes.events();
        let mut summons = std::mem::take(&mut self.summoner_events);
        if let Some(summoner) = &mut running.summoner {
            summoner.drain(&mut summons);
            summoner.set_blocked(Block::Paste, drops.as_deref().is_some_and(Drops::busy));
            summoner.set_blocked(Block::Capture, state.hidden_for_capture());
            // While recording, Alt is for the zoom (`host.setExclusive`).
            summoner.set_blocked(Block::Recording, recording.exclusive);
            summoner.set_tool_context(ToolContext {
                notes: true,
                recording_available: Action::ToggleRecording.available(),
                recording_enabled: recording.enabled,
                recording: recording.running,
                cards_held: state.is_holding(),
                ..ToolContext::default()
            });
        }
        for event in summons.drain(..) {
            match event {
                SummonerEvent::ToolChosen(chosen) => {
                    let (x, y) = chosen.point;
                    match chosen.tool {
                        // `new_note_frame` puts the card's top-left half a pill up and left of
                        // the point, as a note made from the pill would be.
                        ToolId::NewNote => {
                            running.notes.open_new(Point::new(f64::from(x), f64::from(y)), chosen.origin.clone());
                            self.log.line(
                                now,
                                &format!("notes: summon at ({x}, {y}) from {}", chosen.origin.label()),
                            );
                        }
                        ToolId::RecordScreen => {
                            self.record = true;
                            self.log.line(now, &format!("record screen chosen at ({x}, {y})"));
                        }
                    }
                }
                SummonerEvent::MenuOpened => self.menu_opened = true,
                // The line starts with its own time on the same clock ("  1.234 summon: …").
                SummonerEvent::Log(line) => {
                    let timed = line.trim_start().split_once(' ').and_then(|(at, text)| {
                        at.parse::<f64>().ok().map(|at| (at, text))
                    });
                    match timed {
                        Some((at, text)) => self.log.line(at, text),
                        None => self.log.line(now, &line),
                    }
                }
            }
        }
        self.summoner_events = summons;
        for event in events {
            match event {
                NotesEvent::CardsTaken { ids, hint } => {
                    self.log.line(now, &format!("notes: a note took {} card(s)", ids.len()));
                    dirty |= match drops.as_deref_mut() {
                        Some(drops) => drops.note_took(&ids, &hint, state, now),
                        None => {
                            state.deliver(&ids, &hint, now);
                            true
                        }
                    };
                }
                NotesEvent::Recent(recent) => {
                    self.log.line(now, &format!("notes: {} recent", recent.len()));
                    self.recent = recent;
                }
                NotesEvent::Error(message) => {
                    self.line(now, &format!("notes: error: {message}"));
                    let handled =
                        drops.as_deref_mut().is_some_and(|drops| drops.note_failed(state, now));
                    if handled {
                        dirty = true;
                    } else if state.is_holding() {
                        state.flash_hint(&overlay_hint(&message), 2.5, now);
                        dirty = true;
                    }
                }
            }
        }
        dirty
    }

    /// Restarts the gesture when its thread or keyboard hook died.
    fn health(&mut self, now: f64) {
        let Some(summoner) = self.active.as_mut().and_then(|running| running.summoner.as_mut())
        else {
            return;
        };
        if !summoner.is_alive() {
            let result = summoner.restart();
            let text = match result {
                Ok(()) => "notes: the note gesture was restarted".to_string(),
                Err(error) => format!("notes: the note gesture could not restart: {error}"),
            };
            self.line(now, &text);
        }
    }

    fn wakes(&self, out: &mut Vec<HANDLE>) {
        if let Some(running) = &self.active {
            out.push(running.notes.wake_handle());
            if let Some(summoner) = &running.summoner {
                out.push(summoner.wake_handle());
            }
        }
    }

    /// The NewNote shortcut.
    fn summon_at_cursor(&mut self) {
        match self.summoner() {
            Some(summoner) => {
                summoner.summon_at_cursor();
                self.log.line(hook::now(), "notes: new note shortcut");
            }
            None => self.log.line(hook::now(), "notes: new note shortcut, but notes are off"),
        }
    }

    /// The tray menu's notes section: "Notes…" (the
    /// panel), "Recent Notes" with the five last changed, open ones checked, and "Open Notes
    /// Folder". Shows the last known list; the refresh it asks for answers before the next
    /// opening.
    fn menu_section(&mut self) -> Vec<MenuItem> {
        let Some(running) = &self.active else {
            self.shown.clear();
            return Vec::new();
        };
        running.notes.refresh();
        let recent = menu_notes(&self.recent);
        self.shown = recent.iter().map(|note| note.id).collect();
        let mut items: Vec<MenuItem> = recent
            .iter()
            .enumerate()
            .map(|(index, note)| MenuItem::Action {
                id: MENU_NOTE_BASE + index as u32,
                label: format!("{}\t{}", menu_text(&note.title), menu_text(&note.label)),
                enabled: true,
                checked: note.is_open,
            })
            .collect();
        if items.is_empty() {
            items.push(MenuItem::Label("No notes yet — hold Alt and choose New note".into()));
        }
        vec![
            MenuItem::action(MENU_NOTES_PANEL, "Notes…"),
            MenuItem::Submenu { label: "Recent Notes".into(), items },
            MenuItem::action(MENU_OPEN_NOTES_FOLDER, "Open Notes Folder"),
        ]
    }

    fn menu_action(&mut self, id: u32) {
        let Some(running) = &self.active else { return };
        if id == MENU_NOTES_PANEL {
            self.panel_asked = true;
        } else if id == MENU_OPEN_NOTES_FOLDER {
            let folder = running.folder.clone();
            // Starting Explorer is a process launch: keep it off the UI thread.
            thread::spawn(move || {
                if let Err(error) = std::process::Command::new("explorer.exe").arg(&folder).spawn() {
                    eprintln!("could not open the notes folder: {error}");
                }
            });
            self.log.line(hook::now(), "notes: open the notes folder");
        } else if let Some(&note) = id
            .checked_sub(MENU_NOTE_BASE)
            .and_then(|index| self.shown.get(index as usize))
        {
            running.notes.reopen(note);
            self.log.line(hook::now(), &format!("notes: reopen {note} from the menu"));
        }
    }

    /// Opens (or closes) the notes panel when "Notes…" was chosen, next to the tray icon.
    fn open_panel_if_asked(&mut self, anchor: impl FnOnce() -> Option<windows::Win32::Foundation::RECT>) {
        if !std::mem::take(&mut self.panel_asked) {
            return;
        }
        if let Some(running) = &self.active {
            running.notes.toggle_panel(anchor());
            self.log.line(hook::now(), "notes: the notes panel");
        }
    }
}

/// How the app ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The tray's Quit, a close request, Ctrl+C or Ctrl+Break, an error return or a panic:
    /// every step, each within its own bound.
    Normal,
    /// The session ends (WM_ENDSESSION) or the debug console window closes: Windows may end
    /// the process within seconds, so only what matters runs, most important first.
    SessionEnd,
}

/// The console events that ask the app to quit, and how. CTRL_LOGOFF_EVENT and
/// CTRL_SHUTDOWN_EVENT never reach a process that loaded user32; the end of a session arrives
/// as WM_ENDSESSION instead.
fn console_request(ctrl: u32) -> Option<Ending> {
    match ctrl {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => Some(Ending::Normal),
        CTRL_CLOSE_EVENT => Some(Ending::SessionEnd),
        _ => None,
    }
}

/// The UI thread, for the console handler (which Windows runs on a thread of its own).
static UI_THREAD: AtomicU32 = AtomicU32::new(0);
/// The console window is closing: the quit after the loop uses the session-end steps.
static QUIT_SHORT: AtomicBool = AtomicBool::new(false);
/// The quit path has done its work (at session end: the part that matters).
static QUIT_DONE: AtomicBool = AtomicBool::new(false);

/// Ctrl+C, Ctrl+Break and closing the console window (the debug build is a console program).
/// Only asks the UI thread to quit, then waits (bounded) for the quit path: for a close event
/// Windows ends the process as soon as this returns. FALSE leaves the event to the default
/// handler, which ends the process.
unsafe extern "system" fn console_handler(ctrl: u32) -> BOOL {
    let Some(ending) = console_request(ctrl) else {
        return false.into();
    };
    if !QUIT_DONE.load(SeqCst) {
        let thread = UI_THREAD.load(SeqCst);
        if thread == 0 {
            return false.into();
        }
        if ending == Ending::SessionEnd {
            QUIT_SHORT.store(true, SeqCst);
        }
        // SAFETY: a message without pointers to the UI thread, which lives as long as the
        // process. A WM_QUIT also ends a modal loop (the tray menu), which posts it again.
        if unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_err() {
            return false.into();
        }
    }
    let wait = match ending {
        Ending::Normal => CONSOLE_BREAK_WAIT,
        Ending::SessionEnd => CONSOLE_CLOSE_WAIT,
    };
    let until = Instant::now() + wait;
    while !QUIT_DONE.load(SeqCst) && Instant::now() < until {
        thread::sleep(Duration::from_millis(5));
    }
    QUIT_DONE.load(SeqCst).into()
}

/// Hands out what the quit path stops exactly once, to whichever caller comes first: the end
/// of the loop, WM_ENDSESSION inside the tray's window procedure, or [`QuitOnDrop`].
struct QuitOnce<T>(RefCell<Option<T>>);

#[derive(Debug, PartialEq)]
enum Claim<T> {
    First(T),
    /// The quit already ran: nothing to do.
    Done,
    /// The loop holds the parts right now (the caller runs nested inside it).
    Busy,
}

impl<T> QuitOnce<T> {
    fn new(parts: T) -> Self {
        QuitOnce(RefCell::new(Some(parts)))
    }

    /// For the loop: the parts, until the quit has taken them.
    fn get(&self) -> Option<RefMut<'_, T>> {
        RefMut::filter_map(self.0.try_borrow_mut().ok()?, Option::as_mut).ok()
    }

    fn claim(&self) -> Claim<T> {
        match self.0.try_borrow_mut() {
            Ok(mut slot) => slot.take().map_or(Claim::Done, Claim::First),
            Err(_) => Claim::Busy,
        }
    }
}

/// The workers the loop drives and the quit path stops.
struct Parts {
    settings: LiveSettings,
    settings_window: SettingsWindow,
    guide: GuideWindow,
    hotkeys: Option<Hotkeys>,
    detector: Detector,
    drops: Option<Drops>,
}

/// What session end leaves running. Windows ends it with the process; if the process lives
/// on (the session end was only a message), it is stopped after the loop.
struct Rest {
    parts: Parts,
    summoner: Option<Summoner>,
}

/// The one quit path, for every way the app ends. See [`Quit::stop_all`] and
/// [`Quit::stop_what_matters`] for the steps and their bounds.
struct Quit {
    parts: QuitOnce<Parts>,
    notes: Rc<RefCell<NotesFeature>>,
    recording: Rc<RefCell<RecordingFeature>>,
    log: RefCell<DebugLog>,
    rest: RefCell<Option<Rest>>,
}

impl Quit {
    fn line(&self, text: &str) {
        if let Ok(mut log) = self.log.try_borrow_mut() {
            log.line(hook::now(), text);
        }
    }

    /// A problem: in the debug log, or on stderr when there is none.
    fn problem(&self, text: &str) {
        if self.log.try_borrow().is_ok_and(|log| !log.enabled()) {
            eprintln!("{text}");
        }
        self.line(text);
    }

    /// Runs the quit path once; a later call does nothing.
    fn run(&self, ending: Ending) {
        let parts = match self.parts.claim() {
            Claim::First(parts) => parts,
            Claim::Done => return,
            Claim::Busy => {
                self.problem("quit: asked while the loop holds the workers; they stop after it");
                return;
            }
        };
        let started = Instant::now();
        match ending {
            Ending::Normal => {
                self.line("quit: start");
                self.stop_all(parts);
            }
            Ending::SessionEnd => {
                self.line("quit: start (session end)");
                let rest = self.stop_what_matters(parts);
                *self.rest.borrow_mut() = Some(rest);
            }
        }
        self.line(&format!("quit: done in {:.3} s", started.elapsed().as_secs_f64()));
        QUIT_DONE.store(true, SeqCst);
    }

    /// Every step, in this order. Worst cases, each for a stuck thread (a quit normally takes
    /// milliseconds): the settings window 0.95 s; the guide 0.95 s; a screen recording in progress 3 s for its
    /// file, its UI included ([`RECORDING_STOP`]; finishing a file usually takes well under
    /// a second, plus a copy for long ones); the shortcut thread 1 s; the note gesture 1 s and
    /// the notes 2 s (plus up to 8 s while a notes start is in flight); screenshot detection
    /// 1.8 s (plus up to 3.6 s while its start is in flight); then the drop router: the
    /// clipboard restore 1 s, the clipboard thread 2 s, UI Automation 1.5 s, the key watch 1 s,
    /// the Explorer lookup 1 s and the PNG saver 1 s. About 18.3 s with nothing in flight.
    fn stop_all(&self, parts: Parts) {
        let Parts { settings, settings_window, mut guide, mut hotkeys, detector, drops } = parts;
        // The settings window goes first: a shortcut being recorded in it resumes the hotkeys.
        let service = hotkeys.as_ref().map(|hotkeys| hotkeys as &dyn ShortcutService);
        settings_window.shutdown(service, &settings.snapshot.shortcuts);
        self.line("quit: settings window closed");
        if !guide.close() {
            self.problem("the guide thread did not stop within its budget");
        }
        self.line("quit: guide closed");
        // A screen recording in progress: the UI first (its input gates), then the file is
        // finished. It is not put on the pile: the app is ending.
        match self.recording.try_borrow_mut() {
            Ok(mut recording) => {
                recording.finish_now(RECORDING_STOP, hook::now());
            }
            Err(_) => self.problem("quit: the recording is in use; it stops with the process"),
        }
        self.line("quit: recording stopped");
        if let Some(hotkeys) = hotkeys.take()
            && !hotkeys.shutdown()
        {
            self.problem("the shortcut thread did not stop within its shutdown budget");
        }
        self.line("quit: shortcuts stopped");
        // The note gesture, then the notes (pending text written), then the workers they use.
        match self.notes.try_borrow_mut() {
            Ok(mut notes) => notes.stop_now(),
            Err(_) => self.problem("quit: the notes are in use; they stop with the process"),
        }
        self.line("quit: notes stopped");
        drop(detector);
        self.line("quit: screenshot detection stopped");
        // Puts the user's clipboard back if a paste left ours there, then stops the workers.
        drop(drops);
        self.line("quit: clipboard checked, workers stopped");
    }

    /// Session end: what the user would lose, most important first, within 4.0 s: the
    /// clipboard restore (0.8 s); the notes' pending text, then their window positions (1.2 s,
    /// a notes start or stop in flight included); the clipboard thread, which renders a staged
    /// screenshot we still own (2 s). Then a screen recording in progress is finished (5 s,
    /// [`END_RECORDING`]): only then is there anything to wait for, and while a recording
    /// runs the tray window holds a shutdown block reason (`ShutdownBlockReasonCreate`, set by
    /// `RecordingFeature`), so Windows shows "Saving a screen recording" and waits instead of
    /// ending the process after a few seconds. A file not finished in time stays as its
    /// `.partial.mp4`. Hooks, hotkeys and the other workers are left: Windows releases them
    /// when the process ends.
    fn stop_what_matters(&self, mut parts: Parts) -> Rest {
        if let Some(drops) = &mut parts.drops {
            drops.restore_clipboard(END_RESTORE);
        }
        self.line("quit: clipboard checked");
        let summoner = match self.notes.try_borrow_mut() {
            Ok(mut notes) => notes.stop_notes_within(END_NOTES),
            Err(_) => {
                self.problem("quit: the notes are in use; they stop with the process");
                None
            }
        };
        self.line("quit: notes stopped");
        if let Some(drops) = &mut parts.drops {
            drops.stop_clipboard();
        }
        self.line("quit: clipboard thread stopped");
        match self.recording.try_borrow_mut() {
            Ok(mut recording) => {
                recording.finish_now(END_RECORDING, hook::now());
            }
            Err(_) => self.problem("quit: the recording is in use; it stops with the process"),
        }
        self.line("quit: recording stopped");
        Rest { parts, summoner }
    }

    /// After a session end the process lived on: stop the rest in the normal order.
    fn stop_rest(&self) {
        let Some(rest) = self.rest.borrow_mut().take() else {
            return;
        };
        if let Some(summoner) = rest.summoner
            && !summoner.shutdown()
        {
            self.problem("notes: the note gesture did not stop within its budget");
        }
        self.stop_all(rest.parts);
        self.line("quit: the rest stopped");
    }
}

/// The ending of a quit after the loop: the session-end steps when the console window closes.
fn ending_after_loop() -> Ending {
    if QUIT_SHORT.load(SeqCst) { Ending::SessionEnd } else { Ending::Normal }
}

/// Runs the quit path when `main` ends, on every way out (errors and unwinding included).
/// After a normal end it finds nothing left to do.
struct QuitOnDrop(Rc<Quit>);

impl Drop for QuitOnDrop {
    fn drop(&mut self) {
        self.0.run(ending_after_loop());
        self.0.stop_rest();
    }
}

/// A label for a Windows menu: `&` would be taken for an access key, a tab for the column.
fn menu_text(text: &str) -> String {
    text.replace('&', "&&").replace('\t', " ")
}

/// The tray menu's recording section: "Record Screen" or
/// "Stop Recording" with the shortcut in the key column, and while recording the Pen and
/// Highlighter, the one in hand checked. Nothing while the capability is off.
fn recording_menu(recording: &RecordingFeature) -> Vec<MenuItem> {
    if !recording.enabled() {
        return Vec::new();
    }
    let shortcuts = recording.shortcuts();
    let keyed = |label: &str, action: Action| match shortcuts.get(action) {
        Some(chord) => format!("{label}\t{}", menu_text(&display_for_layout(&chord))),
        None => label.to_owned(),
    };
    let state = recording.state();
    let (label, enabled) = rec::menu_label(state);
    let mut items = vec![MenuItem::Action {
        id: MENU_RECORD,
        label: keyed(label, Action::ToggleRecording),
        enabled,
        checked: false,
    }];
    if state == RecordingState::Recording {
        for (id, tool, label, action) in [
            (MENU_PEN, InkTool::Pen, "Pen", Action::Pen),
            (MENU_HIGHLIGHTER, InkTool::Highlighter, "Highlighter", Action::Highlighter),
        ] {
            items.push(MenuItem::Action {
                id,
                label: keyed(label, action),
                enabled: true,
                checked: recording.tool() == Some(tool),
            });
        }
    }
    items
}

/// Once the hotkey thread registered the Pen and Highlighter for this recording: logs the
/// ones another program holds (they still work from the tray). False while it has not yet.
fn check_pen_keys(hotkeys: &Hotkeys, recording: &mut RecordingFeature, now: f64) -> bool {
    let status = hotkeys.status();
    let pending = status
        .iter()
        .any(|(action, state)| action.recording_only() && *state == Registration::WhileRecording);
    if pending {
        return false;
    }
    for (action, state) in status.iter().filter(|(action, _)| action.recording_only()) {
        match state {
            Registration::Active => {}
            Registration::Taken => recording.log_line(
                now,
                &format!("{action:?} shortcut is held by another program; use the tray menu"),
            ),
            other => recording.log_line(now, &format!("{action:?} shortcut: {other:?}")),
        }
    }
    true
}

/// A hint as a notice sentence: capitalized, with a full stop.
fn sentence(hint: &str) -> String {
    let mut chars = hint.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// The overlay's short form of a notes error: the part before the reason, in the hint style.
fn overlay_hint(message: &str) -> String {
    let short = message.split(':').next().unwrap_or(message).trim().trim_end_matches('.');
    let mut chars = short.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The named mutex needs only to exist. Request no access so either build can detect the
/// other across integrity levels. Windows removes the name when the last handle closes.
fn single_instance() -> windows::core::Result<Option<OwnedHandle>> {
    // SAFETY: the name is static and the non-inheritable handle is newly owned by this call.
    let (handle, already_running) = unsafe {
        SetLastError(ERROR_SUCCESS);
        let handle = CreateMutexExW(None, w!(r"Local\Igorio.SingleInstance"), 0, 0)?;
        let already_running = GetLastError() == ERROR_ALREADY_EXISTS;
        (OwnedHandle::from_raw_handle(handle.0), already_running)
    };
    Ok((!already_running).then_some(handle))
}

fn main() -> windows::core::Result<()> {
    let Some(_instance) = single_instance()? else {
        DebugLog::from_env().line(hook::now(), "Igorio is already running in this session");
        // SAFETY: the debug message is a static, terminated string.
        unsafe { OutputDebugStringW(w!("Igorio is already running in this session\n")) };
        return Ok(());
    };
    // First, so a panic or a fault anywhere below leaves a record in crash.log.
    igorio_app::crash::install();
    overlay::ensure_dpi_awareness();
    let settings_path = settings::default_settings_path();
    let loaded = Settings::load(&settings_path);
    let mut settings = loaded.as_ref().cloned().unwrap_or_default();
    // Before anything creates the default notes folder. Saved only when the file was read.
    if settings.keep_old_notes_folder(&DefaultFolders::from_system()) && loaded.is_ok() {
        let _ = settings.save(&settings_path);
    }
    let settings = LiveSettings::from(settings);
    // Note window positions live next to the settings.
    let notes = Rc::new(RefCell::new(NotesFeature::new(
        settings_path.with_file_name("note-windows.json"),
    )));
    let recording = Rc::new(RefCell::new(RecordingFeature::new(FRAME)));

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64 | 1);
    let mut overlay = Overlay::new()?;
    if std::env::var_os("IGORIO_CAPTURABLE").is_some_and(|v| v == "1") {
        overlay.set_capturable(true)?;
    }
    let app = Rc::new(RefCell::new(App {
        state: IgorioState::new(seed),
        shake: ShakeGate::new(),
        follower: Follower::new(),
        renderer: Renderer::new(),
        overlay,
        pixmap: None,
        origin: (0, 0),
        scale: 0.0,
        last_frame: hook::now(),
        dirty: false,
        was_animating: false,
        demo_count: 0,
        tried: Vec::new(),
    }));
    app.borrow_mut().apply_card_size(settings.card_size, hook::now());
    for _ in 0..demo_count_from_args() {
        app.borrow_mut().add_demo();
    }

    // The tray calls these from inside `DispatchMessageW`, when `app` is not borrowed.
    let menu_app = Rc::clone(&app);
    let action_app = Rc::clone(&app);
    let open_settings = Rc::new(Cell::new(false));
    let action_settings = Rc::clone(&open_settings);
    let open_guide = Rc::new(Cell::new(false));
    let action_guide = Rc::clone(&open_guide);
    let menu_notes = Rc::clone(&notes);
    let action_notes = Rc::clone(&notes);
    let menu_recording = Rc::clone(&recording);
    let action_recording = Rc::clone(&recording);
    let tray = Tray::new(
        "Igorio",
        Box::new(move || {
            let can_restore = menu_app.borrow().state.can_restore();
            let mut items = vec![
                MenuItem::Action {
                    id: MENU_RESTORE,
                    label: "Restore Last Screenshot".into(),
                    enabled: can_restore,
                    checked: false,
                },
                MenuItem::action(MENU_ADD_DEMO, "Add Demo Screenshot"),
            ];
            let section = menu_notes.borrow_mut().menu_section();
            if !section.is_empty() {
                items.push(MenuItem::Separator);
                items.extend(section);
            }
            let section = recording_menu(&menu_recording.borrow());
            if !section.is_empty() {
                items.push(MenuItem::Separator);
                items.extend(section);
            }
            items.extend([
                MenuItem::Separator,
                MenuItem::action(MENU_GUIDE, "Guide…"),
                MenuItem::action(MENU_SETTINGS, "Settings…"),
                MenuItem::action(MENU_QUIT, "Quit"),
            ]);
            items
        }),
        Box::new(move |id| match id {
            MENU_RESTORE => action_app.borrow_mut().restore_last(),
            MENU_ADD_DEMO => action_app.borrow_mut().add_demo(),
            MENU_SETTINGS => action_settings.set(true),
            MENU_GUIDE => action_guide.set(true),
            // SAFETY: posts WM_QUIT to this thread's own queue.
            MENU_QUIT => unsafe { PostQuitMessage(0) },
            // The loop's pass right after this message starts or stops it.
            MENU_RECORD => action_recording.borrow_mut().request_toggle(),
            MENU_PEN => action_recording.borrow_mut().tool_shortcut(InkTool::Pen, hook::now()),
            MENU_HIGHLIGHTER => {
                action_recording.borrow_mut().tool_shortcut(InkTool::Highlighter, hook::now());
            }
            id => action_notes.borrow_mut().menu_action(id),
        }),
    )?;
    recording.borrow_mut().set_window(tray.hwnd());

    let mut mouse_hook = MouseHook::start()?;
    let mut events = Vec::new();
    let sleeper = Sleeper::new()?;

    // The drop router and its workers (UI Automation, clipboard). Without them the app
    // still carries, shakes and restores. Stopping it (in the quit path, or by dropping it on
    // an early error return) puts the user's clipboard back if a paste left ours there.
    let drops = match Drops::start(settings.snapshot.clone(), DebugLog::from_env()) {
        Ok(drops) => Some(drops),
        Err(why) => {
            eprintln!("double-click drops are off: {why}");
            None
        }
    };
    let settings_window = SettingsWindow::new(settings_path)?;
    // The first-run guide, until it was shown once; then from the tray's "Guide…".
    let mut guide = GuideWindow::new();
    if !settings.snapshot.guide_seen
        && let Err(error) = guide.open(&settings.snapshot, app.borrow().state.is_holding())
    {
        eprintln!("could not open the guide: {error}");
    }
    let hotkeys = match Hotkeys::start(&settings.snapshot.shortcuts) {
        Ok(hotkeys) => Some(hotkeys),
        Err(error) => {
            eprintln!("keyboard shortcuts are off: {error}");
            None
        }
    };
    let mut presses = Vec::new();
    let mut snip_events = Vec::new();
    let mut wakes = vec![mouse_hook.wake_handle(), settings_window.wake_handle()];
    if let Some(hotkeys) = &hotkeys {
        wakes.push(hotkeys.wake_handle());
    }
    if let Some(drops) = &drops {
        wakes.extend(drops.wake_handles());
    }
    let permanent_wakes = wakes.len();
    // From here every way out of `main` runs the one quit path (see `Quit` for the order).
    let quit = Rc::new(Quit {
        parts: QuitOnce::new(Parts {
            settings,
            settings_window,
            guide,
            hotkeys,
            detector: Detector::default(),
            drops,
        }),
        notes: Rc::clone(&notes),
        recording: Rc::clone(&recording),
        log: RefCell::new(DebugLog::from_env()),
        rest: RefCell::new(None),
    });
    let _quit_on_drop = QuitOnDrop(Rc::clone(&quit));
    let end_quit = Rc::clone(&quit);
    tray.on_end(Box::new(move |request| match request {
        // SAFETY: posts WM_QUIT to this thread's own queue.
        EndRequest::Close => unsafe { PostQuitMessage(0) },
        EndRequest::QueryEndSession => end_quit.line("quit: Windows asks to end the session: yes"),
        EndRequest::EndSession => {
            // Windows may end the process as soon as this returns, so the quit runs here. If
            // the process lives on, the loop ends and stops the rest.
            end_quit.run(Ending::SessionEnd);
            // SAFETY: posts WM_QUIT to this thread's own queue.
            unsafe { PostQuitMessage(0) };
        }
    }));
    // SAFETY: no arguments.
    UI_THREAD.store(unsafe { GetCurrentThreadId() }, SeqCst);
    // SAFETY: `console_handler` is a plain function that lives as long as the process; it only
    // reads atomics and posts a message. Without a console (the release build) it never runs.
    if let Err(error) = unsafe { SetConsoleCtrlHandler(Some(console_handler), true) } {
        eprintln!("console events are not handled: {error}");
    }
    // When the next frame runs; `None` waits for a mouse move or a menu action.
    let mut due = Some(hook::now());
    let mut next_health_check = hook::now() + HEALTH_CHECK;
    // The Pen and Highlighter registrations of this recording were looked at.
    let mut pen_keys_checked = false;
    // For the guide: the pastes and saves counted so far, and whether a recording ran.
    let mut deliveries = 0;
    let mut was_recording = false;

    while pump_messages() {
        // None once the quit ran inside the window procedure (WM_ENDSESSION).
        let Some(mut parts) = quit.parts.get() else {
            break;
        };
        let Parts { settings, settings_window, guide, hotkeys, detector, drops } = &mut *parts;
        let now = hook::now();
        if let Some(hotkeys) = &hotkeys {
            hotkeys.drain(&mut presses);
        }
        for pressed in presses.drain(..) {
            match pressed.action {
                Action::OpenSettings => open_settings.set(true),
                Action::NewNote => notes.borrow_mut().summon_at_cursor(),
                Action::ToggleRecording => recording.borrow_mut().request_toggle(),
                Action::Pen => recording.borrow_mut().tool_shortcut(InkTool::Pen, now),
                Action::Highlighter => recording.borrow_mut().tool_shortcut(InkTool::Highlighter, now),
                action => app.borrow_mut().shortcut(action, now),
            }
        }
        notes.borrow_mut().open_panel_if_asked(|| tray.icon_rect().ok());
        if open_settings.replace(false) && let Err(error) = settings_window.open() {
            eprintln!("could not queue Settings: {error}");
        }
        if open_guide.replace(false)
            && let Err(error) = guide.open(&settings.snapshot, app.borrow().state.is_holding())
        {
            eprintln!("could not open the guide: {error}");
        }
        // The guide is on screen for the first time: it is not opened on start again.
        if guide.take_shown() && !settings.snapshot.guide_seen {
            settings_window.note_guide_seen();
        }
        // Also carries the shortcut recorder's pause, resume and registration checks.
        let service = hotkeys.as_ref().map(|hotkeys| hotkeys as &dyn ShortcutService);
        for saved in settings_window.saved(service, &settings.snapshot.shortcuts) {
            *settings = saved.into();
            if let Some(drops) = drops.as_mut() {
                drops.set_settings(&settings.snapshot);
            }
            let mut app = app.borrow_mut();
            app.apply_card_size(settings.card_size, now);
            if !settings.screenshot_detector {
                detector.stop();
                if app.state.hidden_by(Hidden::CaptureUi) {
                    app.capture_ui(false, now);
                }
            }
        }
        detector.update(settings.screenshot_detector, drops.as_ref(), now);
        notes
            .borrow_mut()
            .update(settings.notes, &settings.snapshot, drops.as_mut(), now);
        mouse_hook.drain(&mut events);
        // Before the capture rule below empties the batch: the gesture keeps its button state,
        // the recording UI its cursor for the next Follow.
        notes.borrow().feed(&events);
        recording.borrow().feed(&events);
        if let Some(snip) = &detector.active {
            snip.set_carry_copies(settings.snapshot.carry_copied_images);
            snip.drain(&mut snip_events);
        }
        let recorded = recording.borrow_mut().pass(settings.recording, &settings.snapshot, now);
        let recording_due = recorded.due;
        if recorded.start_hint_shown {
            settings_window.note_recording_hint_shown();
        }
        let facts = {
            let mut recording = recording.borrow_mut();
            let pen_keys = recording.pen_keys();
            if let Some(hotkeys) = &hotkeys {
                hotkeys.set_recording(pen_keys);
                if pen_keys && !pen_keys_checked {
                    pen_keys_checked = check_pen_keys(hotkeys, &mut recording, now);
                }
            }
            pen_keys_checked &= pen_keys;
            let started = recording.state() == RecordingState::Recording;
            if started && !was_recording {
                guide.tried(Tried::RecordingStarted);
            }
            was_recording = started;
            RecordingFacts {
                enabled: settings.recording,
                running: recording.state() != RecordingState::Idle,
                exclusive: recording.exclusive(),
            }
        };
        let mut router_due = None;
        {
            let mut app = app.borrow_mut();
            let app = &mut *app;
            if let Some(finished) = recorded.carry {
                app.carry_recording(finished, now);
            }
            if let Some(hint) = recorded.hint {
                if app.state.is_holding() {
                    app.state.flash_hint(&hint, 2.5, now);
                    app.dirty = true;
                } else if let Err(error) = tray.notice("Igorio", &sentence(&hint)) {
                    eprintln!("{hint} ({error})");
                }
            }
            // While a pen is in hand the pile steps aside, like for the screenshot UI.
            app.hide(Hidden::Drawing, recording.borrow().drawing(), now);
            // Treat the whole batch as capture input if visibility changed during
            // it. A shown/gone pair can arrive together with the selection clicks.
            if app.screenshots(&mut snip_events, &settings.snapshot) || app.state.hidden_for_capture() {
                events.clear();
            }
            let moved = app.mouse(&events);
            if let Some(drops) = drops.as_mut() {
                if drops.run(&events, &mut app.state, now) {
                    app.dirty = true;
                }
                router_due = drops.next_deadline();
            }
            if notes.borrow_mut().pass(&mut app.state, drops.as_mut(), facts, now) {
                app.dirty = true;
                router_due = drops.as_ref().and_then(Drops::next_deadline);
            }
            // What the guide's "Try it now" lines wait for.
            if std::mem::take(&mut notes.borrow_mut().menu_opened) {
                app.tried.push(Tried::MenuOpened);
            }
            if app.state.deliveries() != deliveries {
                deliveries = app.state.deliveries();
                app.tried.push(Tried::Dropped);
            }
            for tried in app.tried.drain(..) {
                guide.tried(tried);
            }
            guide.set_holding(app.state.is_holding());
            events.clear();
            if app.needs_frame() {
                due = Some(now);
            } else if moved && app.pile_visible() {
                // The pile follows the cursor, but not faster than the frame rate.
                due = earlier(due, Some((app.last_frame + FRAME).max(now)));
            }
            if due.is_some_and(|at| at <= now) {
                due = app.frame(now)?;
            }
        }
        if now >= next_health_check {
            next_health_check = now + HEALTH_CHECK;
            if !mouse_hook.is_alive() || mouse_hook.seems_stalled() {
                // On failure the next check tries again.
                let _ = mouse_hook.restart();
            }
            if let Some(snip) = &mut detector.active
                && (!snip.is_alive() || snip.seems_stalled())
            {
                let mut app = app.borrow_mut();
                if app.state.hidden_by(Hidden::CaptureUi) {
                    app.capture_ui(false, now);
                    due = Some(now);
                }
                detector.stop();
            }
            notes.borrow_mut().health(now);
        }
        wakes.truncate(permanent_wakes);
        if let Some(snip) = &detector.active {
            wakes.push(snip.wake_handle());
        }
        notes.borrow().wakes(&mut wakes);
        recording.borrow().wakes(&mut wakes);
        // The tool menu's Record screen: started by the next pass, at once.
        let record_due = std::mem::take(&mut notes.borrow_mut().record).then(|| {
            recording.borrow_mut().request_toggle();
            now
        });
        let transition_due = (detector.transition.is_some()
            || notes.borrow().transition.is_some())
        .then_some(now + FRAME);
        let transition_due = earlier(transition_due, earlier(recording_due, record_due));
        let until = earlier(earlier(due, router_due), earlier(transition_due, Some(next_health_check)))
            .unwrap_or(next_health_check);
        let wait = Duration::from_secs_f64((until - hook::now()).max(0.0));
        sleeper.wait(wait, &wakes)?;
    }
    quit.run(ending_after_loop());
    quit.stop_rest();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::time::Instant;

    use igorio_app::notes::store::{NoteOrigin, Trash};
    use igorio_app::notes::window::TestOptions;

    use super::*;

    /// Tests never touch the Recycle Bin.
    struct NoTrash;

    impl Trash for NoTrash {
        fn trash(&self, _path: &Path) -> io::Result<()> {
            Err(io::Error::other("no Recycle Bin in tests"))
        }
    }

    /// A notes thread in a scratch folder with one new note holding typed, unsaved text.
    fn running_with_typed_note(root: &Path, text: &str) -> NotesRunning {
        let folder = root.join("Notes");
        let notes = Notes::start(NotesConfig {
            folder: folder.clone(),
            windows_file: root.join("note-windows.json"),
            trash: Box::new(NoTrash),
            theme: Theme::Light,
            restore: false,
            test: Some(TestOptions { dpi: Some(96), motion: Some(false), no_activate: true }),
        })
        .expect("notes thread");
        notes.open_new(Point::new(240.0, 240.0), NoteOrigin::desktop());
        let until = Instant::now() + Duration::from_secs(5);
        let id = loop {
            if let Some(&id) = notes.test_open_ids().first() {
                break id;
            }
            assert!(Instant::now() < until, "the note did not open");
            thread::sleep(Duration::from_millis(10));
        };
        let text = text.to_owned();
        // Keys sent to the editor control itself (window messages, not system input). The
        // store writes 0.5 s later, so right after this the text is only in memory.
        notes.test_with(id, move |note| {
            let editor = note.editor();
            for ch in text.chars() {
                editor.send_key(ch.to_ascii_uppercase() as u16, &ch.to_string());
            }
        });
        NotesRunning { summoner: None, notes, folder, chosen: None }
    }

    fn on_disk(folder: &Path, text: &str) -> bool {
        fs::read_dir(folder).is_ok_and(|entries| {
            entries
                .flatten()
                .filter_map(|entry| fs::read_to_string(entry.path()).ok())
                .any(|file| file.contains(text))
        })
    }

    /// The two ways the quit path stops the notes: every step (`stop_now`, the tray's Quit,
    /// errors and panics) and the session-end steps (`stop_notes_within`). Both write a note's
    /// unsaved text before the workers stop. (The gesture is not started here: it needs a
    /// keyboard hook and UI Automation; `NotesRunning::stop` shuts it down before the notes by
    /// construction.)
    #[test]
    fn quitting_writes_unsaved_note_text_on_every_way_out() {
        let root = std::env::temp_dir().join(format!("igorio-main-notes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let normal = root.join("normal");
        let mut feature = NotesFeature::new(normal.join("note-windows.json"));
        feature.active = Some(running_with_typed_note(&normal, "quit"));
        assert!(!on_disk(&normal.join("Notes"), "quit"), "written before the quit");
        feature.stop_now();
        assert!(feature.active.is_none());
        assert!(on_disk(&normal.join("Notes"), "quit"));

        let ended = root.join("ended");
        let mut feature = NotesFeature::new(ended.join("note-windows.json"));
        feature.active = Some(running_with_typed_note(&ended, "end"));
        let started = Instant::now();
        assert!(feature.stop_notes_within(END_NOTES).is_none(), "no gesture was started");
        assert!(started.elapsed() <= END_NOTES + Duration::from_millis(100));
        assert!(feature.active.is_none());
        assert!(on_disk(&ended.join("Notes"), "end"));
        assert!(ended.join("note-windows.json").exists(), "window positions written");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_quit_runs_once_whoever_asks_first() {
        let once = QuitOnce::new(7);
        *once.get().expect("before the quit") += 1;
        {
            // The loop holds the parts: a nested caller cannot take them.
            let _held = once.get();
            assert_eq!(once.claim(), Claim::Busy);
        }
        assert_eq!(once.claim(), Claim::First(8));
        assert_eq!(once.claim(), Claim::Done);
        assert_eq!(once.claim(), Claim::Done);
        assert!(once.get().is_none(), "the loop ends after the quit");
    }

    #[test]
    fn messages_and_console_events_that_quit() {
        assert!(ends_loop(WM_QUIT));
        assert!(ends_loop(WM_CLOSE));
        assert!(!ends_loop(windows::Win32::UI::WindowsAndMessaging::WM_NULL));
        assert_eq!(console_request(CTRL_C_EVENT), Some(Ending::Normal));
        assert_eq!(console_request(CTRL_BREAK_EVENT), Some(Ending::Normal));
        assert_eq!(console_request(CTRL_CLOSE_EVENT), Some(Ending::SessionEnd));
        // CTRL_LOGOFF_EVENT and CTRL_SHUTDOWN_EVENT: the session end comes as WM_ENDSESSION.
        assert_eq!(console_request(5), None);
        assert_eq!(console_request(6), None);
        // The session-end steps fit in Windows' few seconds.
        let clipboard_thread_stop = Duration::from_secs(2);
        assert!(END_RESTORE + END_NOTES + clipboard_thread_stop <= Duration::from_secs(4));
        assert!(END_RESTORE + END_NOTES + clipboard_thread_stop < CONSOLE_CLOSE_WAIT);
    }

    #[test]
    fn menu_labels_and_overlay_hints() {
        assert_eq!(menu_text("Tom & Jerry\tnotes"), "Tom && Jerry notes");
        assert_eq!(
            overlay_hint("A screenshot could not be added to the note: access denied"),
            "a screenshot could not be added to the note"
        );
        assert_eq!(
            overlay_hint("The note's file could not be moved to the Recycle Bin."),
            "the note's file could not be moved to the Recycle Bin"
        );
    }
}
