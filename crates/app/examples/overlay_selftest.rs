//! Self-check for `igorio_app::overlay`. Shows the overlay with a test image for about
//! three seconds, following the cursor, then prints PASS or FAIL per check.
//! Exits 0 only if every check passes.
//!
//! `overlay_selftest --probe X Y HWND` is the helper mode: run as a separate process, it
//! reports what `WindowFromPoint` and UI Automation find at (X, Y).

use std::ffi::c_void;
use std::fmt::Display;
use std::mem::size_of;
use std::process::{Command, ExitCode, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering::Relaxed};
use std::time::{Duration, Instant};

use igorio_app::layout::PANEL_SIZE;
use igorio_app::overlay::{Overlay, cursor_pos, ensure_dpi_awareness, monitor_at};
use tiny_skia::{Color, Paint, Pixmap, Transform};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC,
    HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Threading::{GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, HWINEVENTHOOK, IUIAutomation, SetWinEventHook, UnhookWinEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GWL_EXSTYLE, GetForegroundWindow,
    GetWindowDisplayAffinity, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW,
    QS_ALLINPUT, SetWindowLongPtrW, WDA_EXCLUDEFROMCAPTURE, WINEVENT_OUTOFCONTEXT,
    WS_EX_TRANSPARENT, WindowFromPoint,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [flag, x, y, hwnd] if flag == "--probe" => probe(x, y, hwnd).map(|()| true),
        [] => run(),
        _ => Err("usage: overlay_selftest [--probe X Y HWND]".into()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("overlay_selftest: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool> {
    ensure_dpi_awareness();
    let mut overlay = Overlay::new()?;
    let hwnd = overlay.hwnd();
    let watch = ForegroundWatch::new(hwnd)?;

    // Follow the cursor for three seconds with exponential smoothing.
    let mut scale = 0.0;
    let mut pixmap = test_image(1, 1)?;
    let mut position: Option<(f64, f64)> = None;
    let started = Instant::now();
    let mut last = started;
    while started.elapsed() < Duration::from_secs(3) {
        pump_messages();
        watch.sample();
        let now = Instant::now();
        let dt = (now - last).as_secs_f64().min(0.05);
        last = now;

        let cursor = cursor_pos();
        let monitor = monitor_at(cursor);
        let (width, height) = panel_px(monitor.scale);
        let gap = (14.0 * monitor.scale, 18.0 * monitor.scale);
        let work = monitor.work_area;
        let axis = |cursor: f64, gap: f64, size: f64, start: f64, end: f64| {
            let forward = cursor + gap;
            let origin = if forward + size > end { cursor - gap - size } else { forward };
            origin.clamp(start, (end - size).max(start))
        };
        let target = (
            axis(cursor.x, gap.0, f64::from(width), work.min_x(), work.max_x()),
            axis(cursor.y, gap.1, f64::from(height), work.min_y(), work.max_y()),
        );
        let (x, y) = position.unwrap_or(target);
        let alpha = 1.0 - (-16.0 * dt).exp();
        let next = (x + (target.0 - x) * alpha, y + (target.1 - y) * alpha);
        position = Some(next);
        let origin = (next.0.round() as i32, next.1.round() as i32);

        if monitor.scale != scale {
            scale = monitor.scale;
            pixmap = test_image(width, height)?;
            overlay.present(&pixmap, origin)?;
            overlay.show();
        } else {
            overlay.move_to(origin)?;
        }
        if overlay.take_display_changed() {
            overlay.reassert();
        }
        wait_for_messages(8);
    }

    let mut pass = true;
    let rect = window_rect(hwnd)?;
    let center = POINT {
        x: (rect.left + rect.right) / 2,
        y: (rect.top + rect.bottom) / 2,
    };

    // a. Click-through, seen from another process. The control run removes
    // WS_EX_TRANSPARENT to show that the probe does find the overlay when it should.
    let normal = run_probe(center, hwnd)?;
    // SAFETY: our own window; the style is restored right after the control probe.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !(WS_EX_TRANSPARENT.0 as isize)) };
    let control = run_probe(center, hwnd);
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style) };
    let control = control?;
    let own_pid = std::process::id();
    pass &= report(
        !normal.hit_overlay && normal.window_pid != own_pid && control.hit_overlay,
        format_args!(
            "a. click-through from another process (WindowFromPoint at {},{}: overlay hit {}, pid {}; control without WS_EX_TRANSPARENT: overlay hit {})",
            center.x, center.y, normal.hit_overlay, normal.window_pid, control.hit_overlay
        ),
    );
    pass &= report(
        normal.uia_pid.is_some_and(|pid| pid != own_pid),
        format_args!(
            "a. UI Automation ElementFromPoint from another process is not us (element pid {:?}, ours {own_pid})",
            normal.uia_pid
        ),
    );

    // d. Display affinity.
    let mut affinity = 0;
    // SAFETY: writes into a local.
    let affinity_result = unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) };
    pass &= report(
        affinity_result.is_ok() && affinity == WDA_EXCLUDEFROMCAPTURE.0,
        format_args!("d. GetWindowDisplayAffinity returned 0x{affinity:02X}"),
    );

    // c. Capture exclusion.
    pass &= match compare_captures(&mut overlay, rect) {
        Ok((excluded, included, tolerance)) => report(
            excluded <= tolerance && included > tolerance,
            format_args!(
                "c. BitBlt capture ({}x{}: {excluded} differing pixels excluded, {included} capturable, tolerance {tolerance})",
                rect.right - rect.left,
                rect.bottom - rect.top
            ),
        ),
        Err(err) => report(false, format_args!("c. BitBlt capture ({err})")),
    };
    overlay.show();

    // e. Size change, as after a DPI change.
    let origin = (rect.left, rect.top);
    let mut sizes_ok = true;
    let mut seen = Vec::new();
    for (width, height) in [(200, 170), (300, 255)] {
        overlay.present(&test_image(width, height)?, origin)?;
        let got = window_rect(hwnd)?;
        let want = RECT {
            left: origin.0,
            top: origin.1,
            right: origin.0 + width as i32,
            bottom: origin.1 + height as i32,
        };
        sizes_ok &= got == want;
        seen.push(format!(
            "{}x{} at {},{}",
            got.right - got.left,
            got.bottom - got.top,
            got.left,
            got.top
        ));
    }
    pass &= report(
        sizes_ok,
        format_args!("e. present with 200x170 then 300x255 (window: {})", seen.join(", ")),
    );

    // f, g. Many presents and moves: no GDI growth, average time.
    let (width, height) = (pixmap.width(), pixmap.height());
    let mut other = test_image(width, height)?;
    other.fill(Color::from_rgba8(40, 120, 200, 255));
    overlay.present(&pixmap, origin)?;
    let gdi_before = gdi_objects();
    let started = Instant::now();
    for i in 0..2000 {
        overlay.present(if i % 2 == 0 { &other } else { &pixmap }, origin)?;
    }
    let present_avg = started.elapsed() / 2000;
    let gdi_after = gdi_objects();
    pass &= report(
        gdi_after <= gdi_before + 3,
        format_args!("f. GDI objects before 2000 presents {gdi_before}, after {gdi_after}"),
    );
    let started = Instant::now();
    for i in 0..2000 {
        overlay.move_to((origin.0 + i % 2, origin.1))?;
    }
    let move_avg = started.elapsed() / 2000;
    pass &= report(
        true,
        format_args!(
            "g. average present {:.1} us ({width}x{height}), move_to {:.1} us",
            present_avg.as_secs_f64() * 1e6,
            move_avg.as_secs_f64() * 1e6
        ),
    );

    // b. Foreground, sampled during the whole run and checked last.
    pump_messages();
    watch.sample();
    pass &= report(
        !WAS_FOREGROUND.load(Relaxed),
        "b. overlay never became the foreground window",
    );

    overlay.hide();
    Ok(pass)
}

/// A card that is opaque in the middle, with a translucent border around it.
fn test_image(width: u32, height: u32) -> Result<Pixmap> {
    let mut pixmap = Pixmap::new(width, height).ok_or("could not allocate the pixmap")?;
    let mut paint = Paint::default();
    let (w, h) = (width as f32, height as f32);
    paint.set_color_rgba8(25, 20, 43, 90);
    if let Some(rect) = tiny_skia::Rect::from_xywh(w * 0.05, h * 0.05, w * 0.9, h * 0.9) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
    paint.set_color_rgba8(255, 244, 224, 255);
    if let Some(rect) = tiny_skia::Rect::from_xywh(w * 0.15, h * 0.15, w * 0.7, h * 0.7) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
    paint.set_color_rgba8(216, 137, 87, 255);
    if let Some(rect) = tiny_skia::Rect::from_xywh(w * 0.22, h * 0.22, w * 0.56, h * 0.4) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
    Ok(pixmap)
}

fn panel_px(scale: f64) -> (u32, u32) {
    (
        (PANEL_SIZE.width * scale).round() as u32,
        (PANEL_SIZE.height * scale).round() as u32,
    )
}

struct ProbeResult {
    hit_overlay: bool,
    window_pid: u32,
    uia_pid: Option<u32>,
}

/// Runs this program again in probe mode. Pumps messages while it waits, because UI
/// Automation in the child may send window messages to our thread.
fn run_probe(point: POINT, hwnd: HWND) -> Result<ProbeResult> {
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--probe".to_string(),
            point.x.to_string(),
            point.y.to_string(),
            (hwnd.0 as isize).to_string(),
        ])
        .stdout(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("the probe process did not finish in 10 s".into());
        }
        pump_messages();
        wait_for_messages(5);
    }
    let output = child.wait_with_output()?;
    let text = String::from_utf8(output.stdout)?;
    let field = |name: &str| {
        text.split_whitespace()
            .find_map(|part| part.strip_prefix(name)?.strip_prefix('='))
            .ok_or_else(|| format!("probe output has no {name}: {text:?}"))
    };
    Ok(ProbeResult {
        hit_overlay: field("hit_overlay")? == "true",
        window_pid: field("window_pid")?.parse()?,
        uia_pid: field("uia_pid")?.parse().ok(),
    })
}

/// Probe mode: what other processes see at (x, y).
fn probe(x: &str, y: &str, hwnd: &str) -> Result<()> {
    ensure_dpi_awareness();
    let point = POINT {
        x: x.parse()?,
        y: y.parse()?,
    };
    let overlay = hwnd.parse::<isize>()?;
    // SAFETY: plain value argument.
    let hit = unsafe { WindowFromPoint(point) };
    let mut window_pid = 0;
    // SAFETY: writes into a local.
    unsafe { GetWindowThreadProcessId(hit, Some(&mut window_pid)) };
    let uia_pid = match uia_process_at(point) {
        Ok(pid) => pid.to_string(),
        Err(err) => format!("error:{}", err.to_string().replace(' ', "_")),
    };
    println!(
        "hit_overlay={} window_pid={window_pid} uia_pid={uia_pid}",
        hit.0 as isize == overlay
    );
    Ok(())
}

fn uia_process_at(point: POINT) -> windows::core::Result<u32> {
    // SAFETY: COM is initialized once for this short-lived probe thread and never
    // uninitialized; the interface pointers drop before the process exits.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let element = automation.ElementFromPoint(point)?;
        Ok(element.CurrentProcessId()? as u32)
    }
}

/// Captures the overlay rectangle shown and hidden, with exclusion on and then off.
/// Returns (differing pixels excluded, differing pixels capturable, tolerance).
fn compare_captures(overlay: &mut Overlay, rect: RECT) -> Result<(usize, usize, usize)> {
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    let mut capture = Capture::new(width, height)?;
    let mut compare_pair = |overlay: &mut Overlay| -> Result<usize> {
        overlay.show();
        // SAFETY: no arguments; waits for the compositor to show the change.
        unsafe { DwmFlush()? };
        let shown = capture.grab(rect)?.to_vec();
        overlay.hide();
        unsafe { DwmFlush()? };
        let hidden = capture.grab(rect)?;
        // The fourth byte of a screen BitBlt is unspecified. Compare RGB only.
        Ok(shown
            .as_chunks::<4>()
            .0
            .iter()
            .zip(hidden.as_chunks::<4>().0)
            .filter(|(a, b)| a[..3] != b[..3])
            .count())
    };
    // The desktop under the overlay may be changing. Keep the quietest of three tries.
    let mut excluded = compare_pair(overlay)?;
    for _ in 0..2 {
        if excluded == 0 {
            break;
        }
        excluded = excluded.min(compare_pair(overlay)?);
    }
    overlay.set_capturable(true)?;
    let included = compare_pair(overlay);
    overlay.set_capturable(false)?;
    // Allow 0.1% of the region to change between captures on a live desktop.
    let tolerance = width as usize * height as usize / 1000;
    Ok((excluded, included?, tolerance))
}

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

    fn grab(&mut self, rect: RECT) -> Result<&[u8]> {
        // SAFETY: the screen DC is released before returning; `bits` is the selected DIB
        // of `len` bytes, read only after GdiFlush.
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err("GetDC for the screen failed".into());
            }
            // CAPTUREBLT includes layered windows, so the capturable run does see the overlay.
            let copied = BitBlt(
                self.dc,
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                Some(screen),
                rect.left,
                rect.top,
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

static TARGET: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static WAS_FOREGROUND: AtomicBool = AtomicBool::new(false);

/// Records whether the overlay ever became the foreground window.
struct ForegroundWatch {
    hwnd: HWND,
    hook: HWINEVENTHOOK,
}

impl ForegroundWatch {
    fn new(hwnd: HWND) -> Result<ForegroundWatch> {
        TARGET.store(hwnd.0, Relaxed);
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
        Ok(ForegroundWatch { hwnd, hook })
    }

    fn sample(&self) {
        // SAFETY: no arguments.
        if unsafe { GetForegroundWindow() } == self.hwnd {
            WAS_FOREGROUND.store(true, Relaxed);
        }
    }
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
    if hwnd.0 == TARGET.load(Relaxed) {
        WAS_FOREGROUND.store(true, Relaxed);
    }
}

fn window_rect(hwnd: HWND) -> Result<RECT> {
    let mut rect = RECT::default();
    // SAFETY: writes into a local.
    unsafe { GetWindowRect(hwnd, &mut rect)? };
    Ok(rect)
}

fn gdi_objects() -> u32 {
    // SAFETY: the pseudo handle of our own process.
    unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) }
}

fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a local; dispatch goes to our own window procedures.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

fn wait_for_messages(ms: u32) {
    // SAFETY: no handles; returns on timeout or when input arrives.
    unsafe { MsgWaitForMultipleObjectsEx(None, ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
}

fn report(pass: bool, description: impl Display) -> bool {
    println!("{} {description}", if pass { "PASS" } else { "FAIL" });
    pass
}
