//! Note windows: every open note, on one notes thread with its own message loop.
//!
//! Ported from `NotesController.swift` and `NoteView.swift`. The app's UI thread talks to the
//! notes thread through [`Notes`]: commands go in through a channel (never waiting), events come
//! back through another channel with a wake handle, and [`DropZones`] answers "which note is
//! under this point" from a snapshot the UI thread reads without asking the notes thread.
//!
//! The notes thread owns the [`NoteStore`] and drives its debounce with a timer that runs only
//! while a write is pending. Each note is a borderless, resizable, always-on-top tool window
//! (the Swift panel is a borderless non-activating floating panel that never shows in the Dock
//! or the app switcher: here no taskbar button and no Alt+Tab entry), with DWM rounded corners
//! and shadow, a header (origin chip, delete, put away), the editor, and a footer (when it was
//! edited, a resize grip).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use igorio_core::geom::{CardId, Point, Rect};
use igorio_kit::held::HeldCard;
use windows::Win32::Foundation::{
    COLORREF, CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::Controls::{
    ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, MARGINS, TOOLTIPS_CLASSW,
    TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_SETMAXTIPWIDTH, TTS_ALWAYSTIP, TTS_NOPREFIX,
    TTTOOLINFOW, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_RETURN,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use super::panel::{self, NoteState, Panel, PanelNote, PanelRequest};
use super::editor::{
    self, Editor, EditorConfig, EditorEvent, GdiFonts, Palette, Surface, Transition, mix, px,
};
use super::store::{Note, NoteId, NoteKind, NoteOrigin, NoteStore, Trash, seed_bytes};
use crate::overlay::{cursor_pos, monitor_at};

// ---------------------------------------------------------------------------
// Layout constants (logical pixels), from `NoteLayout` in NoteView.swift
// ---------------------------------------------------------------------------

/// A new note's card.
pub const NOTE_SIZE: (f64, f64) = (304.0, 224.0);
pub const MIN_NOTE_SIZE: (f64, f64) = (190.0, 120.0);
/// The summon pill a new note grows out of.
pub const SUMMON_SIZE: (f64, f64) = (46.0, 34.0);
const HEADER: f64 = 32.0;
const FOOTER: f64 = 26.0;
const EDGE: f64 = 6.0;
const BUTTON: f64 = 20.0;
const GRIP: f64 = 18.0;
/// What the overlay says over a note while the fairy carries screenshots (Swift `DropZone`).
pub const DROP_HINT: &str = "double-click to add to note";

/// Control ids inside a note window.
pub const ORIGIN_ID: i32 = 101;
pub const DELETE_ID: i32 = 102;
pub const CLOSE_ID: i32 = 103;
pub const EDITOR_ID: i32 = 104;
pub const HIDE_ID: i32 = 105;

const NOTE_CLASS: PCWSTR = w!("IgorioNote");
const HUB_CLASS: PCWSTR = w!("IgorioNotesThread");
const COMMAND: u32 = WM_APP + 1;
const EDITOR_EVENTS: u32 = WM_APP + 2;
const TEST_RUN: u32 = WM_APP + 3;
const SAVE_TIMER: usize = 1;
const MOTION_TIMER: usize = 2;
const WATCHDOG_TIMER: usize = 3;
const RETRY_TIMER: usize = 4;
const APPEAR_SECONDS: f64 = 0.45;
const FADE_OUT_MS: f64 = 120.0;
const BUTTONS_MS: f64 = 120.0;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Light, dark, or whatever Windows uses for apps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// How the notes thread starts.
pub struct NotesConfig {
    /// The notes folder (one `.md` file per note).
    pub folder: PathBuf,
    /// Where window positions are kept (next to the app's settings).
    pub windows_file: PathBuf,
    /// The Recycle Bin, or a fake in tests.
    pub trash: Box<dyn Trash + Send>,
    pub theme: Theme,
    /// Reopen the notes that were on screen when the app last quit (Swift `restore`).
    pub restore: bool,
    #[doc(hidden)]
    pub test: Option<TestOptions>,
}

/// Test-only switches.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct TestOptions {
    /// Draw at this DPI instead of the monitor's.
    pub dpi: Option<u32>,
    /// Force motion on or off (otherwise the system setting).
    pub motion: Option<bool>,
    /// Show windows without activating them (leaves the user's foreground window alone).
    pub no_activate: bool,
}

/// A note in the recent-notes menu (Swift `appendMenuItems`).
#[derive(Clone, Debug, PartialEq)]
pub struct RecentNote {
    pub id: NoteId,
    pub title: String,
    /// The origin chip's label ("Desktop", "github.com").
    pub label: String,
    /// The longer origin text for a tooltip.
    pub detail: String,
    pub updated_at: SystemTime,
    pub is_open: bool,
    /// Hidden with the header's hide button (or from the notes panel).
    pub is_hidden: bool,
}

/// What the notes thread tells the UI thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// These carried cards went into a note: the pile lets go of them. The text is the
    /// Swift hint ("added to note", "added 2 to note").
    CardsTaken { ids: Vec<CardId>, hint: String },
    /// The recent notes changed, most recently edited first.
    Recent(Vec<RecentNote>),
    /// Something failed that the user should hear about.
    Error(String),
}

/// One open note's window, as the UI thread sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct NoteZone {
    pub id: NoteId,
    /// The card in physical pixels.
    pub rect: Rect,
    /// The window handle as a number (for tests and the paste router's own-window check).
    pub hwnd: isize,
    /// The note that was active last.
    pub active: bool,
}

/// The note windows' screen rectangles, readable from any thread without waiting on the notes
/// thread. The paste router asks it whether a double-click belongs to a note.
#[derive(Clone, Default)]
pub struct DropZones {
    zones: Arc<RwLock<Vec<NoteZone>>>,
}

impl DropZones {
    /// The note under a screen point (physical pixels), topmost first.
    pub fn note_at(&self, point: Point) -> Option<NoteId> {
        let zones = self.zones.read().ok()?;
        zones.iter().find(|zone| zone.rect.contains(point)).map(|zone| zone.id)
    }

    pub fn zones(&self) -> Vec<NoteZone> {
        self.zones.read().map(|zones| zones.clone()).unwrap_or_default()
    }

    fn set(&self, zones: Vec<NoteZone>) {
        if let Ok(mut current) = self.zones.write() {
            *current = zones;
        }
    }
}

enum Command {
    OpenNew {
        at: Point,
        origin: NoteOrigin,
    },
    Reopen(NoteId),
    PutAway(NoteId),
    PutAwayAll,
    Delete(NoteId),
    GiveCards {
        at: Point,
        cards: Vec<HeldCard>,
    },
    SetFolder(PathBuf),
    SetTheme(Theme),
    Refresh,
    Shutdown,
    /// Internal: a closed window finished fading out.
    Reap(NoteId),
    Hide(NoteId),
    /// Opens the notes panel next to the tray icon (`None`: the tray's corner), or closes it.
    TogglePanel(Option<RECT>),
    /// What the panel asked for (it runs on this thread; acting later keeps it out of its own
    /// message handling).
    Panel(PanelRequest),
}

struct Wake(HANDLE);

// SAFETY: a kernel event can be signalled and waited on from any thread.
unsafe impl Send for Wake {}
// SAFETY: as above; the Arc keeps it open while either side uses it.
unsafe impl Sync for Wake {}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: this owns the event handle and closes it once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl Wake {
    fn signal(&self) {
        // SAFETY: the Arc keeps the event alive during the call.
        let _ = unsafe { SetEvent(self.0) };
    }
}

struct Shared {
    hwnd: AtomicIsize,
    done: AtomicBool,
}

/// The notes module, used from the app's UI thread. Every method returns at once.
pub struct Notes {
    commands: Sender<Command>,
    events: Receiver<Event>,
    wake: Arc<Wake>,
    zones: DropZones,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Notes {
    /// Starts the notes thread. Returns once its message window exists (a few milliseconds).
    pub fn start(config: NotesConfig) -> io::Result<Notes> {
        // SAFETY: an unnamed auto-reset event, owned by Wake.
        let wake = Arc::new(Wake(
            unsafe { CreateEventW(None, false, false, None) }.map_err(io::Error::other)?,
        ));
        let (commands, receiver) = mpsc::channel();
        let (event_sender, events) = mpsc::channel();
        let zones = DropZones::default();
        let shared = Arc::new(Shared { hwnd: AtomicIsize::new(0), done: AtomicBool::new(false) });
        let (ready_sender, ready) = mpsc::channel();
        let thread = {
            let wake = wake.clone();
            let zones = zones.clone();
            let shared = shared.clone();
            let commands = commands.clone();
            thread::Builder::new().name("notes".into()).spawn(move || {
                let result = run(
                    config,
                    receiver,
                    commands,
                    event_sender,
                    wake,
                    zones,
                    &shared,
                    &ready_sender,
                );
                if let Err(error) = result {
                    let _ = ready_sender.send(Err(error.to_string()));
                }
                shared.hwnd.store(0, SeqCst);
                shared.done.store(true, SeqCst);
            })?
        };
        match ready.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(message)) => return Err(io::Error::other(message)),
            Err(_) => return Err(io::Error::other("the notes thread did not start")),
        }
        Ok(Notes { commands, events, wake, zones, shared, thread: Some(thread) })
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            let hwnd = self.shared.hwnd.load(SeqCst);
            if hwnd != 0 {
                // SAFETY: a scalar message to the notes thread's message window.
                let _ = unsafe {
                    PostMessageW(Some(HWND(hwnd as *mut c_void)), COMMAND, WPARAM(0), LPARAM(0))
                };
            }
        }
    }

    /// A new note grown out of the summon point `at` (physical pixels), written where `origin`
    /// says. It takes focus with the caret in the editor.
    pub fn open_new(&self, at: Point, origin: NoteOrigin) {
        self.send(Command::OpenNew { at, origin });
    }

    /// Shows a note from the recent list (or brings it forward and focuses it).
    pub fn reopen(&self, id: NoteId) {
        self.send(Command::Reopen(id));
    }

    /// Hides a note: saved first, then off screen until it is shown from the notes panel or the
    /// menu, also after a restart.
    pub fn hide(&self, id: NoteId) {
        self.send(Command::Hide(id));
    }

    /// Opens the notes panel next to the tray icon at `anchor` (physical pixels; `None` when
    /// the icon is in the overflow area), or closes it when it is open.
    pub fn toggle_panel(&self, anchor: Option<RECT>) {
        self.send(Command::TogglePanel(anchor));
    }

    /// Runs `f` on the notes thread with the open panel and waits for it (tests only).
    #[doc(hidden)]
    pub fn test_panel<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Panel) -> R + Send + 'static,
    ) -> Option<R> {
        let result = Arc::new(Mutex::new(None));
        let out = result.clone();
        self.run_test(Box::new(move |hub: &Hub| {
            if let Some(panel) = hub.panel.borrow().as_ref() {
                *out.lock().expect("test result") = Some(f(panel));
            }
        }));
        result.lock().ok()?.take()
    }

    /// Takes a note off screen; a blank note is discarded (Swift `putAway`).
    pub fn put_away(&self, id: NoteId) {
        self.send(Command::PutAway(id));
    }

    /// Takes every note off screen but remembers which were up (Swift `hideAll`).
    pub fn put_away_all(&self) {
        self.send(Command::PutAwayAll);
    }

    /// Deletes a note: its file goes to the Recycle Bin.
    pub fn delete(&self, id: NoteId) {
        self.send(Command::Delete(id));
    }

    /// The pile's cards for the note under `at` (a double-click there): each becomes a file in
    /// the notes folder's `attachments` and a Markdown link at the click. The taken ids come
    /// back as [`Event::CardsTaken`]; nothing comes back when no note is there.
    pub fn give_cards(&self, at: Point, cards: Vec<HeldCard>) {
        self.send(Command::GiveCards { at, cards });
    }

    /// Another notes folder: this one's notes are put away, the new one's open notes come up.
    pub fn set_folder(&self, folder: PathBuf) {
        self.send(Command::SetFolder(folder));
    }

    pub fn set_theme(&self, theme: Theme) {
        self.send(Command::SetTheme(theme));
    }

    /// Picks up files changed in Explorer (call when the menu opens, as Swift does); answers
    /// with [`Event::Recent`].
    pub fn refresh(&self) {
        self.send(Command::Refresh);
    }

    /// Events since the last call, oldest first. Call when [`Notes::wake_handle`] is signalled.
    pub fn events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    /// An auto-reset event signalled when events are waiting.
    pub fn wake_handle(&self) -> HANDLE {
        self.wake.0
    }

    /// The note rectangles for the paste router and the overlay's drop hint.
    pub fn drop_zones(&self) -> DropZones {
        self.zones.clone()
    }

    /// Saves everything, closes the windows and ends the thread. True when it ended within
    /// `within`; otherwise the thread is left to finish on its own.
    pub fn shutdown(mut self, within: Duration) -> bool {
        self.stop(within)
    }

    fn stop(&mut self, within: Duration) -> bool {
        let Some(thread) = self.thread.take() else { return true };
        self.send(Command::Shutdown);
        let until = Instant::now() + within;
        while !self.shared.done.load(SeqCst) && Instant::now() < until {
            thread::sleep(Duration::from_millis(2));
        }
        if self.shared.done.load(SeqCst) {
            let _ = thread.join();
            true
        } else {
            false
        }
    }

    /// Runs `f` on the notes thread with the note's window and waits for it (tests only).
    #[doc(hidden)]
    pub fn test_with<R: Send + 'static>(
        &self,
        id: NoteId,
        f: impl FnOnce(&TestNote<'_>) -> R + Send + 'static,
    ) -> Option<R> {
        let result = Arc::new(Mutex::new(None));
        let out = result.clone();
        let job: TestJob = Box::new(move |hub: &Hub| {
            let window = hub.window(id);
            if let Some(window) = window {
                let value = f(&TestNote { window: &window, hub });
                *out.lock().expect("test result") = Some(value);
            }
        });
        self.run_test(job);
        result.lock().ok()?.take()
    }

    /// Is the notes thread idle: no save pending, no animation timer anywhere (tests only)?
    #[doc(hidden)]
    pub fn test_idle(&self) -> bool {
        let result = Arc::new(Mutex::new(false));
        let out = result.clone();
        self.run_test(Box::new(move |hub: &Hub| {
            *out.lock().expect("test result") = hub.idle();
        }));
        *result.lock().expect("test result")
    }

    /// Ids of the open notes in creation order (tests only).
    #[doc(hidden)]
    pub fn test_open_ids(&self) -> Vec<NoteId> {
        let result = Arc::new(Mutex::new(Vec::new()));
        let out = result.clone();
        self.run_test(Box::new(move |hub: &Hub| {
            *out.lock().expect("test result") = hub.windows.borrow().keys().copied().collect();
        }));
        result.lock().map(|ids| ids.clone()).unwrap_or_default()
    }

    fn run_test(&self, job: TestJob) {
        let hwnd = self.shared.hwnd.load(SeqCst);
        if hwnd == 0 {
            return;
        }
        let boxed: Box<TestJob> = Box::new(job);
        let pointer = Box::into_raw(boxed);
        // SAFETY: a synchronous cross-thread send; the notes thread takes the box back and runs
        // it before SendMessage returns.
        unsafe {
            SendMessageW(
                HWND(hwnd as *mut c_void),
                TEST_RUN,
                Some(WPARAM(pointer as usize)),
                Some(LPARAM(0)),
            )
        };
    }
}

impl Drop for Notes {
    fn drop(&mut self) {
        self.stop(Duration::from_secs(1));
    }
}

type TestJob = Box<dyn FnOnce(&Hub) + Send>;

/// A note window seen from a test closure on the notes thread.
#[doc(hidden)]
pub struct TestNote<'a> {
    window: &'a NoteWindow,
    hub: &'a Hub,
}

impl TestNote<'_> {
    pub fn hwnd(&self) -> HWND {
        self.window.hwnd
    }

    pub fn editor(&self) -> std::cell::Ref<'_, Editor> {
        std::cell::Ref::map(self.window.editor.borrow(), |editor| editor.as_ref().expect("editor"))
    }

    /// Holds the window's animations at `at` seconds into the appear (None runs them again).
    pub fn freeze_appear(&self, at: Option<f64>) {
        self.window.freeze_appear(at);
    }

    /// Shows the header buttons and hovers one (0 hide, 1 delete, 2 put away), as the pointer
    /// would.
    pub fn hover_button(&self, index: Option<usize>) {
        self.window.hovering.set(true);
        self.window.buttons_shown.set(Transition::at(1.0));
        self.window.hot.set(index);
        for (i, motion) in self.window.motion.borrow_mut().iter_mut().enumerate() {
            *motion = Transition::at(if Some(i) == index.map(|i| i + 1) { 1.0 } else { 0.0 });
        }
        self.window.invalidate();
    }

    pub fn store_text(&self) -> Option<String> {
        self.hub.store.borrow().note(self.window.id).map(|note| note.text.clone())
    }

    pub fn file_path(&self) -> Option<PathBuf> {
        self.hub.store.borrow().file_path(self.window.id).map(Path::to_path_buf)
    }

    /// Runs the store's refresh as a window activation does.
    pub fn refresh(&self) {
        self.hub.refresh_from_disk();
    }

    /// A repaint the window still owes, and how long ago it was asked for.
    pub fn paint_debt(&self) -> Option<Duration> {
        self.window.asked.get().map(|at| at.elapsed())
    }

    /// The frame the store holds for this note (what will be saved).
    pub fn saved_frame(&self) -> Option<Rect> {
        self.hub.store.borrow().note(self.window.id).map(|note| note.frame)
    }

    pub fn dpi(&self) -> u32 {
        self.window.dpi.get()
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The window rectangle (physical pixels) of a new note summoned at `at` on a monitor with this
/// work area and scale. From Swift `create(at:origin:)`: the card's top-left starts where the
/// summon pill's was, so it grows out of it; then it is kept on the screen.
pub fn new_note_frame(at: Point, work: Rect, scale: f64) -> Rect {
    let (w, h) = (NOTE_SIZE.0 * scale, NOTE_SIZE.1 * scale);
    let x = at.x - SUMMON_SIZE.0 * scale / 2.0;
    let y = at.y - SUMMON_SIZE.1 * scale / 2.0;
    let x = x.min(work.max_x() - w).max(work.min_x());
    let y = y.min(work.max_y() - h).max(work.min_y());
    Rect::new(x.round(), y.round(), w.round(), h.round())
}

/// A saved frame made fit for the monitors there are now: it keeps its place when enough of its
/// header is on a work area to grab it; otherwise it moves onto the work area nearest to it.
/// It is never smaller than the minimum size or larger than that work area.
pub fn clamp_frame(frame: Rect, works: &[Rect], min: (f64, f64)) -> Rect {
    let mut frame = Rect::new(frame.x, frame.y, frame.width.max(min.0), frame.height.max(min.1));
    let header = Rect::new(frame.x, frame.y, frame.width, HEADER.min(frame.height));
    let grabbable = works.iter().any(|work| {
        header.intersection(work).is_some_and(|part| part.width >= 40.0 && part.height >= 16.0)
    });
    if grabbable || works.is_empty() {
        return frame;
    }
    let center = Point::new(frame.mid_x(), frame.mid_y());
    let distance = |work: &Rect| {
        let dx = (work.min_x() - center.x).max(0.0).max(center.x - work.max_x());
        let dy = (work.min_y() - center.y).max(0.0).max(center.y - work.max_y());
        dx * dx + dy * dy
    };
    let work = works
        .iter()
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
        .copied()
        .unwrap_or_default();
    frame.width = frame.width.min(work.width);
    frame.height = frame.height.min(work.height);
    frame.x = frame.x.min(work.max_x() - frame.width).max(work.min_x());
    frame.y = frame.y.min(work.max_y() - frame.height).max(work.min_y());
    frame
}

/// The footer's text, from Swift `editedLabel`: "Created" until the first edit a second after
/// creation, then "Edited"; "just now" within a minute, then how long ago.
pub fn edited_label(created: SystemTime, updated: SystemTime, now: SystemTime) -> String {
    let verb = if updated.duration_since(created).map_or(true, |d| d < Duration::from_secs(1)) {
        "Created"
    } else {
        "Edited"
    };
    format!("{verb} {}", ago(updated, now))
}

/// How long before `now` a note changed: "just now", "5 minutes ago", "yesterday".
pub fn ago(updated: SystemTime, now: SystemTime) -> String {
    let ago = now.duration_since(updated).unwrap_or_default().as_secs();
    match ago {
        0..60 => "just now".to_owned(),
        60..3600 => plural(ago / 60, "minute"),
        3600..86_400 => plural(ago / 3600, "hour"),
        86_400..172_800 => "yesterday".to_owned(),
        172_800..1_209_600 => plural(ago / 86_400, "day"),
        _ => plural(ago / 604_800, "week"),
    }
}

fn plural(count: u64, unit: &str) -> String {
    if count == 1 { format!("1 {unit} ago") } else { format!("{count} {unit}s ago") }
}

/// The damped spring of Swift's `.spring(response: 0.34, dampingFraction: 0.78)`: 0 at the
/// start, settling at 1 with a small overshoot.
pub fn spring(seconds: f64) -> f64 {
    let omega = 2.0 * std::f64::consts::PI / 0.34;
    let zeta: f64 = 0.78;
    let damped = omega * (1.0 - zeta * zeta).sqrt();
    let t = seconds.max(0.0);
    1.0 - (-zeta * omega * t).exp()
        * ((damped * t).cos() + zeta * omega / damped * (damped * t).sin())
}

/// A rectangle between two others.
pub fn lerp_rect(from: Rect, to: Rect, amount: f64) -> Rect {
    let mix = |a: f64, b: f64| a + (b - a) * amount;
    Rect::new(
        mix(from.x, to.x),
        mix(from.y, to.y),
        mix(from.width, to.width),
        mix(from.height, to.height),
    )
}

/// The file name for a screenshot or recording dropped into a note (Swift
/// `attachCarriedScreenshots`): "shot-" or "recording-" and the first eight characters of a new
/// UUID in lowercase.
pub fn attachment_name(kind: &str, id: NoteId, extension: &str) -> String {
    let uuid = id.to_string().to_lowercase();
    format!("{kind}-{}.{extension}", &uuid[..8])
}

/// The Markdown for one attachment, as Swift writes it.
pub fn attachment_link(name: &str, movie: bool) -> String {
    if movie {
        format!("[recording](attachments/{name})")
    } else {
        format!("![screenshot](attachments/{name})")
    }
}

/// The overlay hint after a drop (Swift `onScreenshotsTaken`).
pub fn taken_hint(count: usize) -> String {
    if count == 1 { "added to note".to_owned() } else { format!("added {count} to note") }
}

fn rect_of(r: RECT) -> Rect {
    Rect::new(
        f64::from(r.left),
        f64::from(r.top),
        f64::from(r.right - r.left),
        f64::from(r.bottom - r.top),
    )
}

fn win_rect(r: Rect) -> RECT {
    RECT {
        left: r.x.round() as i32,
        top: r.y.round() as i32,
        right: (r.x + r.width).round() as i32,
        bottom: (r.y + r.height).round() as i32,
    }
}

// ---------------------------------------------------------------------------
// The notes thread
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run(
    config: NotesConfig,
    receiver: Receiver<Command>,
    commands: Sender<Command>,
    events: Sender<Event>,
    wake: Arc<Wake>,
    zones: DropZones,
    shared: &Shared,
    ready: &Sender<std::result::Result<(), String>>,
) -> Result<()> {
    // SAFETY: once on this thread, balanced by OleUninitialize below. OLE (not only COM) is
    // needed for RichEdit's drag and drop and for reading the clipboard on paste.
    unsafe { OleInitialize(None)? };
    struct Ole;
    impl Drop for Ole {
        fn drop(&mut self) {
            // SAFETY: balances OleInitialize on the same thread.
            unsafe { OleUninitialize() };
        }
    }
    let _ole = Ole;
    editor::load_richedit()?;
    // SAFETY: the process's module handle and static class data.
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let controls = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_WIN95_CLASSES,
    };
    // SAFETY: the struct lives through the call.
    let _ = unsafe { InitCommonControlsEx(&controls) };
    register(instance, HUB_CLASS, hub_proc, false);
    register(instance, NOTE_CLASS, note_proc, true);
    // SAFETY: a message-only window of a registered class, owned by this thread.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            HUB_CLASS,
            w!("Igorio notes"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )?
    };
    let now = SystemTime::now();
    let store = NoteStore::with_trash(
        &config.folder,
        &config.windows_file,
        now,
        Box::new(SendTrash(config.trash)),
    );
    let test = config.test.unwrap_or_default();
    let motion = test.motion.unwrap_or_else(editor::animation_effects);
    let dark = match config.theme {
        Theme::System => editor::system_dark(),
        Theme::Light => false,
        Theme::Dark => true,
    };
    let hub = Box::new(Hub {
        hwnd,
        instance,
        store: RefCell::new(store),
        windows: RefCell::new(BTreeMap::new()),
        closing: RefCell::new(BTreeMap::new()),
        commands,
        receiver,
        events,
        wake,
        zones,
        theme: Cell::new(config.theme),
        palette: Cell::new(Palette::for_theme(dark)),
        motion: Cell::new(motion),
        test,
        epoch: Instant::now(),
        save_timer: Cell::new(false),
        next_seed: Cell::new(0),
        active: Cell::new(None),
        quitting: Cell::new(false),
        handling: Cell::new(false),
        panel: RefCell::new(None),
    });
    // SAFETY: the hub outlives the message window (destroyed below before the box drops).
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*hub as *const Hub as isize) };
    shared.hwnd.store(hwnd.0 as isize, SeqCst);
    let _ = ready.send(Ok(()));
    if config.restore {
        hub.restore();
    }
    hub.publish_recent();
    pump(&hub);
    hub.close_everything();
    // SAFETY: our message window; detach the hub pointer first.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}

/// The store's trash must be `Send` to cross into the thread; inside it is used as is.
struct SendTrash(Box<dyn Trash + Send>);

impl Trash for SendTrash {
    fn trash(&self, path: &Path) -> io::Result<()> {
        self.0.trash(path)
    }
}

fn register(
    instance: HINSTANCE,
    name: PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    note: bool,
) {
    // SAFETY: static class data; registering twice in one process fails harmlessly.
    unsafe {
        let class = WNDCLASSW {
            style: if note { CS_DBLCLKS } else { WNDCLASS_STYLES(0) },
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: name,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
    }
}

fn pump(hub: &Hub) {
    let mut message = MSG::default();
    loop {
        // SAFETY: this thread's queue; -1 is an error and ends the loop.
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if status == 0 || status == -1 {
            return;
        }
        // SAFETY: windows of this thread; IsDialogMessage gives Tab and Enter to the buttons.
        unsafe {
            let root = GetAncestor(message.hwnd, GA_ROOT);
            let note = hub.windows.borrow().values().any(|window| window.hwnd == root)
                || hub.panel.borrow().as_ref().is_some_and(|panel| panel.hwnd() == root);
            if note
                && matches!(message.message, WM_KEYDOWN)
                && message.wParam.0 == usize::from(VK_RETURN.0)
            {
                let focused = GetFocus();
                if GetParent(focused).ok() == Some(root)
                    && !matches!(GetDlgCtrlID(focused), EDITOR_ID | panel::FILTER_ID)
                {
                    SendMessageW(focused, BM_CLICK, None, None);
                    continue;
                }
            }
            if note && IsDialogMessageW(root, &message).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Everything the notes thread owns.
#[doc(hidden)]
pub struct Hub {
    hwnd: HWND,
    instance: HINSTANCE,
    store: RefCell<NoteStore>,
    windows: RefCell<BTreeMap<NoteId, Rc<NoteWindow>>>,
    /// Windows fading out after a put away.
    closing: RefCell<BTreeMap<NoteId, Rc<NoteWindow>>>,
    commands: Sender<Command>,
    receiver: Receiver<Command>,
    events: Sender<Event>,
    wake: Arc<Wake>,
    zones: DropZones,
    theme: Cell<Theme>,
    palette: Cell<Palette>,
    motion: Cell<bool>,
    test: TestOptions,
    epoch: Instant,
    save_timer: Cell<bool>,
    next_seed: Cell<u64>,
    active: Cell<Option<NoteId>>,
    quitting: Cell<bool>,
    handling: Cell<bool>,
    /// The notes panel while it is open.
    panel: RefCell<Option<Panel>>,
}

impl Hub {
    fn seconds(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    fn window(&self, id: NoteId) -> Option<Rc<NoteWindow>> {
        self.windows.borrow().get(&id).cloned()
    }

    fn emit(&self, event: Event) {
        if self.events.send(event).is_ok() {
            self.wake.signal();
        }
    }

    fn error(&self, message: String) {
        self.emit(Event::Error(message));
    }

    fn publish_recent(&self) {
        let recent = self
            .store
            .borrow()
            .recent()
            .into_iter()
            .map(|note| RecentNote {
                id: note.id,
                title: note.title(),
                label: note.origin.label(),
                detail: note.origin.detail(),
                updated_at: note.updated_at,
                is_open: self.windows.borrow().contains_key(&note.id),
                is_hidden: note.is_hidden,
            })
            .collect();
        self.emit(Event::Recent(recent));
        if let Some(panel) = self.panel.borrow().as_ref() {
            panel.set_notes(self.panel_notes());
        }
    }

    /// Every note for the panel, newest edit first, with where it is.
    fn panel_notes(&self) -> Vec<PanelNote> {
        let windows = self.windows.borrow();
        let mut notes: Vec<PanelNote> = self
            .store
            .borrow()
            .notes()
            .filter(|note| !note.is_blank() || windows.contains_key(&note.id))
            .map(|note| {
                let state = if windows.contains_key(&note.id) {
                    NoteState::Shown
                } else if note.is_hidden {
                    NoteState::Hidden
                } else {
                    NoteState::Closed
                };
                PanelNote::new(note, state)
            })
            .collect();
        panel::sort(&mut notes);
        notes
    }

    fn idle(&self) -> bool {
        let windows = self.windows.borrow();
        !self.save_timer.get()
            && self.closing.borrow().is_empty()
            && windows.values().all(|window| {
                !window.timer.get()
                    && window.editor.borrow().as_ref().is_none_or(|editor| !editor.animating())
            })
    }

    /// Applies a store change and arms the save timer. False when the store is busy (a write
    /// in progress that pumps messages, such as the Recycle Bin): the caller tries again later.
    fn update(&self, id: NoteId, change: impl FnOnce(&mut Note)) -> bool {
        let now = self.seconds();
        let done = match self.store.try_borrow_mut() {
            Ok(mut store) => {
                store.update(id, now, change);
                true
            }
            Err(_) => false,
        };
        self.schedule_save();
        done
    }

    fn schedule_save(&self) {
        let deadline = self.store.try_borrow().ok().and_then(|store| store.next_deadline());
        // SAFETY: the message window's own timer on this thread.
        unsafe {
            match deadline {
                Some(deadline) => {
                    let ms = ((deadline - self.seconds()) * 1000.0).ceil().max(1.0) as u32;
                    SetTimer(Some(self.hwnd), SAVE_TIMER, ms, None);
                    self.save_timer.set(true);
                }
                None => {
                    let _ = KillTimer(Some(self.hwnd), SAVE_TIMER);
                    self.save_timer.set(false);
                }
            }
        }
    }

    fn save_due(&self) {
        let errors = match self.store.try_borrow_mut() {
            Ok(mut store) => store.tick(self.seconds()),
            Err(_) => Vec::new(),
        };
        self.report(errors);
        self.schedule_save();
        self.publish_recent();
    }

    fn report(&self, errors: Vec<(NoteId, io::Error)>) {
        for (_, error) in errors {
            self.error(format!("A note could not be saved: {error}"));
        }
    }

    fn handle_commands(&self) {
        // Never nested: a command that pumps messages (the Recycle Bin) must not run another
        // command inside it; the outer loop takes the rest of the queue afterwards.
        if self.handling.replace(true) {
            return;
        }
        while let Ok(command) = self.receiver.try_recv() {
            if self.quitting.get() {
                continue;
            }
            match command {
                Command::OpenNew { at, origin } => self.create(at, origin),
                Command::Reopen(id) => self.reveal(id, true),
                Command::Hide(id) => self.hide(id),
                Command::TogglePanel(anchor) => self.toggle_panel(anchor),
                Command::Panel(request) => self.panel_request(request),
                Command::PutAway(id) => self.put_away(id),
                Command::PutAwayAll => {
                    self.hide_all();
                    self.publish_recent();
                }
                Command::Delete(id) => self.delete(id),
                Command::GiveCards { at, cards } => self.give_cards(at, cards),
                Command::SetFolder(folder) => self.switch_folder(folder),
                Command::SetTheme(theme) => {
                    self.theme.set(theme);
                    self.apply_theme();
                }
                Command::Refresh => {
                    self.refresh_from_disk();
                }
                Command::Shutdown => {
                    self.quitting.set(true);
                    self.close_everything();
                    // SAFETY: ends this thread's message loop.
                    unsafe { PostQuitMessage(0) };
                }
                Command::Reap(id) => {
                    let window = self.closing.borrow_mut().remove(&id);
                    if let Some(window) = window {
                        window.destroy();
                    }
                }
            }
        }
        self.handling.set(false);
    }

    fn apply_theme(&self) {
        let dark = match self.theme.get() {
            Theme::System => editor::system_dark(),
            Theme::Light => false,
            Theme::Dark => true,
        };
        let palette = Palette::for_theme(dark);
        if palette == self.palette.get() {
            return;
        }
        self.palette.set(palette);
        if let Some(panel) = self.panel.borrow().as_ref() {
            panel.set_palette(palette);
        }
        let windows: Vec<_> = self.windows.borrow().values().cloned().collect();
        for window in windows {
            window.apply_palette(palette);
        }
    }

    fn set_motion(&self, motion: bool) {
        let motion = self.test.motion.unwrap_or(motion);
        self.motion.set(motion);
        let windows: Vec<_> = self.windows.borrow().values().cloned().collect();
        for window in windows {
            if let Some(editor) = window.editor.borrow().as_ref() {
                editor.set_motion(motion);
            }
        }
    }

    fn new_id(&self) -> NoteId {
        let counter = self.next_seed.get();
        self.next_seed.set(counter + 1);
        NoteId::new_random(seed_bytes(SystemTime::now(), counter.wrapping_add(0x5eed)))
    }

    /// Swift `restore`: the notes that were up when the app quit come back, oldest first.
    fn restore(&self) {
        let notes: Vec<Note> = self
            .store
            .borrow()
            .recent()
            .into_iter()
            .rev()
            .filter(|note| note.is_open && !note.is_hidden)
            .collect();
        for note in notes {
            self.show(note, false, false);
        }
    }

    /// Swift `create(at:origin:)`.
    fn create(&self, at: Point, origin: NoteOrigin) {
        let monitor = monitor_at(at);
        let scale = self.test.dpi.map_or(monitor.scale, |dpi| f64::from(dpi) / 96.0);
        let frame = new_note_frame(at, monitor.work_area, scale);
        let now = SystemTime::now();
        let note = Note {
            id: self.new_id(),
            text: String::new(),
            created_at: now,
            updated_at: now,
            origin,
            frame,
            is_open: true,
            is_hidden: false,
        };
        if let Err(error) = self.store.borrow_mut().insert(note.clone()) {
            self.error(format!("The note could not be saved: {error}"));
        }
        self.show(note, true, true);
        self.publish_recent();
    }

    /// Swift `show(_:growsIn:focus:)`.
    fn show(&self, note: Note, grows_in: bool, focus: bool) {
        if let Some(window) = self.window(note.id) {
            window.bring_forward(focus);
            return;
        }
        let works = work_areas();
        let frame = if note.frame.width <= 0.0 || note.frame.height <= 0.0 {
            let at = cursor_pos();
            let monitor = monitor_at(at);
            new_note_frame(
                Point::new(monitor.work_area.mid_x(), monitor.work_area.mid_y()),
                monitor.work_area,
                monitor.scale,
            )
        } else {
            let scale = monitor_at(Point::new(note.frame.mid_x(), note.frame.mid_y())).scale;
            clamp_frame(note.frame, &works, (MIN_NOTE_SIZE.0 * scale, MIN_NOTE_SIZE.1 * scale))
        };
        let window = match NoteWindow::create(self, &note, frame) {
            Ok(window) => window,
            Err(error) => {
                self.error(format!("The note window could not open: {error}"));
                return;
            }
        };
        self.windows.borrow_mut().insert(note.id, window.clone());
        if !note.is_open || note.frame != frame {
            self.update(note.id, |stored| {
                stored.is_open = true;
                stored.frame = frame;
            });
        }
        window.appear(grows_in, focus);
        self.update_zones();
    }

    /// Shows a note from the menu or the panel and brings it to the front. A hidden note comes
    /// back with the motion of a new note (out of a small pill at its corner); a note on
    /// screen just comes forward.
    fn reveal(&self, id: NoteId, focus: bool) {
        let Some(note) = self.store.borrow().note(id).cloned() else { return };
        let was_hidden = note.is_hidden;
        if was_hidden {
            self.update(id, |stored| stored.is_hidden = false);
        }
        let note = Note { is_hidden: false, ..note };
        self.show(note, was_hidden, focus);
        self.publish_recent();
    }

    /// Hides a shown note: its text is saved at once, it fades out like a put-away note, and
    /// it stays open but hidden (also after a restart). A blank note is put away instead.
    fn hide(&self, id: NoteId) {
        let Some(window) = self.window(id) else { return };
        window.sync_text();
        if self.store.borrow().note(id).is_none_or(Note::is_blank) {
            self.put_away(id);
            return;
        }
        self.update(id, |note| {
            note.is_open = true;
            note.is_hidden = true;
        });
        if let Ok(mut store) = self.store.try_borrow_mut() {
            let errors = store.flush();
            drop(store);
            self.report(errors);
        }
        self.schedule_save();
        self.close_window(id, true);
        self.publish_recent();
    }

    fn toggle_panel(&self, anchor: Option<RECT>) {
        if self.panel.borrow_mut().take().is_some() {
            return;
        }
        let sender = self.commands.clone();
        let hub = self.hwnd.0 as isize;
        let request = Box::new(move |request: PanelRequest| {
            if sender.send(Command::Panel(request)).is_ok() {
                // SAFETY: a scalar message to the notes thread's own message window.
                let _ = unsafe {
                    PostMessageW(Some(HWND(hub as *mut c_void)), COMMAND, WPARAM(0), LPARAM(0))
                };
            }
        });
        match Panel::open(
            self.instance,
            anchor,
            self.panel_notes(),
            self.palette.get(),
            self.motion.get(),
            self.test.dpi,
            !self.test.no_activate,
            request,
        ) {
            Ok(panel) => *self.panel.borrow_mut() = Some(panel),
            Err(error) => self.error(format!("The notes panel could not open: {error}")),
        }
    }

    fn panel_request(&self, request: PanelRequest) {
        match request {
            PanelRequest::Open(id) => self.reveal(id, true),
            PanelRequest::Toggle(id) => {
                if self.windows.borrow().contains_key(&id) {
                    self.hide(id);
                } else {
                    self.reveal(id, false);
                }
            }
            PanelRequest::Delete(id) => self.delete(id),
            PanelRequest::HideAll => {
                let ids: Vec<NoteId> = self.windows.borrow().keys().copied().collect();
                for id in ids {
                    self.hide(id);
                }
            }
            PanelRequest::ShowAll => {
                let ids: Vec<NoteId> = self
                    .store
                    .borrow()
                    .recent()
                    .into_iter()
                    .rev()
                    .filter(|note| note.is_hidden)
                    .map(|note| note.id)
                    .collect();
                for id in ids {
                    self.reveal(id, false);
                }
            }
            PanelRequest::Close => {
                self.panel.borrow_mut().take();
            }
        }
    }

    /// Swift `putAway`: off screen, still in the menu; a blank note is discarded.
    fn put_away(&self, id: NoteId) {
        self.close_window(id, true);
        let blank = self.store.borrow().note(id).is_none_or(Note::is_blank);
        if blank {
            self.store.borrow_mut().delete(id);
        } else {
            self.update(id, |note| note.is_open = false);
        }
        self.publish_recent();
    }

    /// Swift `hideAll`: every window goes, each remembered as open.
    fn hide_all(&self) {
        let ids: Vec<NoteId> = self.windows.borrow().keys().copied().collect();
        for id in ids {
            self.update(id, |note| note.is_open = true);
            self.close_window(id, false);
        }
        let errors = self.store.borrow_mut().flush();
        self.report(errors);
        self.schedule_save();
    }

    fn delete(&self, id: NoteId) {
        self.close_window(id, true);
        let deleted = self.store.borrow_mut().delete(id);
        if let Some(deleted) = deleted
            && deleted.path.is_some()
            && !deleted.trashed
        {
            self.error("The note's file could not be moved to the Recycle Bin.".to_owned());
        }
        self.schedule_save();
        self.publish_recent();
    }

    fn close_window(&self, id: NoteId, animate: bool) {
        let window = self.windows.borrow_mut().remove(&id);
        let Some(window) = window else { return };
        if self.active.get() == Some(id) {
            self.active.set(None);
        }
        self.update_zones();
        if animate && self.motion.get() && !self.quitting.get() {
            window.fade_out();
            self.closing.borrow_mut().insert(id, window);
        } else {
            window.destroy();
        }
    }

    fn switch_folder(&self, folder: PathBuf) {
        if self.store.borrow().folder() == folder {
            return;
        }
        self.hide_all();
        let errors = self.store.borrow_mut().switch_folder(&folder, SystemTime::now());
        self.report(errors);
        self.restore();
        self.publish_recent();
    }

    /// Swift `refreshFromDisk`, plus reloading open notes whose files changed: an open note with
    /// no change waiting to be written takes the file's text; one with a change waiting keeps
    /// the editor's text (the store leaves pending notes alone, as Swift's `refresh(keeping:)`).
    fn refresh_from_disk(&self) {
        let windows: Vec<Rc<NoteWindow>> = self.windows.borrow().values().cloned().collect();
        let keeping: Vec<NoteId> = {
            let store = self.store.borrow();
            windows
                .iter()
                .filter(|window| store.file_path(window.id).is_none_or(|path| !path.exists()))
                .map(|window| window.id)
                .collect()
        };
        self.store.borrow_mut().refresh(&keeping, SystemTime::now());
        for window in windows {
            let stored = self.store.borrow().note(window.id).map(|note| note.text.clone());
            let Some(stored) = stored else { continue };
            window.reload(&stored);
        }
        self.publish_recent();
    }

    /// Swift `attachCarriedScreenshots`: each card becomes a file in `<notes>/attachments`
    /// and a Markdown link; the links go in at the double-click.
    fn give_cards(&self, at: Point, cards: Vec<HeldCard>) {
        let Some(id) = self.zones.note_at(at) else { return };
        let Some(window) = self.window(id) else { return };
        if cards.is_empty() {
            return;
        }
        let folder = self.store.borrow().folder().join("attachments");
        if let Err(error) = fs::create_dir_all(&folder) {
            self.error(format!("The attachments folder could not be made: {error}"));
            return;
        }
        let mut taken = Vec::new();
        let mut lines = Vec::new();
        for card in &cards {
            match self.attach(&folder, card) {
                Ok(link) => {
                    taken.push(card.id);
                    lines.push(link);
                }
                Err(error) => {
                    self.error(format!("A screenshot could not be added to the note: {error}"))
                }
            }
        }
        if lines.is_empty() {
            return;
        }
        let point = POINT { x: at.x.round() as i32, y: at.y.round() as i32 };
        if let Some(editor) = window.editor.borrow().as_ref() {
            editor.insert_screenshots(Some(point), &lines.join("\n"));
        }
        window.sync_text();
        let hint = taken_hint(taken.len());
        self.emit(Event::CardsTaken { ids: taken, hint });
    }

    fn attach(&self, folder: &Path, card: &HeldCard) -> io::Result<String> {
        if let Some(movie) = &card.movie {
            let extension = movie.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_owned();
            let path = self.free_name(folder, "recording", &extension);
            fs::copy(movie, &path)?;
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
            return Ok(attachment_link(&name, true));
        }
        let bytes = crate::saver::encode_png(&card.image, card.scale).map_err(io::Error::other)?;
        let path = self.free_name(folder, "shot", "png");
        write_new(&path, &bytes)?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        Ok(attachment_link(&name, false))
    }

    fn free_name(&self, folder: &Path, kind: &str, extension: &str) -> PathBuf {
        loop {
            let path = folder.join(attachment_name(kind, self.new_id(), extension));
            if !path.exists() {
                return path;
            }
        }
    }

    fn update_zones(&self) {
        let active = self.active.get();
        let mut zones: Vec<NoteZone> = self
            .windows
            .borrow()
            .values()
            .filter(|window| !window.closing.get())
            .map(|window| NoteZone {
                id: window.id,
                rect: window.card_rect(),
                hwnd: window.hwnd.0 as isize,
                active: Some(window.id) == active,
            })
            .collect();
        // The active note first: it is on top of the others.
        zones.sort_by_key(|zone| !zone.active);
        self.zones.set(zones);
    }

    /// Shutdown: text first (crash safety), then window states, then the windows.
    fn close_everything(&self) {
        self.panel.borrow_mut().take();
        let ids: Vec<NoteId> = self.windows.borrow().keys().copied().collect();
        for id in &ids {
            let window = self.window(*id);
            if let Some(window) = window {
                window.sync_text();
            }
            self.update(*id, |note| note.is_open = true);
        }
        let errors = self.store.borrow_mut().flush();
        self.report(errors);
        for id in ids {
            self.close_window(id, false);
        }
        let closing: Vec<_> =
            std::mem::take(&mut *self.closing.borrow_mut()).into_values().collect();
        for window in closing {
            window.destroy();
        }
        // SAFETY: our timer.
        let _ = unsafe { KillTimer(Some(self.hwnd), SAVE_TIMER) };
        self.save_timer.set(false);
        self.zones.set(Vec::new());
    }
}

/// Writes a new file through a temp file in the same folder, then renames it into place.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = folder.join(format!(
        ".attach.tmp-{}-{}",
        std::process::id(),
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn work_areas() -> Vec<Rect> {
    unsafe extern "system" fn each(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        let mut info =
            MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: `data` is the Vec pointer passed below, alive for the enumeration.
        unsafe {
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                (*(data.0 as *mut Vec<Rect>)).push(rect_of(info.rcWork));
            }
        }
        true.into()
    }
    let mut areas: Vec<Rect> = Vec::new();
    // SAFETY: the callback only pushes into the local Vec during the call.
    let _ = unsafe {
        EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut areas as *mut Vec<Rect> as isize))
    };
    areas
}

unsafe extern "system" fn hub_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the Hub while the window lives (0 before and after).
    let hub = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Hub).as_ref() };
    if let Some(hub) = hub {
        match msg {
            COMMAND => {
                hub.handle_commands();
                return LRESULT(0);
            }
            WM_TIMER if wp.0 == SAVE_TIMER => {
                hub.save_due();
                return LRESULT(0);
            }
            TEST_RUN => {
                // SAFETY: `run_test` boxed the job and waits in SendMessage while it runs.
                let job = unsafe { Box::from_raw(wp.0 as *mut TestJob) };
                (*job)(hub);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    // SAFETY: default handling.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ---------------------------------------------------------------------------
// One note window
// ---------------------------------------------------------------------------

/// The window's own animation: growing in, or fading out.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    Still,
    Growing { from: Rect, to: Rect, start: f64 },
    Fading { start: f64 },
}

struct NoteWindow {
    id: NoteId,
    hwnd: HWND,
    hub: *const Hub,
    editor: RefCell<Option<Editor>>,
    editor_events: Rc<RefCell<Vec<EditorEvent>>>,
    /// Origin chip, hide, delete, put away.
    buttons: [HWND; 4],
    tooltip: HWND,
    origin: RefCell<NoteOrigin>,
    dpi: Cell<u32>,
    palette: Cell<Palette>,
    fonts: Cell<GdiFonts>,
    hovering: Cell<bool>,
    buttons_shown: Cell<Transition>,
    hot: Cell<Option<usize>>,
    /// Hover of the header controls (origin, hide, delete, close).
    motion: RefCell<[Transition; 4]>,
    window_motion: Cell<Motion>,
    frozen: Cell<Option<f64>>,
    timer: Cell<bool>,
    closing: Cell<bool>,
    layered: Cell<bool>,
    surface: RefCell<Option<Surface>>,
    epoch: Instant,
    saved_focus: Cell<HWND>,
    /// The text this window last gave the store (or loaded). A reload from disk replaces the
    /// editor's text only while the editor still holds exactly this, so nothing unsaved is lost.
    synced: RefCell<String>,
    /// When a repaint was asked for and has not happened yet (the watchdog reads it).
    asked: Cell<Option<Instant>>,
    painted: Cell<Option<Instant>>,
}

impl NoteWindow {
    fn hub(&self) -> &Hub {
        // SAFETY: the Hub outlives every window it owns (it destroys them before it drops).
        unsafe { &*self.hub }
    }

    fn now(&self) -> f64 {
        self.frozen
            .get()
            .map_or_else(|| self.epoch.elapsed().as_secs_f64() * 1000.0, |at| at * 1000.0)
    }

    fn create(hub: &Hub, note: &Note, frame: Rect) -> Result<Rc<NoteWindow>> {
        let style = WINDOW_STYLE(WS_POPUP.0 | WS_THICKFRAME.0 | WS_CLIPCHILDREN.0);
        let ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_CONTROLPARENT;
        let rect = win_rect(frame);
        // SAFETY: a top-level window of our class, created on the notes thread.
        let hwnd = unsafe {
            CreateWindowExW(
                ex,
                NOTE_CLASS,
                w!("Note"),
                style,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(hub.instance),
                None,
            )?
        };
        // SAFETY: scalar DPI query of our window.
        let dpi = hub.test.dpi.unwrap_or_else(|| unsafe { GetDpiForWindow(hwnd) }.max(96));
        let make = |id: i32, text: &str| {
            let text: Vec<u16> = text.encode_utf16().chain([0]).collect();
            // SAFETY: a child push button of our window; the caption is copied.
            unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    PCWSTR(text.as_ptr()),
                    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
                    0,
                    0,
                    1,
                    1,
                    Some(hwnd),
                    Some(HMENU(id as usize as *mut c_void)),
                    Some(hub.instance),
                    None,
                )
            }
        };
        let label = note.origin.label();
        let buttons = [
            make(ORIGIN_ID, &format!("From {label}"))?,
            make(HIDE_ID, "Hide note")?,
            make(DELETE_ID, "Delete note")?,
            make(CLOSE_ID, "Put away")?,
        ];
        // SAFETY: a tooltip owned by our window.
        let tooltip = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                TOOLTIPS_CLASSW,
                None,
                WINDOW_STYLE(WS_POPUP.0 | TTS_ALWAYSTIP | TTS_NOPREFIX),
                0,
                0,
                0,
                0,
                Some(hwnd),
                None,
                Some(hub.instance),
                None,
            )?
        };
        let window = Rc::new(NoteWindow {
            id: note.id,
            hwnd,
            hub,
            editor: RefCell::new(None),
            editor_events: Rc::new(RefCell::new(Vec::new())),
            buttons,
            tooltip,
            origin: RefCell::new(note.origin.clone()),
            dpi: Cell::new(dpi),
            palette: Cell::new(hub.palette.get()),
            fonts: Cell::new(GdiFonts::new(dpi)),
            hovering: Cell::new(false),
            buttons_shown: Cell::new(Transition::at(0.0)),
            hot: Cell::new(None),
            motion: RefCell::new([Transition::at(0.0); 4]),
            window_motion: Cell::new(Motion::Still),
            frozen: Cell::new(None),
            timer: Cell::new(false),
            closing: Cell::new(false),
            layered: Cell::new(false),
            surface: RefCell::new(None),
            epoch: Instant::now(),
            saved_focus: Cell::new(HWND::default()),
            synced: RefCell::new(String::new()),
            asked: Cell::new(None),
            painted: Cell::new(None),
        });
        // SAFETY: the Rc in the hub's map keeps the window state alive until WM_NCDESTROY
        // clears this pointer (destroy() runs before the map drops it).
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::as_ptr(&window) as isize) };
        for button in buttons {
            // SAFETY: same-thread subclass; the parent state outlives the buttons.
            unsafe {
                let _ =
                    SetWindowSubclass(button, Some(button_proc), 1, Rc::as_ptr(&window) as usize);
            }
        }
        let queue = window.editor_events.clone();
        let sink_hwnd = hwnd.0 as isize;
        let sink: Box<dyn Fn(EditorEvent)> = Box::new(move |event| {
            queue.borrow_mut().push(event);
            // SAFETY: a scalar message to our own window; handled after the editor returns.
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(sink_hwnd as *mut c_void)),
                    EDITOR_EVENTS,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        });
        let config = EditorConfig {
            palette: hub.palette.get(),
            dpi,
            base: hub.store.borrow().folder().to_path_buf(),
            motion: hub.motion.get(),
        };
        let editor = Editor::create(hwnd, EDITOR_ID, config, sink)?;
        editor.set_text(&note.text);
        let end = editor.text().len();
        editor.select(end..end);
        *window.synced.borrow_mut() = editor.text();
        *window.editor.borrow_mut() = Some(editor);
        window.add_tools();
        window.style_frame();
        window.layout();
        Ok(window)
    }

    fn add_tools(&self) {
        let origin = self.origin.borrow();
        let tips = [
            origin.detail(),
            "Hide (find it again in Notes… in the tray menu)".to_owned(),
            "Delete note".to_owned(),
            "Put away (find it again in the tray menu)".to_owned(),
        ];
        for (button, tip) in self.buttons.iter().zip(tips) {
            let mut text: Vec<u16> = tip.encode_utf16().chain([0]).collect();
            let info = TTTOOLINFOW {
                cbSize: size_of::<TTTOOLINFOW>() as u32,
                uFlags: TTF_IDISHWND | TTF_SUBCLASS,
                hwnd: self.hwnd,
                uId: button.0 as usize,
                lpszText: windows::core::PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            // SAFETY: the tooltip copies the struct and text during the call.
            unsafe {
                SendMessageW(
                    self.tooltip,
                    TTM_ADDTOOLW,
                    None,
                    Some(LPARAM(&info as *const _ as isize)),
                );
                SendMessageW(self.tooltip, TTM_SETMAXTIPWIDTH, None, Some(LPARAM(400)));
            }
        }
    }

    fn style_frame(&self) {
        let palette = self.palette.get();
        let corner = DWMWCP_ROUND;
        let dark = i32::from(palette.dark);
        let margins =
            MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 0, cyBottomHeight: 1 };
        // SAFETY: DWM copies the values during each call; the window is ours.
        unsafe {
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
                4,
            );
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&dark as *const i32).cast(),
                4,
            );
            // A one-pixel frame extension keeps the DWM shadow on a frameless window.
            let _ = DwmExtendFrameIntoClientArea(self.hwnd, &margins);
        }
    }

    fn apply_palette(&self, palette: Palette) {
        self.palette.set(palette);
        self.style_frame();
        if let Some(editor) = self.editor.borrow().as_ref() {
            editor.set_palette(palette);
        }
        self.invalidate();
    }

    fn invalidate(&self) {
        if self.asked.get().is_none() {
            self.asked.set(Some(Instant::now()));
            if editor::debug_enabled() {
                // SAFETY: a one-shot timer on our own window.
                unsafe { SetTimer(Some(self.hwnd), WATCHDOG_TIMER, 1200, None) };
            }
        }
        // SAFETY: our window and its header buttons, which paint their part of the card.
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
            for button in self.buttons {
                let _ = InvalidateRect(Some(button), None, false);
            }
        }
    }

    fn client(&self) -> RECT {
        let mut rect = RECT::default();
        // SAFETY: our window, a writable local.
        let _ = unsafe { GetClientRect(self.hwnd, &mut rect) };
        rect
    }

    fn card_rect(&self) -> Rect {
        let mut rect = RECT::default();
        // SAFETY: our window, a writable local.
        let _ = unsafe { GetWindowRect(self.hwnd, &mut rect) };
        rect_of(rect)
    }

    fn s(&self, value: f64) -> i32 {
        px(value, self.dpi.get())
    }

    fn button_rects(&self) -> [RECT; 4] {
        let client = self.client();
        let top = (self.s(HEADER) - self.s(BUTTON)) / 2;
        let close_right = client.right - self.s(8.0);
        let close = RECT {
            left: close_right - self.s(BUTTON),
            top,
            right: close_right,
            bottom: top + self.s(BUTTON),
        };
        let delete = RECT {
            left: close.left - self.s(6.0 + BUTTON),
            top,
            right: close.left - self.s(6.0),
            bottom: close.bottom,
        };
        let hide = RECT {
            left: delete.left - self.s(6.0 + BUTTON),
            top,
            right: delete.left - self.s(6.0),
            bottom: close.bottom,
        };
        let label = self.origin.borrow().label();
        let fonts = self.fonts.get();
        let text_width =
            text_width(fonts.semibold, &label).min(hide.left - self.s(60.0)).max(self.s(20.0));
        let chip_top = (self.s(HEADER) - self.s(22.0)) / 2;
        let origin = RECT {
            left: self.s(10.0),
            top: chip_top,
            right: self.s(10.0 + 6.0 + 14.0 + 5.0 + 6.0 + 2.0) + text_width,
            bottom: chip_top + self.s(22.0),
        };
        [origin, hide, delete, close]
    }

    fn editor_rect(&self) -> RECT {
        let client = self.client();
        RECT {
            left: self.s(14.0),
            top: self.s(HEADER + 1.0 + 6.0),
            right: client.right - self.s(8.0),
            bottom: client.bottom - self.s(FOOTER),
        }
    }

    fn layout(&self) {
        for (button, rect) in self.buttons.iter().zip(self.button_rects()) {
            // SAFETY: our child windows.
            let _ = unsafe {
                SetWindowPos(
                    *button,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        if let Some(editor) = self.editor.borrow().as_ref() {
            editor.set_rect(self.editor_rect());
        }
        self.invalidate();
    }

    fn set_dpi(&self, dpi: u32) {
        if self.dpi.get() == dpi {
            return;
        }
        self.dpi.set(dpi);
        self.fonts.set(GdiFonts::new(dpi));
        if let Ok(mut surface) = self.surface.try_borrow_mut() {
            *surface = None;
        }
        if let Some(editor) = self.editor.borrow().as_ref() {
            editor.set_dpi(dpi);
        }
        self.layout();
    }

    fn show_children(&self, show: bool) {
        let command = if show { SW_SHOWNA } else { SW_HIDE };
        // SAFETY: our child windows.
        unsafe {
            for button in self.buttons {
                let _ = ShowWindow(button, command);
            }
            if let Some(editor) = self.editor.borrow().as_ref() {
                let _ = ShowWindow(editor.hwnd(), command);
            }
        }
    }

    fn set_layered(&self, alpha: Option<u8>) {
        // SAFETY: style bits and layered attributes of our own window.
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            match alpha {
                Some(alpha) => {
                    if !self.layered.get() {
                        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
                        self.layered.set(true);
                    }
                    let _ = SetLayeredWindowAttributes(self.hwnd, COLORREF(0), alpha, LWA_ALPHA);
                }
                None => {
                    if self.layered.get() {
                        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as isize));
                        self.layered.set(false);
                        let _ = RedrawWindow(
                            Some(self.hwnd),
                            None,
                            None,
                            RDW_ERASE | RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN,
                        );
                    }
                }
            }
        }
    }

    /// Shows the window. A new note grows out of the summon pill (Swift spring, response 0.34,
    /// damping 0.78): the window's rectangle grows from the pill's and its opacity fades in,
    /// with the content hidden until it is nearly full size. Reduced motion: at once.
    fn appear(&self, grows_in: bool, focus: bool) {
        let hub = self.hub();
        let to = self.card_rect();
        let show = if hub.test.no_activate || !focus { SW_SHOWNOACTIVATE } else { SW_SHOW };
        if grows_in && hub.motion.get() {
            let scale = f64::from(self.dpi.get()) / 96.0;
            let (w, h) = (SUMMON_SIZE.0 * scale, SUMMON_SIZE.1 * scale);
            let from = Rect::new(to.x, to.y, w, h);
            self.window_motion.set(Motion::Growing { from, to, start: self.now() });
            self.show_children(false);
            self.set_layered(Some(0));
            self.place(from);
            // SAFETY: our window.
            let _ = unsafe { ShowWindow(self.hwnd, show) };
            self.start_timer();
            self.step();
        } else {
            // SAFETY: our window.
            let _ = unsafe { ShowWindow(self.hwnd, show) };
        }
        if focus && !hub.test.no_activate {
            self.bring_forward(true);
        }
    }

    fn place(&self, rect: Rect) {
        let r = win_rect(rect);
        // SAFETY: our window.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOACTIVATE,
            )
        };
    }

    fn bring_forward(&self, focus: bool) {
        // SAFETY: our window, brought up on its own thread.
        unsafe {
            let _ = ShowWindow(
                self.hwnd,
                if self.hub().test.no_activate || !focus { SW_SHOWNOACTIVATE } else { SW_SHOW },
            );
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE
                    | SWP_NOSIZE
                    | if self.hub().test.no_activate || !focus {
                        SWP_NOACTIVATE
                    } else {
                        SET_WINDOW_POS_FLAGS(0)
                    },
            );
            if focus && !self.hub().test.no_activate {
                let _ = SetForegroundWindow(self.hwnd);
                if let Some(editor) = self.editor.borrow().as_ref() {
                    let _ = SetFocus(Some(editor.hwnd()));
                }
            }
        }
    }

    fn fade_out(&self) {
        self.closing.set(true);
        self.window_motion.set(Motion::Fading { start: self.now() });
        self.set_layered(Some(255));
        self.start_timer();
    }

    fn destroy(&self) {
        // SAFETY: our window; WM_NCDESTROY detaches its state pointer.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), MOTION_TIMER);
            if IsWindow(Some(self.hwnd)).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
        self.editor.borrow_mut().take();
    }

    fn freeze_appear(&self, at: Option<f64>) {
        match at {
            Some(seconds) => {
                if let Motion::Growing { from, to, .. } = self.window_motion.get() {
                    self.window_motion.set(Motion::Growing { from, to, start: 0.0 });
                }
                self.frozen.set(Some(seconds));
            }
            None => self.frozen.set(None),
        }
        self.step();
    }

    fn start_timer(&self) {
        let run = self.frozen.get().is_none() && self.active();
        if run != self.timer.get() {
            // SAFETY: our window's own timer. The flag follows what Windows did, so a refused
            // timer is asked for again on the next change.
            let running = unsafe {
                if run {
                    SetTimer(Some(self.hwnd), MOTION_TIMER, 10, None) != 0
                } else {
                    let _ = KillTimer(Some(self.hwnd), MOTION_TIMER);
                    false
                }
            };
            self.timer.set(running);
        }
    }

    /// Everything drawn again: the card, its buttons, the editor (after the window came back
    /// from being hidden, covered, deactivated, or a display or theme change). A layered style
    /// left over from an animation that never finished is removed first.
    fn redraw_all(&self) {
        if matches!(self.window_motion.get(), Motion::Still) && self.layered.get() {
            self.set_layered(None);
        }
        self.invalidate();
        // SAFETY: our own window and its children.
        let _ = unsafe {
            RedrawWindow(
                Some(self.hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN | RDW_ERASE,
            )
        };
    }

    /// One line of the window's paint state, for the watchdog.
    fn paint_state(&self) -> String {
        // SAFETY: plain queries about our own window.
        let (visible, alpha) = unsafe {
            let mut alpha = 0u8;
            let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
            let _ = GetLayeredWindowAttributes(self.hwnd, None, Some(&mut alpha), Some(&mut flags));
            (IsWindowVisible(self.hwnd).as_bool(), alpha)
        };
        format!(
            "window visible={visible} motion={:?} layered={} alpha={alpha} timer={} closing={} asked={:?} painted={:?} surface={}",
            self.window_motion.get(),
            self.layered.get(),
            self.timer.get(),
            self.closing.get(),
            self.asked.get().map(|at| at.elapsed()),
            self.painted.get().map(|at| at.elapsed()),
            self.surface
                .try_borrow()
                .map_or("busy".to_owned(), |surface| surface.is_some().to_string()),
        )
    }

    fn watchdog(&self) {
        let Some(asked) = self.asked.get() else { return };
        // SAFETY: a plain query about our own window.
        let visible = unsafe { IsWindowVisible(self.hwnd) }.as_bool();
        if visible && asked.elapsed() > Duration::from_secs(1) {
            editor::debug_line(&format!(
                "notes-paint: note {} not painted for {:?}: {}",
                self.id,
                asked.elapsed(),
                self.paint_state()
            ));
            self.redraw_all();
        } else if visible {
            // SAFETY: a one-shot timer on our own window.
            unsafe { SetTimer(Some(self.hwnd), WATCHDOG_TIMER, 1200, None) };
        }
    }

    fn active(&self) -> bool {
        let now = self.now();
        !matches!(self.window_motion.get(), Motion::Still)
            || self.buttons_shown.get().active(now)
            || self.motion.borrow().iter().any(|track| track.active(now))
    }

    /// One animation frame.
    fn step(&self) {
        let now = self.now();
        match self.window_motion.get() {
            Motion::Growing { from, to, start } => {
                let t = (now - start) / 1000.0;
                let progress = spring(t);
                let alpha = (t / 0.15).clamp(0.0, 1.0);
                self.place(lerp_rect(from, to, progress));
                self.set_layered(Some((alpha * 255.0).round() as u8));
                if t >= APPEAR_SECONDS && self.frozen.get().is_none() {
                    self.window_motion.set(Motion::Still);
                    self.place(to);
                    self.set_layered(None);
                    self.show_children(true);
                    self.layout();
                    // Only a note that already has the foreground: focusing the editor of a
                    // note shown without focus would take the foreground (and close the panel).
                    // SAFETY: plain query.
                    if !self.hub().test.no_activate
                        && unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } == self.hwnd
                        && let Some(editor) = self.editor.borrow().as_ref()
                    {
                        editor.focus();
                    }
                    self.hub().update_zones();
                } else if progress > 0.97 && self.frozen.get().is_none() {
                    self.show_children(true);
                }
                self.invalidate();
            }
            Motion::Fading { start } => {
                let progress = ((now - start) / FADE_OUT_MS).clamp(0.0, 1.0);
                self.set_layered(Some(((1.0 - editor::ease(progress)) * 255.0).round() as u8));
                if progress >= 1.0 {
                    self.window_motion.set(Motion::Still);
                    // SAFETY: our window.
                    let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
                    let hub = self.hub();
                    let _ = hub.commands.send(Command::Reap(self.id));
                    // SAFETY: the hub's message window, on this thread.
                    let _ = unsafe { PostMessageW(Some(hub.hwnd), COMMAND, WPARAM(0), LPARAM(0)) };
                }
            }
            Motion::Still => self.invalidate(),
        }
        self.start_timer();
    }

    /// Reads the editor's text into the store.
    fn sync_text(&self) {
        let text = match self.editor.borrow().as_ref() {
            Some(editor) => editor.text(),
            None => return,
        };
        let unchanged = match self.hub().store.try_borrow() {
            Ok(store) => store.note(self.id).is_some_and(|note| note.text == text),
            Err(_) => false,
        };
        if unchanged {
            return;
        }
        let now = SystemTime::now();
        let given = text.clone();
        let stored = self.hub().update(self.id, |note| {
            note.text = text;
            note.updated_at = now;
        });
        if stored {
            *self.synced.borrow_mut() = given;
        } else {
            // The store is busy (a write that pumps messages): try again after this message.
            self.editor_events.borrow_mut().push(EditorEvent::Changed);
            // SAFETY: a scalar message to our own window.
            let _ = unsafe { PostMessageW(Some(self.hwnd), EDITOR_EVENTS, WPARAM(0), LPARAM(0)) };
        }
    }

    /// The file changed on disk: take its text unless the editor has something unsaved.
    fn reload(&self, stored: &str) {
        let guard = self.editor.borrow();
        let Some(editor) = guard.as_ref() else { return };
        let current = editor.text();
        let normalized = stored.replace("\r\n", "\n").replace('\r', "\n");
        if current == normalized {
            *self.synced.borrow_mut() = current;
            return;
        }
        if current != *self.synced.borrow() {
            // The editor has text the store has not got yet: it wins and is saved (the Swift
            // rule: an open note keeps its own text).
            drop(guard);
            self.sync_text();
            return;
        }
        let caret = editor.selection().end;
        // Keep the caret on the same line and column where the line still exists.
        let line = current[..caret.min(current.len())].matches('\n').count();
        let column = current[..caret.min(current.len())].rsplit('\n').next().map_or(0, str::len);
        editor.set_text(&normalized);
        *self.synced.borrow_mut() = normalized.clone();
        let start = normalized
            .split('\n')
            .take(line)
            .map(|l| l.len() + 1)
            .sum::<usize>()
            .min(normalized.len());
        let line_len = normalized[start..].split('\n').next().map_or(0, str::len);
        let mut at = start + column.min(line_len);
        while !normalized.is_char_boundary(at) {
            at -= 1;
        }
        editor.select(at..at);
    }

    fn handle_editor_events(&self) {
        let events: Vec<EditorEvent> = std::mem::take(&mut *self.editor_events.borrow_mut());
        for event in events {
            match event {
                EditorEvent::Changed => self.sync_text(),
                EditorEvent::FocusNext => self.focus_step(false),
                EditorEvent::FocusPrevious => self.focus_step(true),
                EditorEvent::OpenLink(url) => open_in_shell(url),
                EditorEvent::OpenImage(path) => open_in_shell(path.to_string_lossy().into_owned()),
            }
        }
    }

    fn focus_step(&self, previous: bool) {
        // SAFETY: tab order of our own children.
        unsafe {
            let current = GetFocus();
            if let Ok(next) = GetNextDlgTabItem(self.hwnd, Some(current), previous) {
                let _ = SetFocus(Some(next));
                self.update_buttons_shown();
            }
        }
    }

    /// The header buttons show while the pointer is over the note or one of them has focus
    /// (Swift shows them on hover).
    fn update_buttons_shown(&self) {
        // SAFETY: focus query on this thread.
        let focused = unsafe { GetFocus() };
        let header_focus = self.buttons[1..].contains(&focused);
        let show = self.hovering.get() || header_focus;
        let now = self.now();
        let mut track = self.buttons_shown.get();
        let duration = if self.hub().motion.get() { BUTTONS_MS } else { 0.0 };
        track.retarget(if show { 1.0 } else { 0.0 }, now, duration);
        self.buttons_shown.set(track);
        self.invalidate();
        self.start_timer();
    }

    fn set_hot(&self, hot: Option<usize>) {
        if self.hot.get() == hot {
            return;
        }
        self.hot.set(hot);
        let now = self.now();
        let motion = self.hub().motion.get();
        for (index, track) in self.motion.borrow_mut().iter_mut().enumerate() {
            let on = hot == Some(index);
            track.retarget(
                if on { 1.0 } else { 0.0 },
                now,
                if !motion {
                    0.0
                } else if on {
                    80.0
                } else {
                    120.0
                },
            );
        }
        self.invalidate();
        self.start_timer();
    }

    fn pointer_moved(&self) {
        if !self.hovering.get() {
            self.hovering.set(true);
            self.update_buttons_shown();
        }
        let mut request = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: self.hwnd,
            dwHoverTime: 0,
        };
        // SAFETY: a leave request for our own window.
        let _ = unsafe { TrackMouseEvent(&mut request) };
    }

    /// The pointer left the window or one of its children: still over the note?
    fn pointer_left(&self) {
        let mut point = POINT::default();
        // SAFETY: reads (never moves) the cursor position.
        let inside = unsafe { GetCursorPos(&mut point) }.is_ok() && {
            let r = self.card_rect();
            r.contains(Point::new(f64::from(point.x), f64::from(point.y)))
        };
        if !inside && self.hovering.get() {
            self.hovering.set(false);
            self.set_hot(None);
            self.update_buttons_shown();
        }
    }

    fn hit_test(&self, x: i32, y: i32) -> u32 {
        let mut window = RECT::default();
        // SAFETY: our window, a writable local.
        let _ = unsafe { GetWindowRect(self.hwnd, &mut window) };
        let (cx, cy) = (x - window.left, y - window.top);
        let (w, h) = (window.right - window.left, window.bottom - window.top);
        let edge = self.s(EDGE);
        let left = cx < edge;
        let right = cx >= w - edge;
        let top = cy < edge;
        let bottom = cy >= h - edge;
        let grip = cx >= w - self.s(GRIP + 4.0) && cy >= h - self.s(GRIP + 3.0);
        match (left, right, top, bottom) {
            (true, _, true, _) => HTTOPLEFT,
            (_, true, true, _) => HTTOPRIGHT,
            (true, _, _, true) => HTBOTTOMLEFT,
            (_, true, _, true) => HTBOTTOMRIGHT,
            _ if grip => HTBOTTOMRIGHT,
            (true, ..) => HTLEFT,
            (_, true, ..) => HTRIGHT,
            (_, _, true, _) => HTTOP,
            (_, _, _, true) => HTBOTTOM,
            _ if cy < self.s(HEADER) || cy >= h - self.s(FOOTER) => HTCAPTION,
            _ => HTCLIENT,
        }
    }

    fn paint(&self, target: HDC) {
        self.paint_at(target, POINT::default(), None);
    }

    /// Renders the whole card and copies it to `target`, whose top-left is `origin` in the
    /// card (a header button paints its own part of the card this way).
    fn paint_at(&self, target: HDC, origin: POINT, size: Option<(i32, i32)>) {
        let client = self.client();
        let (width, height) = (client.right.max(1) as u32, client.bottom.max(1) as u32);
        let Ok(mut storage) = self.surface.try_borrow_mut() else {
            // SAFETY: a one-shot timer on our own window: paint again shortly.
            unsafe { SetTimer(Some(self.hwnd), RETRY_TIMER, 250, None) };
            return;
        };
        if storage.as_ref().is_none_or(|surface| surface.width != width || surface.height != height)
        {
            *storage = Surface::new(width, height).ok();
        }
        let Some(surface) = storage.as_mut() else {
            // No bitmap (GDI refused one): try again shortly instead of leaving stale pixels.
            // SAFETY: a one-shot timer on our own window.
            unsafe { SetTimer(Some(self.hwnd), RETRY_TIMER, 250, None) };
            return;
        };
        let palette = self.palette.get();
        let fonts = self.fonts.get();
        let now = self.now();
        {
            let mut canvas = surface.canvas();
            canvas.fill(client, 0.0, palette.paper, 1.0);
            {
                // The header and footer draw while the card grows too; only the editor waits.
                // Header hairline, inset like Swift's.
                let line = RECT {
                    left: self.s(12.0),
                    top: self.s(HEADER),
                    right: client.right - self.s(12.0),
                    bottom: self.s(HEADER) + 1,
                };
                canvas.fill(line, 0.0, palette.hairline, 1.0);
                let rects = self.button_rects();
                let motion = *self.motion.borrow();
                let shown = self.buttons_shown.get().value(now);
                // SAFETY: focus query on this thread.
                let focused = unsafe { GetFocus() };
                // The origin chip: a capsule with a glyph and the label.
                let chip = rects[0];
                let chip_fill = mix(
                    mix(palette.paper, palette.hover, 0.6),
                    palette.hover,
                    motion[0].value(now),
                );
                canvas.fill(chip, (chip.bottom - chip.top) as f32 / 2.0, chip_fill, 1.0);
                let glyph = RECT {
                    left: chip.left + self.s(6.0),
                    top: chip.top + self.s(4.0),
                    right: chip.left + self.s(20.0),
                    bottom: chip.bottom - self.s(4.0),
                };
                self.origin_glyph(&mut canvas, glyph, palette.muted);
                let label = RECT {
                    left: glyph.right + self.s(5.0),
                    top: chip.top,
                    right: chip.right - self.s(6.0),
                    bottom: chip.bottom,
                };
                canvas.label(
                    label,
                    &self.origin.borrow().label(),
                    palette.muted,
                    fonts.semibold,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                );
                if focused == self.buttons[0] {
                    focus_ring(
                        &mut canvas,
                        chip,
                        (chip.bottom - chip.top) / 2,
                        self.s(2.0),
                        palette,
                    );
                }
                for (index, rect) in rects.iter().enumerate().skip(1) {
                    let hover = motion[index].value(now);
                    // SAFETY: scalar button state query.
                    let pressed =
                        unsafe { SendMessageW(self.buttons[index], BM_GETSTATE, None, None) }.0
                            as u32
                            & BST_PUSHED
                            != 0;
                    let visible = shown.max(if focused == self.buttons[index] { 1.0 } else { 0.0 });
                    if visible <= 0.0 {
                        continue;
                    }
                    let fill = if pressed {
                        mix(palette.hover, palette.muted, 0.18)
                    } else {
                        palette.hover
                    };
                    canvas.fill(*rect, self.s(BUTTON / 2.0) as f32, fill, hover * visible);
                    let ink = mix(palette.paper, mix(palette.muted, palette.text, hover), visible);
                    match index {
                        1 => hide_icon(&mut canvas, *rect, ink, self.dpi.get()),
                        2 => trash_icon(&mut canvas, *rect, ink, self.dpi.get()),
                        _ => close_icon(&mut canvas, *rect, ink, self.dpi.get()),
                    }
                    if focused == self.buttons[index] {
                        focus_ring(&mut canvas, *rect, self.s(BUTTON / 2.0), self.s(2.0), palette);
                    }
                }
                // Footer: when it was edited, and the resize grip.
                let footer = RECT {
                    left: self.s(14.0),
                    top: client.bottom - self.s(FOOTER),
                    right: client.right - self.s(GRIP + 8.0),
                    bottom: client.bottom - self.s(3.0),
                };
                let label = self
                    .hub()
                    .store
                    .try_borrow()
                    .ok()
                    .and_then(|store| {
                        store.note(self.id).map(|note| {
                            edited_label(note.created_at, note.updated_at, SystemTime::now())
                        })
                    })
                    .unwrap_or_default();
                canvas.label(
                    footer,
                    &label,
                    mix(palette.paper, palette.muted, 0.85),
                    fonts.small_regular(self.dpi.get()),
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
                grip(&mut canvas, client, mix(palette.paper, palette.muted, 0.45), self.dpi.get());
            }
            canvas.finish_labels();
        }
        if origin == POINT::default() {
            self.asked.set(None);
            self.painted.set(Some(Instant::now()));
        }
        // SAFETY: both DCs are live; the whole client area is copied.
        unsafe {
            let _ = BitBlt(
                target,
                0,
                0,
                size.map_or(client.right - origin.x, |(width, _)| width),
                size.map_or(client.bottom - origin.y, |(_, height)| height),
                Some(surface.dc),
                origin.x,
                origin.y,
                SRCCOPY,
            );
        }
    }

    fn origin_glyph(&self, canvas: &mut editor::Canvas<'_>, rect: RECT, ink: COLORREF) {
        let kind = self.origin.borrow().kind;
        match kind {
            NoteKind::Desktop => {
                // A small monitor.
                let screen = RECT {
                    left: rect.left + self.s(1.0),
                    top: rect.top + self.s(1.0),
                    right: rect.right - self.s(1.0),
                    bottom: rect.bottom - self.s(4.0),
                };
                canvas.stroke(screen, self.s(2.0) as f32, ink);
                let stand = RECT {
                    left: rect.left + self.s(4.0),
                    top: rect.bottom - self.s(2.0),
                    right: rect.right - self.s(4.0),
                    bottom: rect.bottom - self.s(1.0),
                };
                canvas.fill(stand, 0.0, ink, 1.0);
            }
            NoteKind::Web => {
                // A globe.
                let r = (rect.right - rect.left).min(rect.bottom - rect.top);
                let circle = RECT {
                    left: rect.left,
                    top: rect.top,
                    right: rect.left + r,
                    bottom: rect.top + r,
                };
                canvas.stroke(circle, r as f32 / 2.0, ink);
                let band = RECT {
                    left: circle.left + r / 3,
                    top: circle.top,
                    right: circle.right - r / 3,
                    bottom: circle.bottom,
                };
                canvas.stroke(band, r as f32 / 3.0, ink);
            }
            NoteKind::App => {
                let r = (rect.right - rect.left).min(rect.bottom - rect.top);
                let square = RECT {
                    left: rect.left,
                    top: rect.top,
                    right: rect.left + r,
                    bottom: rect.top + r,
                };
                canvas.fill(square, self.s(3.0) as f32, ink, 0.7);
            }
        }
    }
}

impl GdiFonts {
    /// The footer's regular 11 px font, cached per DPI.
    fn small_regular(&self, dpi: u32) -> HFONT {
        thread_local! {
            static CACHE: RefCell<BTreeMap<u32, isize>> = const { RefCell::new(BTreeMap::new()) };
        }
        CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let handle = *cache.entry(dpi).or_insert_with(|| {
                // SAFETY: a static family name; the font is kept for the thread's lifetime.
                unsafe {
                    CreateFontW(
                        -px(11.0, dpi),
                        0,
                        0,
                        0,
                        400,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET,
                        OUT_DEFAULT_PRECIS,
                        CLIP_DEFAULT_PRECIS,
                        CLEARTYPE_QUALITY,
                        DEFAULT_PITCH.0.into(),
                        w!("Segoe UI Variable Text"),
                    )
                    .0 as isize
                }
            });
            HFONT(handle as *mut c_void)
        })
    }
}

fn text_width(font: HFONT, text: &str) -> i32 {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut size = windows::Win32::Foundation::SIZE::default();
    // SAFETY: a screen-compatible memory DC for measuring, released below.
    unsafe {
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, font.into());
        let _ = GetTextExtentPoint32W(dc, &units, &mut size);
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
    }
    size.cx
}

/// The settings window's rounded focus ring: a ring around the shape with a gap.
pub(super) fn focus_ring(
    canvas: &mut editor::Canvas<'_>,
    shape: RECT,
    radius: i32,
    gap: i32,
    palette: Palette,
) {
    let ring = gap.max(2);
    let outer = RECT {
        left: shape.left - gap - ring / 2,
        top: shape.top - gap - ring / 2,
        right: shape.right + gap + ring / 2,
        bottom: shape.bottom + gap + ring / 2,
    };
    canvas.stroke(outer, (radius + gap + ring / 2) as f32, palette.text);
}

pub(super) fn trash_icon(canvas: &mut editor::Canvas<'_>, rect: RECT, ink: COLORREF, dpi: u32) {
    let s = |v: f64| px(v, dpi);
    let (cx, cy) = ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
    let lid = RECT { left: cx - s(5.0), top: cy - s(4.5), right: cx + s(5.0), bottom: cy - s(3.5) };
    canvas.fill(lid, 0.0, ink, 1.0);
    let handle =
        RECT { left: cx - s(1.5), top: cy - s(6.0), right: cx + s(1.5), bottom: cy - s(4.5) };
    canvas.fill(handle, 0.0, ink, 1.0);
    let body =
        RECT { left: cx - s(3.8), top: cy - s(3.0), right: cx + s(3.8), bottom: cy + s(5.5) };
    canvas.stroke(body, s(1.5) as f32, ink);
}

/// The hide button: a short bar, as on a minimize button.
pub(super) fn hide_icon(canvas: &mut editor::Canvas<'_>, rect: RECT, ink: COLORREF, dpi: u32) {
    let s = |v: f64| px(v, dpi) as f32;
    let (cx, cy) = ((rect.left + rect.right) as f32 / 2.0, (rect.top + rect.bottom) as f32 / 2.0);
    canvas.line((cx - s(4.5), cy + s(0.5)), (cx + s(4.5), cy + s(0.5)), ink, s(1.5));
}

fn close_icon(canvas: &mut editor::Canvas<'_>, rect: RECT, ink: COLORREF, dpi: u32) {
    let s = |v: f64| px(v, dpi) as f32;
    let (cx, cy) = ((rect.left + rect.right) as f32 / 2.0, (rect.top + rect.bottom) as f32 / 2.0);
    canvas.cross(cx, cy, s(3.6), ink, s(1.5));
}

fn grip(canvas: &mut editor::Canvas<'_>, client: RECT, ink: COLORREF, dpi: u32) {
    let s = |v: f64| px(v, dpi) as f32;
    let (right, bottom) = ((client.right as f32) - s(4.0), (client.bottom as f32) - s(3.0));
    let origin = (right - s(GRIP), bottom - s(GRIP));
    canvas.line(
        (origin.0 + s(13.0), origin.1 + s(6.0)),
        (origin.0 + s(6.0), origin.1 + s(13.0)),
        ink,
        s(1.2),
    );
    canvas.line(
        (origin.0 + s(13.0), origin.1 + s(10.0)),
        (origin.0 + s(10.0), origin.1 + s(13.0)),
        ink,
        s(1.2),
    );
}

fn open_in_shell(target: String) {
    // Shell calls can wait on other processes: on a short-lived worker, never the notes thread.
    thread::spawn(move || {
        let wide: Vec<u16> = target.encode_utf16().chain([0]).collect();
        // SAFETY: COM for this worker, a NUL-terminated target, no window.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED
                    | windows::Win32::System::Com::COINIT_DISABLE_OLE1DDE,
            );
            ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
        }
    });
}

/// Swift `openOrigin`: back to where the note was written.
fn open_origin(origin: &NoteOrigin) {
    match origin.kind {
        NoteKind::Desktop => open_in_shell("shell:Desktop".to_owned()),
        NoteKind::Web => {
            if let Some(url) = &origin.url {
                open_in_shell(url.clone());
            }
        }
        NoteKind::App => {
            if let Some(url) = origin.url.as_deref().and_then(|url| url.strip_prefix("file:///")) {
                open_in_shell(url.replace("%20", " "));
            } else if let Some(exe) =
                origin.bundle_id.as_deref().filter(|path| Path::new(path).is_file())
            {
                open_in_shell(exe.to_owned());
            }
        }
    }
}

unsafe extern "system" fn button_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: the parent attached its state pointer; its children are destroyed first.
    let window = unsafe { &*(data as *const NoteWindow) };
    match msg {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: the button paints its part of the parent's card (the parent clips its
            // children, so it cannot draw there itself); the origin maps the button into it.
            // BUTTON has CS_PARENTDC: its paint DC is not clipped to the button, so the copy is
            // limited to the button's own rectangle (the whole card copied from here covered
            // the editor and its text vanished whenever a button repainted, on hover).
            unsafe {
                let dc = BeginPaint(hwnd, &mut paint);
                let mut origin = [POINT::default()];
                MapWindowPoints(Some(hwnd), Some(window.hwnd), &mut origin);
                let mut own = RECT::default();
                let _ = GetClientRect(hwnd, &mut own);
                if !dc.is_invalid() {
                    let saved = SaveDC(dc);
                    IntersectClipRect(dc, 0, 0, own.right, own.bottom);
                    window.paint_at(dc, origin[0], Some((own.right, own.bottom)));
                    let _ = RestoreDC(dc, saved);
                }
                let _ = EndPaint(hwnd, &paint);
            }
            return LRESULT(0);
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            // The wheel scrolls the text wherever over the note it turns.
            if let Some(editor) = window.editor.borrow().as_ref() {
                // SAFETY: the same message, to our own editor control.
                unsafe { SendMessageW(editor.hwnd(), msg, Some(wp), Some(lp)) };
            }
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCDESTROY => {
            // SAFETY: removes this subclass on the last message.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(button_proc), id) };
        }
        WM_MOUSEMOVE => {
            let index = window.buttons.iter().position(|button| *button == hwnd);
            window.pointer_moved();
            window.set_hot(index);
            let mut request = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: a leave request for our own child.
            let _ = unsafe { TrackMouseEvent(&mut request) };
        }
        WM_MOUSELEAVE => {
            window.set_hot(None);
            window.pointer_left();
        }
        _ => {}
    }
    // SAFETY: the native button handles the rest (focus, keyboard, accessibility).
    let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    if matches!(
        msg,
        WM_SETFOCUS
            | WM_KILLFOCUS
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | BM_SETSTATE
            | WM_CAPTURECHANGED
    ) {
        window.update_buttons_shown();
        window.invalidate();
    }
    result
}

unsafe extern "system" fn note_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the window's state while it lives (0 before and after).
    let window = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const NoteWindow).as_ref() };
    // SAFETY: default handling.
    let default = || unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    let Some(window) = window else {
        if msg == WM_NCCALCSIZE && wp.0 != 0 {
            return LRESULT(0);
        }
        return default();
    };
    match msg {
        WM_NCCALCSIZE if wp.0 != 0 => return LRESULT(0),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            // The wheel scrolls the text wherever over the note it turns.
            if let Some(editor) = window.editor.borrow().as_ref() {
                // SAFETY: the same message, to our own editor control.
                unsafe { SendMessageW(editor.hwnd(), msg, Some(wp), Some(lp)) };
            }
            return LRESULT(0);
        }
        WM_NCHITTEST => {
            let (x, y) = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
            return LRESULT(window.hit_test(x, y) as isize);
        }
        WM_NCACTIVATE => return LRESULT(1),
        WM_WINDOWPOSCHANGING if matches!(window.window_motion.get(), Motion::Still) => {
            // A drag is held at the minimum by WM_GETMINMAXINFO; a move or size from code
            // (SetWindowPos) is held here. The grow animation starts small on purpose.
            let position = lp.0 as *mut WINDOWPOS;
            // SAFETY: Windows passes a valid WINDOWPOS for this message; we may change it.
            if let Some(position) = unsafe { position.as_mut() }
                && position.flags & SWP_NOSIZE == SET_WINDOW_POS_FLAGS(0)
            {
                let (min_w, min_h) = (window.s(MIN_NOTE_SIZE.0), window.s(MIN_NOTE_SIZE.1));
                if position.cx < min_w {
                    position.cx = min_w;
                }
                if position.cy < min_h {
                    position.cy = min_h;
                }
            }
            return default();
        }
        WM_GETMINMAXINFO => {
            let info = lp.0 as *mut MINMAXINFO;
            if !info.is_null() {
                // SAFETY: Windows passes a valid MINMAXINFO for this call.
                unsafe {
                    (*info).ptMinTrackSize =
                        POINT { x: window.s(MIN_NOTE_SIZE.0), y: window.s(MIN_NOTE_SIZE.1) };
                }
            }
            return LRESULT(0);
        }
        WM_SIZE => {
            if matches!(window.window_motion.get(), Motion::Still) {
                window.layout();
            }
            if let Ok(mut surface) = window.surface.try_borrow_mut() {
                *surface = None;
            }
        }
        WM_WINDOWPOSCHANGED => {
            let result = default();
            if matches!(window.window_motion.get(), Motion::Still) && !window.closing.get() {
                let frame = window.card_rect();
                let hub = window.hub();
                let stored = hub
                    .store
                    .try_borrow()
                    .ok()
                    .and_then(|store| store.note(window.id).map(|note| note.frame));
                if stored.is_some_and(|stored| stored != frame) {
                    hub.update(window.id, |note| note.frame = frame);
                }
                hub.update_zones();
            }
            return result;
        }
        WM_DPICHANGED => {
            {
                window.set_dpi((wp.0 & 0xffff) as u32);
                if lp.0 != 0 {
                    // SAFETY: lParam points to the suggested rectangle for this message.
                    let r = unsafe { *(lp.0 as *const RECT) };
                    // SAFETY: our window.
                    let _ = unsafe {
                        SetWindowPos(
                            hwnd,
                            None,
                            r.left,
                            r.top,
                            r.right - r.left,
                            r.bottom - r.top,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        )
                    };
                }
            }
            return LRESULT(0);
        }
        WM_ACTIVATE => {
            let active = (wp.0 & 0xffff) as u32 != WA_INACTIVE;
            let hub = window.hub();
            window.redraw_all();
            if active {
                hub.active.set(Some(window.id));
                hub.update_zones();
                // Swift refreshes from disk when the menu opens; a note also checks its file when
                // it is activated (not on a timer).
                hub.refresh_from_disk();
                let saved = window.saved_focus.get();
                // SAFETY: focus within our own window.
                unsafe {
                    let target = if !saved.is_invalid() && IsChild(hwnd, saved).as_bool() {
                        Some(saved)
                    } else {
                        window.editor.borrow().as_ref().map(Editor::hwnd)
                    };
                    if let Some(target) = target {
                        let _ = SetFocus(Some(target));
                    }
                }
                if let Some(editor) = window.editor.borrow().as_ref() {
                    editor.set_focused(true);
                }
            } else {
                // SAFETY: focus query on this thread.
                window.saved_focus.set(unsafe { GetFocus() });
                if let Some(editor) = window.editor.borrow().as_ref() {
                    editor.set_focused(false);
                }
                window.invalidate();
            }
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_ACTIVATE as isize),
        WM_SETFOCUS => {
            if let Some(editor) = window.editor.borrow().as_ref() {
                editor.focus();
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE | WM_NCMOUSEMOVE => window.pointer_moved(),
        WM_MOUSELEAVE | WM_NCMOUSELEAVE => window.pointer_left(),
        WM_COMMAND => {
            let code = (wp.0 >> 16) as u32;
            let id = (wp.0 & 0xffff) as i32;
            if id == EDITOR_ID {
                if let Some(editor) = window.editor.borrow().as_ref() {
                    editor.notify(code);
                }
                return LRESULT(0);
            }
            if code == BN_CLICKED {
                let hub = window.hub();
                match id {
                    ORIGIN_ID => open_origin(&window.origin.borrow()),
                    HIDE_ID => {
                        let _ = hub.commands.send(Command::Hide(window.id));
                    }
                    // Swift deletes without asking (the file goes to the Trash, so it can come back).
                    DELETE_ID => {
                        let _ = hub.commands.send(Command::Delete(window.id));
                    }
                    CLOSE_ID => {
                        let _ = hub.commands.send(Command::PutAway(window.id));
                    }
                    // Escape on a header button goes back to the text.
                    2 => {
                        if let Some(editor) = window.editor.borrow().as_ref() {
                            editor.focus();
                        }
                    }
                    _ => {}
                }
                // SAFETY: handled after this message returns, outside the window's own procedure.
                let _ = unsafe { PostMessageW(Some(hub.hwnd), COMMAND, WPARAM(0), LPARAM(0)) };
                return LRESULT(0);
            }
        }
        WM_NOTIFY => {
            let header = lp.0 as *const windows::Win32::UI::Controls::NMHDR;
            // SAFETY: WM_NOTIFY passes a valid NMHDR for the call.
            if !header.is_null() && unsafe { (*header).idFrom } == EDITOR_ID as usize {
                // SAFETY: as above.
                let code = unsafe { (*header).code };
                if let Some(editor) = window.editor.borrow().as_ref() {
                    editor.notify(code);
                }
                return LRESULT(0);
            }
        }
        EDITOR_EVENTS => {
            window.handle_editor_events();
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == MOTION_TIMER => {
            window.step();
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == WATCHDOG_TIMER || wp.0 == RETRY_TIMER => {
            // SAFETY: one-shot timers of our own window.
            let _ = unsafe { KillTimer(Some(hwnd), wp.0) };
            if wp.0 == RETRY_TIMER {
                window.redraw_all();
            } else {
                window.watchdog();
            }
            return LRESULT(0);
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            let hub = window.hub();
            hub.set_motion(editor::animation_effects());
            hub.apply_theme();
            window.redraw_all();
        }
        WM_DISPLAYCHANGE | WM_DWMCOMPOSITIONCHANGED => {
            // A new display layout or a restarted compositor: the bitmaps are made again on
            // the next paint, and everything is drawn.
            if let Ok(mut surface) = window.surface.try_borrow_mut() {
                *surface = None;
            }
            window.redraw_all();
        }
        WM_SHOWWINDOW if wp.0 != 0 => window.redraw_all(),
        WM_CTLCOLORBTN => {
            // SAFETY: an empty brush; the parent paints the buttons.
            return LRESULT(unsafe { GetStockObject(NULL_BRUSH) }.0 as isize);
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: BeginPaint/EndPaint pair for our window.
            let dc = unsafe { BeginPaint(hwnd, &mut paint) };
            if !dc.is_invalid() {
                window.paint(dc);
            }
            // SAFETY: matches BeginPaint.
            let _ = unsafe { EndPaint(hwnd, &paint) };
            return LRESULT(0);
        }
        WM_PRINTCLIENT => {
            window.paint(HDC(wp.0 as *mut c_void));
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_CLOSE => {
            let _ = window.hub().commands.send(Command::PutAway(window.id));
            // SAFETY: the hub's message window, on this thread.
            let _ = unsafe { PostMessageW(Some(window.hub().hwnd), COMMAND, WPARAM(0), LPARAM(0)) };
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            // SAFETY: detach before late messages.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        }
        _ => {}
    }
    default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_note_starts_at_the_pill_and_stays_on_screen() {
        let work = Rect::new(0.0, 0.0, 1920.0, 1040.0);
        let frame = new_note_frame(Point::new(500.0, 400.0), work, 1.0);
        assert_eq!(frame, Rect::new(477.0, 383.0, 304.0, 224.0));
        let corner = new_note_frame(Point::new(1910.0, 1030.0), work, 1.25);
        assert_eq!(corner, Rect::new(1540.0, 760.0, 380.0, 280.0));
        let left =
            new_note_frame(Point::new(-5.0, -5.0), Rect::new(-1920.0, 0.0, 1920.0, 1080.0), 1.0);
        assert_eq!(left.x + left.width, 0.0);
    }

    #[test]
    fn saved_frames_are_kept_or_brought_back() {
        let works = [Rect::new(0.0, 0.0, 1920.0, 1040.0), Rect::new(1920.0, 0.0, 1280.0, 1000.0)];
        let min = (190.0, 120.0);
        let kept = Rect::new(100.0, 100.0, 304.0, 224.0);
        assert_eq!(clamp_frame(kept, &works, min), kept);
        let partly = Rect::new(1800.0, 100.0, 304.0, 224.0);
        assert_eq!(clamp_frame(partly, &works, min), partly, "spans two monitors: kept");
        let lost = Rect::new(5000.0, 3000.0, 304.0, 224.0);
        let back = clamp_frame(lost, &works, min);
        assert_eq!(back, Rect::new(2896.0, 776.0, 304.0, 224.0));
        let above = Rect::new(100.0, -200.0, 304.0, 224.0);
        assert_eq!(clamp_frame(above, &works, min).y, 0.0);
        let tiny = Rect::new(100.0, 100.0, 50.0, 40.0);
        assert_eq!(clamp_frame(tiny, &works, min).width, 190.0);
        let huge = Rect::new(-9000.0, 0.0, 4000.0, 3000.0);
        let fitted = clamp_frame(huge, &works, min);
        assert!(fitted.width <= 1920.0 && fitted.height <= 1040.0);
    }

    #[test]
    fn the_footer_says_when() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert_eq!(edited_label(t0, t0, at(5)), "Created just now");
        assert_eq!(edited_label(t0, at(30), at(40)), "Edited just now");
        assert_eq!(edited_label(t0, at(30), at(30 + 61)), "Edited 1 minute ago");
        assert_eq!(edited_label(t0, at(30), at(30 + 7200)), "Edited 2 hours ago");
        assert_eq!(edited_label(t0, at(30), at(30 + 90_000)), "Edited yesterday");
        assert_eq!(edited_label(t0, at(30), at(30 + 3 * 86_400)), "Edited 3 days ago");
        assert_eq!(edited_label(t0, at(30), at(30 + 30 * 86_400)), "Edited 4 weeks ago");
    }

    #[test]
    fn the_spring_settles_with_a_small_overshoot() {
        assert_eq!(spring(0.0), 0.0);
        let peak = (1..60).map(|i| spring(f64::from(i) / 100.0)).fold(0.0, f64::max);
        assert!(peak > 1.0 && peak < 1.05, "{peak}");
        assert!((spring(APPEAR_SECONDS) - 1.0).abs() < 0.01);
        assert!(spring(0.1) > 0.5);
    }

    #[test]
    fn attachments_follow_the_swift_names() {
        let id = NoteId::parse("6F9A1B2C-3D4E-4F5A-6B7C-8D9EAFB0C1D2").unwrap();
        assert_eq!(attachment_name("shot", id, "png"), "shot-6f9a1b2c.png");
        assert_eq!(attachment_name("recording", id, "mp4"), "recording-6f9a1b2c.mp4");
        assert_eq!(
            attachment_link("shot-6f9a1b2c.png", false),
            "![screenshot](attachments/shot-6f9a1b2c.png)"
        );
        assert_eq!(
            attachment_link("recording-6f9a1b2c.mp4", true),
            "[recording](attachments/recording-6f9a1b2c.mp4)"
        );
        assert_eq!(taken_hint(1), "added to note");
        assert_eq!(taken_hint(3), "added 3 to note");
    }

    #[test]
    fn drop_zones_answer_for_points() {
        let zones = DropZones::default();
        let id = NoteId::parse("6F9A1B2C-3D4E-4F5A-6B7C-8D9EAFB0C1D2").unwrap();
        zones.set(vec![NoteZone {
            id,
            rect: Rect::new(10.0, 10.0, 100.0, 80.0),
            hwnd: 1,
            active: true,
        }]);
        assert_eq!(zones.note_at(Point::new(50.0, 50.0)), Some(id));
        assert_eq!(zones.note_at(Point::new(5.0, 50.0)), None);
        assert_eq!(zones.note_at(Point::new(200.0, 50.0)), None);
    }
}
