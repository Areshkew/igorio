//! The pen and highlighter while recording (`InkOverlay.swift`), and the thread that drives
//! everything a recording shows on screen ([`RecordingUi`]: the ink window, the outline and
//! the pill of `frame`, the keys and wheel of `input`).
//!
//! # The ink window
//!
//! One layered, topmost window over the recorded monitor, in its physical pixels. It is *in*
//! the video (no `WDA_EXCLUDEFROMCAPTURE`): the ink is for the viewer. It never activates and
//! never takes the keyboard focus (`WS_EX_NOACTIVATE`, `MA_NOACTIVATE`), like the Swift panel
//! that can never become key: the recorded app keeps looking active in the video.
//!
//! - No tool: `WS_EX_TRANSPARENT` and `HTTRANSPARENT`, so every click goes to the app below.
//!   With no ink on screen the window is hidden, and its big buffers are only made the first
//!   time a tool is picked up.
//! - A tool in hand: the window takes the mouse. A layered window lets the mouse through
//!   wherever its alpha is 0, so the whole window is presented with an alpha floor of 1/255
//!   (`LayeredSurface::present`'s `floor`; a 0.4% darkening no one can see, also in the
//!   video). The cursor is the tool's tip (`ink_draw::tool_cursor`, set on `WM_SETCURSOR`).
//!   A press starts a stroke, moves extend it, the release ends it. The press captures the
//!   mouse, so a stroke dragged off the window still ends; a lost capture
//!   (`WM_CAPTURECHANGED`) ends it too (Swift's timer that looks for a lost mouse-up).
//! - Put down: click-through at once, and the ink fades away over 0.25 s, eased out, then is
//!   cleared at 0.3 s (`fadeAway`). The fade changes only the window's constant alpha, so
//!   it costs no drawing. Reduced motion clears at once.
//!
//! # Latency
//!
//! The points come from the ink window's own mouse messages, on the recording UI thread, which
//! the app's UI loop never blocks. `WM_MOUSEMOVE` is coalesced by Windows (one per read of the
//! queue), so on every move the points the system recorded since the last one are read back
//! with `GetMouseMovePointsEx` (up to 64, display resolution): a fast stroke keeps every point
//! the mouse reported (125 to 1000 a second, by device) even when a frame is late. All the
//! messages waiting are read before one frame is drawn. The app's mouse hook is not used for
//! the ink: its events reach the app's UI thread first, which also draws the overlay.
//!
//! A frame renders only what changed (`InkRenderer`'s damage rectangle) and presents only that
//! rectangle (`UpdateLayeredWindowIndirect` with `prcDirty`), so a 4K monitor costs what the
//! tip of the stroke covers. `recording_ui_selftest` measures both.
//!
//! # The driver
//!
//! [`RecordingUi`] owns one thread with a message loop for all the windows. The app's UI thread
//! talks to it with calls that never block and reads [`UiEvent`]s after its wake handle is set.
//! `examples/recording_ui_demo.rs` shows the whole loop on the app side: the mouse hook, a
//! `RecordingViewport` stepped with the events, and `set_area` after every step.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use igorio_core::geom::{Point, Rect};
use igorio_core::ink::{Ink, InkKey, InkTool};
use tiny_skia::Pixmap;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WAIT_FAILED,
    WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::MK_LBUTTON;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GMMP_USE_DISPLAY_POINTS, GetAsyncKeyState, GetCapture, GetMouseMovePointsEx, MOUSEMOVEPOINT,
    ReleaseCapture, SetCapture, VK_LBUTTON, VK_RBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyCursor, DestroyWindow,
    DispatchMessageW, GWL_EXSTYLE, GetMessageTime, GetWindowLongPtrW, HCURSOR, HICON, HTCLIENT,
    HTTRANSPARENT, HWND_TOPMOST, ICONINFO, IDC_ARROW, LoadCursorW, MA_NOACTIVATE, MSG,
    MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
    QS_ALLINPUT, RegisterClassW, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetCursor, SetWindowLongPtrW, SetWindowPos,
    TranslateMessage, WM_CAPTURECHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_NCHITTEST, WM_SETCURSOR, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{Error, PCWSTR, w};

use super::frame::{self, FrameWindows, Labels, PillAction, PillSnapshot, ease_out};
use super::input::{
    Action, DRAW_KEYS, Input, KeyHook, KeyInput, Machine, Mods, ZoomModifier, send_mask,
};
use crate::hook::{self, MouseEvent, WHEEL_GATE};
use crate::ink_draw::{InkRenderer, tool_cursor};
use crate::overlay::{LayeredSurface, MonitorBounds, cursor_pos};

const CLASS: PCWSTR = w!("IgorioInk");

/// The ink's fade when the pen is put down (`fadeAway`: 0.25 s ease-out, cleared at 0.3 s).
pub const INK_FADE: f64 = 0.25;
pub const INK_CLEAR: f64 = 0.3;
/// Animation frame interval while something fades.
const FRAME: Duration = Duration::from_millis(8);
/// How long `start` waits for the thread to be ready.
const START_WAIT: Duration = Duration::from_secs(3);
/// Most commands kept between two passes of the thread.
const COMMAND_CAPACITY: usize = 1024;

/// The ink's opacity `t` seconds after it was put down; `None` once it is cleared.
pub fn ink_fade_opacity(t: f64) -> Option<f64> {
    if t >= INK_CLEAR {
        None
    } else if t >= INK_FADE {
        Some(0.0)
    } else {
        Some(1.0 - ease_out(t.max(0.0) / INK_FADE))
    }
}

// ---------------------------------------------------------------------------------------
// The ink window's procedure: it only queues what it sees
// ---------------------------------------------------------------------------------------

/// A mouse event on the ink window, in client (physical, monitor-relative) pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
enum InkMouse {
    Down(i32, i32),
    Move(i32, i32),
    Up(i32, i32),
    /// The capture went elsewhere: end the stroke.
    Lost,
}

thread_local! {
    static INK_QUEUE: RefCell<Vec<InkMouse>> = const { RefCell::new(Vec::new()) };
    /// The window takes the mouse (a tool is in hand).
    static INK_TAKES: Cell<bool> = const { Cell::new(false) };
    /// The tool's cursor (0: the arrow).
    static INK_CURSOR: Cell<isize> = const { Cell::new(0) };
    /// The window's top-left in screen pixels, for `GetMouseMovePointsEx`.
    static INK_ORIGIN: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// The last point handed on (screen px) and its message time, to read the history since.
    static INK_LAST: Cell<Option<(i32, i32, u32)>> = const { Cell::new(None) };
    /// Points that came from the move history rather than a message.
    static INK_HISTORY: Cell<u64> = const { Cell::new(0) };
}

fn push(event: InkMouse) {
    INK_QUEUE.with(|queue| {
        if let Ok(mut queue) = queue.try_borrow_mut() {
            queue.push(event);
        }
    });
}

fn client_point(l: LPARAM) -> (i32, i32) {
    let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

/// Queues the points the system recorded between the last point handed on and this one.
fn queue_history(x: i32, y: i32, time: u32) {
    let (ox, oy) = INK_ORIGIN.get();
    let (sx, sy) = (x + ox, y + oy);
    let Some(last) = INK_LAST.get() else {
        return;
    };
    let current = MOUSEMOVEPOINT {
        x: sx & 0xFFFF,
        y: sy & 0xFFFF,
        time,
        dwExtraInfo: 0,
    };
    let mut history = [MOUSEMOVEPOINT::default(); 64];
    // SAFETY: `current` and `history` are locals; the size is that of the struct.
    let count = unsafe {
        GetMouseMovePointsEx(
            size_of::<MOUSEMOVEPOINT>() as u32,
            &current,
            &mut history,
            GMMP_USE_DISPLAY_POINTS,
        )
    };
    if count <= 1 {
        return;
    }
    // Newest first; `history[0]` is the current point. Take the older ones back to the last
    // point handed on.
    let unwrap = |v: i32| if v > 32767 { v - 65536 } else { v };
    let mut older = Vec::new();
    for point in &history[1..count as usize] {
        let (px, py) = (unwrap(point.x), unwrap(point.y));
        if point.time.wrapping_sub(last.2) > u32::MAX / 2 {
            break; // older than the last point
        }
        if (px, py) == (last.0, last.1) {
            break;
        }
        older.push((px - ox, py - oy));
    }
    INK_HISTORY.set(INK_HISTORY.get() + older.len() as u64);
    for (px, py) in older.into_iter().rev() {
        push(InkMouse::Move(px, py));
    }
}

/// Touches only thread-locals, so it is safe to re-enter.
unsafe extern "system" fn ink_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            if INK_TAKES.get() {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            }
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR if INK_CURSOR.get() != 0 => {
            // SAFETY: a cursor this thread made and keeps until the window is gone.
            unsafe { SetCursor(Some(HCURSOR(INK_CURSOR.get() as *mut c_void))) };
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = client_point(l);
            // Capture only a real press (a button is physically down): a stroke dragged off
            // the window still ends here. A message posted by the self-test has no button.
            // SAFETY: plain value arguments; our own window, on this thread.
            unsafe {
                if GetAsyncKeyState(i32::from(VK_LBUTTON.0)) < 0
                    || GetAsyncKeyState(i32::from(VK_RBUTTON.0)) < 0
                {
                    SetCapture(hwnd);
                }
            }
            let (ox, oy) = INK_ORIGIN.get();
            // SAFETY: no arguments.
            let time = unsafe { GetMessageTime() } as u32;
            INK_LAST.set(Some((x + ox, y + oy, time)));
            push(InkMouse::Down(x, y));
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if w.0 & MK_LBUTTON.0 as usize != 0 {
                let (x, y) = client_point(l);
                // SAFETY: no arguments.
                let time = unsafe { GetMessageTime() } as u32;
                queue_history(x, y, time);
                let (ox, oy) = INK_ORIGIN.get();
                INK_LAST.set(Some((x + ox, y + oy, time)));
                push(InkMouse::Move(x, y));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = client_point(l);
            push(InkMouse::Up(x, y));
            INK_LAST.set(None);
            // SAFETY: releases only our own capture.
            unsafe {
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            if l.0 != hwnd.0 as isize {
                push(InkMouse::Lost);
            }
            LRESULT(0)
        }
        // SAFETY: the arguments come unchanged from the system.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

fn register_class(instance: HINSTANCE) -> windows::core::Result<()> {
    // SAFETY: a system cursor, shared, never freed.
    let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
    let class = WNDCLASSW {
        lpfnWndProc: Some(ink_proc),
        hInstance: instance,
        hCursor: arrow,
        lpszClassName: CLASS,
        ..Default::default()
    };
    // SAFETY: `class` points only to static data and a `'static` function.
    if unsafe { RegisterClassW(&class) } == 0 {
        let error = Error::from_thread();
        if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(error);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The tool cursors
// ---------------------------------------------------------------------------------------

/// A cursor made from a premultiplied pixmap, destroyed on drop.
struct Cursor(HCURSOR);

impl Cursor {
    fn new(pixmap: &Pixmap, hot: (u32, u32)) -> Option<Cursor> {
        let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = ptr::null_mut();
        // SAFETY: `info` and `bits` are locals; the bitmaps are deleted below once the
        // cursor holds its own copies.
        unsafe {
            let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            // Icons take straight (not premultiplied) BGRA.
            let dst = std::slice::from_raw_parts_mut(bits.cast::<u8>(), (w * h * 4) as usize);
            for (s, d) in pixmap
                .data()
                .as_chunks::<4>()
                .0
                .iter()
                .zip(dst.as_chunks_mut::<4>().0)
            {
                let a = u32::from(s[3]);
                let straight = |c: u8| {
                    (u32::from(c) * 255 + a / 2)
                        .checked_div(a)
                        .map_or(0, |v| v.min(255) as u8)
                };
                *d = [straight(s[2]), straight(s[1]), straight(s[0]), s[3]];
            }
            // An AND mask of zeros (rows padded to 16 bits): the alpha channel does the rest.
            let stride = ((w + 15) / 16 * 2) as usize;
            let zeros = vec![0u8; stride * h as usize];
            let mask = CreateBitmap(w, h, 1, 1, Some(zeros.as_ptr().cast()));
            let icon = ICONINFO {
                fIcon: false.into(),
                xHotspot: hot.0,
                yHotspot: hot.1,
                hbmMask: mask,
                hbmColor: color,
            };
            let made = CreateIconIndirect(&icon);
            let _ = DeleteObject(color.into());
            let _ = DeleteObject(mask.into());
            made.ok().map(|icon: HICON| Cursor(HCURSOR(icon.0)))
        }
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        // SAFETY: made by `CreateIconIndirect`, destroyed once, after the window that used it
        // stopped showing it (the window is destroyed first, see `InkWindow`'s field order).
        let _ = unsafe { DestroyCursor(self.0) };
    }
}

// ---------------------------------------------------------------------------------------
// The ink window
// ---------------------------------------------------------------------------------------

/// Timing of the ink's frames, for the self-test and the demo.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InkStats {
    pub frames: u64,
    /// Seconds in `InkRenderer::render`, summed, and the longest.
    pub render: f64,
    pub render_worst: f64,
    /// Seconds copying and presenting the damage rectangle, summed, and the longest.
    pub present: f64,
    pub present_worst: f64,
    /// The longest frame (render and present together).
    pub frame_worst: f64,
    /// Points handed to the ink, and how many of them came from the move history.
    pub points: u64,
    pub history_points: u64,
    /// Presents of the whole window (pick-up, floor changes, rebuilds), the time of the last.
    pub full_presents: u64,
    pub last_full_present: f64,
    /// Damaged pixels, summed.
    pub damage_pixels: u64,
}

/// What the ink shows, as of the last pass of the thread.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InkState {
    pub tool: Option<InkTool>,
    pub strokes: usize,
    /// The window is on screen.
    pub shown: bool,
    /// A fade is running.
    pub fading: bool,
    /// The pixmap may hold ink (false: fully transparent).
    pub content: bool,
    /// The window's constant alpha.
    pub alpha: u8,
}

struct InkWindow {
    hwnd: HWND,
    surface: LayeredSurface,
    /// Made on the first pick-up: the monitor-sized pixmap.
    pixmap: Option<Pixmap>,
    renderer: InkRenderer,
    ink: Ink,
    monitor: MonitorBounds,
    origin: (i32, i32),
    take_mouse: bool,
    cursors: Option<[Cursor; 2]>,
    fade: Option<f64>,
    alpha: u8,
    shown: bool,
    dirty: bool,
    full: bool,
    content: bool,
    stats: InkStats,
}

impl InkWindow {
    fn new(monitor: MonitorBounds, take_mouse: bool) -> windows::core::Result<InkWindow> {
        // SAFETY: a null module name returns the exe's own module.
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        register_class(instance)?;
        let origin = (
            monitor.bounds.x.round() as i32,
            monitor.bounds.y.round() as i32,
        );
        // SAFETY: the class is registered above with `ink_proc`; strings are static; no
        // creation parameter is passed.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW,
                CLASS,
                w!("Igorio ink"),
                WS_POPUP,
                origin.0,
                origin.1,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )?
        };
        INK_ORIGIN.set(origin);
        let surface = match LayeredSurface::new() {
            Ok(surface) => surface,
            Err(error) => {
                // SAFETY: our own window, just made on this thread.
                let _ = unsafe { DestroyWindow(hwnd) };
                return Err(error);
            }
        };
        Ok(InkWindow {
            hwnd,
            surface,
            pixmap: None,
            renderer: InkRenderer::new(),
            ink: Ink::new(),
            monitor,
            origin,
            take_mouse,
            cursors: None,
            fade: None,
            alpha: 255,
            shown: false,
            dirty: false,
            full: false,
            content: false,
            stats: InkStats::default(),
        })
    }

    fn size(&self) -> (u32, u32) {
        (
            self.monitor.bounds.width.round().max(1.0) as u32,
            self.monitor.bounds.height.round().max(1.0) as u32,
        )
    }

    /// The tool in hand changed (the ink already changed).
    fn set_tool(&mut self, tool: Option<InkTool>) {
        let takes = tool.is_some() && self.take_mouse;
        INK_TAKES.set(takes);
        // SAFETY: our own window; only WS_EX_TRANSPARENT changes.
        unsafe {
            let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            let transparent = WS_EX_TRANSPARENT.0 as isize;
            let next = if takes {
                style & !transparent
            } else {
                style | transparent
            };
            if next != style {
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, next);
            }
        }
        if tool.is_some() {
            if self.pixmap.is_none() {
                let (w, h) = self.size();
                self.pixmap = Pixmap::new(w, h);
            }
            if self.cursors.is_none() {
                let make = |tool| {
                    let (pixmap, hot) = tool_cursor(tool, self.monitor.scale);
                    Cursor::new(&pixmap, hot)
                };
                if let (Some(pen), Some(highlighter)) =
                    (make(InkTool::Pen), make(InkTool::Highlighter))
                {
                    self.cursors = Some([pen, highlighter]);
                }
            }
            // Picked up again during a fade: the faded ink is gone already.
            if self.fade.take().is_some() {
                self.alpha = 255;
            }
        }
        let cursor = match (tool, &self.cursors) {
            (Some(InkTool::Pen), Some(c)) => c[0].0.0 as isize,
            (Some(InkTool::Highlighter), Some(c)) => c[1].0.0 as isize,
            _ => 0,
        };
        INK_CURSOR.set(cursor);
        if cursor != 0 {
            // Shows at once if the cursor is over the window; otherwise on the next move.
            // SAFETY: a cursor this thread made.
            unsafe { SetCursor(Some(HCURSOR(cursor as *mut c_void))) };
        }
        self.dirty = true;
        self.full = true;
    }

    /// Hands queued mouse events to the ink.
    fn on_mouse(&mut self, events: &[InkMouse]) {
        if self.ink.tool().is_none() {
            return;
        }
        let scale = self.monitor.scale;
        let logical = |x: i32, y: i32| Point::new(f64::from(x) / scale, f64::from(y) / scale);
        for event in events {
            match *event {
                InkMouse::Down(x, y) => {
                    self.ink.begin_stroke(logical(x, y));
                    self.stats.points += 1;
                }
                InkMouse::Move(x, y) => {
                    if self.ink.current().is_none() {
                        // The press went elsewhere (or was lost): start the stroke here.
                        self.ink.begin_stroke(logical(x, y));
                    }
                    self.ink.extend_stroke(logical(x, y));
                    self.stats.points += 1;
                }
                InkMouse::Up(..) | InkMouse::Lost => self.ink.end_stroke(),
            }
            self.dirty = true;
        }
    }

    fn fade_away(&mut self, now: f64) {
        if !self.content || !frame::animation_effects() {
            self.fade = None;
            self.alpha = 255;
            self.dirty = true;
            return;
        }
        self.fade = Some(now);
    }

    /// Advances the fade. True while it runs.
    fn frame(&mut self, now: f64) -> bool {
        let Some(start) = self.fade else {
            return false;
        };
        match ink_fade_opacity(now - start) {
            Some(opacity) => {
                let alpha = frame::alpha_byte(opacity);
                if alpha != self.alpha && self.shown {
                    self.alpha = alpha;
                    let _ = self.surface.set_alpha(self.hwnd, self.origin, alpha);
                }
                true
            }
            None => {
                self.fade = None;
                self.alpha = 255;
                self.dirty = true;
                false
            }
        }
    }

    /// Renders and presents what changed.
    fn draw(&mut self) {
        if !self.dirty || self.fade.is_some() {
            return;
        }
        self.dirty = false;
        let Some(pixmap) = self.pixmap.as_mut() else {
            return;
        };
        let started = Instant::now();
        self.renderer
            .render(&self.ink, self.monitor.scale, 1.0, pixmap);
        let rendered = Instant::now();
        self.content = !self.ink.strokes().is_empty() || self.ink.current().is_some();
        let floor = self.ink.tool().is_some() && self.take_mouse;
        let damage = if self.full {
            Some((0, 0, pixmap.width(), pixmap.height()))
        } else {
            self.renderer.damage()
        };
        let Some(damage) = damage else {
            return;
        };
        let whole = damage.2 == pixmap.width() && damage.3 == pixmap.height();
        let _ = self.surface.present(
            self.hwnd,
            pixmap,
            self.origin,
            Some(damage),
            floor,
            self.alpha,
        );
        let presented = Instant::now();
        self.full = false;
        let render = (rendered - started).as_secs_f64();
        let present = (presented - rendered).as_secs_f64();
        let stats = &mut self.stats;
        stats.frames += 1;
        stats.render += render;
        stats.present += present;
        stats.render_worst = stats.render_worst.max(render);
        stats.present_worst = stats.present_worst.max(present);
        stats.frame_worst = stats.frame_worst.max(render + present);
        stats.damage_pixels += u64::from(damage.2) * u64::from(damage.3);
        if whole {
            stats.full_presents += 1;
            stats.last_full_present = present;
        }
        let visible = self.ink.tool().is_some() || self.content;
        if visible && !self.shown {
            self.place(true);
        } else if !visible && self.shown {
            self.place(false);
        }
    }

    /// For the self-test: presents the whole pixmap once and returns the seconds it took.
    fn full_present(&mut self) -> Option<f64> {
        let pixmap = self.pixmap.as_ref()?;
        let floor = self.ink.tool().is_some() && self.take_mouse;
        let started = Instant::now();
        let _ = self.surface.present(
            self.hwnd,
            pixmap,
            self.origin,
            Some((0, 0, pixmap.width(), pixmap.height())),
            floor,
            self.alpha,
        );
        Some(started.elapsed().as_secs_f64())
    }

    fn place(&mut self, visible: bool) {
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        // SAFETY: our own window; `SWP_NOACTIVATE` keeps it from taking focus.
        unsafe {
            let _ = if visible {
                SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    flags | SWP_SHOWWINDOW,
                )
            } else {
                SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    flags | SWP_NOZORDER | SWP_HIDEWINDOW,
                )
            };
        }
        self.shown = visible;
    }

    fn state(&self) -> InkState {
        InkState {
            tool: self.ink.tool(),
            strokes: self.ink.strokes().len(),
            shown: self.shown,
            fading: self.fade.is_some(),
            content: self.content,
            alpha: self.alpha,
        }
    }
}

impl Drop for InkWindow {
    fn drop(&mut self) {
        INK_TAKES.set(false);
        INK_CURSOR.set(0);
        // SAFETY: created on this thread (the ink window never leaves it), destroyed once.
        let _ = unsafe { DestroyWindow(self.hwnd) };
        // `cursors` drop after this, when no window shows them any more.
    }
}

// ---------------------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------------------

/// How the recording UI behaves. The defaults are for the app.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingUiConfig {
    pub modifier: ZoomModifier,
    /// The shortcut labels for the pill.
    pub labels: Labels,
    /// Send the mask key in Alt mode (see `input`). Off in tests: they must not inject.
    pub inject_mask: bool,
    /// With a tool in hand, the ink window takes the mouse. Off only for the self-test's
    /// timing run, so a person's clicks keep working while it draws.
    pub take_mouse: bool,
    /// Install the keyboard hook (drawing keys, the modifier). Off only for that timing run.
    pub key_hook: bool,
    /// Let screenshots see the outline and the pill (`FAIRY_CAPTURABLE=1`, for demos).
    pub capturable: bool,
    /// Emit [`UiEvent::Log`] lines (`FAIRY_DEBUG=1`).
    pub debug: bool,
    /// Show "Hold Alt and scroll to zoom" under the strip for a few seconds after it appears.
    /// The app decides (the first recordings only).
    pub show_start_hint: bool,
}

impl Default for RecordingUiConfig {
    fn default() -> Self {
        RecordingUiConfig {
            modifier: ZoomModifier::Alt,
            labels: Labels::default(),
            inject_mask: true,
            take_mouse: true,
            key_hook: true,
            capturable: false,
            debug: false,
            show_start_hint: true,
        }
    }
}

/// What the recording UI tells the app.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// `RecordingViewport::zoom_by(amount)`.
    Zoom(f64),
    /// The area follows the cursor (`on`) or stays where it is. `cursor` is the cursor then
    /// (physical px); while following, step the viewport with the current cursor.
    Follow { on: bool, cursor: (i32, i32) },
    /// `RecordingViewport::reset()`: back to the whole screen.
    ResetZoom,
    /// The tool in hand changed (`setCaptureUIVisible`: the fairy steps aside while drawing).
    ToolChanged(Option<InkTool>),
    /// The stop button on the strip was clicked: stop the recording (as the toggle would,
    /// but never start one).
    StopRequested,
    /// After [`RecordingUi::stop`]: the fades ended and every window is hidden.
    Hidden,
    /// A debug line (only with `RecordingUiConfig::debug`).
    Log(String),
}

enum Command {
    Elapsed(f64),
    Tool(InkTool),
    Key(InkKey),
    Stop,
    Shutdown,
    Capturable(bool),
    FullPresent,
    ResetStats,
}

/// An auto-reset event handle, closed on drop.
struct Event(HANDLE);

// SAFETY: a kernel event handle may be used and closed from any thread.
unsafe impl Send for Event {}
// SAFETY: `SetEvent` and waits on the same handle from several threads are allowed.
unsafe impl Sync for Event {}

impl Event {
    fn new() -> Result<Event, String> {
        // SAFETY: no pointers; auto-reset, not signalled, unnamed. Owned by `Event`.
        unsafe { CreateEventW(None, false, false, None) }
            .map(Event)
            .map_err(|e| format!("event: {e}"))
    }

    fn set(&self) {
        // SAFETY: the handle lives as long as `self`.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: created by `CreateEventW`, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The window handles, for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiWindows {
    pub ink: isize,
    /// Four outline strips, then the pill.
    pub frame: Vec<isize>,
}

struct Shared {
    commands: Mutex<VecDeque<Command>>,
    /// The latest area, not yet taken by the thread.
    area: Mutex<Option<Rect>>,
    wake: Event,
    out: Mutex<VecDeque<UiEvent>>,
    out_wake: Event,
    windows: Mutex<UiWindows>,
    stats: Mutex<InkStats>,
    state: Mutex<InkState>,
    full_present: Mutex<Option<f64>>,
    pill: Mutex<PillSnapshot>,
    passes: AtomicU64,
    /// The latest cursor from `feed`: x in the high 32 bits, y in the low; and its presence.
    cursor: AtomicI64,
    has_cursor: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn send(&self, command: Command) {
        {
            let mut commands = lock(&self.commands);
            if commands.len() >= COMMAND_CAPACITY {
                return;
            }
            commands.push_back(command);
        }
        self.wake.set();
    }

    fn emit(&self, event: UiEvent) {
        lock(&self.out).push_back(event);
        self.out_wake.set();
    }
}

/// Releases both input gates however the thread ends, panics included.
struct ReleaseGates;

impl Drop for ReleaseGates {
    fn drop(&mut self) {
        DRAW_KEYS.store(false, SeqCst);
        WHEEL_GATE.disarm();
    }
}

/// Everything a recording shows on screen and the input that drives it. Owned by the app's
/// UI thread; every call returns at once. One at a time per process (the input gates are
/// process-wide).
pub struct RecordingUi {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    stopped: bool,
}

impl RecordingUi {
    /// Starts the thread: makes the windows on `monitor` (physical px), shows the outline
    /// and the pill, installs the keyboard hook, arms the wheel gate. The recording counts
    /// as running from now: start this when the recorder starts.
    pub fn start(monitor: MonitorBounds, config: RecordingUiConfig) -> Result<RecordingUi, String> {
        hook::now();
        let shared = Arc::new(Shared {
            commands: Mutex::new(VecDeque::new()),
            area: Mutex::new(Some(monitor.bounds)),
            wake: Event::new()?,
            out: Mutex::new(VecDeque::new()),
            out_wake: Event::new()?,
            windows: Mutex::new(UiWindows::default()),
            stats: Mutex::new(InkStats::default()),
            state: Mutex::new(InkState::default()),
            full_present: Mutex::new(None),
            pill: Mutex::new(PillSnapshot::default()),
            passes: AtomicU64::new(0),
            cursor: AtomicI64::new(0),
            has_cursor: AtomicU64::new(0),
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("recording-ui".into())
            .spawn(move || run(thread_shared, monitor, config, ready_tx))
            .map_err(|e| format!("recording UI thread: {e}"))?;
        match ready_rx.recv_timeout(START_WAIT) {
            Ok(Ok(())) => Ok(RecordingUi {
                shared,
                thread: Some(thread),
                stopped: false,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("recording UI thread did not start".into()),
        }
    }

    /// The area being recorded (physical px), from the viewport. Call it whenever the
    /// viewport's area changes; the outline follows. Only the latest one is kept.
    pub fn set_area(&self, area: Rect) {
        *lock(&self.shared.area) = Some(area);
        self.shared.wake.set();
    }

    /// The recording's elapsed time in seconds, as the recorder counts it. The pill counts on
    /// from it by itself; call this at start and whenever the recorder's clock is the better
    /// one (after a pause, say).
    pub fn set_elapsed(&self, seconds: f64) {
        self.shared.send(Command::Elapsed(seconds));
    }

    /// The mouse events the app's hook delivered. Only moves are used: the latest is the
    /// cursor point of the next [`UiEvent::Follow`]. (Wheel events taken for the zoom never
    /// reach the hook's queue; the ink takes its points from its own window.)
    pub fn feed(&self, events: &[MouseEvent]) {
        if let Some(MouseEvent::Move { x, y, .. }) = events
            .iter()
            .rev()
            .find(|event| matches!(event, MouseEvent::Move { .. }))
        {
            let packed = (i64::from(*x) << 32) | i64::from(*y as u32);
            self.shared.cursor.store(packed, Relaxed);
            self.shared.has_cursor.store(1, Relaxed);
        }
    }

    /// The Pen or Highlighter shortcut (from `shortcuts::Hotkeys`): picks the tool up,
    /// switches to it, or puts it down, as `Ink::press` says.
    pub fn tool_shortcut(&self, tool: InkTool) {
        self.shared.send(Command::Tool(tool));
    }

    /// A key for the ink from elsewhere than the keyboard (a tray menu item, the self-test).
    pub fn ink_key(&self, key: InkKey) {
        self.shared.send(Command::Key(key));
    }

    /// Appends the events since the last call, oldest first.
    pub fn drain(&self, out: &mut Vec<UiEvent>) {
        out.extend(lock(&self.shared.out).drain(..));
    }

    /// An auto-reset event set when events are waiting.
    pub fn wake_handle(&self) -> HANDLE {
        self.shared.out_wake.0
    }

    /// The recording stopped: the input gates are released at once (on this thread), the
    /// keyboard hook is removed, a pen in hand is put down and its ink fades, the outline and
    /// the pill fade out. [`UiEvent::Hidden`] follows when all is hidden. Then call
    /// [`shutdown`](Self::shutdown).
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        DRAW_KEYS.store(false, SeqCst);
        WHEEL_GATE.disarm();
        self.shared.send(Command::Stop);
    }

    /// Stops (if not yet), ends the thread and destroys the windows, cutting any fade short.
    /// True when the thread ended within `within`; otherwise it is left to end by itself
    /// (the gates are released either way).
    pub fn shutdown(mut self, within: Duration) -> bool {
        self.end(within)
    }

    fn end(&mut self, within: Duration) -> bool {
        self.stop();
        let Some(thread) = self.thread.take() else {
            return true;
        };
        self.shared.send(Command::Shutdown);
        let handle = HANDLE(thread.as_raw_handle());
        // SAFETY: the live handle owned by `thread`, which outlives the wait.
        let ended =
            unsafe { WaitForSingleObject(handle, within.as_millis() as u32) } == WAIT_OBJECT_0;
        DRAW_KEYS.store(false, SeqCst);
        WHEEL_GATE.disarm();
        if ended {
            let _ = thread.join();
        }
        ended
    }

    /// The window handles.
    pub fn windows(&self) -> UiWindows {
        lock(&self.shared.windows).clone()
    }

    /// The ink's frame timings so far.
    pub fn ink_stats(&self) -> InkStats {
        *lock(&self.shared.stats)
    }

    /// The ink as of the thread's last pass.
    pub fn ink_state(&self) -> InkState {
        *lock(&self.shared.state)
    }

    /// Passes of the thread's loop so far (each wake is one): an idle UI adds none.
    /// The strip as of the thread's last pass: where its parts are on screen, the tip shown.
    pub fn pill(&self) -> PillSnapshot {
        lock(&self.shared.pill).clone()
    }

    pub fn passes(&self) -> u64 {
        self.shared.passes.load(Relaxed)
    }

    /// For the self-test's control run: let screenshots see the outline and the pill.
    #[doc(hidden)]
    pub fn test_set_capturable(&self, capturable: bool) {
        self.shared.send(Command::Capturable(capturable));
    }

    /// For the self-test: present the whole ink window once; the seconds it took arrive in
    /// [`test_full_present_time`](Self::test_full_present_time).
    #[doc(hidden)]
    pub fn test_full_present(&self) {
        *lock(&self.shared.full_present) = None;
        self.shared.send(Command::FullPresent);
    }

    #[doc(hidden)]
    pub fn test_full_present_time(&self) -> Option<f64> {
        *lock(&self.shared.full_present)
    }

    /// For the self-test: start the ink's frame statistics over.
    #[doc(hidden)]
    pub fn test_reset_stats(&self) {
        self.shared.send(Command::ResetStats);
    }
}

impl Drop for RecordingUi {
    fn drop(&mut self) {
        self.end(Duration::from_secs(1));
    }
}

fn run(
    shared: Arc<Shared>,
    monitor: MonitorBounds,
    config: RecordingUiConfig,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let _gates = ReleaseGates;
    let mut msg = MSG::default();
    // SAFETY: plain calls on this thread: physical pixels for every window and point; the
    // message queue exists before anyone waits on us.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    }
    let frame = match FrameWindows::new(
        monitor,
        config.labels.clone(),
        config.modifier,
        config.capturable,
        config.show_start_hint,
        hook::now(),
    ) {
        Ok(frame) => frame,
        Err(e) => {
            let _ = ready.send(Err(format!("frame windows: {e}")));
            return;
        }
    };
    let ink = match InkWindow::new(monitor, config.take_mouse) {
        Ok(ink) => ink,
        Err(e) => {
            let _ = ready.send(Err(format!("ink window: {e}")));
            return;
        }
    };
    // Armed before the keyboard hook starts: the hook sets the held bit from the keys down
    // when it starts, and `arm` clears it.
    WHEEL_GATE.arm(config.modifier == ZoomModifier::CtrlAlt);
    let keys = if config.key_hook {
        match KeyHook::start(config.modifier) {
            Ok(keys) => Some(keys),
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        }
    } else {
        None
    };
    *lock(&shared.windows) = UiWindows {
        ink: ink.hwnd.0 as isize,
        frame: frame.hwnds().iter().map(|h| h.0 as isize).collect(),
    };
    let now = hook::now();
    let machine = Machine::new(config.modifier, config.inject_mask);
    let mut driver = Driver {
        shared,
        config,
        machine,
        frame,
        ink,
        keys,
        elapsed: (0.0, now),
        stopping: false,
        hidden_sent: false,
        strikes: 0,
        next_health: now + 1.0,
        key_buffer: Vec::new(),
    };
    driver.apply(Input::Start, now);
    if driver.keys.is_some() {
        driver.apply(Input::KeysReset(Mods::now()), now);
    }
    driver.frame.set_pill(0.0, None, now);
    driver.frame.appear(now);
    let _ = ready.send(Ok(()));
    drop(ready);
    driver.log("started");
    driver.run();
}

struct Driver {
    shared: Arc<Shared>,
    config: RecordingUiConfig,
    machine: Machine,
    frame: FrameWindows,
    ink: InkWindow,
    keys: Option<KeyHook>,
    /// The elapsed seconds given last, and when (the clock of `hook::now`).
    elapsed: (f64, f64),
    stopping: bool,
    hidden_sent: bool,
    strikes: u32,
    next_health: f64,
    key_buffer: Vec<KeyInput>,
}

impl Driver {
    fn log(&self, text: &str) {
        if self.config.debug {
            self.shared.emit(UiEvent::Log(format!(
                "{:10.3} recording: {text}",
                hook::now()
            )));
        }
    }

    fn elapsed_now(&self, now: f64) -> f64 {
        self.elapsed.0 + (now - self.elapsed.1).max(0.0)
    }

    fn run(&mut self) {
        let mut animating = false;
        loop {
            let now = hook::now();
            let mut due = f64::INFINITY;
            if !self.stopping {
                due = due.min(now + frame::until_next_second(self.elapsed_now(now)));
            }
            if let Some(at) = self.machine.next_deadline() {
                due = due.min(at);
            }
            if let Some(at) = self.frame.next_deadline(now) {
                due = due.min(at);
            }
            if animating {
                due = due.min(now + FRAME.as_secs_f64());
            }
            let timeout = if due.is_finite() {
                ((due - now).max(0.0) * 1000.0).ceil() as u32
            } else {
                u32::MAX // INFINITE
            };
            let mut handles = vec![self.shared.wake.0];
            if let Some(keys) = &self.keys {
                handles.push(keys.wake_handle());
            }
            let wheel = WHEEL_GATE.wake_handle();
            if !wheel.is_invalid() {
                handles.push(wheel);
            }
            // SAFETY: every handle is alive for the call: ours, the hook's (owned by
            // `self.keys`) and the gate's (never closed).
            let result = unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&handles),
                    timeout,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
            if result == WAIT_FAILED {
                thread::sleep(Duration::from_millis(10));
            }
            self.shared.passes.fetch_add(1, Relaxed);
            pump_messages();
            if !self.turn() {
                break;
            }
            let now = hook::now();
            let ink_fading = self.ink.frame(now);
            self.ink.draw();
            let frame_moving = self.frame.frame(now);
            animating = ink_fading || frame_moving;
            self.publish();
            if self.stopping && !self.hidden_sent && !animating && self.frame.is_hidden(now) {
                if self.ink.shown {
                    self.ink.place(false);
                }
                self.hidden_sent = true;
                self.shared.emit(UiEvent::Hidden);
                self.log("hidden");
            }
        }
        // Shutdown: nothing armed, nothing shown, the hook removed (the gates by `_gates`).
        DRAW_KEYS.store(false, SeqCst);
        WHEEL_GATE.disarm();
        self.keys = None;
        if !self.hidden_sent {
            self.shared.emit(UiEvent::Hidden);
        }
        self.log("stopped");
    }

    /// One pass over everything that arrived. False on shutdown.
    fn turn(&mut self) -> bool {
        let commands: Vec<Command> = lock(&self.shared.commands).drain(..).collect();
        let now = hook::now();
        for command in commands {
            match command {
                Command::Elapsed(seconds) => self.elapsed = (seconds, now),
                Command::Tool(tool) => self.apply(Input::ToolShortcut(tool), now),
                Command::Key(key) => self.apply(Input::InkKey(key), now),
                Command::Stop => {
                    self.apply(Input::Stop, now);
                    self.stopping = true;
                    self.keys = None;
                    self.frame.disappear(now);
                    self.log("stop");
                }
                Command::Shutdown => return false,
                Command::Capturable(on) => self.frame.set_capturable(on),
                Command::ResetStats => self.ink.stats = InkStats::default(),
                Command::FullPresent => {
                    *lock(&self.shared.full_present) = self.ink.full_present();
                }
            }
        }
        // Keys first: a wheel event comes after the key that made it a zoom.
        let mut keys = std::mem::take(&mut self.key_buffer);
        if let Some(hook) = &self.keys {
            hook.drain(&mut keys);
        }
        for key in keys.drain(..) {
            self.apply(Input::Key(key), key.time);
        }
        self.key_buffer = keys;
        let (delta, _) = WHEEL_GATE.take();
        if delta != 0 {
            self.apply(Input::Wheel(delta), now);
        }
        self.apply(Input::Tick, hook::now());
        let events: Vec<InkMouse> = INK_QUEUE.with(|queue| {
            queue
                .try_borrow_mut()
                .map(|mut queue| std::mem::take(&mut *queue))
                .unwrap_or_default()
        });
        if !events.is_empty() {
            self.ink.stats.history_points = INK_HISTORY.get();
            self.ink.on_mouse(&events);
        }
        if let Some(area) = lock(&self.shared.area).take() {
            self.frame.set_area(area);
        }
        let now = hook::now();
        for action in self.frame.input(now) {
            if self.stopping {
                continue;
            }
            match action {
                PillAction::Tool(tool) => self.apply(Input::ToolShortcut(tool), now),
                PillAction::Stop => {
                    self.shared.emit(UiEvent::StopRequested);
                    self.log("stop button");
                }
                PillAction::ResetZoom => {
                    self.shared.emit(UiEvent::ResetZoom);
                    self.log("zoom badge: whole screen");
                }
            }
        }
        if !self.stopping {
            self.frame
                .set_pill(self.elapsed_now(now), self.ink.ink.tool(), now);
            if now >= self.next_health {
                self.health();
                self.next_health = now + 1.0;
            }
        }
        // The gates follow the machine.
        let draw = !self.stopping && self.machine.keys_armed(&self.ink.ink);
        DRAW_KEYS.store(draw, SeqCst);
        if !self.machine.wheel_armed() && WHEEL_GATE.is_armed() {
            WHEEL_GATE.disarm();
        }
        true
    }

    /// Once a second: is the keyboard hook still there? Windows removes slow hooks without
    /// notice; then the modifiers it last saw disagree with the system's, twice in a row.
    fn health(&mut self) {
        let Some(keys) = &self.keys else {
            return;
        };
        let bad = !keys.is_alive() || keys.disagrees();
        self.strikes = if bad { self.strikes + 1 } else { 0 };
        if self.strikes < 2 {
            return;
        }
        self.strikes = 0;
        self.keys = None;
        match KeyHook::start(self.config.modifier) {
            Ok(keys) => {
                self.keys = Some(keys);
                self.log("keyboard hook restarted");
            }
            Err(e) => self.log(&format!("keyboard hook restart failed: {e}")),
        }
        self.apply(Input::KeysReset(Mods::now()), hook::now());
    }

    fn apply(&mut self, input: Input, now: f64) {
        let actions = self.machine.step(input, now, &mut self.ink.ink);
        for action in actions {
            match action {
                Action::Zoom(amount) => self.shared.emit(UiEvent::Zoom(amount)),
                Action::Follow(on) => {
                    let cursor = self.cursor();
                    self.shared.emit(UiEvent::Follow { on, cursor });
                    self.log(&format!("follow {on}"));
                }
                Action::ResetZoom => {
                    self.shared.emit(UiEvent::ResetZoom);
                    self.log("double tap: whole screen");
                }
                Action::Tool(tool) => {
                    self.ink.set_tool(tool);
                    let now = hook::now();
                    self.frame.set_pill(self.elapsed_now(now), tool, now);
                    if tool.is_some() {
                        // The ink window just came up; the outline and the pill stay above.
                        self.ink.draw();
                        self.frame.reassert();
                    }
                    self.shared.emit(UiEvent::ToolChanged(tool));
                    self.log(&format!("tool {tool:?}"));
                }
                Action::Redraw => self.ink.dirty = true,
                Action::FadeAway => self.ink.fade_away(hook::now()),
                Action::Mask { changes } => {
                    if !self.config.inject_mask {
                        continue;
                    }
                    let current = self.keys.as_ref().map_or(changes + 1, KeyHook::changes);
                    match send_mask(changes, current) {
                        Ok(()) => self.log("mask key sent (Alt held)"),
                        Err(why) => self.log(&format!("mask key not sent: {why}")),
                    }
                }
            }
        }
    }

    fn cursor(&self) -> (i32, i32) {
        if self.shared.has_cursor.load(Relaxed) != 0 {
            let packed = self.shared.cursor.load(Relaxed);
            return ((packed >> 32) as i32, packed as u32 as i32);
        }
        let point = cursor_pos();
        (point.x.round() as i32, point.y.round() as i32)
    }

    fn publish(&self) {
        *lock(&self.shared.stats) = self.ink.stats;
        *lock(&self.shared.state) = self.ink.state();
        *lock(&self.shared.pill) = self.frame.snapshot();
    }
}

fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid MSG for the calls to fill and read. The window procedures of
    // this thread (the ink's and the overlay's) touch only thread-locals.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ink_fades_out_eased_and_is_cleared_after_the_swift_delay() {
        assert_eq!(ink_fade_opacity(0.0), Some(1.0));
        let early = ink_fade_opacity(0.05).unwrap();
        let mid = ink_fade_opacity(0.125).unwrap();
        assert!(
            early < 1.0 && mid < early && mid < 0.5,
            "ease-out drops fast"
        );
        assert_eq!(ink_fade_opacity(INK_FADE), Some(0.0));
        assert_eq!(ink_fade_opacity(0.29), Some(0.0));
        assert_eq!(ink_fade_opacity(INK_CLEAR), None);
        assert_eq!(ink_fade_opacity(-1.0), Some(1.0));
    }

    #[test]
    fn mouse_points_are_signed_client_coordinates() {
        assert_eq!(client_point(LPARAM(0x0002_0001)), (1, 2));
        assert_eq!(client_point(LPARAM(0xFFFE_FFFF)), (-1, -2));
    }
}
