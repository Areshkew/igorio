//! Pinned cards (`ROADMAP.md`, "Pinned cards"): the top card taken off the pile and left on the
//! screen as a floating picture, from the tool menu's Pin to screen or the "Pin top card"
//! shortcut.
//!
//! The app's UI thread talks to the pins thread through [`Pins`], as it does to the notes thread:
//! commands go in through a channel (never waiting), events come back through another channel
//! with a wake handle. The pins thread owns every pin window and its picture; closing a pin
//! frees it.
//!
//! A pin is a borderless, topmost tool window (no taskbar button, no Alt+Tab entry) with DWM
//! rounded corners and shadow, like a note. It shows without taking the foreground; a click may
//! activate it. It is not excluded from capture, so screenshots and recordings show it. Pins are
//! not restored after a restart.
//!
//! On a pin:
//! - Drag anywhere to move it.
//! - The wheel zooms it around the cursor ([`wheel_target`]), easing to each new frame on a
//!   spring ([`zoom_spring`]) at the display's rate, the picture stretched on the way and made
//!   sharp when it stops; Ctrl+wheel changes its opacity ([`wheel_opacity`]).
//! - Hovered, a close button shows in the top-right corner, drawn like the note header's.
//! - Esc closes it while it is hovered (the key is only watched, never taken from the app in
//!   front) or focused.
//! - A double-click asks to carry the card again ([`Event::Carry`]); the app refuses it while
//!   the user carries other cards (a pin takes no cards), and otherwise takes the card back
//!   ([`Pins::take`]).
//! - A right-click opens a native menu: Carry again, Copy, Save as PNG…, Close.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::{OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use igorio_core::geom::{Point, Rect};
use igorio_kit::{HeldCard, Image};
use tiny_skia::{
    Color, FillRule, IntSize, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform,
};
use windows::Win32::Foundation::{
    CloseHandle, COLORREF, ERROR_CANCELLED, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT,
    WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmExtendFrameIntoClientArea, DwmFlush, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, ClientToScreen, CreateCompatibleDC,
    CreateDIBSection, DEVMODEW, DIB_RGB_COLORS, DeleteDC, DeleteObject, ENUM_CURRENT_SETTINGS,
    EndPaint, EnumDisplaySettingsW, GdiFlush, GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ,
    InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromPoint, PAINTSTRUCT,
    RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, SRCCOPY, SelectObject,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject};
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_ESCAPE,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FileSaveDialog, IFileSaveDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HRESULT, PCWSTR, w};

use crate::clipboard;
use crate::draw;
use crate::notes::editor::{Palette, Transition, animation_effects, ease, mix, px, system_dark};
use crate::notes::window::{lerp_rect, spring};
use crate::overlay::{monitor_at, monitor_bounds_at};
use crate::saver;
use crate::settings::{DefaultFolders, Settings};

/// The most pins on screen at once; another is refused with a hint. Each holds its picture,
/// so this bounds the memory pins can take.
pub const MAX_PINS: usize = 20;
/// A new pin is its card's natural size, made smaller to fit in this share of the monitor's
/// work area.
pub const PIN_SHARE: f64 = 0.4;
/// The shorter side of a pin is never below this, DIPs.
pub const MIN_SIDE: f64 = 64.0;
/// One wheel notch resizes a pin by this factor.
pub const WHEEL_STEP: f64 = 1.1;
/// Smaller wheel turns (high-resolution wheels, touchpads) add up to this share of a notch
/// before the pin moves.
pub const MIN_TURN: f64 = 0.25;
/// How fast a wheel zoom follows the wheel: the stiffness of its critically damped spring, 1/s.
/// It comes within half a pixel in about 140 ms for a notch, about 200 ms for a long way, and
/// never overshoots from rest.
pub const ZOOM_SPRING: f64 = 50.0;
/// A zoom ends after this long, s, whatever is left (it never takes this long).
const ZOOM_LONGEST: f64 = 1.0;
/// Ctrl+wheel: one notch changes the opacity by this much, down to [`MIN_OPACITY`].
pub const OPACITY_STEP: f64 = 0.1;
pub const MIN_OPACITY: f64 = 0.3;
/// The close button, DIPs: its size and its distance from the top and right edges.
pub const CLOSE: f64 = 20.0;
pub const CLOSE_INSET: f64 = 8.0;
/// What the overlay says when the user tries to pin more than [`MAX_PINS`].
pub const FULL_HINT: &str = "20 pins at most · close one first";
/// What the overlay says when a double-click on a pin finds cards in hand.
pub const REFUSE_HINT: &str = "a pin can't take cards";

/// The right-click menu's commands (also sent as `WM_COMMAND` by the self-test).
pub const MENU_CARRY: usize = 1;
pub const MENU_COPY: usize = 2;
pub const MENU_SAVE: usize = 3;
pub const MENU_CLOSE: usize = 4;

const PIN_CLASS: PCWSTR = w!("IgorioPin");
const HUB_CLASS: PCWSTR = w!("IgorioPinsThread");
const COMMAND: u32 = WM_APP + 1;
const TEST_RUN: u32 = WM_APP + 2;
/// To the hub: a display frame for the wheel zooms ([`Pacer`]).
const FRAME: u32 = WM_APP + 3;
const TICK_TIMER: usize = 1;
const REAP_TIMER: usize = 2;
const TICK_MS: u32 = 15;
const APPEAR_SECONDS: f64 = 0.45;
/// A new pin grows from this share of its size.
const APPEAR_FROM: f64 = 0.6;
const FADE_OUT_MS: f64 = 120.0;
const BUTTON_MS: f64 = 120.0;
/// The Ctrl key in a mouse message's key state.
const MK_CONTROL: u32 = 0x0008;

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// A card's size on a monitor at `scale`, physical px: the size it had where it was captured.
pub fn natural_size(image: (u32, u32), card_scale: f64, scale: f64) -> (f64, f64) {
    let card_scale = if card_scale > 0.0 { card_scale } else { 1.0 };
    (f64::from(image.0) / card_scale * scale, f64::from(image.1) / card_scale * scale)
}

/// A new pin's size, physical px: `natural`, made smaller (keeping its shape) to fit in
/// [`PIN_SHARE`] of the work area, and at least `min_side` on its shorter side.
pub fn pin_size(natural: (f64, f64), work: Rect, min_side: f64) -> (f64, f64) {
    let (w, h) = (natural.0.max(1.0), natural.1.max(1.0));
    let fit = (work.width * PIN_SHARE / w).min(work.height * PIN_SHARE / h).min(1.0);
    let grow = (min_side / w.min(h)).max(1.0);
    let k = if fit < 1.0 { fit.max(min_side / w.min(h)).min(1.0) } else { grow };
    ((w * k).round(), (h * k).round())
}

/// A new pin's frame: centred on `at` (where the tool menu opened), kept inside the work area.
pub fn pin_frame(at: Point, size: (f64, f64), work: Rect) -> Rect {
    let (w, h) = (size.0.min(work.width), size.1.min(work.height));
    let x = (at.x - w / 2.0).min(work.max_x() - w).max(work.min_x());
    let y = (at.y - h / 2.0).min(work.max_y() - h).max(work.min_y());
    Rect::new(x.round(), y.round(), w.round(), h.round())
}

/// The frame after `notches` wheel notches (positive grows) with the cursor at `cursor`: the
/// point under the cursor stays put, the shape is kept, the shorter side stays at least
/// `min_side` and the whole fits in `max` (the monitor).
pub fn wheel_frame(frame: Rect, cursor: Point, notches: f64, min_side: f64, max: (f64, f64)) -> Rect {
    let (w, h) = (frame.width.max(1.0), frame.height.max(1.0));
    let largest = (max.0 / w).min(max.1 / h);
    let smallest = (min_side / w.min(h)).min(largest);
    let k = WHEEL_STEP.powf(notches).clamp(smallest, largest);
    let x = cursor.x - (cursor.x - frame.x) * k;
    let y = cursor.y - (cursor.y - frame.y) * k;
    Rect::new(x.round(), y.round(), (w * k).round(), (h * k).round())
}

/// Where the wheel sends a pin: [`wheel_frame`] from `base` (the frame a running zoom heads
/// for, else the pin's frame) by `notches` plus `carry`, the part of earlier turns too small to
/// move it (high-resolution wheels and touchpads send small steps). Returns the frame and the
/// new carry; the frame is `base` while the turns add up to less than [`MIN_TURN`]. Fewer,
/// larger steps keep the shape: each step rounds the frame to whole pixels.
pub fn wheel_target(
    base: Rect,
    cursor: Point,
    notches: f64,
    carry: f64,
    min_side: f64,
    max: (f64, f64),
) -> (Rect, f64) {
    let carry = if carry * notches < 0.0 { 0.0 } else { carry };
    let total = notches + carry;
    // A little slack: steps such as 1/120 of a notch do not add up exactly.
    if total.abs() < MIN_TURN - 1e-9 {
        return (base, total);
    }
    (wheel_frame(base, cursor, total, min_side, max), 0.0)
}

/// One coordinate of a critically damped spring of stiffness [`ZOOM_SPRING`], `seconds` after
/// it set out from `from` at `speed` (per second) toward `to`: where it is and its speed.
pub fn zoom_spring(from: f64, speed: f64, to: f64, seconds: f64) -> (f64, f64) {
    let t = seconds.max(0.0);
    let away = from - to;
    let b = speed + ZOOM_SPRING * away;
    let decay = (-ZOOM_SPRING * t).exp();
    (to + (away + b * t) * decay, (speed - ZOOM_SPRING * b * t) * decay)
}

/// A wheel zoom on its way: the frame it set out from and its speed then (px/s for x, y, width
/// and height), the frame it heads for, and when it set out (s).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Zoom {
    from: Rect,
    speed: [f64; 4],
    to: Rect,
    start: f64,
}

fn parts(r: Rect) -> [f64; 4] {
    [r.x, r.y, r.width, r.height]
}

impl Zoom {
    fn new(from: Rect, to: Rect, now: f64) -> Zoom {
        Zoom { from, speed: [0.0; 4], to, start: now }
    }

    /// The frame and its speed at `now`.
    fn at(&self, now: f64) -> (Rect, [f64; 4]) {
        let (from, to) = (parts(self.from), parts(self.to));
        let moved: [(f64, f64); 4] =
            std::array::from_fn(|i| zoom_spring(from[i], self.speed[i], to[i], now - self.start));
        let [x, y, width, height] = moved.map(|(at, _)| at);
        (Rect::new(x, y, width, height), moved.map(|(_, speed)| speed))
    }

    /// Heads for `to` from where it is at `now`, keeping its speed.
    fn retarget(&self, to: Rect, now: f64) -> Zoom {
        let (from, speed) = self.at(now);
        Zoom { from, speed, to, start: now }
    }

    /// Within half a pixel of its frame and slow, or out of time.
    fn settled(&self, now: f64) -> bool {
        let (frame, speed) = self.at(now);
        let near = parts(frame).iter().zip(parts(self.to)).all(|(a, b)| (a - b).abs() < 0.5);
        near && speed.iter().all(|v| v.abs() < 50.0) || now - self.start > ZOOM_LONGEST
    }
}

/// The opacity after `notches` Ctrl+wheel notches (positive is more opaque).
pub fn wheel_opacity(opacity: f64, notches: f64) -> f64 {
    let stepped = ((opacity + notches * OPACITY_STEP) * 100.0).round() / 100.0;
    stepped.clamp(MIN_OPACITY, 1.0)
}

/// Can another pin be made with `count` on screen?
pub fn can_pin(count: usize) -> bool {
    count < MAX_PINS
}

/// The close button in a client area `width` wide, physical px.
pub fn close_rect(width: i32, dpi: u32) -> RECT {
    let size = px(CLOSE, dpi);
    let inset = px(CLOSE_INSET, dpi);
    RECT { left: width - inset - size, top: inset, right: width - inset, bottom: inset + size }
}

fn inside(rect: RECT, (x, y): (i32, i32)) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn win_rect(r: Rect) -> RECT {
    RECT {
        left: r.x.round() as i32,
        top: r.y.round() as i32,
        right: (r.x + r.width).round() as i32,
        bottom: (r.y + r.height).round() as i32,
    }
}

/// Scales `src` (`from` pixels of four 8-bit channels, rows packed) into the top-left `to`
/// pixels of `dst` (rows `stride` pixels apart), bilinear, in fixed point with two channels
/// per multiply.
/// Quick enough for every frame of a zoom (GDI's HALFTONE takes several times as long).
pub fn stretch(src: &[u32], from: (usize, usize), dst: &mut [u32], stride: usize, to: (usize, usize)) {
    let ((sw, sh), (dw, dh)) = (from, to);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw * sh {
        return;
    }
    // For each output column or row: the two source ones around its centre, and the weight
    // of the second (0 to 256).
    let taps = |out: usize, len: usize| -> Vec<(usize, usize, u32)> {
        let step = len as f64 / out as f64;
        (0..out)
            .map(|i| {
                let at = ((i as f64 + 0.5) * step - 0.5).clamp(0.0, (len - 1) as f64);
                let first = at.floor() as usize;
                (first, (first + 1).min(len - 1), ((at - first as f64) * 256.0).round() as u32)
            })
            .collect()
    };
    let (columns, rows) = (taps(dw, sw), taps(dh, sh));
    let mix = |a: u32, b: u32, weight: u32| -> u32 {
        let keep = 256 - weight;
        let low = ((a & 0x00ff_00ff) * keep + (b & 0x00ff_00ff) * weight) >> 8 & 0x00ff_00ff;
        let high = ((a >> 8 & 0x00ff_00ff) * keep + (b >> 8 & 0x00ff_00ff) * weight) & 0xff00_ff00;
        low | high
    };
    // Each output row: its two source rows mixed first (a straight run), then across.
    let mut blended = vec![0u32; sw];
    for (y, &(top, bottom, down)) in rows.iter().enumerate() {
        let (above, below) = (&src[top * sw..][..sw], &src[bottom * sw..][..sw]);
        for ((out, &a), &b) in blended.iter_mut().zip(above).zip(below) {
            *out = mix(a, b, down);
        }
        let Some(line) = dst.get_mut(y * stride..y * stride + dw) else { return };
        for (out, &(left, right, across)) in line.iter_mut().zip(&columns) {
            *out = mix(blended[left], blended[right], across);
        }
    }
}

fn rect_of(r: RECT) -> Rect {
    Rect::new(
        f64::from(r.left),
        f64::from(r.top),
        f64::from(r.right - r.left),
        f64::from(r.bottom - r.top),
    )
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Test-only switches.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct TestOptions {
    /// Size and draw at this DPI instead of the monitor's.
    pub dpi: Option<u32>,
    /// Force motion on or off (otherwise the system setting).
    pub motion: Option<bool>,
    /// Force the dark theme on or off (otherwise the system's app mode).
    pub dark: Option<bool>,
    /// Never activate a pin, also on a click (leaves the user's foreground window alone).
    pub no_activate: bool,
}

/// What the pins thread tells the UI thread.
#[derive(Clone, Debug)]
pub enum Event {
    /// The user asked to carry this pin's card again: by a double-click on it, or from its
    /// menu. The pin stays until [`Pins::take`].
    Carry { pin: u64, card: HeldCard, double_click: bool },
    /// Something failed that the user should hear about.
    Error(String),
}

enum Command {
    Pin { id: u64, card: HeldCard, at: Point },
    Take(u64),
    CapturesFolder(Option<PathBuf>),
    Shutdown,
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

struct Shared {
    hwnd: AtomicIsize,
    done: AtomicBool,
    /// Pins on screen or asked for and not closed yet.
    count: AtomicUsize,
}

/// The pins thread, used from the app's UI thread. Every method returns at once.
pub struct Pins {
    commands: Sender<Command>,
    events: Receiver<Event>,
    wake: Arc<Wake>,
    shared: Arc<Shared>,
    next_id: Cell<u64>,
    thread: Option<JoinHandle<()>>,
}

impl Pins {
    /// Starts the pins thread. Returns once its message window exists (a few milliseconds).
    pub fn start(captures_folder: Option<PathBuf>, test: Option<TestOptions>) -> io::Result<Pins> {
        // SAFETY: an unnamed auto-reset event, owned by Wake.
        let wake = Arc::new(Wake(
            unsafe { CreateEventW(None, false, false, None) }.map_err(io::Error::other)?,
        ));
        let (commands, receiver) = mpsc::channel();
        let (event_sender, events) = mpsc::channel();
        let shared = Arc::new(Shared {
            hwnd: AtomicIsize::new(0),
            done: AtomicBool::new(false),
            count: AtomicUsize::new(0),
        });
        let (ready_sender, ready) = mpsc::channel();
        let thread = {
            let wake = wake.clone();
            let shared = shared.clone();
            thread::Builder::new().name("pins".into()).spawn(move || {
                let setup = Setup { receiver, events: event_sender, wake, captures_folder };
                if let Err(error) = run(setup, test.unwrap_or_default(), &shared, &ready_sender) {
                    let _ = ready_sender.send(Err(error.to_string()));
                }
                shared.hwnd.store(0, SeqCst);
                shared.done.store(true, SeqCst);
            })?
        };
        match ready.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(message)) => return Err(io::Error::other(message)),
            Err(_) => return Err(io::Error::other("the pins thread did not start")),
        }
        Ok(Pins { commands, events, wake, shared, next_id: Cell::new(1), thread: Some(thread) })
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            let hwnd = self.shared.hwnd.load(SeqCst);
            if hwnd != 0 {
                // SAFETY: a scalar message to the pins thread's message window.
                let _ = unsafe {
                    PostMessageW(Some(HWND(hwnd as *mut c_void)), COMMAND, WPARAM(0), LPARAM(0))
                };
            }
        }
    }

    /// Pins `card` centred on `at` (physical px). The pin shows without taking the foreground.
    /// Returns its id. The caller checks [`can_pin`] with [`Pins::count`] first.
    pub fn pin(&self, card: HeldCard, at: Point) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.shared.count.fetch_add(1, SeqCst);
        self.send(Command::Pin { id, card, at });
        id
    }

    /// Closes a pin whose card went back on the pile.
    pub fn take(&self, pin: u64) {
        self.send(Command::Take(pin));
    }

    /// Pins on screen (or asked for).
    pub fn count(&self) -> usize {
        self.shared.count.load(SeqCst)
    }

    /// The captures folder setting, where Save as PNG… starts.
    pub fn set_captures_folder(&self, folder: Option<PathBuf>) {
        self.send(Command::CapturesFolder(folder));
    }

    /// Events since the last call, oldest first. Call when [`Pins::wake_handle`] is signalled.
    pub fn events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    /// An auto-reset event signalled when events are waiting.
    pub fn wake_handle(&self) -> HANDLE {
        self.wake.0
    }

    /// Closes every pin and ends the thread. True when it ended within `within`; otherwise the
    /// thread is left to finish on its own.
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

    /// Runs `f` on the pins thread with the pin's window and waits for it (tests only).
    #[doc(hidden)]
    pub fn test_with<R: Send + 'static>(
        &self,
        pin: u64,
        f: impl FnOnce(&TestPin<'_>) -> R + Send + 'static,
    ) -> Option<R> {
        let result = Arc::new(Mutex::new(None));
        let out = result.clone();
        self.run_test(Box::new(move |hub: &Hub| {
            let window = hub.windows.borrow().get(&pin).cloned();
            if let Some(window) = window {
                *out.lock().expect("test result") = Some(f(&TestPin { window: &window }));
            }
        }));
        result.lock().ok()?.take()
    }

    /// Ids of the pins on screen, oldest first (tests only).
    #[doc(hidden)]
    pub fn test_ids(&self) -> Vec<u64> {
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
        let pointer = Box::into_raw(Box::new(job));
        // SAFETY: a synchronous cross-thread send; the pins thread takes the box back and runs
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

impl Drop for Pins {
    fn drop(&mut self) {
        self.stop(Duration::from_secs(1));
    }
}

type TestJob = Box<dyn FnOnce(&Hub) + Send>;

/// A pin window seen from a test closure on the pins thread.
#[doc(hidden)]
pub struct TestPin<'a> {
    window: &'a PinWindow,
}

impl TestPin<'_> {
    pub fn hwnd(&self) -> HWND {
        self.window.hwnd
    }

    /// The window's rectangle, physical px.
    pub fn frame(&self) -> Rect {
        self.window.frame()
    }

    pub fn dpi(&self) -> u32 {
        self.window.dpi.get()
    }

    pub fn opacity(&self) -> f64 {
        self.window.opacity.get()
    }

    /// No grow-in or fade is running.
    pub fn still(&self) -> bool {
        matches!(self.window.motion.get(), Motion::Still)
    }

    /// The pointer over the pin (and over its close button), with the button's motion done.
    pub fn hover(&self, hovering: bool, on_button: bool) {
        let window = self.window;
        window.hovering.set(hovering);
        window.hot.set(hovering && on_button);
        window.shown.set(Transition::at(if hovering { 1.0 } else { 0.0 }));
        window.lit.set(Transition::at(if hovering && on_button { 1.0 } else { 0.0 }));
        window.invalidate();
    }

    /// Moves a wheel zoom on by one frame now, as the next display frame would.
    pub fn zoom_frame(&self) -> bool {
        self.window.zoom_frame()
    }

    /// A wheel zoom is on its way.
    pub fn zooming(&self) -> bool {
        self.window.zoom.get().is_some()
    }

    /// The picture is made for the window's size and none is being made.
    pub fn sharp(&self) -> bool {
        let (width, height) = self.window.client();
        let key = picture_key(width, height, self.window.dpi.get());
        self.window.picture.borrow().as_ref().map(Picture::key) == Some(key)
            && self.window.rendering.get().is_none()
    }

    /// Paints since the last call: their count, total and longest time, ms.
    pub fn paint_stats(&self) -> (u32, f64, f64) {
        self.window.paints.replace((0, 0.0, 0.0))
    }

    /// What the pin shows now, at its frame's size (premultiplied RGBA).
    pub fn picture(&self) -> Option<Pixmap> {
        self.window.compose()
    }
}

// ---------------------------------------------------------------------------
// The pins thread
// ---------------------------------------------------------------------------

struct Setup {
    receiver: Receiver<Command>,
    events: Sender<Event>,
    wake: Arc<Wake>,
    captures_folder: Option<PathBuf>,
}

fn run(
    setup: Setup,
    test: TestOptions,
    shared: &Arc<Shared>,
    ready: &Sender<std::result::Result<(), String>>,
) -> windows::core::Result<()> {
    // SAFETY: once on this thread for the Save dialog, balanced below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: balances CoInitializeEx on the same thread.
            unsafe { CoUninitialize() };
        }
    }
    let _com = Com;
    // SAFETY: the process's module handle.
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    register(instance, HUB_CLASS, hub_proc, WNDCLASS_STYLES(0));
    register(instance, PIN_CLASS, pin_proc, CS_DBLCLKS);
    // SAFETY: a message-only window of a registered class, owned by this thread.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            HUB_CLASS,
            w!("Igorio pins"),
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
    let motion = test.motion.unwrap_or_else(animation_effects);
    let dark = test.dark.unwrap_or_else(system_dark);
    let pacer = Pacer::start(hwnd)?;
    let hub = Box::new(Hub {
        hwnd,
        instance,
        windows: RefCell::new(BTreeMap::new()),
        receiver: setup.receiver,
        events: setup.events,
        wake: setup.wake,
        shared: shared.clone(),
        captures_folder: RefCell::new(setup.captures_folder),
        motion: Cell::new(motion),
        palette: Cell::new(Palette::for_theme(dark)),
        test,
        dialog: RefCell::new(None),
        quitting: Cell::new(false),
        pacer,
        rendered: Arc::new(Mutex::new(Vec::new())),
    });
    // SAFETY: the hub outlives the message window (destroyed below before the box drops).
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*hub as *const Hub as isize) };
    shared.hwnd.store(hwnd.0 as isize, SeqCst);
    let _ = ready.send(Ok(()));
    let mut message = MSG::default();
    loop {
        // SAFETY: this thread's queue; -1 is an error and ends the loop.
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if status == 0 || status == -1 {
            break;
        }
        // SAFETY: a message for a window of this thread.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    let windows: Vec<Rc<PinWindow>> = hub.windows.borrow_mut().values().cloned().collect();
    hub.windows.borrow_mut().clear();
    for window in windows {
        window.destroy();
    }
    // SAFETY: our message window; detach the hub pointer first.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}

fn register(
    instance: HINSTANCE,
    name: PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    style: WNDCLASS_STYLES,
) {
    // SAFETY: static class data; registering twice in one process fails harmlessly.
    unsafe {
        let class = WNDCLASSW {
            style,
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: name,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
    }
}

/// Everything the pins thread owns.
#[doc(hidden)]
pub struct Hub {
    hwnd: HWND,
    instance: HINSTANCE,
    windows: RefCell<BTreeMap<u64, Rc<PinWindow>>>,
    receiver: Receiver<Command>,
    events: Sender<Event>,
    wake: Arc<Wake>,
    shared: Arc<Shared>,
    captures_folder: RefCell<Option<PathBuf>>,
    motion: Cell<bool>,
    palette: Cell<Palette>,
    test: TestOptions,
    /// The Save dialog while it is open, and the pin it belongs to.
    dialog: RefCell<Option<(IFileSaveDialog, u64)>>,
    quitting: Cell<bool>,
    pacer: Pacer,
    /// Pictures made on workers, for the pins that asked (`None` when one failed).
    rendered: Rendered,
}

impl Hub {
    /// Hands the pictures made on workers to their pins.
    fn take_rendered(&self) {
        let done = std::mem::take(&mut *self.rendered.lock().unwrap_or_else(|e| e.into_inner()));
        for (id, picture) in done {
            let window = self.windows.borrow().get(&id).cloned();
            if let Some(window) = window {
                window.rendered(picture);
            }
        }
    }

    /// A display frame: every wheel zoom moves on; the frames stop when none is left.
    fn frame(&self) {
        self.pacer.state.posted.store(false, SeqCst);
        let windows: Vec<Rc<PinWindow>> = self.windows.borrow().values().cloned().collect();
        let mut moving = false;
        for window in windows {
            moving |= window.zoom_frame();
        }
        if !moving {
            self.pacer.stop_frames();
        }
    }

    fn emit(&self, event: Event) {
        if self.events.send(event).is_ok() {
            // SAFETY: the Arc keeps the event alive during the call.
            let _ = unsafe { SetEvent(self.wake.0) };
        }
    }

    fn error(&self, message: String) {
        self.emit(Event::Error(message));
    }

    fn dialog_owner(&self) -> Option<u64> {
        self.dialog.borrow().as_ref().map(|(_, owner)| *owner)
    }

    /// Cancels the Save dialog; its `Show` returns once the dialog's loop sees it.
    fn cancel_dialog(&self) {
        let dialog = self.dialog.borrow().as_ref().map(|(dialog, _)| dialog.clone());
        if let Some(dialog) = dialog {
            // SAFETY: Close on the STA that owns Show.
            let _ = unsafe { dialog.Close(HRESULT::from_win32(ERROR_CANCELLED.0)) };
        }
    }

    fn commands(&self) {
        while let Ok(command) = self.receiver.try_recv() {
            match command {
                Command::Pin { id, card, at } => self.create(id, card, at),
                Command::Take(id) => {
                    let window = self.windows.borrow().get(&id).cloned();
                    if let Some(window) = window {
                        window.close();
                    }
                }
                Command::CapturesFolder(folder) => *self.captures_folder.borrow_mut() = folder,
                Command::Shutdown => {
                    self.quitting.set(true);
                    if self.dialog_owner().is_some() {
                        self.cancel_dialog();
                    } else {
                        // SAFETY: ends this thread's message loop.
                        unsafe { PostQuitMessage(0) };
                    }
                }
            }
        }
    }

    fn scale_at(&self, at: Point) -> f64 {
        self.test.dpi.map_or_else(|| monitor_at(at).scale, |dpi| f64::from(dpi) / 96.0)
    }

    fn create(&self, id: u64, card: HeldCard, at: Point) {
        let work = monitor_at(at).work_area;
        let scale = self.scale_at(at);
        let natural = natural_size((card.image.width, card.image.height), card.scale, scale);
        let size = pin_size(natural, work, MIN_SIDE * scale);
        let frame = pin_frame(at, size, work);
        match PinWindow::create(self, id, card, frame) {
            Ok(window) => {
                self.windows.borrow_mut().insert(id, window.clone());
                window.appear();
            }
            Err(error) => {
                self.shared.count.fetch_sub(1, SeqCst);
                self.error(format!("The card could not be pinned: {error}"));
            }
        }
    }

    /// A closed pin finished fading out: its window and picture go.
    fn reap(&self) {
        let owner = self.dialog_owner();
        let done: Vec<u64> = self
            .windows
            .borrow()
            .iter()
            .filter(|(id, window)| window.gone.get() && Some(**id) != owner)
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            let window = self.windows.borrow_mut().remove(&id);
            if let Some(window) = window {
                window.destroy();
                self.shared.count.fetch_sub(1, SeqCst);
            }
        }
        if self.windows.borrow().values().any(|window| window.gone.get()) {
            // The Save dialog's pin waits until the dialog is gone.
            // SAFETY: a timer on our message window.
            unsafe { SetTimer(Some(self.hwnd), REAP_TIMER, 50, None) };
        }
    }

    fn post_reap(&self) {
        // SAFETY: our own message window; handled after the current message.
        let _ = unsafe { PostMessageW(Some(self.hwnd), COMMAND, WPARAM(1), LPARAM(0)) };
    }

    fn settings_changed(&self) {
        if self.test.motion.is_none() {
            self.motion.set(animation_effects());
        }
        if self.test.dark.is_none() {
            self.palette.set(Palette::for_theme(system_dark()));
        }
        for window in self.windows.borrow().values() {
            window.invalidate();
        }
    }

    /// Save as PNG…: a Save dialog that starts in the captures folder with the screenshot's
    /// usual name; the file is written on a worker by the saver.
    fn save(&self, window: &PinWindow) {
        if self.dialog_owner().is_some() {
            return;
        }
        let chosen = Settings { captures_folder: self.captures_folder.borrow().clone(), ..Settings::default() };
        let (folder, _) = chosen.captures_dir(&DefaultFolders::from_system());
        let name = saver::screenshot_file_name(window.card.captured_at);
        let result = self.ask_path(window, &folder, &name);
        if self.quitting.get() {
            // SAFETY: ends this thread's message loop, which the dialog held up.
            unsafe { PostQuitMessage(0) };
            return;
        }
        match result {
            Ok(Some(path)) => {
                let image = window.card.image.clone();
                let scale = window.card.scale;
                let events = self.events.clone();
                let wake = self.wake.clone();
                thread::spawn(move || {
                    if let Err(error) = saver::save_png_as(&image, scale, &path)
                        && events.send(Event::Error(format!("The picture was not saved: {error}"))).is_ok()
                    {
                        // SAFETY: the Arc keeps the event alive during the call.
                        let _ = unsafe { SetEvent(wake.0) };
                    }
                });
            }
            Ok(None) => {}
            Err(error) => self.error(format!("The Save dialog failed: {error}")),
        }
        self.reap();
    }

    fn ask_path(
        &self,
        window: &PinWindow,
        folder: &std::path::Path,
        name: &str,
    ) -> windows::core::Result<Option<PathBuf>> {
        // SAFETY: COM was initialized on this thread, and owns the returned interface.
        let dialog: IFileSaveDialog =
            unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)? };
        let wide = |text: &std::ffi::OsStr| -> Vec<u16> { text.encode_wide().chain([0]).collect() };
        let folder_text = wide(folder.as_os_str());
        let name_text = wide(std::ffi::OsStr::new(name));
        let filter = [COMDLG_FILTERSPEC { pszName: w!("PNG image"), pszSpec: w!("*.png") }];
        // SAFETY: owned COM interfaces and terminated strings that outlive the calls.
        unsafe {
            dialog.SetOptions(dialog.GetOptions()? | FOS_OVERWRITEPROMPT | FOS_FORCEFILESYSTEM)?;
            dialog.SetTitle(w!("Save as PNG"))?;
            dialog.SetFileTypes(&filter)?;
            dialog.SetDefaultExtension(w!("png"))?;
            dialog.SetFileName(PCWSTR(name_text.as_ptr()))?;
            if let Ok(start) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(folder_text.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&start);
            }
        }
        self.dialog.replace(Some((dialog.clone(), window.id)));
        // SAFETY: the owner lives through the modal call: its pin is not destroyed while the
        // dialog is open (`reap` waits for it).
        let shown = unsafe { dialog.Show(Some(window.hwnd)) };
        self.dialog.replace(None);
        if let Err(error) = shown {
            return if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: copy the terminated path returned by the Shell and release its allocation.
        let path = unsafe {
            let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = PathBuf::from(OsString::from_wide(name.as_wide()));
            CoTaskMemFree(Some(name.0.cast()));
            path
        };
        Ok(Some(path))
    }
}

/// Copy: the card's picture on the clipboard, from a worker with its own message-only window
/// as the owner (rule 4: never on a window thread). The window stays a moment so the
/// screenshot detector sees the copy is ours.
fn copy(card: &HeldCard, events: Sender<Event>, wake: Arc<Wake>) {
    let image = card.image.clone();
    thread::spawn(move || {
        // SAFETY: a message-only window of a system class, owned and destroyed by this worker.
        let owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Igorio pin copy"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        };
        let result = match &owner {
            Ok(owner) => clipboard::copy_image(*owner, &image).map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = result
            && events.send(Event::Error(format!("The picture was not copied: {error}"))).is_ok()
        {
            // SAFETY: the Arc keeps the event alive during the call.
            let _ = unsafe { SetEvent(wake.0) };
        }
        if let Ok(owner) = owner {
            let until = Instant::now() + Duration::from_secs(2);
            let mut message = MSG::default();
            while Instant::now() < until {
                // SAFETY: this worker's own queue (sent messages such as WM_DESTROYCLIPBOARD).
                unsafe {
                    while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&message);
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: our own window, on its thread.
            let _ = unsafe { DestroyWindow(owner) };
        }
    });
}

unsafe extern "system" fn hub_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the hub while the window lives (0 before and after).
    let hub = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Hub).as_ref() };
    if let Some(hub) = hub {
        match msg {
            COMMAND => {
                match wp.0 {
                    1 => hub.reap(),
                    2 => hub.take_rendered(),
                    _ => hub.commands(),
                }
                return LRESULT(0);
            }
            FRAME => {
                hub.frame();
                return LRESULT(0);
            }
            WM_TIMER if wp.0 == REAP_TIMER => {
                // SAFETY: our own one-shot timer.
                let _ = unsafe { KillTimer(Some(hwnd), REAP_TIMER) };
                hub.reap();
                return LRESULT(0);
            }
            TEST_RUN => {
                // SAFETY: `run_test` boxed this job and waits in SendMessage until it ran.
                let job = unsafe { Box::from_raw(wp.0 as *mut TestJob) };
                job(hub);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    // SAFETY: default handling.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Display frames for the wheel zooms, and only while one moves (rule 8): a thread waits for
/// each composition of the desktop (`DwmFlush`) and posts [`FRAME`] to the hub, one at a time.
/// The desktop is composed at its fastest monitor's rate; frames come no faster than the rate
/// of the monitor the zoom is on.
struct Pacer {
    state: Arc<PacerState>,
}

struct PacerState {
    running: AtomicBool,
    /// A frame is in the hub's queue.
    posted: AtomicBool,
    quit: AtomicBool,
    /// The time between frames on the zoom's monitor, µs.
    period: AtomicU64,
    wake: Wake,
}

/// The refresh rate of the monitor nearest `point` (physical px), Hz; 60 when unknown.
fn refresh_at(point: Point) -> u32 {
    let point = POINT { x: point.x.round() as i32, y: point.y.round() as i32 };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    // SAFETY: plain queries into sized locals; the device name is NUL-terminated.
    let known = unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool()
            && EnumDisplaySettingsW(PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode).as_bool()
    };
    // 0 and 1 mean the hardware's default.
    if known && mode.dmDisplayFrequency > 1 { mode.dmDisplayFrequency } else { 60 }
}

impl Pacer {
    fn start(hub: HWND) -> windows::core::Result<Pacer> {
        // SAFETY: an unnamed auto-reset event, owned by Wake.
        let wake = Wake(unsafe { CreateEventW(None, false, false, None) }?);
        let state = Arc::new(PacerState {
            running: AtomicBool::new(false),
            posted: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            period: AtomicU64::new(16_667),
            wake,
        });
        let shared = state.clone();
        let hub = hub.0 as isize;
        thread::Builder::new()
            .name("pins frames".into())
            .spawn(move || pace(&shared, hub))
            .map_err(|error| windows::core::Error::new(HRESULT(-1), error.to_string()))?;
        Ok(Pacer { state })
    }

    /// Frames for a zoom on a monitor of `hz`.
    fn start_frames(&self, hz: u32) {
        self.state.period.store(1_000_000 / u64::from(hz.max(1)), SeqCst);
        if !self.state.running.swap(true, SeqCst) {
            // SAFETY: the Arc keeps the event alive during the call.
            let _ = unsafe { SetEvent(self.state.wake.0) };
        }
    }

    fn stop_frames(&self) {
        self.state.running.store(false, SeqCst);
    }
}

impl Drop for Pacer {
    /// The thread ends after its current wait; it is not joined.
    fn drop(&mut self) {
        self.state.quit.store(true, SeqCst);
        // SAFETY: the Arc keeps the event alive during the call.
        let _ = unsafe { SetEvent(self.state.wake.0) };
    }
}

fn pace(state: &PacerState, hub: isize) {
    let mut last = Instant::now();
    while !state.quit.load(SeqCst) {
        if !state.running.load(SeqCst) {
            // SAFETY: our own event, alive while the Arc is.
            unsafe { WaitForSingleObject(state.wake.0, INFINITE) };
            continue;
        }
        let before = Instant::now();
        // SAFETY: a plain wait for the next composition.
        let flushed = unsafe { DwmFlush() }.is_ok();
        // Without composition (or nothing to compose), about 60 frames a second.
        if !flushed || before.elapsed() < Duration::from_millis(1) {
            thread::sleep(Duration::from_millis(if flushed { 4 } else { 15 }));
        }
        // A composition the zoom's (slower) monitor does not show.
        let period = Duration::from_micros(state.period.load(SeqCst));
        if last.elapsed() < period.mul_f64(0.85) {
            continue;
        }
        if state.running.load(SeqCst) && !state.posted.swap(true, SeqCst) {
            last = Instant::now();
            // SAFETY: a scalar message to the hub's window; it fails once that is gone.
            let sent = unsafe { PostMessageW(Some(HWND(hub as *mut c_void)), FRAME, WPARAM(0), LPARAM(0)) };
            if sent.is_err() {
                state.posted.store(false, SeqCst);
            }
        }
    }
}

/// The card's picture at one size, ready to paint: one `u32` per pixel (BGRA in memory, as GDI
/// takes it), rows top first.
struct Picture {
    width: u32,
    height: u32,
    dpi: u32,
    bgra: Vec<u32>,
}

/// What a picture is made for: its size and the DPI of its badge.
type PictureKey = (u32, u32, u32);

/// Pictures made on workers, with the pins they are for.
type Rendered = Arc<Mutex<Vec<(u64, Option<Picture>)>>>;

fn picture_key(width: i32, height: i32, dpi: u32) -> PictureKey {
    (width.max(1) as u32, height.max(1) as u32, dpi)
}

impl Picture {
    fn key(&self) -> PictureKey {
        (self.width, self.height, self.dpi)
    }
}

/// A 32-bit top-down DIB header.
fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Makes a card's picture, on white, with the recording badge when `badge` holds a recording's
/// duration. Slow for a big card: it runs once per size, on a worker after the first.
fn render(image: &Image, (width, height, dpi): PictureKey, badge: Option<Option<f64>>) -> Option<Picture> {
    let mut picture = Pixmap::new(width, height)?;
    picture.fill(Color::WHITE);
    let fitted = draw::fit_image(image, width, height)?;
    picture.draw_pixmap(0, 0, fitted.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    if let Some(duration) = badge {
        draw::draw_movie_badge(&mut picture, duration, f64::from(dpi) / 96.0);
    }
    let bgra = picture.data().as_chunks::<4>().0.iter();
    let bgra = bgra.map(|&[r, g, b, a]| u32::from_le_bytes([b, g, r, a])).collect();
    Some(Picture { width, height, dpi, bgra })
}

/// Where a paint is put together: the picture stretched (or copied) into it, the close button
/// drawn over, then one copy to the window. At least the window's size; it is kept between
/// paints so a zoom does not make one per frame.
struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    width: i32,
    height: i32,
}

impl Canvas {
    fn new(width: i32, height: i32) -> Option<Canvas> {
        // SAFETY: a memory DC and a DIB section owned by the canvas, freed in Drop.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let info = bitmap_info(width, height);
            let mut bits = std::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(dc);
                return None;
            };
            if bits.is_null() {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap.into());
            Some(Canvas { dc, bitmap, old, bits: bits.cast(), width, height })
        }
    }

    /// The pixels: rows of `width`, top first, one `u32` (BGRA in memory) each.
    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: the DIB section (page-aligned) holds width × height 32-bit pixels while the
        // canvas lives, and GDI is flushed before this is touched.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.width as usize * self.height as usize) }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: the objects made in `new`, freed once.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// ---------------------------------------------------------------------------
// One pin
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Motion {
    Still,
    Growing { from: Rect, to: Rect, start: f64 },
    Fading { start: f64 },
}

struct PinWindow {
    id: u64,
    hwnd: HWND,
    hub: *const Hub,
    card: HeldCard,
    dpi: Cell<u32>,
    opacity: Cell<f64>,
    /// The card's picture with the recording badge: at the frame's size when nothing moves,
    /// stretched to it while the pin grows in or zooms.
    picture: RefCell<Option<Picture>>,
    /// The picture being made on a worker, and the next one wanted.
    rendering: Cell<Option<PictureKey>>,
    wanted: Cell<Option<PictureKey>>,
    canvas: RefCell<Option<Canvas>>,
    motion: Cell<Motion>,
    /// A wheel zoom on its way, moved by the hub's display frames.
    zoom: Cell<Option<Zoom>>,
    /// Wheel turns too small to move the pin yet ([`wheel_target`]).
    carry: Cell<f64>,
    layered: Cell<bool>,
    hovering: Cell<bool>,
    /// The pointer is over the close button.
    hot: Cell<bool>,
    /// The close button showing (0 to 1), and lit under the pointer.
    shown: Cell<Transition>,
    lit: Cell<Transition>,
    /// A press on the close button, waiting for its release.
    pressing: Cell<bool>,
    /// A drag: where the pointer holds the window, from its top-left corner.
    drag: Cell<Option<(i32, i32)>>,
    escape_down: Cell<bool>,
    timer: Cell<bool>,
    closing: Cell<bool>,
    /// Faded out: waiting to be destroyed.
    gone: Cell<bool>,
    epoch: Instant,
    /// Paints since the self-test last asked: count, total and longest time, ms.
    paints: Cell<(u32, f64, f64)>,
}

impl PinWindow {
    fn hub(&self) -> &Hub {
        // SAFETY: the Hub outlives every window it owns (it destroys them before it drops).
        unsafe { &*self.hub }
    }

    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    fn create(hub: &Hub, id: u64, card: HeldCard, frame: Rect) -> windows::core::Result<Rc<PinWindow>> {
        // A thick frame keeps the DWM shadow; WM_NCCALCSIZE gives the whole window to the client.
        let style = WINDOW_STYLE(WS_POPUP.0 | WS_THICKFRAME.0);
        let ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
        let rect = win_rect(frame);
        // SAFETY: a top-level window of our class, created on the pins thread.
        let hwnd = unsafe {
            CreateWindowExW(
                ex,
                PIN_CLASS,
                w!("Pinned card"),
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
        let key = picture_key(rect.right - rect.left, rect.bottom - rect.top, dpi);
        let picture = render(&card.image, key, card.is_movie().then_some(card.duration));
        let window = Rc::new(PinWindow {
            id,
            hwnd,
            hub,
            card,
            dpi: Cell::new(dpi),
            opacity: Cell::new(1.0),
            picture: RefCell::new(picture),
            rendering: Cell::new(None),
            wanted: Cell::new(None),
            canvas: RefCell::new(None),
            motion: Cell::new(Motion::Still),
            zoom: Cell::new(None),
            carry: Cell::new(0.0),
            layered: Cell::new(false),
            hovering: Cell::new(false),
            hot: Cell::new(false),
            shown: Cell::new(Transition::at(0.0)),
            lit: Cell::new(Transition::at(0.0)),
            pressing: Cell::new(false),
            drag: Cell::new(None),
            escape_down: Cell::new(false),
            timer: Cell::new(false),
            closing: Cell::new(false),
            gone: Cell::new(false),
            epoch: Instant::now(),
            paints: Cell::new((0, 0.0, 0.0)),
        });
        // SAFETY: the Rc in the hub's map keeps the state alive until WM_NCDESTROY clears this
        // pointer (destroy() runs before the map drops it).
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::as_ptr(&window) as isize) };
        let corner = DWMWCP_ROUND;
        let margins = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 0, cyBottomHeight: 1 };
        // SAFETY: DWM copies the values during each call; the window is ours.
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
                4,
            );
            // A one-pixel frame extension keeps the DWM shadow on a frameless window.
            let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        }
        Ok(window)
    }

    fn frame(&self) -> Rect {
        let mut rect = RECT::default();
        // SAFETY: our window, a writable local.
        let _ = unsafe { GetWindowRect(self.hwnd, &mut rect) };
        rect_of(rect)
    }

    fn client(&self) -> (i32, i32) {
        let mut rect = RECT::default();
        // SAFETY: our window, a writable local.
        let _ = unsafe { GetClientRect(self.hwnd, &mut rect) };
        (rect.right - rect.left, rect.bottom - rect.top)
    }

    fn invalidate(&self) {
        // SAFETY: our window.
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    /// Moves and sizes the window in one step and paints it at once, so no frame shows it at
    /// its new size with old or missing pixels.
    fn place(&self, rect: Rect) {
        let r = win_rect(rect);
        // SAFETY: our window.
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOCOPYBITS | SWP_NOREDRAW,
            );
            let _ = RedrawWindow(Some(self.hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW);
        }
    }

    fn badge(&self) -> Option<Option<f64>> {
        self.card.is_movie().then_some(self.card.duration)
    }

    /// Asks for the picture at `width` × `height`, made on a worker; until it comes, the one
    /// there is stretched.
    fn want(&self, width: i32, height: i32) {
        let key = picture_key(width, height, self.dpi.get());
        let have = self.picture.borrow().as_ref().map(Picture::key);
        if have == Some(key) || self.rendering.get() == Some(key) {
            self.wanted.set(None);
            return;
        }
        self.wanted.set(Some(key));
        self.render_next();
    }

    /// Starts the wanted picture when no other is being made: one worker per pin at most.
    fn render_next(&self) {
        if self.rendering.get().is_some() {
            return;
        }
        let Some(key) = self.wanted.take() else { return };
        self.rendering.set(Some(key));
        let (id, image, badge) = (self.id, self.card.image.clone(), self.badge());
        let (done, hub) = (self.hub().rendered.clone(), self.hub().hwnd.0 as isize);
        let started = thread::Builder::new().name("pin picture".into()).spawn(move || {
            let picture = render(&image, key, badge);
            done.lock().unwrap_or_else(|e| e.into_inner()).push((id, picture));
            // SAFETY: a scalar message to the hub's window; it fails once that is gone.
            let _ = unsafe { PostMessageW(Some(HWND(hub as *mut c_void)), COMMAND, WPARAM(2), LPARAM(0)) };
        });
        if started.is_err() {
            self.rendering.set(None);
        }
    }

    /// A picture from a worker.
    fn rendered(&self, picture: Option<Picture>) {
        self.rendering.set(None);
        if let Some(picture) = picture {
            let mut current = self.picture.borrow_mut();
            // While zooming, one made for an earlier, smaller target would blur the pin.
            let worse = self.zoom.get().is_some()
                && current.as_ref().is_some_and(|current| current.width > picture.width);
            if !worse {
                *current = Some(picture);
                drop(current);
                self.invalidate();
            }
        }
        self.render_next();
    }

    /// Nothing moves: the sharp picture at the window's size, and a canvas not much bigger.
    fn settle(&self) {
        let (width, height) = self.client();
        self.want(width, height);
        let mut canvas = self.canvas.borrow_mut();
        if canvas.as_ref().is_some_and(|canvas| canvas.width * canvas.height > width * height * 2) {
            *canvas = None;
        }
    }

    /// Stops a wheel zoom where it is.
    fn stop_zoom(&self) {
        self.carry.set(0.0);
        if self.zoom.take().is_some() {
            self.settle();
        }
    }

    /// One display frame of a wheel zoom; false when none is on its way.
    fn zoom_frame(&self) -> bool {
        let Some(zoom) = self.zoom.get() else { return false };
        let now = self.now() / 1000.0;
        if zoom.settled(now) {
            self.zoom.set(None);
            self.place(zoom.to);
            self.settle();
            return false;
        }
        self.place(zoom.at(now).0);
        true
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
                            RDW_ERASE | RDW_INVALIDATE | RDW_FRAME,
                        );
                    }
                }
            }
        }
    }

    /// The opacity the user chose, as the window shows it when nothing moves.
    fn rest_alpha(&self) {
        let opacity = self.opacity.get();
        self.set_layered((opacity < 1.0).then(|| (opacity * 255.0).round() as u8));
    }

    /// Shows the pin without taking the foreground. It grows in from the middle (the notes'
    /// spring) and fades in; with reduced motion it is there at once.
    fn appear(&self) {
        let to = self.frame();
        if self.hub().motion.get() {
            let from = Rect::new(
                to.x + to.width * (1.0 - APPEAR_FROM) / 2.0,
                to.y + to.height * (1.0 - APPEAR_FROM) / 2.0,
                to.width * APPEAR_FROM,
                to.height * APPEAR_FROM,
            );
            self.motion.set(Motion::Growing { from, to, start: self.now() });
            self.set_layered(Some(0));
            self.place(from);
        }
        // SAFETY: our window.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        self.step();
    }

    /// Fades the pin out; then it is destroyed with its picture.
    fn close(&self) {
        if self.closing.replace(true) {
            return;
        }
        if self.hub().dialog_owner() == Some(self.id) {
            self.hub().cancel_dialog();
        }
        self.zoom.set(None);
        // SAFETY: our own capture, if it has it.
        let _ = unsafe { ReleaseCapture() };
        if self.hub().motion.get() {
            self.motion.set(Motion::Fading { start: self.now() });
            self.set_layered(Some((self.opacity.get() * 255.0).round() as u8));
            self.step();
        } else {
            self.vanish();
        }
    }

    fn vanish(&self) {
        self.motion.set(Motion::Still);
        // SAFETY: our window.
        let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        self.gone.set(true);
        self.hub().post_reap();
    }

    fn destroy(&self) {
        // SAFETY: our window; WM_NCDESTROY detaches its state pointer.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TICK_TIMER);
            if IsWindow(Some(self.hwnd)).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
        self.zoom.set(None);
        self.picture.borrow_mut().take();
        self.canvas.borrow_mut().take();
    }

    fn animating(&self) -> bool {
        let now = self.now();
        !matches!(self.motion.get(), Motion::Still)
            || self.shown.get().active(now)
            || self.lit.get().active(now)
    }

    fn sync_timer(&self) {
        let run = !self.gone.get() && (self.animating() || self.hovering.get());
        if run != self.timer.get() {
            // SAFETY: our window's own timer. The flag follows what Windows did.
            let running = unsafe {
                if run {
                    SetTimer(Some(self.hwnd), TICK_TIMER, TICK_MS, None) != 0
                } else {
                    let _ = KillTimer(Some(self.hwnd), TICK_TIMER);
                    false
                }
            };
            self.timer.set(running);
        }
    }

    /// One frame: the grow-in or fade, the close button's motion, and Esc while hovered.
    fn step(&self) {
        let now = self.now();
        match self.motion.get() {
            Motion::Growing { from, to, start } => {
                let t = (now - start) / 1000.0;
                let alpha = (t / 0.15).clamp(0.0, 1.0) * self.opacity.get();
                if t >= APPEAR_SECONDS {
                    self.motion.set(Motion::Still);
                    self.place(to);
                    self.rest_alpha();
                    self.settle();
                } else {
                    self.place(lerp_rect(from, to, spring(t)));
                    self.set_layered(Some((alpha * 255.0).round() as u8));
                }
            }
            Motion::Fading { start } => {
                let progress = ((now - start) / FADE_OUT_MS).clamp(0.0, 1.0);
                let alpha = (1.0 - ease(progress)) * self.opacity.get();
                self.set_layered(Some((alpha * 255.0).round() as u8));
                if progress >= 1.0 {
                    self.vanish();
                }
            }
            Motion::Still => {}
        }
        if self.hovering.get() && !self.closing.get() {
            // SAFETY: a plain key state query; the key stays the foreground app's.
            let down = unsafe { GetAsyncKeyState(i32::from(VK_ESCAPE.0)) } < 0;
            if down && !self.escape_down.replace(down) {
                self.close();
            }
            self.escape_down.set(down);
        }
        if self.shown.get().active(now) || self.lit.get().active(now) {
            self.invalidate();
        }
        self.sync_timer();
    }

    fn set_hover(&self, hovering: bool, hot: bool) {
        let now = self.now();
        let duration = if self.hub().motion.get() { BUTTON_MS } else { 0.0 };
        if hovering && !self.hovering.get() {
            // A key already down when the pointer arrives is not a press.
            // SAFETY: a plain key state query.
            self.escape_down.set(unsafe { GetAsyncKeyState(i32::from(VK_ESCAPE.0)) } < 0);
        }
        self.hovering.set(hovering);
        self.hot.set(hot);
        let mut shown = self.shown.get();
        shown.retarget(if hovering { 1.0 } else { 0.0 }, now, duration);
        self.shown.set(shown);
        let mut lit = self.lit.get();
        lit.retarget(if hot { 1.0 } else { 0.0 }, now, duration);
        self.lit.set(lit);
        self.invalidate();
        self.sync_timer();
    }

    fn close_rect(&self) -> RECT {
        close_rect(self.client().0, self.dpi.get())
    }

    fn screen_point(&self, lp: LPARAM) -> POINT {
        let mut point = POINT { x: i32::from(lp.0 as i16), y: i32::from((lp.0 >> 16) as i16) };
        // SAFETY: our window, a writable local.
        let _ = unsafe { ClientToScreen(self.hwnd, &mut point) };
        point
    }

    fn mouse_move(&self, lp: LPARAM) {
        if let Some((dx, dy)) = self.drag.get() {
            let point = self.screen_point(lp);
            // SAFETY: our window.
            let _ = unsafe {
                SetWindowPos(
                    self.hwnd,
                    None,
                    point.x - dx,
                    point.y - dy,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            return;
        }
        if !self.hovering.get() {
            let mut request = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: a leave request for our own window.
            let _ = unsafe { TrackMouseEvent(&mut request) };
        }
        let at = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
        self.set_hover(true, inside(self.close_rect(), at));
    }

    fn button_down(&self, lp: LPARAM) {
        self.stop_zoom();
        let at = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
        // The self-test sends its clicks as messages: with capture, a real pointer move
        // between them would reach this window.
        if !self.hub().test.no_activate {
            // SAFETY: capture for our own window while the button is down.
            unsafe { SetCapture(self.hwnd) };
        }
        if inside(self.close_rect(), at) {
            self.pressing.set(true);
            return;
        }
        let point = self.screen_point(lp);
        let frame = self.frame();
        self.drag.set(Some((point.x - frame.x as i32, point.y - frame.y as i32)));
    }

    fn button_up(&self, lp: LPARAM) {
        let at = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
        let close = self.pressing.replace(false) && inside(self.close_rect(), at);
        self.drag.set(None);
        // SAFETY: our own capture.
        let _ = unsafe { ReleaseCapture() };
        if close {
            self.close();
        }
    }

    fn wheel(&self, wp: WPARAM, lp: LPARAM) {
        if !matches!(self.motion.get(), Motion::Still) || self.closing.get() {
            return;
        }
        let notches = f64::from((wp.0 >> 16) as i16) / f64::from(WHEEL_DELTA);
        let keys = (wp.0 & 0xffff) as u32;
        if keys & MK_CONTROL != 0 {
            self.opacity.set(wheel_opacity(self.opacity.get(), notches));
            self.rest_alpha();
            return;
        }
        let cursor = Point::new(f64::from(lp.0 as i16), f64::from((lp.0 >> 16) as i16));
        let bounds = monitor_bounds_at(cursor).bounds;
        let min_side = MIN_SIDE * f64::from(self.dpi.get()) / 96.0;
        // Turns while zooming add up: the next target goes on from the last.
        let zoom = self.zoom.get();
        let base = zoom.map_or_else(|| self.frame(), |zoom| zoom.to);
        let max = (bounds.width, bounds.height);
        let (target, carry) = wheel_target(base, cursor, notches, self.carry.get(), min_side, max);
        self.carry.set(carry);
        if target == base {
            return;
        }
        let (width, height) = (target.width.round() as i32, target.height.round() as i32);
        if !self.hub().motion.get() {
            // Reduced motion: there at once, stretched until the sharp picture comes.
            self.place(target);
            self.want(width, height);
            return;
        }
        let now = self.now() / 1000.0;
        let zoom = match zoom {
            Some(zoom) => zoom.retarget(target, now),
            None => Zoom::new(self.frame(), target, now),
        };
        self.zoom.set(Some(zoom));
        // Growing: a picture made for the target, so the stretch is not blurry for long.
        if self.picture.borrow().as_ref().is_some_and(|picture| picture.width < width as u32) {
            self.want(width, height);
        }
        self.hub().pacer.start_frames(refresh_at(cursor));
    }

    fn command(&self, id: usize) {
        let hub = self.hub();
        match id {
            MENU_CARRY => hub.emit(Event::Carry { pin: self.id, card: self.card.clone(), double_click: false }),
            MENU_COPY => copy(&self.card, hub.events.clone(), hub.wake.clone()),
            MENU_SAVE => hub.save(self),
            MENU_CLOSE => self.close(),
            _ => {}
        }
    }

    fn context_menu(&self, at: POINT) {
        let hub = self.hub();
        // SAFETY: a popup menu owned here and destroyed below; the strings are static.
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let _ = AppendMenuW(menu, MF_STRING, MENU_CARRY, w!("Carry again"));
            let _ = AppendMenuW(menu, MF_STRING, MENU_COPY, w!("Copy"));
            let _ = AppendMenuW(menu, MF_STRING, MENU_SAVE, w!("Save as PNG…"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, MENU_CLOSE, w!("Close"));
            // The menu closes on a click elsewhere only when its owner is in front.
            if !hub.test.no_activate {
                let _ = SetForegroundWindow(self.hwnd);
            }
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                at.x,
                at.y,
                None,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            if chosen.0 > 0 {
                self.command(chosen.0 as usize);
            }
        }
    }

    /// What the pin shows at its frame's size, with the close button over it (premultiplied
    /// RGBA; tests only). The picture is made here when the one there is for another size.
    fn compose(&self) -> Option<Pixmap> {
        let (width, height) = self.client();
        let key = picture_key(width, height, self.dpi.get());
        let cached = self.picture.borrow().as_ref().filter(|p| p.key() == key).map(|p| p.bgra.clone());
        let bgra = match cached {
            Some(bgra) => bgra,
            None => render(&self.card.image, key, self.badge())?.bgra,
        };
        let rgba = bgra.iter().flat_map(|pixel| {
            let [b, g, r, a] = pixel.to_le_bytes();
            [r, g, b, a]
        });
        let mut pixmap = Pixmap::from_vec(rgba.collect(), IntSize::from_wh(key.0, key.1)?)?;
        self.draw_close(&mut pixmap, width, (0, 0), false);
        Some(pixmap)
    }

    /// The close button as the note header draws it, on a round patch of paper so it reads
    /// over any picture. `pixmap` is the part of a window `width` wide that starts at
    /// `origin`; `bgr` swaps red and blue for a BGRA target.
    fn draw_close(&self, pixmap: &mut Pixmap, width: i32, origin: (i32, i32), bgr: bool) {
        let now = self.now();
        let shown = self.shown.get().value(now);
        if shown <= 0.0 {
            return;
        }
        let hover = self.lit.get().value(now);
        let palette = self.hub().palette.get();
        let dpi = self.dpi.get();
        let rect = close_rect(width, dpi);
        let (cx, cy) = (
            (rect.left + rect.right) as f32 / 2.0 - origin.0 as f32,
            (rect.top + rect.bottom) as f32 / 2.0 - origin.1 as f32,
        );
        let radius = (rect.right - rect.left) as f32 / 2.0;
        let color = |c: COLORREF, alpha: f64| {
            let (r, g, b) = ((c.0 & 0xff) as u8, ((c.0 >> 8) & 0xff) as u8, ((c.0 >> 16) & 0xff) as u8);
            let (r, b) = if bgr { (b, r) } else { (r, b) };
            Color::from_rgba8(r, g, b, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8)
        };
        let paint = |c: Color| {
            let mut paint = Paint::default();
            paint.set_color(c);
            paint.anti_alias = true;
            paint
        };
        if let Some(circle) = PathBuilder::from_circle(cx, cy, radius) {
            let fill = mix(palette.paper, palette.hover, hover);
            pixmap.fill_path(&circle, &paint(color(fill, shown * 0.94)), FillRule::Winding, Transform::identity(), None);
            let line = Stroke { width: 1.0, ..Stroke::default() };
            pixmap.stroke_path(&circle, &paint(color(palette.hairline, shown)), &line, Transform::identity(), None);
        }
        let ink = mix(palette.muted, palette.text, hover);
        let arm = px(3.6, dpi) as f32;
        let mut cross = PathBuilder::new();
        cross.move_to(cx - arm, cy - arm);
        cross.line_to(cx + arm, cy + arm);
        cross.move_to(cx + arm, cy - arm);
        cross.line_to(cx - arm, cy + arm);
        if let Some(cross) = cross.finish() {
            let stroke = Stroke {
                width: px(1.5, dpi) as f32,
                line_cap: tiny_skia::LineCap::Round,
                ..Stroke::default()
            };
            pixmap.stroke_path(&cross, &paint(color(ink, shown)), &stroke, Transform::identity(), None);
        }
    }

    /// The close button over the canvas, in a client area `width` × `height`: drawn on a copy
    /// of the few pixels under it, then put back.
    fn draw_close_on(&self, canvas: &mut Canvas, width: i32, height: i32) {
        if self.shown.get().value(self.now()) <= 0.0 {
            return;
        }
        let rect = close_rect(width, self.dpi.get());
        let (left, top) = ((rect.left - 2).max(0), (rect.top - 2).max(0));
        let (right, bottom) = ((rect.right + 2).min(width), (rect.bottom + 2).min(height));
        if right <= left || bottom <= top {
            return;
        }
        let Some(mut patch) = Pixmap::new((right - left) as u32, (bottom - top) as u32) else { return };
        let stride = canvas.width as usize;
        let row = (right - left) as usize;
        let start = |y: usize| (top as usize + y) * stride + left as usize;
        let pixels = canvas.pixels();
        for (y, line) in patch.data_mut().chunks_exact_mut(row * 4).enumerate() {
            for (to, from) in line.as_chunks_mut::<4>().0.iter_mut().zip(&pixels[start(y)..][..row]) {
                // GDI leaves the fourth byte at 0; the patch is opaque.
                *to = (from | 0xff00_0000).to_le_bytes();
            }
        }
        self.draw_close(&mut patch, width, (left, top), true);
        for (y, line) in patch.data().chunks_exact(row * 4).enumerate() {
            for (to, from) in pixels[start(y)..][..row].iter_mut().zip(line.as_chunks::<4>().0) {
                *to = u32::from_le_bytes(*from);
            }
        }
    }

    fn paint(&self, dc: HDC) {
        let started = Instant::now();
        self.draw(dc);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let (count, total, longest) = self.paints.get();
        self.paints.set((count + 1, total + ms, longest.max(ms)));
    }

    /// The picture, stretched while the window's size differs from it, with the close button
    /// over it. No allocation and no pixel conversion on the way, except for a new canvas.
    fn draw(&self, dc: HDC) {
        let (width, height) = self.client();
        if width <= 0 || height <= 0 {
            return;
        }
        let picture = self.picture.borrow();
        let Some(picture) = picture.as_ref() else { return };
        let mut canvas = self.canvas.borrow_mut();
        if !canvas.as_ref().is_some_and(|canvas| canvas.width >= width && canvas.height >= height) {
            // Room to grow, so a zoom or the grow-in makes few.
            *canvas = None;
            *canvas = Canvas::new(width + width / 4, height + height / 4);
        }
        let Some(canvas) = canvas.as_mut() else { return };
        // SAFETY: plain call; GDI is done with the canvas before its pixels change.
        let _ = unsafe { GdiFlush() };
        let stride = canvas.width as usize;
        let (pw, ph) = (picture.width as usize, picture.height as usize);
        if (pw, ph) == (width as usize, height as usize) {
            for (row, line) in picture.bgra.chunks_exact(pw).zip(canvas.pixels().chunks_exact_mut(stride)) {
                line[..pw].copy_from_slice(row);
            }
        } else {
            stretch(&picture.bgra, (pw, ph), canvas.pixels(), stride, (width as usize, height as usize));
        }
        self.draw_close_on(canvas, width, height);
        // SAFETY: both DCs are ours; the canvas covers the client area.
        let _ = unsafe { BitBlt(dc, 0, 0, width, height, Some(canvas.dc), 0, 0, SRCCOPY) };
    }
}

unsafe extern "system" fn pin_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the window's state while it lives (0 before and after).
    let window = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PinWindow).as_ref() };
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
        WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
        WM_NCACTIVATE => return LRESULT(1),
        WM_GETMINMAXINFO => {
            let info = lp.0 as *mut MINMAXINFO;
            if !info.is_null() {
                // SAFETY: Windows passes a valid MINMAXINFO for this call. A pin may be smaller
                // than a thick-framed window's usual minimum.
                unsafe { (*info).ptMinTrackSize = POINT { x: 1, y: 1 } };
            }
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => {
            let activate = if window.hub().test.no_activate { MA_NOACTIVATE } else { MA_ACTIVATE };
            return LRESULT(activate as isize);
        }
        WM_MOUSEMOVE => window.mouse_move(lp),
        WM_MOUSELEAVE => {
            if window.drag.get().is_none() {
                window.set_hover(false, false);
            }
        }
        WM_LBUTTONDOWN => window.button_down(lp),
        WM_LBUTTONUP => window.button_up(lp),
        WM_CAPTURECHANGED => {
            window.drag.set(None);
            window.pressing.set(false);
        }
        WM_LBUTTONDBLCLK => {
            let at = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
            if inside(window.close_rect(), at) {
                window.close();
            } else if !window.closing.get() {
                window.hub().emit(Event::Carry {
                    pin: window.id,
                    card: window.card.clone(),
                    double_click: true,
                });
            }
            return LRESULT(0);
        }
        WM_CONTEXTMENU => {
            let mut at = POINT { x: i32::from(lp.0 as i16), y: i32::from((lp.0 >> 16) as i16) };
            if lp.0 == -1 {
                // From the keyboard: the pin's top-left corner.
                let frame = window.frame();
                at = POINT { x: frame.x as i32, y: frame.y as i32 };
            }
            window.context_menu(at);
            return LRESULT(0);
        }
        WM_COMMAND => {
            window.command(wp.0 & 0xffff);
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            window.wheel(wp, lp);
            return LRESULT(0);
        }
        WM_KEYDOWN if wp.0 == usize::from(VK_ESCAPE.0) => {
            window.close();
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TICK_TIMER => {
            window.step();
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            window.zoom.set(None);
            if window.hub().test.dpi.is_none() {
                window.dpi.set((wp.0 & 0xffff) as u32);
            }
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
            window.settle();
            window.invalidate();
            return LRESULT(0);
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => window.hub().settings_changed(),
        WM_SIZE => window.invalidate(),
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
            window.close();
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

    const WORK: Rect = Rect { x: 0.0, y: 0.0, width: 1920.0, height: 1040.0 };

    #[test]
    fn a_pin_is_its_natural_size_up_to_a_share_of_the_screen() {
        // Captured at 150%, shown at 100%: two thirds of its pixels.
        assert_eq!(natural_size((1500, 900), 1.5, 1.0), (1000.0, 600.0));
        // Small enough: kept.
        assert_eq!(pin_size((400.0, 300.0), WORK, 64.0), (400.0, 300.0));
        // Too wide for 40% of 1920 (768): shrunk with its shape kept.
        assert_eq!(pin_size((1000.0, 600.0), WORK, 64.0), (693.0, 416.0));
        assert_eq!(pin_size((1920.0, 400.0), WORK, 64.0), (768.0, 160.0));
        // Tiny: grown to the minimum side.
        assert_eq!(pin_size((32.0, 16.0), WORK, 64.0), (128.0, 64.0));
        // A long strip shrunk to fit never goes under the minimum side.
        assert_eq!(pin_size((3000.0, 80.0), WORK, 64.0), (2400.0, 64.0));
    }

    #[test]
    fn a_pin_is_centred_where_the_menu_opened_and_stays_on_screen() {
        assert_eq!(pin_frame(Point::new(500.0, 400.0), (200.0, 100.0), WORK), Rect::new(400.0, 350.0, 200.0, 100.0));
        assert_eq!(pin_frame(Point::new(1910.0, 5.0), (200.0, 100.0), WORK), Rect::new(1720.0, 0.0, 200.0, 100.0));
        let left = Rect::new(-1280.0, 0.0, 1280.0, 1000.0);
        let frame = pin_frame(Point::new(-1270.0, 990.0), (400.0, 300.0), left);
        assert_eq!(frame, Rect::new(-1280.0, 700.0, 400.0, 300.0));
        // Larger than the work area: fitted.
        assert_eq!(pin_frame(Point::new(0.0, 0.0), (3000.0, 2000.0), WORK).width, 1920.0);
    }

    #[test]
    fn the_wheel_resizes_around_the_cursor_within_limits() {
        let frame = Rect::new(100.0, 100.0, 400.0, 200.0);
        let cursor = Point::new(300.0, 200.0);
        let grown = wheel_frame(frame, cursor, 1.0, 64.0, (1920.0, 1080.0));
        assert_eq!(grown, Rect::new(80.0, 90.0, 440.0, 220.0));
        // The point under the cursor stays put: the middle stays the middle.
        assert_eq!(grown.x + grown.width / 2.0, 300.0);
        let back = wheel_frame(grown, cursor, -1.0, 64.0, (1920.0, 1080.0));
        assert_eq!((back.width, back.height), (400.0, 200.0));
        // Never below the minimum side, never above the monitor.
        let small = wheel_frame(frame, cursor, -30.0, 64.0, (1920.0, 1080.0));
        assert_eq!((small.width, small.height), (128.0, 64.0));
        let large = wheel_frame(frame, cursor, 30.0, 64.0, (1920.0, 1080.0));
        assert_eq!((large.width, large.height), (1920.0, 960.0));
        // Around a corner cursor the opposite corner moves.
        let corner = wheel_frame(frame, Point::new(100.0, 100.0), 1.0, 64.0, (1920.0, 1080.0));
        assert_eq!((corner.x, corner.y), (100.0, 100.0));
    }

    #[test]
    fn small_wheel_steps_add_up_and_keep_the_shape() {
        let max = (1920.0, 1080.0);
        let frame = Rect::new(100.0, 100.0, 400.0, 200.0);
        let cursor = Point::new(300.0, 200.0);
        // Whole notches move at once, as before.
        assert_eq!(wheel_target(frame, cursor, 1.0, 0.0, 64.0, max), (wheel_frame(frame, cursor, 1.0, 64.0, max), 0.0));
        // A touchpad's small steps: two notches in 240 steps.
        let (mut at, mut carry) = (frame, 0.0);
        let mut moves = 0;
        for _ in 0..240 {
            let (next, left) = wheel_target(at, cursor, 1.0 / 120.0, carry, 64.0, max);
            moves += usize::from(next != at);
            (at, carry) = (next, left);
        }
        assert_eq!(moves, 8);
        let whole = wheel_frame(frame, cursor, 2.0, 64.0, max);
        assert!((at.width - whole.width).abs() <= 2.0 && (at.height - whole.height).abs() <= 1.0, "{at:?} vs {whole:?}");
        assert!((at.x - whole.x).abs() <= 2.0 && (at.y - whole.y).abs() <= 1.0, "{at:?} vs {whole:?}");
        assert!((at.width / at.height - 2.0).abs() < 0.01, "{at:?}");
        // Turning back drops what was carried the other way.
        let (_, carry) = wheel_target(frame, cursor, 0.1, 0.0, 64.0, max);
        assert_eq!(carry, 0.1);
        assert_eq!(wheel_target(frame, cursor, -0.1, carry, 64.0, max), (frame, -0.1));
    }

    #[test]
    fn the_zoom_spring_settles_quickly_without_overshoot() {
        assert_eq!(zoom_spring(10.0, 5.0, 90.0, 0.0), (10.0, 5.0));
        // From rest it only ever closes in on its target.
        let mut last = 0.0;
        for step in 1..=60 {
            let (at, speed) = zoom_spring(0.0, 0.0, 100.0, f64::from(step) * 0.005);
            assert!(at >= last && at <= 100.0 && speed >= 0.0);
            last = at;
        }
        // One notch on a 725 px pin: done in about 140 ms.
        let from = Rect::new(100.0, 100.0, 725.0, 408.0);
        let to = wheel_frame(from, Point::new(462.0, 304.0), 1.0, 64.0, (1920.0, 1080.0));
        let zoom = Zoom::new(from, to, 2.0);
        assert!(!zoom.settled(2.1));
        assert!(zoom.settled(2.16));
        let (mid, _) = zoom.at(2.05);
        assert!(mid.width > from.width + 30.0 && mid.width < to.width, "{mid:?}");
        // A long way: under a quarter of a second.
        let far = Zoom::new(from, Rect::new(0.0, 0.0, 1800.0, 1013.0), 0.0);
        assert!(!far.settled(0.15) && far.settled(0.22));
    }

    #[test]
    fn a_zoom_retargets_from_where_it_is_and_keeps_its_speed() {
        let max = (1920.0, 1080.0);
        let cursor = Point::new(400.0, 300.0);
        let start = Rect::new(100.0, 100.0, 600.0, 400.0);
        // Quick notches, each from the last target: they end where the instant steps did.
        let mut zoom = Zoom::new(start, wheel_frame(start, cursor, 1.0, 64.0, max), 0.0);
        let mut instant = zoom.to;
        for notch in 1..5 {
            let now = f64::from(notch) * 0.03;
            let (before, speed) = zoom.at(now);
            let next = zoom.retarget(wheel_frame(zoom.to, cursor, 1.0, 64.0, max), now);
            let (after, kept) = next.at(now);
            let same = |a: [f64; 4], b: [f64; 4]| a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-9);
            assert!(same(parts(before), parts(after)) && same(speed, kept), "{before:?} {after:?}");
            assert!(speed[2] > 0.0, "still growing at the turn");
            zoom = next;
            instant = wheel_frame(instant, cursor, 1.0, 64.0, max);
        }
        assert_eq!(zoom.to, instant);
        assert!(zoom.settled(0.12 + 0.3));
        assert_eq!(win_rect(zoom.at(0.12 + 0.3).0), win_rect(instant));
    }

    #[test]
    fn stretch_mixes_neighbours_and_stays_in_its_corner() {
        // The same size: a copy.
        let src = [0x11_22_33_44, 0x55_66_77_88, 0x99_aa_bb_cc, 0xdd_ee_ff_00];
        let mut dst = [7u32; 3 * 2];
        stretch(&src, (2, 2), &mut dst, 3, (2, 2));
        assert_eq!(dst, [src[0], src[1], 7, src[2], src[3], 7]);
        // One colour stays that colour at any size.
        let grey = [0xff_80_80_80u32; 6];
        let mut dst = vec![0u32; 7 * 5];
        stretch(&grey, (3, 2), &mut dst, 7, (7, 5));
        assert!(dst.iter().all(|&p| p == grey[0]));
        // Doubled, black to white: the ends keep their colour, the middle is in between,
        // every channel alike.
        let mut dst = [0u32; 4];
        stretch(&[0xff_00_00_00, 0xff_ff_ff_ff], (2, 1), &mut dst, 4, (4, 1));
        assert_eq!((dst[0], dst[3]), (0xff_00_00_00, 0xff_ff_ff_ff));
        let [b, g, r, a] = dst[1].to_le_bytes();
        assert!(b == g && g == r && (50..80).contains(&r) && a == 255, "{:x}", dst[1]);
        assert!(dst[1] < dst[2]);
        // Halved: each output pixel mixes the two under it.
        let mut dst = [0u32; 1];
        stretch(&[0xff_00_00_00, 0xff_ff_ff_ff], (2, 1), &mut dst, 1, (1, 1));
        assert_eq!(dst[0].to_le_bytes()[1], 0x7f);
        // Nothing to do with an empty side, or too few pixels.
        stretch(&[], (0, 0), &mut dst, 1, (1, 1));
        stretch(&[1, 2], (2, 2), &mut dst, 1, (1, 1));
    }

    #[test]
    fn ctrl_wheel_steps_the_opacity_between_its_limits() {
        assert_eq!(wheel_opacity(1.0, -1.0), 0.9);
        assert_eq!(wheel_opacity(0.9, 1.0), 1.0);
        assert_eq!(wheel_opacity(1.0, 1.0), 1.0);
        assert_eq!(wheel_opacity(0.4, -3.0), MIN_OPACITY);
        assert_eq!(wheel_opacity(0.3, 0.5), 0.35);
    }

    #[test]
    fn twenty_pins_at_most() {
        assert!(can_pin(0));
        assert!(can_pin(MAX_PINS - 1));
        assert!(!can_pin(MAX_PINS));
        assert!(FULL_HINT.starts_with(&MAX_PINS.to_string()));
    }

    #[test]
    fn the_close_button_sits_in_the_top_right_corner() {
        assert_eq!(close_rect(400, 96), RECT { left: 372, top: 8, right: 392, bottom: 28 });
        assert_eq!(close_rect(500, 120), RECT { left: 465, top: 10, right: 490, bottom: 35 });
        assert!(inside(close_rect(400, 96), (380, 10)));
        assert!(!inside(close_rect(400, 96), (392, 10)));
    }
}

