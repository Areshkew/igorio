//! The first-run guide: one card window in the settings window's style that shows each
//! gesture with a looping picture, its keys, and a "Try it now" line that turns into a green
//! "Done" when the user does the gesture.
//!
//! It opens on start until settings.json says `guide_seen`, and from the tray's "Guide…" any
//! time. Like the settings window it runs on its own thread with its own message loop, so it
//! never blocks the overlay loop or a hook thread. It is not topmost and never takes input from
//! the rest of the screen: the gestures keep working while it is open, which is the point.
//! The UI loop tells it what the user did ([`GuideWindow::tried`]) and whether cards are in
//! hand ([`GuideWindow::set_holding`]).

pub mod scenes;

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use tiny_skia::{Pixmap, PixmapPaint, Transform};
use windows::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
    WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::Controls::{
    ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE, VK_LEFT,
    VK_RETURN, VK_RIGHT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, PCWSTR, Result, w};

use crate::notes::summoner::{
    self, ANCHOR, Bounds, MENU_PAD, MENU_SHADOW, MenuPicture, ToolContext, ToolId,
};
use crate::overlay::{cursor_pos, monitor_at};
use igorio_core::geom::Point;
use crate::settings::Settings;
use crate::settings_window::{
    ButtonMotion, Canvas, FOCUS_MS, LiveSettings, OPEN_MS, PAGE_MS, Palette,
    Surface, THEME_MS, Transition, animation_effects, chord_chips, draw_text, font, inset,
    key_chip, key_chip_width, mix, move_y, paint_focus, rgb, scale, scaled, settings_icon,
    system_dark, wide, with_paint,
};
use crate::shortcuts::{Action, Shortcuts, display_for_layout};
use scenes::{Colors, Scene, Stage};

// ---------------------------------------------------------------------------------------
// What the guide shows (pure)
// ---------------------------------------------------------------------------------------

/// One page of the guide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Welcome,
    Screenshot,
    Drop,
    Shake,
    ToolMenu,
    Record,
}

/// Which capabilities are switched on (and built), for the page list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    pub screenshots: bool,
    /// Notes, and with them the tool menu (the summoner runs with the notes).
    pub notes: bool,
    pub recording: bool,
}

impl Features {
    pub fn of(settings: &Settings) -> Features {
        let live = LiveSettings::from(settings.clone());
        Features {
            screenshots: live.screenshot_detector,
            notes: live.notes,
            recording: live.recording && Action::ToggleRecording.available(),
        }
    }
}

/// The pages for what is switched on, in order. Dropping and shaking need
/// something to carry: a screenshot or a recording.
pub fn pages(features: Features) -> Vec<Page> {
    let mut pages = vec![Page::Welcome];
    if features.screenshots {
        pages.push(Page::Screenshot);
    }
    if features.screenshots || features.recording {
        pages.extend([Page::Drop, Page::Shake]);
    }
    if features.notes {
        pages.push(Page::ToolMenu);
    }
    if features.recording {
        pages.push(Page::Record);
    }
    pages
}

/// Something the user did that a page asks them to try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tried {
    /// A screenshot card landed on the pile.
    CardLanded,
    /// A double-click pasted or saved what was carried.
    Dropped,
    /// A shake let go of a card.
    Shaken,
    /// The tool menu opened.
    MenuOpened,
    /// A recording started.
    RecordingStarted,
}

impl Page {
    /// What the page's "Try it now" waits for.
    pub fn tried_by(self) -> Option<Tried> {
        match self {
            Page::Welcome => None,
            Page::Screenshot => Some(Tried::CardLanded),
            Page::Drop => Some(Tried::Dropped),
            Page::Shake => Some(Tried::Shaken),
            Page::ToolMenu => Some(Tried::MenuOpened),
            Page::Record => Some(Tried::RecordingStarted),
        }
    }

    fn scene(self) -> Option<Scene> {
        match self {
            Page::Welcome => None,
            Page::Screenshot => Some(Scene::Screenshot),
            Page::Drop => Some(Scene::Drop),
            Page::Shake => Some(Scene::Shake),
            Page::ToolMenu => Some(Scene::ToolMenu),
            Page::Record => Some(Scene::Record),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Page::Welcome => "Igorio",
            Page::Screenshot => "Screenshot",
            Page::Drop => "Double-click to drop",
            Page::Shake => "Shake to let go",
            Page::ToolMenu => "Tool menu",
            Page::Record => "Record",
        }
    }
}

/// What the user has done while this guide is showing. Kept for this showing only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tries {
    done: [bool; 5],
}

impl Tries {
    fn slot(tried: Tried) -> usize {
        tried as usize
    }

    /// Notes one thing the user did. True when it was not done before.
    pub fn note(&mut self, tried: Tried) -> bool {
        !std::mem::replace(&mut self.done[Self::slot(tried)], true)
    }

    pub fn done(&self, page: Page) -> bool {
        page.tried_by().is_some_and(|tried| self.done[Self::slot(tried)])
    }
}

/// A key chip, or a plain word between chips ("or", "Hold").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub label: String,
    pub chip: bool,
}

fn chip(label: &str) -> Key {
    Key { label: label.to_owned(), chip: true }
}

fn word(label: &str) -> Key {
    Key { label: label.to_owned(), chip: false }
}

fn chord_keys(shortcuts: &Shortcuts, action: Action) -> Option<Vec<Key>> {
    let chord = shortcuts.get(action)?;
    Some(chord_chips(&chord).iter().map(|label| chip(label)).collect())
}

/// The words and keys of one page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageView {
    pub keys: Vec<Key>,
    pub line: String,
    /// More keys that belong to the page, each with what it does (the recording's).
    pub more: Vec<(Vec<Key>, &'static str)>,
    /// One short extra line.
    pub aside: Option<&'static str>,
}

/// What a page says, with the user's own shortcuts.
pub fn view(page: Page, features: Features, shortcuts: &Shortcuts) -> PageView {
    let mut view = PageView { keys: Vec::new(), line: String::new(), more: Vec::new(), aside: None };
    match page {
        Page::Welcome => {
            view.line =
                "A cursor that helps with little things: screenshots, notes and recordings.".into();
        }
        Page::Screenshot => {
            view.keys = vec![chip("Win"), chip("Shift"), chip("S"), word("or"), chip("PrtScn")];
            view.line = "Take a screenshot. Your cursor picks it up and carries it.".into();
        }
        Page::Drop => {
            view.keys = vec![chip("double-click")];
            view.line = "A text box pastes it. A folder or the desktop saves it.".into();
        }
        Page::Shake => {
            view.keys = vec![chip("shake")];
            view.line = "Shake the cursor to drop the top card. Keep shaking to drop them all.".into();
        }
        Page::ToolMenu => {
            view.keys = vec![word("hold"), chip("Alt")];
            view.line = if features.recording {
                "Hold Alt anywhere except a text box. Pick New note or Record screen, or Pin to screen with a card in hand."
            } else {
                "Hold Alt anywhere except a text box. Pick New note, or Pin to screen with a card in hand."
            }
            .into();
            view.aside = Some("Hide a note with its hide button. Notes… in the tray lists them all.");
        }
        Page::Record => {
            view.keys = chord_keys(shortcuts, Action::ToggleRecording).unwrap_or_default();
            view.line = "Records the screen under your cursor. Press it again to stop; your cursor carries the video.".into();
            view.more.push((vec![chip("Alt"), chip("scroll")], "zoom"));
            for (action, does) in [(Action::Pen, "pen"), (Action::Highlighter, "highlighter")] {
                if let Some(keys) = chord_keys(shortcuts, action) {
                    view.more.push((keys, does));
                }
            }
        }
    }
    view
}

/// The action after "Try it now" (`None` on the welcome page). With nothing in hand, the
/// pages that need a card say to take a screenshot first.
pub fn try_action(page: Page, holding: bool, shortcuts: &Shortcuts) -> Option<String> {
    let text = match page {
        Page::Welcome => return None,
        Page::Screenshot => "Press Win+Shift+S and pick an area.".to_owned(),
        Page::Drop if holding => "Double-click a text box, a folder or the desktop.".to_owned(),
        Page::Drop => "Take a screenshot first, then double-click a text box.".to_owned(),
        Page::Shake if holding => "Shake the cursor to drop the card.".to_owned(),
        Page::Shake => "Take a screenshot first, then shake the cursor.".to_owned(),
        Page::ToolMenu => "Hold Alt over another window or the desktop.".to_owned(),
        Page::Record => match shortcuts.get(Action::ToggleRecording) {
            Some(chord) => format!("Press {} to start, and again to stop.", display_for_layout(&chord)),
            None => "Choose Record Screen in the tray menu.".to_owned(),
        },
    };
    Some(text)
}

// ---------------------------------------------------------------------------------------
// The window's endpoint on the UI loop
// ---------------------------------------------------------------------------------------

const CLASS: PCWSTR = w!("IgorioGuide");
const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);
const EX_STYLE: WINDOW_EX_STYLE = WS_EX_APPWINDOW;
/// Bring the open guide to the front.
const OPEN: u32 = WM_APP + 1;
/// Something arrived in [`Shared::inbox`].
const INBOX: u32 = WM_APP + 2;
const TEST_FREEZE: u32 = WM_APP + 10;
const TEST_PAGE: u32 = WM_APP + 11;
const TEST_IDLE: u32 = WM_APP + 12;
const WORKER_EXITED: isize = -1;

/// What the UI loop hands the guide thread.
#[derive(Default)]
struct Inbox {
    tried: Vec<Tried>,
    holding: bool,
}

struct Shared {
    hwnd: AtomicIsize,
    stop: AtomicBool,
    inbox: Mutex<Inbox>,
    /// The last value given to [`GuideWindow::set_holding`].
    holding: AtomicBool,
    /// The window is on screen; taken once by [`GuideWindow::take_shown`].
    shown: AtomicBool,
}

impl Shared {
    fn post(&self, message: u32, wp: usize) {
        let hwnd = self.hwnd.load(SeqCst);
        if hwnd != 0 && hwnd != WORKER_EXITED {
            // SAFETY: scalar message to this worker's window. If it is gone, posting fails.
            let _ = unsafe {
                PostMessageW(Some(HWND(hwnd as *mut c_void)), message, WPARAM(wp), LPARAM(0))
            };
        }
    }

    fn exited(&self) -> bool {
        self.hwnd.load(SeqCst) == WORKER_EXITED
    }
}

/// Isolated window configuration for the self-test. Test windows show without activation.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct TestMode {
    pub dark: bool,
    pub dpi: u32,
    pub motion: bool,
    /// A point on the monitor to open on (physical pixels); `None` is the cursor's monitor.
    pub at: Option<(i32, i32)>,
}

#[derive(Clone)]
struct Options {
    settings: Settings,
    test: Option<TestMode>,
}

/// Overlay-owned endpoint of the guide window: open only spawns or posts.
#[derive(Default)]
pub struct GuideWindow {
    test: Option<TestMode>,
    worker: Option<(Arc<Shared>, JoinHandle<()>)>,
}

impl GuideWindow {
    pub fn new() -> Self {
        Self::default()
    }

    #[doc(hidden)]
    pub fn for_test(test: TestMode) -> Self {
        Self { test: Some(test), worker: None }
    }

    fn running(&self) -> Option<&Arc<Shared>> {
        self.worker
            .as_ref()
            .filter(|(shared, thread)| !thread.is_finished() && !shared.exited())
            .map(|(shared, _)| shared)
    }

    /// Opens the guide with these settings' pages and shortcuts, or brings the open one to
    /// the front (its pages and what was tried stay as they are).
    pub fn open(&mut self, settings: &Settings, holding: bool) -> io::Result<()> {
        if let Some(shared) = self.running() {
            shared.post(OPEN, 0);
            return Ok(());
        }
        let shared = Arc::new(Shared {
            hwnd: AtomicIsize::new(0),
            stop: AtomicBool::new(false),
            inbox: Mutex::new(Inbox { tried: Vec::new(), holding }),
            holding: AtomicBool::new(holding),
            shown: AtomicBool::new(false),
        });
        let options = Options { settings: settings.clone(), test: self.test };
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new().name("guide".into()).spawn(move || {
            /// Marks the worker gone on every way out, unwinding included.
            struct Exit(Arc<Shared>);
            impl Drop for Exit {
                fn drop(&mut self) {
                    self.0.hwnd.store(WORKER_EXITED, SeqCst);
                }
            }
            let exit = Exit(worker);
            if let Err(error) = run_window(&options, &exit.0) {
                eprintln!("could not open the guide: {error}");
            }
        })?;
        self.worker = Some((shared, thread));
        Ok(())
    }

    /// The user did something a page asks for. Ignored while the guide is closed.
    pub fn tried(&self, tried: Tried) {
        if let Some(shared) = self.running() {
            shared.inbox.lock().unwrap_or_else(|e| e.into_inner()).tried.push(tried);
            shared.post(INBOX, 0);
        }
    }

    /// Whether cards are in hand. Cheap to call every pass: it posts only on a change.
    pub fn set_holding(&self, holding: bool) {
        if let Some(shared) = self.running()
            && shared.holding.swap(holding, SeqCst) != holding
        {
            shared.inbox.lock().unwrap_or_else(|e| e.into_inner()).holding = holding;
            shared.post(INBOX, 0);
        }
    }

    /// True once after the window of the last `open` came on screen (for `guide_seen`).
    pub fn take_shown(&self) -> bool {
        self.worker.as_ref().is_some_and(|(shared, _)| shared.shown.swap(false, SeqCst))
    }

    /// Closes the window, waiting at most 950 ms for its thread.
    pub fn close(&mut self) -> bool {
        let Some((shared, thread)) = self.worker.take() else {
            return true;
        };
        shared.stop.store(true, SeqCst);
        shared.post(WM_CLOSE, 0);
        // SAFETY: JoinHandle keeps the borrowed thread handle live throughout the wait.
        let done = unsafe { WaitForSingleObject(HANDLE(thread.as_raw_handle()), 950) };
        done == WAIT_OBJECT_0 && thread.join().is_ok()
    }

    #[doc(hidden)]
    pub fn test_hwnd(&self) -> Option<HWND> {
        self.test.as_ref()?;
        let hwnd = self.running()?.hwnd.load(SeqCst);
        (hwnd != 0).then_some(HWND(hwnd as *mut c_void))
    }

    /// Test only: ends every motion and stops the clock; the scene shows `scene_ms` into its
    /// loop (or its still frame when `None`).
    #[doc(hidden)]
    pub fn test_freeze(&self, scene_ms: Option<u32>) {
        if let Some(hwnd) = self.test_hwnd() {
            let wp = scene_ms.map_or(usize::MAX, |ms| ms as usize);
            // SAFETY: scalar test command to our own guide window.
            unsafe { SendMessageW(hwnd, TEST_FREEZE, Some(WPARAM(wp)), None) };
        }
    }

    /// Test only: the page shown, by its place in the list.
    #[doc(hidden)]
    pub fn test_page(&self) -> Option<usize> {
        let hwnd = self.test_hwnd()?;
        // SAFETY: scalar query to our own guide window.
        Some(unsafe { SendMessageW(hwnd, TEST_PAGE, None, None) }.0 as usize)
    }

    /// Test only: no timer runs.
    #[doc(hidden)]
    pub fn test_idle(&self) -> bool {
        self.test_hwnd().is_some_and(|hwnd| {
            // SAFETY: scalar query to our own guide window.
            unsafe { SendMessageW(hwnd, TEST_IDLE, None, None) }.0 == 1
        })
    }
}

impl Drop for GuideWindow {
    fn drop(&mut self) {
        if !self.close() {
            eprintln!("guide thread did not exit within the quit budget");
        }
    }
}

// ---------------------------------------------------------------------------------------
// Layout, in logical pixels at 96 DPI
// ---------------------------------------------------------------------------------------

const WIDTH: i32 = 520;
const HEIGHT: i32 = 500;
/// The scene's stage, the size of [`scenes::STAGE`].
const STAGE: [i32; 4] = [40, 20, 440, 210];
const TITLE: [i32; 4] = [24, 244, 472, 30];
const LINE: [i32; 4] = [40, 280, 440, 40];
const MORE_TOP: i32 = 330;
const MORE_PITCH: i32 = 28;
const TRY: [i32; 4] = [40, 392, 440, 44];
const LOGO: [i32; 4] = [228, 70, 64, 64];
const NAME: [i32; 4] = [24, 144, 472, 40];
const TAGLINE: [i32; 4] = [40, 188, 440, 40];
const WHERE: [i32; 4] = [40, 256, 440, 96];
const FOOTER_TOP: i32 = 450;
const BUTTON_HEIGHT: i32 = 32;
const BACK: [i32; 4] = [24, FOOTER_TOP, 84, BUTTON_HEIGHT];
const NEXT: [i32; 4] = [412, FOOTER_TOP, 84, BUTTON_HEIGHT];
/// Each page dot's slot: it is clickable across the whole slot.
const DOT_SLOT: i32 = 20;
const MAX_PAGES: usize = 6;
const DONE_MS: f64 = 220.0;
const TIMER: usize = 1;

/// Control ids: Back, Next, and the dots from [`DOT_ID`].
#[doc(hidden)]
pub const BACK_ID: i32 = 100;
#[doc(hidden)]
pub const NEXT_ID: i32 = 101;
#[doc(hidden)]
pub const DOT_ID: i32 = 110;
const CONTROLS: usize = 2 + MAX_PAGES;

fn control_index(id: i32) -> Option<usize> {
    match id {
        BACK_ID => Some(0),
        NEXT_ID => Some(1),
        id if (DOT_ID..DOT_ID + MAX_PAGES as i32).contains(&id) => Some(2 + (id - DOT_ID) as usize),
        _ => None,
    }
}

/// A control's rectangle, logical pixels, with `pages` dots.
fn control_rect(id: i32, pages: usize) -> [i32; 4] {
    match id {
        BACK_ID => BACK,
        NEXT_ID => NEXT,
        _ => {
            let index = id - DOT_ID;
            let left = (WIDTH - pages as i32 * DOT_SLOT) / 2 + index * DOT_SLOT;
            [left, FOOTER_TOP, DOT_SLOT, BUTTON_HEIGHT]
        }
    }
}

/// The client rectangle of a control at `dpi`, for the self-test.
#[doc(hidden)]
pub fn test_control_rect(id: i32, pages: usize, dpi: u32) -> RECT {
    scaled(control_rect(id, pages), dpi)
}

#[doc(hidden)]
pub fn test_client_size(dpi: u32) -> (i32, i32) {
    (scale(WIDTH, dpi), scale(HEIGHT, dpi))
}

const GREEN_LIGHT: COLORREF = rgb(0x1a7f37);
const GREEN_DARK: COLORREF = rgb(0x6fd78f);

/// [big name, page title, heading, body, chip]
struct Fonts([HFONT; 5]);

impl Fonts {
    fn new(dpi: u32) -> Self {
        Self([(26, 600), (19, 600), (14, 600), (13, 400), (12, 600)].map(|(size, weight)| font(size, weight, dpi)))
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for font in self.0 {
            // SAFETY: our fonts are only selected into synchronous paint DCs and restored there.
            let _ = unsafe { DeleteObject(font.into()) };
        }
    }
}

const BIG: usize = 0;
const TITLE_FONT: usize = 1;
const HEADING: usize = 2;
const BODY: usize = 3;
const CHIP: usize = 4;

// ---------------------------------------------------------------------------------------
// Motion
// ---------------------------------------------------------------------------------------

struct Motion {
    enabled: bool,
    intro: Transition,
    theme: Transition,
    /// The shown page's content fading and sliding in.
    content: Transition,
    /// The current dot, as a place along the dots.
    dots: Transition,
    /// The shown page's "Try it now" turning into "Done".
    done: Transition,
    buttons: [ButtonMotion; CONTROLS],
}

impl Motion {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            intro: Transition::default(),
            theme: Transition::default(),
            content: Transition { from: 1.0, to: 1.0, ..Transition::default() },
            dots: Transition::default(),
            done: Transition::default(),
            buttons: [ButtonMotion::default(); CONTROLS],
        }
    }

    fn duration(&self, duration: f64) -> f64 {
        if self.enabled { duration } else { 0.0 }
    }

    fn tracks(&self) -> impl Iterator<Item = Transition> + '_ {
        [self.intro, self.theme, self.content, self.dots, self.done]
            .into_iter()
            .chain(self.buttons.iter().flat_map(ButtonMotion::tracks))
    }

    fn active(&self, now: f64) -> bool {
        self.enabled && self.tracks().any(|track| track.active(now))
    }

    fn settle(&mut self) {
        let snap = |track: &mut Transition| track.retarget(track.to, 0.0, 0.0);
        for track in [&mut self.intro, &mut self.theme, &mut self.content, &mut self.dots, &mut self.done] {
            snap(track);
        }
        for button in &mut self.buttons {
            for track in [&mut button.hover, &mut button.press, &mut button.focus] {
                snap(track);
            }
        }
    }

    fn frame(&self, now: f64) -> Frame {
        Frame {
            intro: self.intro.value(now),
            theme: self.theme.value(now),
            content: self.content.value(now),
            dots: self.dots.value(now),
            done: self.done.value(now),
            buttons: self.buttons.map(|button| ButtonFrame {
                hover: button.hover.value(now),
                press: button.press.value(now),
                focus: button.focus.value(now),
            }),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
struct ButtonFrame {
    hover: f64,
    press: f64,
    focus: f64,
}

#[derive(Clone, Copy, PartialEq)]
struct Frame {
    intro: f64,
    theme: f64,
    content: f64,
    dots: f64,
    done: f64,
    buttons: [ButtonFrame; CONTROLS],
}

// ---------------------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------------------

/// The tool menu as the scene shows it: drawn once per DPI and theme.
struct SceneMenu {
    dpi: u32,
    dark: bool,
    pixmap: Pixmap,
    hot: (f32, f32),
}

struct Window<'a> {
    hwnd: HWND,
    shared: &'a Shared,
    options: &'a Options,
    pages: Vec<Page>,
    views: Vec<PageView>,
    shortcuts: Shortcuts,
    features: Features,
    page: Cell<usize>,
    /// 1 after Next, -1 after Back: where the new page slides in from.
    direction: Cell<f64>,
    tries: Cell<Tries>,
    holding: Cell<bool>,
    dpi: Cell<u32>,
    fonts: RefCell<Fonts>,
    epoch: Instant,
    frozen: Cell<Option<f64>>,
    /// Test only: the scene's moment, in ms; `None` shows its still frame.
    scene_at: Cell<Option<Option<f64>>>,
    /// When the shown page's scene started its first loop (ms on `now`).
    scene_start: Cell<f64>,
    motion: RefCell<Motion>,
    frame: Cell<Frame>,
    hover: Cell<Option<usize>>,
    timer: Cell<bool>,
    started: Cell<bool>,
    surface: RefCell<Option<Surface>>,
    icons: [HICON; 2],
    menu: RefCell<Option<SceneMenu>>,
    logo: RefCell<Option<(u32, COLORREF, Pixmap)>>,
}

impl Window<'_> {
    fn now(&self) -> f64 {
        self.frozen.get().unwrap_or_else(|| self.epoch.elapsed().as_secs_f64() * 1000.0)
    }

    fn child(&self, id: i32) -> HWND {
        // SAFETY: this is our parent HWND, and a partially initialized child may be absent.
        unsafe { GetDlgItem(Some(self.hwnd), id) }.unwrap_or_default()
    }

    fn ids(&self) -> impl Iterator<Item = i32> + '_ {
        [BACK_ID, NEXT_ID].into_iter().chain((0..self.pages.len() as i32).map(|i| DOT_ID + i))
    }

    fn invalidate(&self, rect: Option<RECT>) {
        // SAFETY: this thread owns the HWND, and Win32 copies the optional RECT.
        let _ = unsafe {
            InvalidateRect(Some(self.hwnd), rect.as_ref().map(std::ptr::from_ref), false)
        };
    }

    /// The window is on screen: shown and not minimized.
    fn shown(&self) -> bool {
        // SAFETY: scalar queries about our own window.
        unsafe { IsWindowVisible(self.hwnd).as_bool() && !IsIconic(self.hwnd).as_bool() }
    }

    fn scene(&self) -> Option<Scene> {
        self.pages[self.page.get()].scene()
    }

    /// The scene plays: motion is on, the page has one and the window is on screen.
    fn scene_plays(&self) -> bool {
        self.motion.borrow().enabled
            && self.scene().is_some()
            && self.scene_at.get().is_none()
            && self.shown()
    }

    fn update_animation(&self) {
        let now = self.now();
        let (frame, active) = {
            let motion = self.motion.borrow();
            (motion.frame(now), motion.active(now))
        };
        let previous = self.frame.replace(frame);
        if previous != frame {
            self.invalidate(None);
        }
        let should_run = self.started.get()
            && self.frozen.get().is_none()
            && self.shown()
            && (active || self.scene_plays());
        if should_run != self.timer.get() {
            // SAFETY: this window owns the single timer and dispatches it on this thread.
            let running = unsafe {
                if should_run {
                    SetTimer(Some(self.hwnd), TIMER, 15, None) != 0
                } else {
                    let _ = KillTimer(Some(self.hwnd), TIMER);
                    false
                }
            };
            self.timer.set(running);
        }
    }

    fn tick(&self) {
        self.update_animation();
        if self.scene_plays() {
            self.invalidate(Some(scaled(STAGE, self.dpi.get())));
        }
    }

    fn ui_state(&self) -> u32 {
        // SAFETY: scalar query to this thread's top-level window.
        unsafe { SendMessageW(self.hwnd, WM_QUERYUISTATE, None, None) }.0 as u32
    }

    fn change_ui_state(&self, action: u32, flags: u32) {
        let current = self.ui_state();
        if (action == UIS_SET && current & flags != flags)
            || (action == UIS_CLEAR && current & flags != 0)
        {
            // SAFETY: normal dialog UI cue propagation, with scalar flags.
            unsafe {
                SendMessageW(
                    self.hwnd,
                    WM_CHANGEUISTATE,
                    Some(WPARAM((action | flags << 16) as usize)),
                    None,
                )
            };
        }
    }

    fn button_state(&self, id: i32, child: HWND) {
        let Some(index) = control_index(id) else { return };
        // SAFETY: scalar queries; this thread owns the native button.
        let (pressed, focused) = unsafe {
            (SendMessageW(child, BM_GETSTATE, None, None).0 as u32 & BST_PUSHED != 0, GetFocus() == child)
        };
        let show_focus = self.ui_state() & UISF_HIDEFOCUS == 0;
        let now = self.now();
        let mut motion = self.motion.borrow_mut();
        let (enter, leave, focus_in) = (motion.duration(80.0), motion.duration(120.0), motion.duration(FOCUS_MS));
        let button = &mut motion.buttons[index];
        let hover = self.hover.get() == Some(index);
        button.hover.retarget(if hover { 1.0 } else { 0.0 }, now, if hover { enter } else { leave });
        button.press.retarget(if pressed { 1.0 } else { 0.0 }, now, if pressed { enter } else { leave });
        let focus = focused && show_focus;
        button.focus.retarget(if focus { 1.0 } else { 0.0 }, now, if focus { focus_in } else { 0.0 });
    }

    fn sync_buttons(&self) {
        for id in self.ids() {
            self.button_state(id, self.child(id));
        }
        self.update_animation();
    }

    fn observe_hover(&self, index: usize, hwnd: HWND, msg: u32, point: LPARAM) {
        let mut bounds = RECT::default();
        let x = i32::from(point.0 as i16);
        let y = i32::from((point.0 >> 16) as i16);
        // SAFETY: the native button owns its client rectangle.
        let inside = msg == WM_MOUSEMOVE
            && unsafe { GetClientRect(hwnd, &mut bounds) }.is_ok()
            && x >= bounds.left
            && x < bounds.right
            && y >= bounds.top
            && y < bounds.bottom;
        if inside && self.hover.get() != Some(index) {
            let old = self.hover.replace(Some(index));
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: an initialized request for a live native button.
            let _ = unsafe { TrackMouseEvent(&mut track) };
            if let Some(old) = old {
                let id = self.ids().nth(old).unwrap_or(BACK_ID);
                self.button_state(id, self.child(id));
            }
        } else if !inside && self.hover.get() == Some(index) {
            self.hover.set(None);
        }
    }

    fn set_theme(&self) {
        let dark = self.options.test.as_ref().map_or_else(system_dark, |test| test.dark);
        let now = self.now();
        {
            let mut motion = self.motion.borrow_mut();
            let duration = if self.started.get() { motion.duration(THEME_MS) } else { 0.0 };
            motion.theme.retarget(if dark { 1.0 } else { 0.0 }, now, duration);
        }
        self.update_animation();
        let immersive = i32::from(dark);
        // SAFETY: DWM copies the DWORD immediately; the HWND belongs to this thread; both icon
        // handles stay owned by this window until it is destroyed.
        unsafe {
            let _ = DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&immersive as *const i32).cast(),
                size_of::<i32>() as u32,
            );
            let icon = LPARAM(self.icons[usize::from(dark)].0 as isize);
            SendMessageW(self.hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(icon));
            SendMessageW(self.hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(icon));
        }
        self.invalidate(None);
    }

    fn init_children(&self, instance: HINSTANCE) -> Result<()> {
        for id in self.ids() {
            let caption = wide(self.caption(id));
            let style = if id == BACK_ID || id == NEXT_ID {
                BS_OWNERDRAW as u32 | WS_TABSTOP.0 | WS_GROUP.0
            } else {
                BS_OWNERDRAW as u32
            };
            let rect = scaled(control_rect(id, self.pages.len()), self.dpi.get());
            // SAFETY: captions are copied, ids are unique, and this thread owns the parent.
            let child = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    PCWSTR(caption.as_ptr()),
                    WINDOW_STYLE(style | WS_CHILD.0 | WS_VISIBLE.0),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    Some(self.hwnd),
                    Some(HMENU(id as usize as *mut c_void)),
                    Some(instance),
                    None,
                )?
            };
            // SAFETY: same-thread subclass of our child. The parent state outlives the child.
            if !unsafe { SetWindowSubclass(child, Some(control_proc), 1, self as *const _ as usize) }
                .as_bool()
            {
                return Err(Error::from_thread());
            }
        }
        Ok(())
    }

    /// The accessible name of a control.
    fn caption(&self, id: i32) -> String {
        match id {
            BACK_ID => "Back".into(),
            NEXT_ID if self.page.get() + 1 == self.pages.len() => "Done".into(),
            NEXT_ID => "Next".into(),
            _ => {
                let index = (id - DOT_ID) as usize;
                format!("Page {} of {}: {}", index + 1, self.pages.len(), self.pages[index].name())
            }
        }
    }

    fn relayout(&self, dpi: u32) {
        self.dpi.set(dpi);
        let previous = self.fonts.replace(Fonts::new(dpi));
        for id in self.ids() {
            let rect = scaled(control_rect(id, self.pages.len()), dpi);
            // SAFETY: same-thread children; the parent paints everything.
            let _ = unsafe {
                SetWindowPos(
                    self.child(id),
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
                )
            };
        }
        self.invalidate(None);
        drop(previous);
    }

    fn adjust_size(&self, at: Option<(i32, i32)>) {
        let mut bounds = scaled([0, 0, WIDTH, HEIGHT], self.dpi.get());
        // SAFETY: a writable RECT for the real window style and target DPI.
        let _ = unsafe {
            AdjustWindowRectExForDpi(&mut bounds, STYLE, false, EX_STYLE, GetDpiForWindow(self.hwnd))
        };
        let (x, y) = at.unwrap_or_default();
        let flags = if at.is_some() {
            SWP_NOZORDER | SWP_NOACTIVATE
        } else {
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE
        };
        // SAFETY: owned HWND, scalar position and size.
        let _ = unsafe {
            SetWindowPos(self.hwnd, None, x, y, bounds.right - bounds.left, bounds.bottom - bounds.top, flags)
        };
    }

    /// Shows the page's Back and Next as they should be, with their names.
    fn refresh_buttons(&self) {
        let back = self.child(BACK_ID);
        let first = self.page.get() == 0;
        // SAFETY: this thread owns the children; showing one never activates a window.
        unsafe {
            if first && self.options.test.is_none() && GetFocus() == back {
                let _ = SetFocus(Some(self.child(NEXT_ID)));
            }
            let _ = ShowWindow(back, if first { SW_HIDE } else { SW_SHOWNOACTIVATE });
            let next = wide(self.caption(NEXT_ID));
            let _ = SetWindowTextW(self.child(NEXT_ID), PCWSTR(next.as_ptr()));
        }
    }

    fn set_page(&self, index: usize) {
        let index = index.min(self.pages.len() - 1);
        let old = self.page.replace(index);
        if old == index {
            return;
        }
        self.direction.set(if index > old { 1.0 } else { -1.0 });
        let now = self.now();
        self.scene_start.set(now);
        {
            let mut motion = self.motion.borrow_mut();
            let duration = motion.duration(PAGE_MS);
            motion.content.retarget(0.0, now, 0.0);
            motion.content.retarget(1.0, now, duration);
            motion.dots.retarget(index as f64, now, duration);
            let done = if self.tries.get().done(self.pages[index]) { 1.0 } else { 0.0 };
            motion.done.retarget(done, now, 0.0);
        }
        self.refresh_buttons();
        self.invalidate(None);
        self.update_animation();
    }

    fn go(&self, by: isize) {
        let next = self.page.get() as isize + by;
        if next >= 0 {
            self.set_page(next as usize);
        }
    }

    fn command(&self, id: i32) {
        match id {
            BACK_ID => self.go(-1),
            NEXT_ID if self.page.get() + 1 == self.pages.len() => self.request_close(),
            NEXT_ID => self.go(1),
            id if control_index(id).is_some() => {
                self.set_page((id - DOT_ID) as usize);
                // A dot is not a tab stop: the keys stay with Next. (Not in tests: setting
                // the focus activates the window.)
                if self.options.test.is_none() {
                    // SAFETY: focus moves within this thread's own children.
                    let _ = unsafe { SetFocus(Some(self.child(NEXT_ID))) };
                }
            }
            _ => {}
        }
    }

    /// Takes what the UI loop sent: what was tried, and whether cards are in hand.
    fn receive(&self) {
        let (tried, holding) = {
            let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
            (std::mem::take(&mut inbox.tried), inbox.holding)
        };
        let mut tries = self.tries.get();
        let shown = self.pages[self.page.get()];
        for tried in tried {
            if tries.note(tried) && shown.tried_by() == Some(tried) {
                let now = self.now();
                let mut motion = self.motion.borrow_mut();
                let duration = motion.duration(DONE_MS);
                motion.done.retarget(1.0, now, duration);
            }
        }
        self.tries.set(tries);
        if self.holding.replace(holding) != holding {
            self.invalidate(Some(scaled(TRY, self.dpi.get())));
        }
        self.update_animation();
    }

    fn request_close(&self) {
        // SAFETY: this thread owns the HWND; the window state outlives DestroyWindow.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }

    fn activate(&self) {
        if self.options.test.is_some() {
            return;
        }
        // SAFETY: the HWND belongs to this thread.
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    fn freeze(&self, scene_ms: Option<f64>) {
        {
            let mut motion = self.motion.borrow_mut();
            motion.settle();
            motion.buttons = [ButtonMotion::default(); CONTROLS];
        }
        self.frozen.set(Some(1_000_000.0));
        self.scene_at.set(Some(scene_ms));
        self.update_animation();
        self.invalidate(None);
        // SAFETY: test-only flush of our own client before PrintWindow samples the frame.
        let _ = unsafe { RedrawWindow(Some(self.hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW) };
    }
}

impl Drop for Window<'_> {
    fn drop(&mut self) {
        // SAFETY: both handles were created here, and the window has been destroyed.
        for icon in self.icons.iter().copied().filter(|icon| !icon.is_invalid()) {
            let _ = unsafe { DestroyIcon(icon) };
        }
    }
}

// ---------------------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------------------

/// A COLORREF as straight RGBA.
fn rgba(color: COLORREF) -> scenes::Rgba {
    [color.0 as u8, (color.0 >> 8) as u8, (color.0 >> 16) as u8, 255]
}

fn rect_x(rect: RECT, left: i32, right: i32) -> RECT {
    RECT { left, right, ..rect }
}

impl Window<'_> {
    fn paint(&self, target: HDC) {
        let mut client = RECT::default();
        let mut dirty = RECT::default();
        // SAFETY: the DC belongs to WM_PAINT or PrintWindow; both rectangles are writable.
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut client);
            GetClipBox(target, &mut dirty);
        }
        let dirty = RECT {
            left: dirty.left.max(client.left),
            top: dirty.top.max(client.top),
            right: dirty.right.min(client.right),
            bottom: dirty.bottom.min(client.bottom),
        };
        if dirty.left >= dirty.right || dirty.top >= dirty.bottom {
            return;
        }
        let frame = self.frame.get();
        let (width, height) = (client.right as u32, client.bottom as u32);
        let mut storage = self.surface.borrow_mut();
        if storage.as_ref().is_none_or(|surface| surface.width != width || surface.height != height) {
            *storage = Surface::new(width, height).ok();
        }
        let Some(surface) = storage.as_mut() else { return };
        let palette = Palette::at(frame.theme);
        {
            let mut canvas = surface.canvas(dirty);
            canvas.shape(dirty, 0.0, palette.paper);
            self.paint_page(&mut canvas, frame, palette);
            self.paint_footer(&mut canvas, frame, palette);
            let labels = std::mem::take(&mut canvas.labels);
            drop(canvas);
            // SAFETY: the selected DIB is live, and all font selections are restored after drawing.
            unsafe {
                let level = SaveDC(surface.dc);
                if level > 0 {
                    IntersectClipRect(surface.dc, dirty.left, dirty.top, dirty.right, dirty.bottom);
                    for label in labels {
                        SelectObject(surface.dc, label.font.into());
                        draw_text(surface.dc, label.rect, &label.text, label.ink, label.format);
                    }
                    let _ = RestoreDC(surface.dc, level);
                }
            }
        }
        // SAFETY: the surface DC is live; the target is the paint or print DC.
        let _ = unsafe {
            BitBlt(
                target,
                dirty.left,
                dirty.top,
                dirty.right - dirty.left,
                dirty.bottom - dirty.top,
                Some(surface.dc),
                dirty.left,
                dirty.top,
                SRCCOPY,
            )
        };
    }

    fn paint_page(&self, canvas: &mut Canvas<'_>, frame: Frame, p: Palette) {
        let dpi = self.dpi.get();
        // The shown page fades in and slides in from the side it comes from.
        canvas.dx = scale(((1.0 - frame.content) * 16.0 * self.direction.get()).round() as i32, dpi);
        let amount = frame.intro * frame.content;
        let page = self.pages[self.page.get()];
        if page == Page::Welcome {
            self.paint_welcome(canvas, amount, p);
        } else {
            self.paint_step(canvas, frame, amount, p);
        }
        canvas.dx = 0;
    }

    fn paint_welcome(&self, canvas: &mut Canvas<'_>, amount: f64, p: Palette) {
        let dpi = self.dpi.get();
        let px = |v| scale(v, dpi);
        let fade = |ink| mix(p.paper, ink, amount);
        let fonts = self.fonts.borrow().0;
        let center = DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
        self.paint_logo(canvas, scaled(LOGO, dpi), p.text, amount);
        canvas.text(scaled(NAME, dpi), "Igorio", fade(p.text), center, fonts[BIG]);
        // Two lines, broken after the colon.
        let line = self.views[0].line.replacen(": ", ":
", 1);
        canvas.text(scaled(TAGLINE, dpi), line, fade(p.muted), DT_CENTER | DT_WORDBREAK | DT_NOPREFIX, fonts[BODY]);

        // Where it lives: a small notification area, and a word on finding it.
        let box_ = scaled(WHERE, dpi);
        canvas.shape(box_, px(8) as f32, fade(p.patch));
        let tray = RECT { left: box_.left + px(16), top: box_.top + px(28), right: box_.left + px(140), bottom: box_.top + px(68) };
        canvas.shape(tray, px(6) as f32, fade(mix(p.patch, p.chip, 1.0)));
        // The ^ arrow, the icon and the clock.
        let chevron = RECT { left: tray.left + px(6), right: tray.left + px(30), ..tray };
        canvas.text(move_y(chevron, px(2)), "^", fade(p.muted), center, fonts[HEADING]);
        let icon = scale(20, dpi);
        let icon_rect = RECT {
            left: tray.left + px(36),
            top: (tray.top + tray.bottom - icon) / 2,
            right: tray.left + px(36) + icon,
            bottom: (tray.top + tray.bottom + icon) / 2,
        };
        self.paint_logo(canvas, icon_rect, p.text, amount);
        let clock = RECT { left: tray.left + px(62), right: tray.right - px(6), ..tray };
        canvas.text(clock, "12:30", fade(p.text), center, fonts[BODY]);
        let text = RECT { left: tray.right + px(16), top: box_.top + px(14), right: box_.right - px(16), bottom: box_.top + px(34) };
        canvas.text(text, "Where it lives", fade(p.text), DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX, fonts[HEADING]);
        let words = RECT { top: text.bottom + px(2), bottom: box_.bottom - px(10), ..text };
        canvas.text(
            words,
            "In the notification area by the clock. Hidden? Look under the ^ arrow.",
            fade(p.muted),
            DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
            fonts[BODY],
        );
    }

    /// The app's mark (the tray drawing) in `color`, faded in by `amount`.
    fn paint_logo(&self, canvas: &mut Canvas<'_>, rect: RECT, color: COLORREF, amount: f64) {
        if !canvas.visible(rect) {
            return;
        }
        let rect = canvas.moved(rect);
        let size = (rect.right - rect.left).max(1) as u32;
        let mut cache = self.logo.borrow_mut();
        if cache.as_ref().is_none_or(|(s, c, _)| *s != size || *c != color) {
            // The DIB is BGRA: hand the mark its color with red and blue swapped.
            let [r, g, b, a] = rgba(color);
            *cache = Some((size, color, crate::tray::logo_pixmap(size, [b, g, r, a])));
        }
        if let Some((_, _, pixmap)) = cache.as_ref() {
            let paint = PixmapPaint { opacity: amount as f32, ..PixmapPaint::default() };
            canvas.pixmap.draw_pixmap(rect.left, rect.top, pixmap.as_ref(), &paint, Transform::identity(), None);
        }
    }

    fn menu_picture(&self, dark: bool) {
        let dpi = self.dpi.get();
        if self.menu.borrow().as_ref().is_some_and(|menu| menu.dpi == dpi && menu.dark == dark) {
            return;
        }
        let context = ToolContext {
            notes: true,
            recording_available: Action::ToggleRecording.available(),
            recording_enabled: self.features.recording,
            // The user's order, hidden tools left out, as the real menu.
            menu: self.options.settings.tool_menu,
            ..ToolContext::default()
        };
        let rows: Vec<(ToolId, Vec<String>)> = summoner::offered(&context)
            .into_iter()
            .map(|id| (id, summoner::tool_chips(id, &self.shortcuts)))
            .collect();
        // Every tool hidden: the real menu never opens, so the scene shows none.
        if rows.is_empty() {
            return;
        }
        let scale_ = f64::from(dpi) / 96.0;
        let px = |v: f64| (v * scale_).round() as i32;
        // The hot spot where the window's top-left corner comes out at (0, 0).
        let hot = (px(ANCHOR.0) + px(MENU_SHADOW), px(ANCHOR.1) + px(MENU_PAD) + px(MENU_SHADOW));
        let work = Bounds { left: 0, top: 0, right: 100_000, bottom: 100_000 };
        let width = summoner::menu_width(&rows, scale_);
        let layout = summoner::menu_layout(hot, rows.len(), width, work, scale_);
        let picture = MenuPicture { rows: &rows, layout: &layout, dark, highlight: 0.0, lit: 1.0, flash: 0.0 };
        if let Some(mut pixmap) = summoner::render_menu(&picture) {
            // RGBA to the DIB's BGRA.
            for pixel in pixmap.data_mut().as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            *self.menu.borrow_mut() = Some(SceneMenu { dpi, dark, pixmap, hot: (hot.0 as f32, hot.1 as f32) });
        }
    }

    fn paint_stage(&self, canvas: &mut Canvas<'_>, frame: Frame, amount: f64, p: Palette) {
        let dpi = self.dpi.get();
        let stage = scaled(STAGE, dpi);
        if !canvas.visible(stage) {
            return;
        }
        let Some(scene) = self.scene() else { return };
        // The whole stage is drawn, whatever part is dirty: the picture moves as a whole.
        let dirty = canvas.dirty;
        canvas.dirty = RECT { left: i32::MIN / 2, top: i32::MIN / 2, right: i32::MAX / 2, bottom: i32::MAX / 2 };
        canvas.shape(inset(stage, -scale(2, dpi)), 0.0, p.paper);
        canvas.shape(stage, scale(10, dpi) as f32, mix(p.paper, p.patch, amount));
        let dark = frame.theme >= 0.5;
        if scene == Scene::ToolMenu {
            self.menu_picture(dark);
        }
        let t = match self.scene_at.get() {
            Some(Some(ms)) => ms / 1000.0,
            Some(None) => scene.still(),
            None if self.motion.borrow().enabled => {
                ((self.now() - self.scene_start.get()) / 1000.0).rem_euclid(scene.duration())
            }
            None => scene.still(),
        };
        let faded = self.scene_at.get().is_none() && self.motion.borrow().enabled;
        let colors = Colors {
            stage: rgba(p.patch),
            paper: rgba(p.paper),
            fill: rgba(mix(p.patch, p.chip, 1.0)),
            faint: rgba(mix(p.chip, p.muted, 0.45)),
            ink: rgba(p.text),
            picture: rgba(p.selected),
            accent: igorio_kit::theme::ACCENT,
            edge: rgba(mix(p.patch, p.outline, 0.55)),
            shadow: [0, 0, 0, 255],
        };
        let menu = self.menu.borrow();
        let moved = canvas.moved(stage);
        let texts = {
            let mut stage = Stage {
                pixmap: &mut canvas.pixmap,
                origin: (moved.left as f32, moved.top as f32),
                scale: dpi as f32 / 96.0,
                colors,
                bgr: true,
                menu: if scene == Scene::ToolMenu { menu.as_ref().map(|m| (&m.pixmap, m.hot)) } else { None },
                texts: Vec::new(),
            };
            scenes::draw(scene, t, &mut stage, faded, amount);
            std::mem::take(&mut stage.texts)
        };
        canvas.dirty = dirty;
        let fonts = self.fonts.borrow().0;
        let k = dpi as f32 / 96.0;
        for text in texts {
            let [x, y, w, h] = text.rect;
            let rect = RECT {
                left: stage.left + (x * k).round() as i32,
                top: stage.top + (y * k).round() as i32,
                right: stage.left + ((x + w) * k).round() as i32,
                bottom: stage.top + ((y + h) * k).round() as i32,
            };
            let [r, g, b, _] = text.color;
            let ink = rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b));
            let align = if text.left { DT_LEFT } else { DT_CENTER };
            canvas.text(rect, text.text, ink, align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX, fonts[BODY]);
        }
    }

    /// A line of keys from `left`, `height` tall around `center_y`. Returns where it ends.
    fn paint_keys(&self, canvas: &mut Canvas<'_>, keys: &[Key], left: i32, center_y: i32, fade: &dyn Fn(COLORREF) -> COLORREF, p: Palette) -> i32 {
        let dpi = self.dpi.get();
        let font = self.fonts.borrow().0[CHIP];
        let height = scale(22, dpi);
        let row = RECT { left, top: center_y - height / 2, right: left, bottom: center_y - height / 2 + height };
        let mut x = left;
        for key in keys {
            x = key_chip(canvas, RECT { left: x, ..row }, &key.label, key.chip, [fade(p.chip), fade(p.muted)], font, dpi);
        }
        x - scale(3, dpi)
    }

    fn keys_width(&self, canvas: &Canvas<'_>, keys: &[Key]) -> i32 {
        let dpi = self.dpi.get();
        let font = self.fonts.borrow().0[CHIP];
        let widths: i32 = keys.iter().map(|key| key_chip_width(canvas, &key.label, font, dpi)).sum();
        widths + scale(3, dpi) * (keys.len() as i32 - 1).max(0)
    }

    fn paint_step(&self, canvas: &mut Canvas<'_>, frame: Frame, amount: f64, p: Palette) {
        let dpi = self.dpi.get();
        let px = |v| scale(v, dpi);
        let fade = |ink| mix(p.paper, ink, amount);
        let fonts = self.fonts.borrow().0;
        let index = self.page.get();
        let page = self.pages[index];
        let view = &self.views[index];
        self.paint_stage(canvas, frame, amount, p);

        // The name and its keys, centered as one line.
        let title = scaled(TITLE, dpi);
        let name_width = canvas.width(page.name(), fonts[TITLE_FONT]);
        let keys_width = if view.keys.is_empty() { 0 } else { px(12) + self.keys_width(canvas, &view.keys) };
        let left = (title.left + title.right - name_width - keys_width) / 2;
        let name = rect_x(title, left, left + name_width + px(2));
        canvas.text(name, page.name(), fade(p.text), DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX, fonts[TITLE_FONT]);
        if !view.keys.is_empty() {
            let center_y = (title.top + title.bottom) / 2 + px(1);
            self.paint_keys(canvas, &view.keys, left + name_width + px(12), center_y, &fade, p);
        }
        canvas.text(scaled(LINE, dpi), view.line.as_str(), fade(p.muted), DT_CENTER | DT_WORDBREAK | DT_NOPREFIX, fonts[BODY]);

        // More keys, two to a row; or one short extra line.
        if let Some(aside) = view.aside {
            let rect = scaled([40, MORE_TOP, 440, 40], dpi);
            canvas.text(rect, aside, fade(p.muted), DT_CENTER | DT_WORDBREAK | DT_NOPREFIX, fonts[BODY]);
        }
        for (row, items) in view.more.chunks(2).enumerate() {
            let gap = px(28);
            let widths: Vec<i32> = items
                .iter()
                .map(|(keys, does)| self.keys_width(canvas, keys) + px(7) + canvas.width(does, fonts[BODY]))
                .collect();
            let total = widths.iter().sum::<i32>() + gap * (items.len() as i32 - 1);
            let mut x = (px(WIDTH) - total) / 2;
            let center_y = px(MORE_TOP + 11) + row as i32 * px(MORE_PITCH);
            for ((keys, does), width) in items.iter().zip(widths) {
                let end = self.paint_keys(canvas, keys, x, center_y, &fade, p);
                let rect = RECT { left: end + px(7), top: center_y - px(11), right: x + width + px(2), bottom: center_y + px(11) };
                canvas.text(rect, *does, fade(p.muted), DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX, fonts[BODY]);
                x += width + gap;
            }
        }
        self.paint_try(canvas, frame, amount, p, page);
    }

    /// The soft box under the page: "Try it now" and the action, or a green "Done".
    fn paint_try(&self, canvas: &mut Canvas<'_>, frame: Frame, amount: f64, p: Palette, page: Page) {
        let Some(action) = try_action(page, self.holding.get(), &self.shortcuts) else { return };
        let dpi = self.dpi.get();
        let px = |v| scale(v, dpi);
        let fade = |ink| mix(p.paper, ink, amount);
        let fonts = self.fonts.borrow().0;
        let green = mix(GREEN_LIGHT, GREEN_DARK, frame.theme);
        let done = frame.done;
        let box_ = scaled(TRY, dpi);
        let face = mix(p.patch, green, 0.10 * done);
        canvas.shape(box_, px(8) as f32, fade(face));
        let line = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
        if done < 1.0 {
            let ink = |c| fade(mix(c, face, done));
            let lead = RECT { left: box_.left + px(16), right: box_.right - px(16), ..box_ };
            let lead = move_y(lead, -px((4.0 * done).round() as i32));
            let width = canvas.width("Try it now", fonts[HEADING]);
            canvas.text(lead, "Try it now", ink(p.text), line, fonts[HEADING]);
            let rest = RECT { left: lead.left + width + px(10), ..lead };
            canvas.text(rest, action, ink(p.muted), line | DT_END_ELLIPSIS, fonts[BODY]);
        }
        if done > 0.0 {
            // A check in a green circle that grows in, then "Done".
            let rise = px((4.0 * (1.0 - done)).round() as i32);
            let size = px(20);
            let cx = box_.left + px(16) + size / 2;
            let cy = (box_.top + box_.bottom) / 2 + rise;
            let grow = 0.6 + 0.4 * done;
            let r = (f64::from(size) / 2.0 * grow).round() as i32;
            let circle = RECT { left: cx - r, top: cy - r, right: cx + r, bottom: cy + r };
            canvas.shape(circle, r as f32, fade(mix(face, green, done)));
            self.paint_check(canvas, cx, cy, size, fade(mix(green, p.paper, done)), done);
            let label = RECT { left: box_.left + px(16) + size + px(10), right: box_.right - px(16), top: box_.top + rise, bottom: box_.bottom + rise };
            canvas.text(label, "Done", fade(mix(face, green, done)), line, fonts[HEADING]);
        }
    }

    /// The check mark inside the Done circle, drawn along its stroke as `progress` grows.
    fn paint_check(&self, canvas: &mut Canvas<'_>, cx: i32, cy: i32, size: i32, color: COLORREF, progress: f64) {
        let moved = canvas.moved(RECT { left: cx, top: cy, right: cx, bottom: cy });
        let (cx, cy, s) = (moved.left as f32, moved.top as f32, size as f32 / 20.0);
        let points = [(-4.5, 0.2), (-1.4, 3.3), (4.6, -3.0)];
        let first = 4.4_f32;
        let second = 8.7_f32;
        let along = progress as f32 * (first + second);
        let mut path = tiny_skia::PathBuilder::new();
        path.move_to(cx + points[0].0 * s, cy + points[0].1 * s);
        let leg = (along / first).min(1.0);
        path.line_to(
            cx + (points[0].0 + (points[1].0 - points[0].0) * leg) * s,
            cy + (points[0].1 + (points[1].1 - points[0].1) * leg) * s,
        );
        if along > first {
            let leg = ((along - first) / second).min(1.0);
            path.line_to(
                cx + (points[1].0 + (points[2].0 - points[1].0) * leg) * s,
                cy + (points[1].1 + (points[2].1 - points[1].1) * leg) * s,
            );
        }
        let Some(path) = path.finish() else { return };
        let mut paint = tiny_skia::Paint::default();
        // The DIB is BGRA.
        paint.set_color_rgba8((color.0 >> 16) as u8, (color.0 >> 8) as u8, color.0 as u8, 255);
        paint.anti_alias = true;
        let stroke = tiny_skia::Stroke {
            width: 2.0 * s,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Default::default()
        };
        canvas.pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn paint_footer(&self, canvas: &mut Canvas<'_>, frame: Frame, p: Palette) {
        let dpi = self.dpi.get();
        let px = |v| scale(v, dpi);
        let fade = |ink| mix(p.paper, ink, frame.intro);
        let fonts = self.fonts.borrow().0;
        let center = DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
        let count = self.pages.len();
        if self.page.get() > 0 {
            let state = frame.buttons[0];
            let bounds = scaled(BACK, dpi);
            let shape = inset(bounds, px(3));
            paint_focus(canvas, shape, [px(5), px(1), px(2)], fade(p.paper), fade(p.accent), state.focus);
            let face = mix(mix(p.chip, p.selected, state.hover * 0.6), p.outline, state.press * 0.27);
            canvas.outlined(shape, px(5) as f32, fade(face), fade(p.outline));
            canvas.text(bounds, "Back", fade(p.text), center, fonts[BODY]);
        }
        {
            let state = frame.buttons[1];
            let bounds = scaled(NEXT, dpi);
            let shape = inset(bounds, px(3));
            paint_focus(canvas, shape, [px(5), px(1), px(2)], fade(p.paper), fade(p.accent), state.focus);
            let face = mix(mix(p.accent, p.on, state.hover * 0.45), p.outline, state.press * 0.3);
            canvas.shape(shape, px(5) as f32, fade(face));
            let label = if self.page.get() + 1 == count { "Done" } else { "Next" };
            canvas.text(bounds, label, fade(p.paper), center, fonts[HEADING]);
        }
        // The dots: the current one is long, and slides to its new place.
        for index in 0..count {
            let slot = scaled(control_rect(DOT_ID + index as i32, count), dpi);
            let near = (1.0 - (index as f64 - frame.dots).abs()).max(0.0);
            let width = px(6) + (f64::from(px(10)) * near).round() as i32;
            let cx = (slot.left + slot.right) / 2;
            let cy = (slot.top + slot.bottom) / 2;
            let dot = RECT { left: cx - width / 2, right: cx - width / 2 + width, top: cy - px(3), bottom: cy - px(3) + px(6) };
            let hover = frame.buttons[2 + index].hover;
            let rest = mix(p.disabled, p.muted, hover);
            canvas.shape(dot, px(3) as f32, fade(mix(rest, p.text, near)));
        }
    }
}

// ---------------------------------------------------------------------------------------
// Thread, window procedure and message loop
// ---------------------------------------------------------------------------------------

fn run_window(options: &Options, shared: &Shared) -> Result<()> {
    if shared.stop.load(SeqCst) {
        return Ok(());
    }
    // SAFETY: static module and cursor identifiers, no borrowed pointers.
    let (instance, cursor) = unsafe { (HINSTANCE(GetModuleHandleW(None)?.0), LoadCursorW(None, IDC_ARROW)?) };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: CLASS,
        hCursor: cursor,
        ..Default::default()
    };
    // SAFETY: class fields refer to fixed code/data and valid handles.
    if unsafe { RegisterClassW(&class) } == 0 {
        let error = Error::from_thread();
        if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(error);
        }
    }
    let controls = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
    // SAFETY: the structure is valid for the duration of the call.
    let _ = unsafe { InitCommonControlsEx(&controls) };
    let point = match options.test.and_then(|test| test.at) {
        Some((x, y)) => Point::new(f64::from(x), f64::from(y)),
        None => cursor_pos(),
    };
    let monitor = monitor_at(point);
    let at = monitor.work_area;
    let dpi = options.test.as_ref().map_or((monitor.scale * 96.0).round() as u32, |test| test.dpi);
    // First put the hidden HWND on the target monitor so Windows picks its actual DPI.
    // SAFETY: registered class, static caption and no creation pointer.
    let hwnd = unsafe {
        CreateWindowExW(
            EX_STYLE,
            CLASS,
            w!("Welcome to Igorio"),
            STYLE,
            (at.x + at.width / 2.0) as i32,
            (at.y + at.height / 2.0) as i32,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )?
    };
    let features = Features::of(&options.settings);
    let pages = pages(features);
    let views = pages.iter().map(|&page| view(page, features, &options.settings.shortcuts)).collect();
    let holding = shared.inbox.lock().unwrap_or_else(|e| e.into_inner()).holding;
    let motion = options.test.map_or_else(animation_effects, |test| test.motion);
    let window = Box::new(Window {
        hwnd,
        shared,
        options,
        pages,
        views,
        shortcuts: options.settings.shortcuts.clone(),
        features,
        page: Cell::new(0),
        direction: Cell::new(1.0),
        tries: Cell::new(Tries::default()),
        holding: Cell::new(holding),
        dpi: Cell::new(dpi),
        fonts: RefCell::new(Fonts::new(dpi)),
        epoch: Instant::now(),
        frozen: Cell::new(None),
        scene_at: Cell::new(None),
        scene_start: Cell::new(0.0),
        motion: RefCell::new(Motion::new(motion)),
        frame: Cell::new(Motion::new(false).frame(0.0)),
        hover: Cell::new(None),
        timer: Cell::new(false),
        started: Cell::new(false),
        surface: RefCell::new(None),
        icons: [settings_icon(instance, igorio_kit::theme::TEXT), settings_icon(instance, [234, 240, 252, 255])],
        menu: RefCell::new(None),
        logo: RefCell::new(None),
    });
    // SAFETY: everything reachable from this pointer outlives DestroyWindow; WM_NCDESTROY
    // detaches it, and only this thread reads it.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&*window as *const Window<'_>) as isize) };
    let result = (|| {
        window.init_children(instance)?;
        window.refresh_buttons();
        window.set_theme();
        window.relayout(dpi);
        window.change_ui_state(UIS_SET, UISF_HIDEFOCUS | UISF_HIDEACCEL);
        window.adjust_size(None);
        let mut bounds = RECT::default();
        // SAFETY: same-thread window and writable RECT.
        unsafe { GetWindowRect(hwnd, &mut bounds)? };
        let width = f64::from(bounds.right - bounds.left);
        let height = f64::from(bounds.bottom - bounds.top);
        window.adjust_size(Some((
            (at.x + ((at.width - width) / 2.0).max(0.0)) as i32,
            (at.y + ((at.height - height) / 2.0).max(0.0)) as i32,
        )));
        shared.hwnd.store(hwnd.0 as isize, SeqCst);
        // Anything the UI loop sent while the window was being made.
        window.receive();
        if !shared.stop.load(SeqCst) {
            let now = window.now();
            {
                let mut motion = window.motion.borrow_mut();
                let duration = motion.duration(OPEN_MS);
                motion.intro.retarget(1.0, now, duration);
            }
            window.scene_start.set(now);
            window.started.set(true);
            // SAFETY: this thread owns the window. Test mode leaves the foreground app alone.
            let _ = unsafe {
                ShowWindow(hwnd, if options.test.is_some() { SW_SHOWNOACTIVATE } else { SW_SHOWNORMAL })
            };
            shared.shown.store(true, SeqCst);
            window.update_animation();
            window.activate();
            // SAFETY: focus goes to our own Next button.
            if options.test.is_none() {
                let _ = unsafe { SetFocus(Some(window.child(NEXT_ID))) };
            }
            pump(&window)?;
        }
        Ok(())
    })();
    // SAFETY: the thread owns the HWND, and still owns the state if creation failed partway.
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
    }
    result
}

fn pump(window: &Window<'_>) -> Result<()> {
    let mut message = MSG::default();
    loop {
        // SAFETY: stack MSG for this thread's queue; GetMessage returns -1 on failure.
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        match status {
            0 => return Ok(()),
            -1 => return Err(Error::from_thread()),
            _ => {}
        }
        // SAFETY: a scalar parent query for a window of this thread's queue.
        let ours = message.hwnd == window.hwnd || unsafe { GetParent(message.hwnd) }.ok() == Some(window.hwnd);
        if ours && message.message == WM_KEYDOWN {
            window.change_ui_state(UIS_CLEAR, UISF_HIDEFOCUS);
            let key = message.wParam.0 as u16;
            if key == VK_LEFT.0 || key == VK_RIGHT.0 {
                window.go(if key == VK_LEFT.0 { -1 } else { 1 });
                continue;
            }
            if key == VK_ESCAPE.0 {
                window.request_close();
                continue;
            }
            if key == VK_RETURN.0 {
                // Enter clicks the focused button, Next when none is.
                // SAFETY: GetFocus is this thread's focused control; only our children are clicked.
                unsafe {
                    let focused = GetFocus();
                    if GetParent(focused).ok() == Some(window.hwnd) {
                        SendMessageW(focused, BM_CLICK, None, None);
                    } else {
                        window.command(NEXT_ID);
                    }
                }
                continue;
            }
        }
        // SAFETY: both the message and its target belong to this thread.
        if !unsafe { IsDialogMessageW(window.hwnd, &message) }.as_bool() {
            // SAFETY: dispatch the unhandled message from this same queue.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

/// Keeps native input, focus, tab order and accessibility; the parent paints every pixel.
unsafe extern "system" fn control_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: the owning thread attached the stable parent pointer; its children die first.
    let window = unsafe { &*(data as *const Window<'_>) };
    match msg {
        WM_PAINT => {
            with_paint(hwnd, |_| {});
            return LRESULT(0);
        }
        WM_PRINT | WM_PRINTCLIENT => return LRESULT(0),
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCDESTROY => {
            // SAFETY: remove this exact subclass on the child's final message.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(control_proc), id) };
        }
        _ => {}
    }
    // SAFETY: the id of our own native child.
    let control = unsafe { GetDlgCtrlID(hwnd) };
    let index = control_index(control);
    if let Some(index) = index {
        if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
            window.change_ui_state(UIS_SET, UISF_HIDEFOCUS);
        }
        if matches!(msg, WM_MOUSEMOVE | WM_MOUSELEAVE) {
            window.observe_hover(index, hwnd, msg, lp);
        }
    }
    // SAFETY: forward the original native control message, including WM_GETOBJECT.
    let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    if index.is_some()
        && matches!(
            msg,
            BM_SETSTATE
                | WM_SETFOCUS
                | WM_KILLFOCUS
                | WM_MOUSEMOVE
                | WM_MOUSELEAVE
                | WM_LBUTTONUP
                | WM_LBUTTONDOWN
                | WM_CAPTURECHANGED
                | WM_CANCELMODE
        )
    {
        window.button_state(control, hwnd);
        window.update_animation();
    }
    result
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: this class owns GWLP_USERDATA. Before attachment the value is zero.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Window<'_>;
    // SAFETY: run_window holds the allocation through DestroyWindow, and only this thread reads it.
    if let Some(window) = unsafe { ptr.as_ref() } {
        match msg {
            WM_COMMAND if wp.0 >> 16 == BN_CLICKED as usize => {
                let id = (wp.0 & 0xffff) as i32;
                if id == IDCANCEL.0 {
                    window.request_close();
                } else {
                    window.command(id);
                }
                return LRESULT(0);
            }
            OPEN => {
                window.activate();
                return LRESULT(0);
            }
            INBOX => {
                window.receive();
                return LRESULT(0);
            }
            TEST_FREEZE if window.options.test.is_some() => {
                window.freeze((wp.0 != usize::MAX).then_some(wp.0 as f64));
                return LRESULT(0);
            }
            TEST_PAGE if window.options.test.is_some() => return LRESULT(window.page.get() as isize),
            TEST_IDLE if window.options.test.is_some() => {
                return LRESULT(isize::from(!window.timer.get()));
            }
            WM_CLOSE => {
                window.request_close();
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                window.shared.hwnd.store(WORKER_EXITED, SeqCst);
                // SAFETY: detach before returning to any late messages.
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    PostQuitMessage(0);
                }
            }
            WM_SETTINGCHANGE | WM_THEMECHANGED => {
                if window.options.test.is_none() {
                    let enabled = animation_effects();
                    let mut motion = window.motion.borrow_mut();
                    motion.enabled = enabled;
                    if !enabled {
                        motion.settle();
                    }
                }
                window.set_theme();
            }
            WM_TIMER if wp.0 == TIMER => {
                window.tick();
                return LRESULT(0);
            }
            // Minimized or shown again: the scene stops or plays (it ticks only when seen).
            WM_SIZE | WM_SHOWWINDOW => {
                window.scene_start.set(window.now());
                window.update_animation();
            }
            WM_UPDATEUISTATE => {
                // SAFETY: let Win32 update and propagate the native UI cue bits first.
                let result = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
                window.sync_buttons();
                return result;
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                window.change_ui_state(UIS_SET, UISF_HIDEFOCUS);
            }
            WM_CTLCOLORBTN if lp.0 != 0 => {
                // The native button can paint at once outside WM_PAINT on state changes.
                let child = HWND(lp.0 as *mut c_void);
                let mut rect = RECT::default();
                // SAFETY: WM_CTLCOLORBTN passes a live child HWND and a borrowed painting DC.
                unsafe {
                    let _ = GetClientRect(child, &mut rect);
                    ExcludeClipRect(HDC(wp.0 as *mut c_void), rect.left, rect.top, rect.right, rect.bottom);
                    return LRESULT(GetStockObject(NULL_BRUSH).0 as isize);
                }
            }
            WM_DPICHANGED if window.options.test.is_none() && lp.0 != 0 => {
                // SAFETY: lParam of WM_DPICHANGED points to the suggested RECT for this call.
                let bounds = unsafe { *(lp.0 as *const RECT) };
                window.relayout((wp.0 & 0xffff) as u32);
                window.adjust_size(Some((bounds.left, bounds.top)));
                return LRESULT(0);
            }
            WM_DRAWITEM => return LRESULT(1),
            WM_PAINT => {
                with_paint(hwnd, |dc| window.paint(dc));
                return LRESULT(0);
            }
            WM_PRINTCLIENT => {
                window.paint(HDC(wp.0 as *mut c_void));
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_SETFOCUS => {
                // SAFETY: focus goes to our own Next button.
                let _ = unsafe { SetFocus(Some(window.child(NEXT_ID))) };
                return LRESULT(0);
            }
            _ => {}
        }
    }
    // SAFETY: forward the original message to Win32, including WM_GETOBJECT.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Features = Features { screenshots: true, notes: true, recording: true };

    #[test]
    fn pages_follow_the_switched_on_capabilities() {
        use Page::*;
        assert_eq!(pages(ALL), [Welcome, Screenshot, Drop, Shake, ToolMenu, Record]);
        assert_eq!(pages(Features::default()), [Welcome]);
        assert_eq!(pages(Features { notes: true, ..Features::default() }), [Welcome, ToolMenu]);
        // A recording is carried like a screenshot: it can be dropped and shaken off.
        assert_eq!(pages(Features { recording: true, ..Features::default() }), [Welcome, Drop, Shake, Record]);
        assert_eq!(pages(Features { screenshots: true, ..Features::default() }), [Welcome, Screenshot, Drop, Shake]);
        assert!(pages(ALL).len() <= MAX_PAGES);
    }

    #[test]
    fn features_come_from_the_settings_switches() {
        let mut settings = Settings::default();
        let on = Features::of(&settings);
        assert!(on.screenshots && on.notes);
        assert_eq!(on.recording, Action::ToggleRecording.available());
        settings.set_enabled("igorio.notes", false);
        settings.set_enabled("igorio.screenshots", false);
        let off = Features::of(&settings);
        assert!(!off.notes && !off.screenshots);
    }

    #[test]
    fn each_try_is_done_by_its_own_event_and_stays_done() {
        let mut tries = Tries::default();
        assert!(!tries.done(Page::Screenshot));
        assert!(tries.note(Tried::CardLanded));
        assert!(tries.done(Page::Screenshot));
        assert!(!tries.note(Tried::CardLanded), "a second card changes nothing");
        assert!(!tries.done(Page::Drop) && !tries.done(Page::Shake));
        assert!(tries.note(Tried::Dropped));
        assert!(tries.note(Tried::Shaken));
        assert!(tries.note(Tried::MenuOpened));
        assert!(tries.note(Tried::RecordingStarted));
        for page in [Page::Screenshot, Page::Drop, Page::Shake, Page::ToolMenu, Page::Record] {
            assert!(tries.done(page), "{page:?}");
            assert!(page.tried_by().is_some());
        }
        assert!(!tries.done(Page::Welcome), "the welcome page has nothing to try");
    }

    #[test]
    fn the_try_line_asks_for_a_screenshot_first_when_nothing_is_in_hand() {
        let shortcuts = Shortcuts::defaults();
        assert_eq!(try_action(Page::Welcome, false, &shortcuts), None);
        for page in [Page::Drop, Page::Shake] {
            let empty = try_action(page, false, &shortcuts).unwrap();
            assert!(empty.starts_with("Take a screenshot first"), "{page:?}: {empty}");
            let held = try_action(page, true, &shortcuts).unwrap();
            assert!(!held.contains("screenshot first"), "{page:?}: {held}");
        }
        let screenshot = try_action(Page::Screenshot, false, &shortcuts).unwrap();
        assert_eq!(screenshot, try_action(Page::Screenshot, true, &shortcuts).unwrap());
        let record = try_action(Page::Record, false, &shortcuts).unwrap();
        assert!(record.starts_with("Press Ctrl+Shift+"), "{record}");
        let mut none = shortcuts.clone();
        none.set(Action::ToggleRecording, None).unwrap();
        assert_eq!(try_action(Page::Record, false, &none).unwrap(), "Choose Record Screen in the tray menu.");
    }

    #[test]
    fn key_chips_show_the_users_own_shortcuts_and_skip_unset_ones() {
        let shortcuts = Shortcuts::defaults();
        let record = view(Page::Record, ALL, &shortcuts);
        let labels: Vec<&str> = record.keys.iter().map(|key| key.label.as_str()).collect();
        assert_eq!(&labels[..2], ["Ctrl", "Shift"]);
        assert_eq!(labels.len(), 3);
        assert_eq!(record.more.len(), 3, "zoom, pen and highlighter");
        let mut fewer = shortcuts.clone();
        fewer.set(Action::Pen, None).unwrap();
        let record = view(Page::Record, ALL, &fewer);
        let does: Vec<&str> = record.more.iter().map(|(_, does)| *does).collect();
        assert_eq!(does, ["zoom", "highlighter"]);
        let screenshot = view(Page::Screenshot, ALL, &shortcuts);
        assert!(screenshot.keys.iter().any(|key| !key.chip && key.label == "or"));
        let menu = view(Page::ToolMenu, Features { recording: false, ..ALL }, &shortcuts);
        assert!(!menu.line.contains("Record screen"));
        assert!(menu.aside.is_some());
    }
}
