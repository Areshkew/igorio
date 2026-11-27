//! Self-check for the recording UI (`igorio_app::recording`): the ink window, the outline,
//! the control strip and its tip, and the input gates. Non-interactive; prints PASS, FAIL or
//! INFO per check and exits 0 only if every hard check passed.
//!
//! It sends no keyboard or mouse input and never moves the cursor. The ink is drawn, and the
//! strip's buttons are clicked and hovered, with messages posted to our own windows, the path
//! real mouse messages take. To stay out of the way of a person using the desktop:
//! - the "monitor" of the main run is a small region of the real one, so the ink window, the
//!   outline and the strip only cover that region, for a few seconds;
//! - an opaque backdrop window of our own lies under that region, so the screen captures
//!   compare our own pixels and not whatever the desktop shows there. It is topmost, directly
//!   under the test's windows: before every capture and every probe the order backdrop, ink,
//!   outline, tip, strip is set again at the top of the topmost band and then checked by
//!   walking the z-order (`GetTopWindow`/`GetWindow`). If a visible, uncloaked, capturable
//!   window of another program is above the backdrop inside the region (it re-topped itself),
//!   the check is reported as SKIP with that window's class and program, never as FAIL. The
//!   region itself is first moved to a place no topmost window of another program covers;
//!   `--stay` keeps it at the first place;
//! - a tool is in hand (the region takes the mouse, Esc, Backspace, Delete and Ctrl+Z are
//!   swallowed) for about two seconds;
//! - the mask key is off; the wheel gate is armed while the UI runs (Alt + wheel does not
//!   scroll for those seconds);
//! - the 4K timing runs keep the ink window click-through and install no keyboard hook.
//!
//! `recording_ui_selftest --probe X Y` is the helper mode: run as a second process, it
//! prints the top-level window at (X, Y) and its process id.
//! `recording_ui_selftest --pngs` renders the PNGs in `crates/app/examples/` and exits.

use std::ffi::c_void;
use std::fmt::Display;
use std::fs::File;
use std::io::BufWriter;
use std::mem::size_of;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use igorio_app::hook::{CLICK_GATE, WHEEL_GATE};
use igorio_app::ink_draw::{InkRenderer, tool_cursor};
use igorio_app::overlay::{MonitorBounds, cursor_pos, ensure_dpi_awareness, monitor_bounds_at};
use igorio_app::recording::frame::{
    self, Fonts, Labels, Part, PillLook, Tip, outline_rect, outline_strips, pill_layout,
    pill_model, pill_origin, render_pill, render_tip, tip_origin, tip_segments,
};
use igorio_app::recording::ink_overlay::{InkStats, RecordingUi, RecordingUiConfig, UiEvent};
use igorio_app::recording::input::{DRAW_KEYS, ZoomModifier};
use igorio_core::geom::{Point, Rect};
use igorio_core::ink::{Ink, InkKey, InkTool};
use tiny_skia::{Color, Paint, Pixmap, PixmapPaint, Transform};
use windows::Win32::Foundation::{
    COLORREF, FILETIME, HINSTANCE, HWND, LPARAM, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmFlush, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    CreateSolidBrush, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HBRUSH,
    HDC, HGDIOBJ, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, ReleaseDC, SRCCOPY,
    SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EVENT_SYSTEM_FOREGROUND,
    GA_ROOT, GW_HWNDNEXT, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetForegroundWindow,
    GetTopWindow, GetWindow, GetWindowDisplayAffinity, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, HWND_TOPMOST, IsWindow, IsWindowVisible, MSG, PM_REMOVE,
    PeekMessageW, PostMessageW, RegisterClassW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SetWindowPos, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WINEVENT_OUTOFCONTEXT,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WindowFromPoint,
};
use windows::core::w;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// `WM_MOUSELEAVE`.
const WM_MOUSELEAVE: u32 = 0x02A3;
/// The backdrop's colour (RGB): a quiet blue-grey, so the ink, the outline and the strip all
/// differ from it.
const BACKDROP: [u8; 3] = [205, 213, 224];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [flag, x, y] if flag == "--probe" => probe(x, y).map(|()| true),
        [flag] if flag == "--pngs" => pngs().map(|()| true),
        [] => run(false),
        [flag] if flag == "--stay" => run(true),
        _ => Err("usage: recording_ui_selftest [--probe X Y | --pngs]".into()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            // The gates must never stay armed, whatever failed.
            WHEEL_GATE.disarm();
            DRAW_KEYS.store(false, Relaxed);
            eprintln!("recording_ui_selftest: {err}");
            ExitCode::FAILURE
        }
    }
}

struct Report {
    pass: bool,
    skipped: u32,
}

impl Report {
    fn check(&mut self, ok: bool, text: impl Display) -> bool {
        println!("{} {text}", if ok { "PASS" } else { "FAIL" });
        self.pass &= ok;
        ok
    }

    fn info(&self, text: impl Display) {
        println!("INFO {text}");
    }

    /// A check that could not be made (another program's window covered the test region).
    /// Not a failure.
    fn skip(&mut self, text: impl Display) {
        println!("SKIP {text}");
        self.skipped += 1;
    }

    /// A check that needs the z-order to be right: skipped when `guarded` says it is not.
    fn check_or_skip(
        &mut self,
        guarded: &std::result::Result<(), String>,
        ok: bool,
        text: impl Display,
    ) {
        match guarded {
            Ok(()) => {
                self.check(ok, text);
            }
            Err(why) => self.skip(format_args!("{text}: {why}")),
        }
    }

    /// A pixel check of a capture against the baseline; skipped when either capture could
    /// not be taken. `f` returns the verdict and the detail.
    fn compare(
        &mut self,
        baseline: &Shot,
        shot: &Shot,
        name: &str,
        f: impl FnOnce(&[u8], &[u8]) -> (bool, String),
    ) {
        match (baseline, shot) {
            (Ok(a), Ok(b)) => {
                let (ok, detail) = f(a, b);
                self.check(ok, format_args!("{name}{detail}"));
            }
            (Err(why), _) | (_, Err(why)) => self.skip(format_args!("{name}: {why}")),
        }
    }
}

// MARK: - The z-order

/// A screen capture, or why it was not taken (a foreign window over the region).
type Shot = std::result::Result<Vec<u8>, String>;

/// The windows of the test, bottom to top: the backdrop, then the ink window, the outline
/// strips, the tip and the strip. All are topmost; `enforce` puts them in this order at the top
/// of the topmost band, `verify` walks the real z-order to check it.
struct Order {
    backdrop: HWND,
    /// Ours above the backdrop, bottom to top.
    ours: Vec<HWND>,
    region: (i32, i32, i32, i32),
}

impl Order {
    fn enforce(&self) {
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
        // SAFETY: windows of this process; topmost re-insertion without activation. Each goes
        // to the top of the topmost band, so the last one ends on top.
        unsafe {
            let _ = SetWindowPos(self.backdrop, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            for &hwnd in &self.ours {
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            }
            let _ = RedrawWindow(
                Some(self.backdrop),
                None,
                None,
                RDW_ERASE | RDW_INVALIDATE | RDW_UPDATENOW,
            );
        }
        pump();
        flush();
    }

    /// Walks the z-order from the top down to the backdrop: every visible window of ours must
    /// be above it, and no visible, uncloaked, capturable window of another process may be
    /// above it inside the region. `Err` names what is wrong.
    fn verify(&self) -> std::result::Result<(), String> {
        let own = std::process::id();
        let mut seen = Vec::new();
        // SAFETY: walks the z-order; a stale handle just ends the walk.
        let mut next = unsafe { GetTopWindow(None) }.ok();
        while let Some(hwnd) = next.filter(|h| !h.is_invalid()) {
            if hwnd == self.backdrop {
                let missing = self
                    .ours
                    .iter()
                    .filter(|h| visible_window(**h) && !seen.contains(*h))
                    .count();
                return if missing == 0 {
                    Ok(())
                } else {
                    Err(format!(
                        "{missing} window(s) of the test are below the backdrop"
                    ))
                };
            }
            if self.ours.contains(&hwnd) {
                seen.push(hwnd);
            } else if let Some(what) = covering(hwnd, own, self.region) {
                return Err(format!(
                    "a window of another program covers the test region: {what}"
                ));
            }
            // SAFETY: as above.
            next = unsafe { GetWindow(hwnd, GW_HWNDNEXT) }.ok();
        }
        Err("the backdrop is not in the z-order".into())
    }

    /// Every visible window of ours is above the backdrop (whatever else is in between).
    fn ours_above(&self) -> bool {
        let mut seen = Vec::new();
        // SAFETY: walks the z-order; a stale handle just ends the walk.
        let mut next = unsafe { GetTopWindow(None) }.ok();
        while let Some(hwnd) = next.filter(|h| !h.is_invalid()) {
            if hwnd == self.backdrop {
                return self
                    .ours
                    .iter()
                    .all(|h| !visible_window(*h) || seen.contains(h));
            }
            if self.ours.contains(&hwnd) {
                seen.push(hwnd);
            }
            // SAFETY: as above.
            next = unsafe { GetWindow(hwnd, GW_HWNDNEXT) }.ok();
        }
        false
    }

    /// Puts the order right and checks it, a few times if another program is re-topping its
    /// window at the same moment.
    fn guard(&self) -> std::result::Result<(), String> {
        let mut last = String::new();
        for _ in 0..3 {
            self.enforce();
            match self.verify() {
                Ok(()) => return Ok(()),
                Err(why) => last = why,
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        Err(last)
    }

    /// A capture of the region with the order checked first.
    fn shot(&self, capture: &mut Capture) -> Shot {
        self.guard()?;
        capture
            .grab(self.region)
            .map(<[u8]>::to_vec)
            .map_err(|e| e.to_string())
    }
}

fn visible_window(hwnd: HWND) -> bool {
    // SAFETY: reads a flag of a window handle.
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}

/// If `hwnd` is a window of another process that would show in a capture of `region`: its
/// class and program.
fn covering(hwnd: HWND, own: u32, region: (i32, i32, i32, i32)) -> Option<String> {
    let mut pid = 0;
    // SAFETY: writes into a local.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == own || !visible_window(hwnd) {
        return None;
    }
    let mut cloaked = 0u32;
    // SAFETY: DWMWA_CLOAKED writes one DWORD into a local.
    let got = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
    };
    if got.is_ok() && cloaked != 0 {
        return None;
    }
    // Excluded from capture (like this app's own overlays): not in a capture.
    if affinity(hwnd) == WDA_EXCLUDEFROMCAPTURE.0 {
        return None;
    }
    let (x, y, w, h) = window_rect(hwnd);
    let (rx, ry, rw, rh) = region;
    if w <= 0 || h <= 0 || x >= rx + rw || x + w <= rx || y >= ry + rh || y + h <= ry {
        return None;
    }
    Some(format!("class {}, {}", class_name(hwnd), process_name(pid)))
}

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: writes into a local buffer of the given length.
    let n = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..n.max(0) as usize])
}

fn process_name(pid: u32) -> String {
    // SAFETY: a query-only handle, closed below; the name goes into a local buffer.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return format!("pid {pid}");
        };
        let mut buffer = [0u16; 512];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(process);
        if !ok {
            return format!("pid {pid}");
        }
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit('\\').next().unwrap_or(&path).to_string()
    }
}

/// Topmost windows of other processes that would cover `region` (they stay in the topmost
/// band and may come above the backdrop again): the first one, if any.
fn topmost_over(region: (i32, i32, i32, i32)) -> Option<String> {
    let own = std::process::id();
    // SAFETY: walks the z-order; a stale handle just ends the walk.
    let mut next = unsafe { GetTopWindow(None) }.ok();
    while let Some(hwnd) = next.filter(|h| !h.is_invalid()) {
        if ex_style(hwnd) & WS_EX_TOPMOST.0 == 0 {
            // The topmost band is over.
            return None;
        }
        if let Some(what) = covering(hwnd, own, region) {
            return Some(what);
        }
        // SAFETY: as above.
        next = unsafe { GetWindow(hwnd, GW_HWNDNEXT) }.ok();
    }
    None
}

/// The first place for the test region and what covers it, if anything.
fn first_region(monitor: &MonitorBounds) -> (MonitorBounds, Option<String>, bool) {
    let s = monitor.scale;
    let work = monitor.work_area;
    let bounds = Rect::new(
        (work.x + 80.0 * s).round(),
        (work.y + 120.0 * s).round(),
        (720.0 * s).round().min(work.width - 80.0 * s),
        (400.0 * s).round().min(work.height - 160.0 * s),
    );
    let covered = topmost_over((
        bounds.x as i32,
        bounds.y as i32,
        bounds.width as i32,
        bounds.height as i32,
    ));
    let first = MonitorBounds {
        bounds,
        work_area: bounds,
        scale: s,
    };
    (first, covered, false)
}

/// The test region: the first of a few places on the monitor that no topmost window of
/// another program covers, or the first place and what covers it.
fn free_region(monitor: &MonitorBounds) -> (MonitorBounds, Option<String>, bool) {
    let s = monitor.scale;
    let work = monitor.work_area;
    let width = (720.0 * s).round().min(work.width - 80.0 * s);
    let height = (400.0 * s).round().min(work.height - 160.0 * s);
    let left = work.x + 80.0 * s;
    let right = work.x + work.width - width - 80.0 * s;
    let top = work.y + 120.0 * s;
    let bottom = work.y + work.height - height - 40.0 * s;
    let places = [
        (left, top),
        (right, top),
        (left, bottom),
        (right, bottom),
        ((left + right) / 2.0, (top + bottom) / 2.0),
    ];
    let make = |(x, y): (f64, f64)| {
        let bounds = Rect::new(x.round(), y.round(), width, height);
        MonitorBounds {
            bounds,
            work_area: bounds,
            scale: s,
        }
    };
    let rect = |m: &MonitorBounds| {
        (
            m.bounds.x as i32,
            m.bounds.y as i32,
            m.bounds.width as i32,
            m.bounds.height as i32,
        )
    };
    let mut first_problem = None;
    for place in places {
        let candidate = make(place);
        match topmost_over(rect(&candidate)) {
            None => return (candidate, first_problem, true),
            Some(what) => {
                first_problem.get_or_insert(what);
            }
        }
    }
    (make(places[0]), first_problem, false)
}

// MARK: - The backdrop

/// An opaque window of our own under the test region: what the captures see behind our
/// windows. Topmost (so windows of other programs, topmost ones included, go below it), never
/// activated; [`Order`] keeps it directly under the test's windows.
struct Backdrop {
    hwnd: HWND,
}

impl Backdrop {
    fn new(rect: (i32, i32, i32, i32)) -> Result<Backdrop> {
        // SAFETY: plain calls; the brush is owned by the class for the process's life, the
        // class only points to static data and a system function.
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let brush: HBRUSH = CreateSolidBrush(COLORREF(
                u32::from(BACKDROP[0]) | u32::from(BACKDROP[1]) << 8 | u32::from(BACKDROP[2]) << 16,
            ));
            let class = WNDCLASSW {
                lpfnWndProc: Some(backdrop_proc),
                hInstance: instance,
                hbrBackground: brush,
                lpszClassName: w!("IgorioSelftestBackdrop"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("IgorioSelftestBackdrop"),
                w!("recording_ui_selftest backdrop"),
                WS_POPUP,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                None,
                None,
                Some(instance),
                None,
            )?;
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                RDW_ERASE | RDW_INVALIDATE | RDW_UPDATENOW,
            );
            Ok(Backdrop { hwnd })
        }
    }
}

impl Drop for Backdrop {
    fn drop(&mut self) {
        // SAFETY: made on this thread, destroyed once.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

// MARK: - The strip

fn window_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    let mut r = RECT::default();
    // SAFETY: writes into a local.
    let _ = unsafe { GetWindowRect(hwnd, &mut r) };
    (r.left, r.top, r.right - r.left, r.bottom - r.top)
}

/// The centre of a strip part, in the strip window's client pixels and on screen.
fn part_point(ui: &RecordingUi, part: Part) -> Option<((i32, i32), POINT)> {
    let pill = ui.pill();
    let b = pill.parts.iter().find(|(p, _)| *p == part)?.1;
    let screen = POINT {
        x: b.x + b.w / 2,
        y: b.y + b.h / 2,
    };
    Some(((screen.x - pill.origin.0, screen.y - pill.origin.1), screen))
}

/// A click on a part of the strip: press and release posted to its window.
fn click(ui: &RecordingUi, pill: HWND, part: Part) -> bool {
    let Some((client, _)) = part_point(ui, part) else {
        return false;
    };
    post(pill, WM_LBUTTONDOWN, 1, client);
    post(pill, WM_LBUTTONUP, 0, client);
    true
}

/// Is `upper` above `lower` in the z-order?
fn above(upper: HWND, lower: HWND) -> bool {
    let mut next = upper;
    for _ in 0..10_000 {
        // SAFETY: walks the z-order; a stale handle just ends the walk.
        next = match unsafe { GetWindow(next, GW_HWNDNEXT) } {
            Ok(h) if !h.is_invalid() => h,
            _ => return false,
        };
        if next == lower {
            return true;
        }
    }
    false
}

/// `stay`: keep the test region at its first place even when a window of another program
/// covers it (to exercise the z-order rules).
fn run(stay: bool) -> Result<bool> {
    ensure_dpi_awareness();
    let mut report = Report {
        pass: true,
        skipped: 0,
    };
    let monitor = monitor_bounds_at(cursor_pos());
    let (fake, covered, free) = if stay {
        first_region(&monitor)
    } else {
        free_region(&monitor)
    };
    let scale = fake.scale;
    let r = fake.bounds;
    report.info(format_args!(
        "monitor {}x{} at {},{} scale {scale}; test region {}x{} at {},{}",
        monitor.bounds.width,
        monitor.bounds.height,
        monitor.bounds.x,
        monitor.bounds.y,
        r.width,
        r.height,
        r.x,
        r.y
    ));
    report.info(format_args!(
        "animation effects {}",
        if frame::animation_effects() {
            "on"
        } else {
            "off (reduced motion)"
        }
    ));
    match (&covered, free) {
        (Some(what), true) => report.info(format_args!(
            "test region moved: the first place is covered by {what}"
        )),
        (Some(what), false) => report.info(format_args!(
            "the test region is covered by {what}; it stays there, under the topmost backdrop"
        )),
        _ => {}
    }
    let rect = (r.x as i32, r.y as i32, r.width as i32, r.height as i32);
    let backdrop = Backdrop::new(rect)?;
    settle_plain(150);

    let config = RecordingUiConfig {
        inject_mask: false,
        show_start_hint: true,
        ..RecordingUiConfig::default()
    };
    let mut events = Vec::new();
    let mut ui = RecordingUi::start(fake, config)?;
    let windows = ui.windows();
    let ours: Vec<isize> = std::iter::once(windows.ink)
        .chain(windows.frame.iter().copied())
        .collect();
    let watch = ForegroundWatch::new(&ours)?;
    ui.set_elapsed(0.0);
    // A 2x zoom in the middle of the region, as the viewport would hand it in.
    let area = Rect::new(
        r.x + r.width / 4.0,
        r.y + r.height / 4.0,
        r.width / 2.0,
        r.height / 2.0,
    );
    ui.set_area(area);
    settle(&ui, &mut events, 400);

    // 1. Styles.
    let ink = HWND(windows.ink as *mut c_void);
    let pill_hwnd = HWND(windows.frame[4] as *mut c_void);
    let tip_hwnd = HWND(windows.frame[5] as *mut c_void);
    // Bottom to top above the backdrop: ink, outline strips, tip, strip.
    let order = Order {
        backdrop: backdrop.hwnd,
        ours: std::iter::once(windows.ink)
            .chain(windows.frame[..4].iter().copied())
            .chain([windows.frame[5], windows.frame[4]])
            .map(|h| HWND(h as *mut c_void))
            .collect(),
        region: rect,
    };
    order.enforce();
    report.check(
        order.ours_above(),
        "z-order, walked with GetTopWindow/GetWindow: the topmost backdrop is under every window of the test",
    );
    let ink_style = ex_style(ink);
    let base = WS_EX_LAYERED.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0;
    report.check(
        ink_style & base == base && ink_style & WS_EX_TRANSPARENT.0 != 0,
        format_args!("ink window: layered, topmost, no-activate, tool window, click-through with no tool (ex-style 0x{ink_style:08X})"),
    );
    report.check(
        affinity(ink) == WDA_NONE.0,
        format_args!(
            "ink window is capturable (affinity 0x{:02X}): the ink is in the video",
            affinity(ink)
        ),
    );
    let visible = |h: HWND| {
        // SAFETY: reads a flag of a window handle.
        unsafe { IsWindowVisible(h) }.as_bool()
    };
    let mut strips_ok = windows.frame.len() == 6;
    for &hwnd in &windows.frame[..4] {
        let hwnd = HWND(hwnd as *mut c_void);
        let style = ex_style(hwnd);
        strips_ok &= style & (base | WS_EX_TRANSPARENT.0) == base | WS_EX_TRANSPARENT.0
            && affinity(hwnd) == WDA_EXCLUDEFROMCAPTURE.0
            && visible(hwnd);
    }
    report.check(
        strips_ok,
        "outline strips: layered, click-through, topmost, no-activate, tool windows, excluded from capture, shown",
    );
    let pill_style = ex_style(pill_hwnd);
    report.check(
        pill_style & base == base
            && pill_style & WS_EX_TRANSPARENT.0 == 0
            && affinity(pill_hwnd) == WDA_EXCLUDEFROMCAPTURE.0
            && visible(pill_hwnd),
        format_args!("control strip: layered, topmost, no-activate, tool window, takes the mouse (not click-through), excluded from capture, shown (ex-style 0x{pill_style:08X})"),
    );
    let tip_style = ex_style(tip_hwnd);
    report.check(
        tip_style & (base | WS_EX_TRANSPARENT.0) == base | WS_EX_TRANSPARENT.0
            && affinity(tip_hwnd) == WDA_EXCLUDEFROMCAPTURE.0,
        "tip window: click-through, topmost, no-activate, excluded from capture",
    );
    let (_, _, pill_w, _) = window_rect(pill_hwnd);
    report.check(
        f64::from(pill_w) / scale <= frame::PILL_MAX_WIDTH + 50.0,
        format_args!(
            "the strip is {:.0} DIPs wide with the 2.0x badge",
            f64::from(pill_w) / scale
        ),
    );
    report.check(
        WHEEL_GATE.is_armed(),
        "wheel gate armed while the recording UI runs",
    );
    report.check(
        !DRAW_KEYS.load(Relaxed),
        "drawing keys not taken with no tool in hand",
    );

    // 2. The start hint fades in under the strip.
    let hinted = wait_until(&ui, &mut events, 2000, |ui| {
        ui.pill().tip == Some(Tip::StartHint)
    });
    report.check(
        hinted,
        "the start hint \"Hold Alt and scroll to zoom\" shows under the strip",
    );

    // 3. Baseline capture of the region: our backdrop under excluded windows.
    let mut capture = Capture::new(rect.2, rect.3)?;
    let baseline = order.shot(&mut capture);
    match &baseline {
        Ok(pixels) => {
            let share = share_of(pixels, BACKDROP);
            report.check(
                share > 0.99,
                format_args!(
                    "the capture of the region shows only our backdrop ({:.2}% of its pixels)",
                    share * 100.0
                ),
            );
        }
        Err(why) => report.skip(format_args!(
            "the capture of the region shows only our backdrop: {why}"
        )),
    }

    // 4. Who takes the mouse, seen from a second process.
    let spot = POINT {
        x: rect.0 + rect.2 / 2,
        y: rect.1 + rect.3 * 3 / 4,
    };
    let own_pid = std::process::id();
    let guarded = order.guard();
    let before = run_probe(spot, &ui, &mut events)?;
    report.check_or_skip(
        &guarded,
        before.0 == backdrop.hwnd.0 as isize,
        format_args!("no tool: a second process finds the window below at {},{} (our backdrop 0x{:X}), not the ink window", spot.x, spot.y, before.0),
    );
    let (_, pen_screen) = part_point(&ui, Part::Pen).ok_or("no pen button")?;
    let on_pen = run_probe(pen_screen, &ui, &mut events)?;
    report.check(
        on_pen.0 == windows.frame[4] && on_pen.1 == own_pid,
        format_args!("a second process finds the strip under its pen button at {},{}: the strip takes its own clicks", pen_screen.x, pen_screen.y),
    );
    let top = outline_strips(area, r, scale)[0].ok_or("no top strip")?;
    let outline = outline_rect(area, scale).ok_or("no outline")?;
    let on_line = POINT {
        x: outline.x + outline.w / 2,
        y: outline.y,
    };
    let guarded = order.guard();
    let through = run_probe(on_line, &ui, &mut events)?;
    report.check_or_skip(
        &guarded,
        through.0 != windows.frame[0] && through.0 == backdrop.hwnd.0 as isize,
        format_args!("the outline is click-through (a second process at {},{} on the line finds the backdrop; strip {top:?})", on_line.x, on_line.y),
    );

    // 5. The pen button picks the pen up; the click draws nothing.
    click(&ui, pill_hwnd, Part::Pen);
    let picked = wait_event(&ui, &mut events, 1000, |e| {
        *e == UiEvent::ToolChanged(Some(InkTool::Pen))
    });
    settle(&ui, &mut events, 80);
    let state = ui.ink_state();
    report.check(
        picked && state.strokes == 0 && state.tool == Some(InkTool::Pen),
        "a click on the pen button picks the pen up (ToolChanged) and draws no stroke",
    );
    let tool_hint = wait_until(&ui, &mut events, 500, |ui| {
        ui.pill().tip == Some(Tip::ToolHint)
    });
    report.check(
        tool_hint,
        "picking a tool up shows its keys under the strip",
    );
    let style = ex_style(ink);
    let guarded = order.guard();
    let after = run_probe(spot, &ui, &mut events)?;
    report.check_or_skip(
        &guarded,
        style & WS_EX_TRANSPARENT.0 == 0 && after.0 == windows.ink && after.1 == own_pid,
        format_args!("pen in hand: the second process finds the ink window at {},{} (window 0x{:X}, ours 0x{:X}), with no ink there", spot.x, spot.y, after.0, windows.ink),
    );
    let guarded = order.guard();
    let on_pen = run_probe(pen_screen, &ui, &mut events)?;
    report.check_or_skip(
        &guarded,
        above(pill_hwnd, ink) && on_pen.0 == windows.frame[4],
        "pen in hand: the strip stays above the ink window and still takes its clicks",
    );
    report.check(
        DRAW_KEYS.load(Relaxed),
        "drawing keys taken while the pen is in hand",
    );
    click(&ui, pill_hwnd, Part::Grip);
    if let Some((client, _)) = part_point(&ui, Part::Grip) {
        post(pill_hwnd, WM_LBUTTONDOWN, 1, client);
        post(pill_hwnd, WM_MOUSEMOVE, 1, (client.0 + 3, client.1));
        post(pill_hwnd, WM_LBUTTONUP, 0, (client.0 + 3, client.1));
    }
    settle(&ui, &mut events, 80);
    report.check(
        ui.ink_state().strokes == 0 && ui.ink_state().tool == Some(InkTool::Pen),
        "pen in hand: presses and a drag on the strip draw nothing and change no tool",
    );

    // 6. Draw a stroke with posted messages, in the middle of the region.
    let stroke = |from: (i32, i32), to: (i32, i32)| -> Vec<(i32, i32)> {
        (0..=40)
            .map(|i| {
                let t = f64::from(i) / 40.0;
                let wave = (t * std::f64::consts::PI * 2.0).sin() * 12.0 * scale;
                (
                    from.0 + ((to.0 - from.0) as f64 * t) as i32,
                    from.1 + ((to.1 - from.1) as f64 * t + wave) as i32,
                )
            })
            .collect()
    };
    let mid = (rect.2 / 2, rect.3 / 2);
    let line = stroke((mid.0 - rect.2 / 6, mid.1), (mid.0 + rect.2 / 6, mid.1));
    draw(ink, &line);
    let drawn = wait_state(&ui, &mut events, 1000, |s| s.strokes == 1);
    let with_ink = order.shot(&mut capture);
    let ink_box = bbox(&line, (20.0 * scale) as i32);
    report.compare(
        &baseline,
        &with_ink,
        "the stroke is on screen and in a screen capture",
        |a, b| {
            let ink_pixels = count_diff(a, b, rect, Some(ink_box), None, 40);
            (
                drawn && ink_pixels > (100.0 * scale) as usize,
                format!(" ({ink_pixels} pixels changed around it)"),
            )
        },
    );

    // 7. The outline, the strip and the tip are not in the capture; the control run shows
    //    they are on screen.
    let strips = outline_strips(area, r, scale);
    let mut frame_boxes: Vec<(i32, i32, i32, i32)> = strips
        .iter()
        .flatten()
        .map(|s| (s.x - rect.0, s.y - rect.1, s.w, s.h))
        .collect();
    for hwnd in [pill_hwnd, tip_hwnd] {
        if visible(hwnd) {
            let (x, y, w, h) = window_rect(hwnd);
            frame_boxes.push((x - rect.0, y - rect.1, w, h));
        }
    }
    let frame_area: usize = frame_boxes
        .iter()
        .map(|b| (b.2.max(0) * b.3.max(0)) as usize)
        .sum();
    let tolerance = frame_area / 1000 + 4;
    ui.test_set_capturable(true);
    settle(&ui, &mut events, 60);
    let control = order.shot(&mut capture);
    ui.test_set_capturable(false);
    let pair = match (&with_ink, &control) {
        (Ok(a), Ok(b)) => Ok((a.clone(), b.clone())),
        (Err(why), _) | (_, Err(why)) => Err(why.clone()),
    };
    match (&baseline, &pair) {
        (Ok(base), Ok((with_ink, control))) => {
            let differ = |shot: &[u8]| {
                frame_boxes
                    .iter()
                    .map(|b| count_diff(base, shot, rect, Some(*b), Some(ink_box), 3))
                    .sum::<usize>()
            };
            let (hidden, shown) = (differ(with_ink), differ(control));
            report.check(
                hidden <= tolerance && shown > tolerance * 4,
                format_args!("outline, strip and tip not in the capture: {hidden} pixels differ under them (tolerance {tolerance}); control with the exclusion off: {shown}"),
            );
        }
        (Err(why), _) | (_, Err(why)) => report.skip(format_args!(
            "outline, strip and tip not in the capture: {why}"
        )),
    }
    settle(&ui, &mut events, 60);

    // 8. Undo, clear, put down.
    ui.ink_key(InkKey::Undo);
    let undone = wait_state(&ui, &mut events, 1000, |s| s.strokes == 0 && !s.content);
    let after_undo = order.shot(&mut capture);
    report.compare(
        &baseline,
        &after_undo,
        "undo takes the stroke back",
        |a, b| {
            let left = count_diff(a, b, rect, Some(ink_box), None, 3);
            (
                undone && left <= 4,
                format!(" ({left} pixels still differ)"),
            )
        },
    );

    let second = stroke(
        (mid.0 - rect.2 / 6, mid.1 + rect.3 / 10),
        (mid.0 + rect.2 / 6, mid.1 + rect.3 / 10),
    );
    draw(ink, &line);
    draw(ink, &second);
    let two = wait_state(&ui, &mut events, 1000, |s| s.strokes == 2);
    ui.ink_key(InkKey::Clear);
    let cleared = wait_state(&ui, &mut events, 1000, |s| s.strokes == 0 && !s.content);
    let after_clear = order.shot(&mut capture);
    let both = bbox(
        &[line.clone(), second.clone()].concat(),
        (20.0 * scale) as i32,
    );
    let state = ui.ink_state();
    report.compare(
        &baseline,
        &after_clear,
        "clear wipes both strokes and keeps the pen",
        |a, b| {
            let left = count_diff(a, b, rect, Some(both), None, 3);
            (
                two && cleared && left <= 4 && state.tool == Some(InkTool::Pen),
                format!(" ({left} pixels still differ)"),
            )
        },
    );

    draw(ink, &line);
    wait_state(&ui, &mut events, 1000, |s| s.strokes == 1);
    let put_down_at = Instant::now();
    ui.ink_key(InkKey::Done);
    let mut lowest = 255u8;
    let mut faded = false;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        pump();
        ui.drain(&mut events);
        let s = ui.ink_state();
        lowest = lowest.min(s.alpha);
        if s.tool.is_none() && !s.fading && !s.content && !s.shown {
            faded = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let fade_time = put_down_at.elapsed().as_secs_f64();
    let style = ex_style(ink);
    report.check(
        style & WS_EX_TRANSPARENT.0 != 0 && !DRAW_KEYS.load(Relaxed),
        "put down: click-through again and the drawing keys are free",
    );
    let after_fade = order.shot(&mut capture);
    let animated = frame::animation_effects();
    report.compare(
        &baseline,
        &after_fade,
        &format!(
            "the ink fades (lowest alpha seen {lowest}) and ends fully transparent and hidden after {:.0} ms",
            fade_time * 1000.0
        ),
        |a, b| {
            let left = count_diff(a, b, rect, None, None, 3);
            (
                faded && left <= 4 && (!animated || lowest < 200),
                format!(" ({left} pixels of the region differ from before)"),
            )
        },
    );
    let guarded = order.guard();
    let gone = run_probe(spot, &ui, &mut events)?;
    report.check_or_skip(
        &guarded,
        gone.0 != windows.ink,
        "after put-down a second process no longer finds the ink window",
    );

    // 9. The other buttons.
    click(&ui, pill_hwnd, Part::Highlighter);
    let high = wait_event(&ui, &mut events, 1000, |e| {
        *e == UiEvent::ToolChanged(Some(InkTool::Highlighter))
    });
    events.clear();
    click(&ui, pill_hwnd, Part::Highlighter);
    let down = wait_event(&ui, &mut events, 1000, |e| *e == UiEvent::ToolChanged(None));
    report.check(
        high && down && ui.ink_state().strokes == 0,
        "the highlighter button picks the highlighter up, and a second click puts it down",
    );
    events.clear();
    let badge = click(&ui, pill_hwnd, Part::Zoom);
    let reset = wait_event(&ui, &mut events, 1000, |e| *e == UiEvent::ResetZoom);
    report.check(
        badge && reset,
        "the 2.0x zoom badge shows while zoomed in, and a click on it asks for the whole screen",
    );
    ui.set_area(r);
    let no_badge = wait_until(&ui, &mut events, 500, |ui| {
        ui.pill().parts.iter().all(|(p, _)| *p != Part::Zoom)
    });
    report.check(no_badge, "at the whole screen the badge is gone");
    events.clear();
    click(&ui, pill_hwnd, Part::Stop);
    let stop = wait_event(&ui, &mut events, 1000, |e| *e == UiEvent::StopRequested);
    report.check(
        stop && WHEEL_GATE.is_armed(),
        "the stop button asks the app to stop (StopRequested); the UI itself waits for stop()",
    );

    // 10. A tooltip under a hovered button, and gone when the pointer leaves.
    let (stop_client, _) = part_point(&ui, Part::Stop).ok_or("no stop button")?;
    let hovered_at = Instant::now();
    post(pill_hwnd, WM_MOUSEMOVE, 0, stop_client);
    let tooltip = wait_until(&ui, &mut events, 1500, |ui| {
        ui.pill().tip == Some(Tip::Tooltip(Part::Stop))
    });
    let delay = hovered_at.elapsed().as_secs_f64();
    report.check(
        tooltip && delay >= frame::TOOLTIP_DELAY - 0.05,
        format_args!(
            "hovering the stop button shows its tooltip after {:.0} ms",
            delay * 1000.0
        ),
    );
    post(pill_hwnd, WM_MOUSELEAVE, 0, (0, 0));
    let untipped = wait_until(&ui, &mut events, 1000, |ui| ui.pill().tip.is_none());
    report.check(untipped, "leaving the strip takes the tooltip away");

    // 11. Idle: nothing moving, no timer but the strip's once-a-second time.
    settle(&ui, &mut events, 400);
    let passes = ui.passes();
    let cpu = process_cpu();
    let idle = Duration::from_millis(3000);
    let started = Instant::now();
    while started.elapsed() < idle {
        std::thread::sleep(Duration::from_millis(50));
    }
    let woke = ui.passes() - passes;
    let spent = process_cpu() - cpu;
    report.check(
        woke <= 4 && spent < 0.05,
        format_args!("idle for 3 s: the UI thread woke {woke} times (the strip's once-a-second time), process CPU {:.1} ms", spent * 1000.0),
    );

    // 12. Stop: gates at once, then the fade, then everything hidden.
    ui.tool_shortcut(InkTool::Highlighter);
    wait_event(&ui, &mut events, 1000, |e| {
        *e == UiEvent::ToolChanged(Some(InkTool::Highlighter))
    });
    ui.stop();
    report.check(
        !WHEEL_GATE.is_armed() && !WHEEL_GATE.is_held() && !DRAW_KEYS.load(Relaxed),
        "stop releases the wheel gate and the drawing keys at once (with a tool in hand)",
    );
    let stopped_at = Instant::now();
    let hidden_event = wait_event(&ui, &mut events, 2000, |e| *e == UiEvent::Hidden);
    let all_hidden = ours.iter().all(|&h| !visible(HWND(h as *mut c_void)));
    report.check(
        hidden_event && all_hidden,
        format_args!(
            "stop fades out and hides every window ({:.0} ms)",
            stopped_at.elapsed().as_secs_f64() * 1000.0
        ),
    );
    report.check(
        events.contains(&UiEvent::ToolChanged(None)),
        "stop puts the tool down (ToolChanged(None))",
    );

    // 13. Shutdown within its bound.
    let started = Instant::now();
    let ended = ui.shutdown(Duration::from_secs(1));
    let took = started.elapsed();
    let destroyed = ours.iter().all(|&h| {
        // SAFETY: asks whether a handle still names a window.
        !unsafe { IsWindow(Some(HWND(h as *mut c_void))) }.as_bool()
    });
    report.check(
        ended && took < Duration::from_secs(1) && destroyed,
        format_args!(
            "shutdown ended the thread in {:.1} ms and destroyed the windows",
            took.as_secs_f64() * 1000.0
        ),
    );
    report.check(
        !WHEEL_GATE.is_armed()
            && !WHEEL_GATE.is_held()
            && !DRAW_KEYS.load(Relaxed)
            && !CLICK_GATE.is_armed(),
        "after shutdown every gate is released (wheel, drawing keys; click gate untouched)",
    );

    pump();
    report.check(
        !watch.was_foreground(),
        "no window of the recording UI ever became the foreground window",
    );
    drop(watch);
    drop(backdrop);

    // 14. Timing on 3840x2160, at 1x and 2x.
    for scale in [1.0, 2.0] {
        timing(&mut report, &monitor, scale)?;
    }
    report.info(format_args!("{} checks skipped", report.skipped));
    Ok(report.pass)
}

/// The share of pixels (BGRA capture) that are `rgb`, within one level.
fn share_of(capture: &[u8], rgb: [u8; 3]) -> f64 {
    let pixels = capture.as_chunks::<4>().0;
    let same = pixels
        .iter()
        .filter(|p| {
            p[2].abs_diff(rgb[0]) <= 1 && p[1].abs_diff(rgb[1]) <= 1 && p[0].abs_diff(rgb[2]) <= 1
        })
        .count();
    same as f64 / pixels.len().max(1) as f64
}

/// Pumps for `ms` with no UI to drain.
fn settle_plain(ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_until(
    ui: &RecordingUi,
    events: &mut Vec<UiEvent>,
    ms: u64,
    want: impl Fn(&RecordingUi) -> bool,
) -> bool {
    let until = Instant::now() + Duration::from_millis(ms);
    loop {
        pump();
        ui.drain(events);
        if want(ui) {
            return true;
        }
        if Instant::now() > until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn timing(report: &mut Report, monitor: &MonitorBounds, scale: f64) -> Result<()> {
    let bounds = Rect::new(monitor.bounds.x, monitor.bounds.y, 3840.0, 2160.0);
    let big = MonitorBounds {
        bounds,
        work_area: bounds,
        scale,
    };
    let config = RecordingUiConfig {
        inject_mask: false,
        take_mouse: false,
        key_hook: false,
        ..RecordingUiConfig::default()
    };
    let mut events = Vec::new();
    let ui = RecordingUi::start(big, config)?;
    let ink = HWND(ui.windows().ink as *mut c_void);
    ui.tool_shortcut(InkTool::Pen);
    wait_event(&ui, &mut events, 1000, |e| {
        *e == UiEvent::ToolChanged(Some(InkTool::Pen))
    });
    settle(&ui, &mut events, 100);
    let pick_up = ui.ink_stats();
    // The signature-like stroke of `ink_demo`'s timing, in logical px, placed to fit 1080p.
    let point = |i: usize| {
        let t = i as f64;
        let x = 300.0 + t * 1.6;
        let y = 500.0 + (t * 0.05).sin() * 110.0 + (t * 0.013).sin() * 35.0;
        ((x * scale).round() as i32, (y * scale).round() as i32)
    };
    post(ink, WM_LBUTTONDOWN, 1, point(0));
    wait_frames(&ui, 0, 200);
    let warm = 10;
    for i in 1..warm {
        let frames = ui.ink_stats().frames;
        post(ink, WM_MOUSEMOVE, 1, point(i));
        wait_frames(&ui, frames, 200);
    }
    ui.test_reset_stats();
    settle(&ui, &mut events, 20);
    let started = Instant::now();
    for i in warm..400 {
        let frames = ui.ink_stats().frames;
        post(ink, WM_MOUSEMOVE, 1, point(i));
        wait_frames(&ui, frames, 200);
    }
    let wall = started.elapsed().as_secs_f64();
    let stats: InkStats = ui.ink_stats();
    post(ink, WM_LBUTTONUP, 0, point(399));
    let frames = stats.frames;
    wait_frames(&ui, frames, 500);
    let end = ui.ink_stats();
    let mut full = Vec::new();
    for _ in 0..5 {
        ui.test_full_present();
        let deadline = Instant::now() + Duration::from_secs(2);
        while ui.test_full_present_time().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(t) = ui.test_full_present_time() {
            full.push(t);
        }
    }
    let full_avg = full.iter().sum::<f64>() / full.len().max(1) as f64;
    let n = stats.frames.max(1) as f64;
    let ms = |s: f64| s * 1000.0;
    report.info(format_args!(
        "3840x2160 at {scale}x, 390 moves of a 400-point stroke: {} frames, render {:.3} ms, present {:.3} ms (dirty rect, avg {:.0} px), total {:.3} ms per frame; worst frame {:.3} ms (render {:.3}, present {:.3}); wall {:.2} ms per move",
        stats.frames,
        ms(stats.render / n),
        ms(stats.present / n),
        stats.damage_pixels as f64 / n,
        ms((stats.render + stats.present) / n),
        ms(stats.frame_worst),
        ms(stats.render_worst),
        ms(stats.present_worst),
        ms(wall / 390.0),
    ));
    report.info(format_args!(
        "3840x2160 at {scale}x: a full-window present takes {:.2} ms (avg of {}); pick-up frame {:.2} ms; stroke end frame (finished stroke with blurred shadow) {:.2} ms",
        ms(full_avg),
        full.len(),
        ms(pick_up.frame_worst),
        ms(end.frame_worst),
    ));
    report.check(
        stats.frames >= 380 && stats.render / n <= 0.001,
        format_args!("4K at {scale}x: render at or under 1.0 ms per frame while extending the stroke ({:.3} ms)", ms(stats.render / n)),
    );
    let ended = ui.shutdown(Duration::from_secs(1));
    report.check(
        ended && !WHEEL_GATE.is_armed(),
        format_args!("timing run at {scale}x shut down cleanly"),
    );
    Ok(())
}

fn wait_frames(ui: &RecordingUi, frames: u64, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while ui.ink_stats().frames <= frames && Instant::now() < deadline {
        std::thread::yield_now();
    }
}

fn post(hwnd: HWND, message: u32, buttons: usize, (x, y): (i32, i32)) {
    let l = ((y as u32 as usize & 0xFFFF) << 16) | (x as u32 as usize & 0xFFFF);
    // SAFETY: posts a message with no pointers to a window of this process.
    let _ = unsafe { PostMessageW(Some(hwnd), message, WPARAM(buttons), LPARAM(l as isize)) };
}

/// Posts press, moves and release for a stroke through client points.
fn draw(hwnd: HWND, points: &[(i32, i32)]) {
    post(hwnd, WM_LBUTTONDOWN, 1, points[0]);
    for &point in &points[1..] {
        post(hwnd, WM_MOUSEMOVE, 1, point);
    }
    post(hwnd, WM_LBUTTONUP, 0, *points.last().expect("a point"));
}

fn bbox(points: &[(i32, i32)], pad: i32) -> (i32, i32, i32, i32) {
    let min_x = points.iter().map(|p| p.0).min().unwrap_or(0) - pad;
    let min_y = points.iter().map(|p| p.1).min().unwrap_or(0) - pad;
    let max_x = points.iter().map(|p| p.0).max().unwrap_or(0) + pad;
    let max_y = points.iter().map(|p| p.1).max().unwrap_or(0) + pad;
    (min_x, min_y, max_x - min_x, max_y - min_y)
}

/// Pixels (RGB) differing by more than `threshold` in a channel, inside `within` (region
/// coordinates; all when `None`) and outside `except`.
fn count_diff(
    a: &[u8],
    b: &[u8],
    rect: (i32, i32, i32, i32),
    within: Option<(i32, i32, i32, i32)>,
    except: Option<(i32, i32, i32, i32)>,
    threshold: u8,
) -> usize {
    let (w, h) = (rect.2, rect.3);
    let inside = |b: (i32, i32, i32, i32), x: i32, y: i32| {
        x >= b.0 && x < b.0 + b.2 && y >= b.1 && y < b.1 + b.3
    };
    let mut count = 0;
    for y in 0..h {
        for x in 0..w {
            if within.is_some_and(|b| !inside(b, x, y)) || except.is_some_and(|b| inside(b, x, y)) {
                continue;
            }
            let i = ((y * w + x) * 4) as usize;
            if (0..3).any(|c| a[i + c].abs_diff(b[i + c]) > threshold) {
                count += 1;
            }
        }
    }
    count
}

fn ex_style(hwnd: HWND) -> u32 {
    // SAFETY: reads a window long of a window handle.
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

fn affinity(hwnd: HWND) -> u32 {
    let mut affinity = 0;
    // SAFETY: writes into a local.
    let _ = unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) };
    affinity
}

fn flush() {
    // SAFETY: no arguments; waits for the compositor to show the latest changes.
    unsafe {
        let _ = DwmFlush();
        let _ = DwmFlush();
    }
}

fn process_cpu() -> f64 {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the pseudo handle of our own process; writes into locals.
    let _ = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    let ticks =
        |t: FILETIME| (u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)) as f64;
    (ticks(kernel) + ticks(user)) / 1e7
}

fn pump() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a local; this thread owns no windows (WinEvent callbacks only).
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

/// Pumps and collects events for `ms`.
fn settle(ui: &RecordingUi, events: &mut Vec<UiEvent>, ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        pump();
        ui.drain(events);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_event(
    ui: &RecordingUi,
    events: &mut Vec<UiEvent>,
    ms: u64,
    want: impl Fn(&UiEvent) -> bool,
) -> bool {
    let until = Instant::now() + Duration::from_millis(ms);
    loop {
        pump();
        ui.drain(events);
        if events.iter().any(&want) {
            return true;
        }
        if Instant::now() > until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_state(
    ui: &RecordingUi,
    events: &mut Vec<UiEvent>,
    ms: u64,
    want: impl Fn(&igorio_app::recording::ink_overlay::InkState) -> bool,
) -> bool {
    let until = Instant::now() + Duration::from_millis(ms);
    loop {
        pump();
        ui.drain(events);
        if want(&ui.ink_state()) {
            // One more pass for the present to land.
            std::thread::sleep(Duration::from_millis(20));
            return true;
        }
        if Instant::now() > until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

// MARK: - The second process

/// Runs this program again in probe mode; returns the window and its process id.
fn run_probe(point: POINT, ui: &RecordingUi, events: &mut Vec<UiEvent>) -> Result<(isize, u32)> {
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--probe".to_string(),
            point.x.to_string(),
            point.y.to_string(),
        ])
        .stdout(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("the probe process did not finish in 10 s".into());
        }
        pump();
        ui.drain(events);
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output()?;
    let text = String::from_utf8(output.stdout)?;
    let mut parts = text.split_whitespace();
    let hwnd = parts
        .next()
        .ok_or("no window in the probe output")?
        .parse()?;
    let pid = parts.next().ok_or("no pid in the probe output")?.parse()?;
    Ok((hwnd, pid))
}

fn probe(x: &str, y: &str) -> Result<()> {
    ensure_dpi_awareness();
    let point = POINT {
        x: x.parse()?,
        y: y.parse()?,
    };
    // SAFETY: plain value arguments.
    let (root, pid) = unsafe {
        let hit = WindowFromPoint(point);
        let root = GetAncestor(hit, GA_ROOT);
        let root = if root.is_invalid() { hit } else { root };
        let mut pid = 0;
        GetWindowThreadProcessId(root, Some(&mut pid));
        (root, pid)
    };
    println!("{} {pid}", root.0 as isize);
    Ok(())
}

// MARK: - Foreground watch

static WATCHED: [AtomicIsize; 8] = [const { AtomicIsize::new(0) }; 8];
static WAS_FOREGROUND: AtomicBool = AtomicBool::new(false);

struct ForegroundWatch {
    hook: HWINEVENTHOOK,
}

impl ForegroundWatch {
    fn new(windows: &[isize]) -> Result<ForegroundWatch> {
        for (slot, &hwnd) in WATCHED.iter().zip(windows) {
            slot.store(hwnd, Relaxed);
        }
        // SAFETY: out-of-context hook on all processes; the callback only reads atomics.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(foreground_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.is_invalid() {
            return Err("SetWinEventHook failed".into());
        }
        Ok(ForegroundWatch { hook })
    }

    fn was_foreground(&self) -> bool {
        // SAFETY: no arguments.
        let now = unsafe { GetForegroundWindow() };
        is_watched(now.0 as isize) || WAS_FOREGROUND.load(Relaxed)
    }
}

fn is_watched(hwnd: isize) -> bool {
    hwnd != 0 && WATCHED.iter().any(|slot| slot.load(Relaxed) == hwnd)
}

impl Drop for ForegroundWatch {
    fn drop(&mut self) {
        // SAFETY: the hook was set by `new` and is removed once.
        let _ = unsafe { UnhookWinEvent(self.hook) };
    }
}

unsafe extern "system" fn foreground_event(
    _: HWINEVENTHOOK,
    _: u32,
    hwnd: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if is_watched(hwnd.0 as isize) {
        WAS_FOREGROUND.store(true, Relaxed);
    }
}

// MARK: - Screen capture

/// A memory DC with a top-down 32-bit DIB to BitBlt the screen into.
struct Capture {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u8,
    len: usize,
}

impl Capture {
    fn new(width: i32, height: i32) -> Result<Capture> {
        let info = BITMAPINFO {
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
        };
        let mut bits: *mut c_void = ptr::null_mut();
        // SAFETY: locals only; the handles are freed in `Drop`.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err("CreateCompatibleDC failed".into());
            }
            let bitmap = match CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bitmap) => bitmap,
                Err(err) => {
                    let _ = DeleteDC(dc);
                    return Err(err.into());
                }
            };
            let original = SelectObject(dc, bitmap.into());
            Ok(Capture {
                dc,
                bitmap,
                original,
                bits: bits.cast(),
                len: width as usize * height as usize * 4,
            })
        }
    }

    /// The screen at `rect` (x, y, w, h), BGRA.
    fn grab(&mut self, rect: (i32, i32, i32, i32)) -> Result<&[u8]> {
        // SAFETY: the screen DC is released before returning; `bits` is the selected DIB of
        // `len` bytes, read only after GdiFlush.
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err("GetDC for the screen failed".into());
            }
            // CAPTUREBLT includes layered windows: the ink window is captured, the excluded
            // outline and pill are not.
            let copied = BitBlt(
                self.dc,
                0,
                0,
                rect.2,
                rect.3,
                Some(screen),
                rect.0,
                rect.1,
                SRCCOPY | CAPTUREBLT,
            );
            ReleaseDC(None, screen);
            copied?;
            let _ = GdiFlush();
            Ok(std::slice::from_raw_parts(self.bits, self.len))
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: owned handles, freed once, the bitmap after it is selected out.
        unsafe {
            SelectObject(self.dc, self.original);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// MARK: - PNGs

/// One state of the strip for the sheets: content, hovered part, tip.
struct Scene {
    label: &'static str,
    seconds: f64,
    zoom: f64,
    tool: Option<InkTool>,
    hover: Option<Part>,
    tip: Option<Tip>,
}

const SCENES: [Scene; 6] = [
    Scene {
        label: "rest",
        seconds: 42.0,
        zoom: 1.0,
        tool: None,
        hover: None,
        tip: None,
    },
    Scene {
        label: "start hint",
        seconds: 2.0,
        zoom: 1.0,
        tool: None,
        hover: None,
        tip: Some(Tip::StartHint),
    },
    Scene {
        label: "pen selected",
        seconds: 75.0,
        zoom: 1.0,
        tool: Some(InkTool::Pen),
        hover: None,
        tip: Some(Tip::ToolHint),
    },
    Scene {
        label: "zoomed",
        seconds: 75.0,
        zoom: 2.0,
        tool: None,
        hover: None,
        tip: None,
    },
    Scene {
        label: "hover pen",
        seconds: 42.0,
        zoom: 1.0,
        tool: None,
        hover: Some(Part::Pen),
        tip: Some(Tip::Tooltip(Part::Pen)),
    },
    Scene {
        label: "hover zoom",
        seconds: 75.0,
        zoom: 2.5,
        tool: Some(InkTool::Highlighter),
        hover: Some(Part::Zoom),
        tip: Some(Tip::Tooltip(Part::Zoom)),
    },
];

/// Draws one scene: the strip at the top centre of a `width`-wide band, its tip below.
fn scene(fonts: &Fonts, s: &Scene, scale: f64, dark: bool, width: u32) -> Pixmap {
    let band_h = ((frame::PILL_HEIGHT + 60.0) * scale) as u32;
    let mut page = Pixmap::new(width, band_h).expect("band");
    page.fill(backdrop(dark));
    let model = pill_model(s.seconds, s.zoom, s.tool);
    let layout = pill_layout(fonts, &model, scale);
    let mut look = PillLook::default();
    if let Some(part) = s.hover {
        look.hover[Part::ALL.iter().position(|p| *p == part).unwrap_or(0)] = 1.0;
    }
    let pill = render_pill(fonts, &model, &layout, &look, scale, dark);
    let band = Rect::new(0.0, 0.0, f64::from(width), f64::from(band_h));
    let monitor = MonitorBounds {
        bounds: band,
        work_area: band,
        scale,
    };
    let at = pill_origin(&monitor, layout.width, 0);
    page.draw_pixmap(
        at.0,
        at.1,
        pill.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    if let Some(tip) = s.tip {
        let segments = tip_segments(tip, &Labels::default(), ZoomModifier::Alt);
        let tip_pixmap = render_tip(fonts, &segments, scale, dark);
        let strip = frame::Strip {
            x: at.0,
            y: at.1,
            w: layout.width as i32,
            h: layout.height as i32,
        };
        let anchor = match (tip, s.hover.and_then(|p| layout.part(p))) {
            (Tip::Tooltip(_), Some(b)) => at.0 + b.x + b.w / 2,
            _ => at.0 + layout.width as i32 / 2,
        };
        let (x, y) = tip_origin(
            &monitor,
            strip,
            anchor,
            (tip_pixmap.width(), tip_pixmap.height()),
        );
        page.draw_pixmap(
            x,
            y,
            tip_pixmap.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
    let _ = s.label;
    page
}

fn backdrop(dark: bool) -> Color {
    if dark {
        Color::from_rgba8(0x1b, 0x1f, 0x27, 255)
    } else {
        Color::from_rgba8(0xdf, 0xe6, 0xee, 255)
    }
}

/// Renders the strip's states, the outline and the ink off screen into
/// `crates/app/examples/`.
fn pngs() -> Result<()> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let fonts = Fonts::new();
    for (dark, theme) in [(false, "light"), (true, "dark")] {
        for (scale, percent) in [(1.0, 100), (1.25, 125)] {
            let width = (420.0 * scale) as u32;
            let bands: Vec<Pixmap> = SCENES
                .iter()
                .map(|s| scene(&fonts, s, scale, dark, width))
                .collect();
            let height = bands.iter().map(Pixmap::height).sum();
            let mut sheet = Pixmap::new(width, height).ok_or("sheet")?;
            let mut y = 0;
            for band in &bands {
                sheet.draw_pixmap(
                    0,
                    y,
                    band.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
                y += band.height() as i32;
            }
            save(
                &dir.join(format!("recording-strip-{theme}-{percent}.png")),
                &sheet,
            )?;
        }
    }

    // The outline around a 2x zoom on a 1280x720 "screen", with the strip.
    let mut screen = text_page(1280, 720);
    let screen_rect = Rect::new(0.0, 0.0, 1280.0, 720.0);
    let area = Rect::new(420.0, 230.0, 640.0, 360.0);
    if let Some(outline) = outline_rect(area, 1.0) {
        frame::draw_outline(&mut screen, outline, (0, 0), 1.0);
    }
    let model = pill_model(42.0, 2.0, None);
    let layout = pill_layout(&fonts, &model, 1.0);
    let pill = render_pill(&fonts, &model, &layout, &PillLook::default(), 1.0, false);
    let at = pill_origin(
        &MonitorBounds {
            bounds: screen_rect,
            work_area: screen_rect,
            scale: 1.0,
        },
        layout.width,
        0,
    );
    screen.draw_pixmap(
        at.0,
        at.1,
        pill.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    save(&dir.join("recording-outline-zoomed.png"), &screen)?;

    // Pen and highlighter over text, with each cursor at the end of the stroke.
    for tool in [InkTool::Pen, InkTool::Highlighter] {
        let mut page = text_page(800, 450);
        let mut ink = Ink::new();
        ink.press(match tool {
            InkTool::Pen => InkKey::Pen,
            InkTool::Highlighter => InkKey::Highlighter,
        });
        let points: Vec<Point> = match tool {
            InkTool::Pen => (0..160)
                .map(|i| {
                    let t = f64::from(i) / 159.0;
                    Point::new(
                        120.0 + 520.0 * t + 40.0 * (t * 9.0).sin(),
                        250.0 + (t * 14.0).sin() * 50.0 * (1.0 - 0.3 * t),
                    )
                })
                .collect(),
            InkTool::Highlighter => (0..120)
                .map(|i| Point::new(78.0 + f64::from(i) * 4.5, 103.0 + f64::from(i) * 0.02))
                .collect(),
        };
        ink.begin_stroke(points[0]);
        for point in &points[1..] {
            ink.extend_stroke(*point);
        }
        ink.end_stroke();
        let mut layer = Pixmap::new(800, 450).ok_or("layer")?;
        InkRenderer::new().render(&ink, 1.0, 1.0, &mut layer);
        page.draw_pixmap(
            0,
            0,
            layer.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        let (cursor, hot) = tool_cursor(tool, 1.0);
        let end = points.last().copied().unwrap_or(Point::ZERO);
        page.draw_pixmap(
            end.x as i32 - hot.0 as i32 + 30,
            end.y as i32 - hot.1 as i32 + 20,
            cursor.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        save(
            &dir.join(format!("recording-ink-{}.png", tool.as_str())),
            &page,
        )?;
    }
    Ok(())
}

fn text_page(width: u32, height: u32) -> Pixmap {
    let mut page = Pixmap::new(width, height).expect("page");
    page.fill(Color::from_rgba8(250, 249, 246, 255));
    let mut paint = Paint::default();
    paint.set_color_rgba8(32, 36, 44, 255);
    let rect = |x: f32, y: f32, w: f32, h: f32| tiny_skia::Rect::from_xywh(x, y, w, h);
    if let Some(r) = rect(80.0, 58.0, 260.0, 12.0) {
        page.fill_rect(r, &paint, Transform::identity(), None);
    }
    paint.set_color_rgba8(110, 116, 128, 255);
    let mut y = 96.0;
    let mut i = 0;
    while y < height as f32 - 40.0 {
        let w = (width as f32 - 170.0) * (0.55 + 0.45 * ((i * 37 % 10) as f32 / 10.0));
        if let Some(r) = rect(80.0, y, w, 6.0) {
            page.fill_rect(r, &paint, Transform::identity(), None);
        }
        y += 24.0;
        i += 1;
    }
    page
}

fn save(path: &Path, page: &Pixmap) -> Result<()> {
    let mut data = page.data().to_vec();
    // Premultiplied to straight alpha.
    for pixel in data.as_chunks_mut::<4>().0 {
        let a = u32::from(pixel[3]);
        if a > 0 && a < 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), page.width(), page.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&data)?;
    println!("wrote {}", path.display());
    Ok(())
}

unsafe extern "system" fn backdrop_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // SAFETY: the arguments come unchanged from the system.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
