//! Self-check for the recording UI (`igorio_app::recording`): the ink window, the outline
//! and the pill, and the input gates. Non-interactive; prints PASS, FAIL or INFO per check
//! and exits 0 only if every hard check passed.
//!
//! It sends no keyboard or mouse input and never moves the cursor. The ink is drawn with
//! messages posted to our own ink window, the path real mouse messages take. To stay out of
//! the way of a person using the desktop:
//! - the "monitor" of the main run is a small region of the real one, so the ink window, the
//!   outline and the pill only cover that region, for a few seconds;
//! - a tool is in hand (the region takes the mouse, Esc, Backspace, Delete and Ctrl+Z are
//!   swallowed) for about a second;
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
    self, Fonts, Labels, outline_rect, outline_strips, pill_origin, pill_text, render_pill,
};
use igorio_app::recording::ink_overlay::{InkStats, RecordingUi, RecordingUiConfig, UiEvent};
use igorio_app::recording::input::{DRAW_KEYS, ZoomModifier};
use igorio_core::geom::{Point, Rect};
use igorio_core::ink::{Ink, InkKey, InkTool};
use tiny_skia::{Color, Paint, Pixmap, PixmapPaint, Transform};
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GA_ROOT, GWL_EXSTYLE, GetAncestor,
    GetForegroundWindow, GetWindowDisplayAffinity, GetWindowLongPtrW, GetWindowThreadProcessId,
    IsWindow, IsWindowVisible, MSG, PM_REMOVE, PeekMessageW, PostMessageW, WDA_EXCLUDEFROMCAPTURE,
    WDA_NONE, WINEVENT_OUTOFCONTEXT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WindowFromPoint,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [flag, x, y] if flag == "--probe" => probe(x, y).map(|()| true),
        [flag] if flag == "--pngs" => pngs().map(|()| true),
        [] => run(),
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
}

/// The physical-pixel rectangle of the main run: a region of the monitor under the cursor.
fn region(monitor: &MonitorBounds) -> MonitorBounds {
    let s = monitor.scale;
    let work = monitor.work_area;
    let width = (720.0 * s).round().min(work.width - 80.0 * s);
    let height = (400.0 * s).round().min(work.height - 160.0 * s);
    let bounds = Rect::new(
        (work.x + 80.0 * s).round(),
        (work.y + 120.0 * s).round(),
        width,
        height,
    );
    MonitorBounds {
        bounds,
        work_area: bounds,
        scale: s,
    }
}

fn run() -> Result<bool> {
    ensure_dpi_awareness();
    let mut report = Report { pass: true };
    let monitor = monitor_bounds_at(cursor_pos());
    let fake = region(&monitor);
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

    let config = RecordingUiConfig {
        inject_mask: false,
        debug: false,
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
    let mut frame_ok = windows.frame.len() == 5;
    let mut frame_text = Vec::new();
    for &hwnd in &windows.frame {
        let hwnd = HWND(hwnd as *mut c_void);
        let style = ex_style(hwnd);
        let affinity = affinity(hwnd);
        // SAFETY: reads a flag of a window handle.
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        frame_ok &= style & (base | WS_EX_TRANSPARENT.0) == base | WS_EX_TRANSPARENT.0
            && affinity == WDA_EXCLUDEFROMCAPTURE.0
            && visible;
        frame_text.push(format!(
            "0x{style:08X}/{affinity:02X}/{}",
            if visible { "shown" } else { "hidden" }
        ));
    }
    report.check(
        frame_ok,
        format_args!(
            "outline strips and pill: layered, click-through, topmost, no-activate, tool windows, excluded from capture, shown ({})",
            frame_text.join(" ")
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

    // 2. Baseline capture of the region (the outline and the pill are up, excluded).
    let rect = (r.x as i32, r.y as i32, r.width as i32, r.height as i32);
    let mut capture = Capture::new(rect.2, rect.3)?;
    flush();
    let baseline = capture.grab(rect)?.to_vec();

    // 3. Click-through from another process with no tool.
    let spot = POINT {
        x: rect.0 + rect.2 / 2,
        y: rect.1 + rect.3 * 3 / 4,
    };
    let own_pid = std::process::id();
    let before = run_probe(spot, &ui, &mut events)?;
    report.check(
        before.0 != windows.ink && before.1 != own_pid,
        format_args!("no tool: a second process finds the window below at {},{} (window 0x{:X}, pid {}), not the ink window", spot.x, spot.y, before.0, before.1),
    );

    // 4. Pen in hand: the ink window takes the point, also where there is no ink.
    ui.tool_shortcut(InkTool::Pen);
    let picked = wait_event(&ui, &mut events, 1000, |e| {
        *e == UiEvent::ToolChanged(Some(InkTool::Pen))
    });
    report.check(picked, "pen shortcut picks the pen up (ToolChanged event)");
    settle(&ui, &mut events, 60);
    let style = ex_style(ink);
    let after = run_probe(spot, &ui, &mut events)?;
    report.check(
        style & WS_EX_TRANSPARENT.0 == 0 && after.0 == windows.ink && after.1 == own_pid,
        format_args!("pen in hand: the second process finds the ink window at {},{} (window 0x{:X}, ours 0x{:X}), with no ink there", spot.x, spot.y, after.0, windows.ink),
    );
    report.check(
        DRAW_KEYS.load(Relaxed),
        "drawing keys taken while the pen is in hand",
    );

    // 5. Draw a stroke with posted messages, in the middle of the region.
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
    flush();
    let with_ink = capture.grab(rect)?.to_vec();
    let ink_box = bbox(&line, (20.0 * scale) as i32);
    let ink_pixels = count_diff(&baseline, &with_ink, rect, Some(ink_box), None, 40);
    report.check(
        drawn && ink_pixels > (100.0 * scale) as usize,
        format_args!("the stroke is on screen and in a screen capture ({ink_pixels} pixels changed around it)"),
    );

    // 6. The outline and the pill are not in the capture; the control run shows they are
    //    on screen.
    let strips = outline_strips(area, r, scale);
    let fonts = Fonts::new();
    let pill = render_pill(
        &fonts,
        &pill_text(
            1.0,
            Some(InkTool::Pen),
            &Labels::default(),
            ZoomModifier::Alt,
        ),
        scale,
        frame::system_dark(),
    );
    let pill_at = pill_origin(&fake, (pill.width(), pill.height()));
    let mut frame_boxes: Vec<(i32, i32, i32, i32)> = strips
        .iter()
        .flatten()
        .map(|s| (s.x - rect.0, s.y - rect.1, s.w, s.h))
        .collect();
    frame_boxes.push((
        pill_at.0 - rect.0,
        pill_at.1 - rect.1,
        pill.width() as i32,
        pill.height() as i32,
    ));
    let frame_area: usize = frame_boxes.iter().map(|b| (b.2 * b.3) as usize).sum();
    let tolerance = frame_area / 1000 + 4;
    let hidden = frame_boxes
        .iter()
        .map(|b| count_diff(&baseline, &with_ink, rect, Some(*b), Some(ink_box), 3))
        .sum::<usize>();
    ui.test_set_capturable(true);
    settle(&ui, &mut events, 60);
    flush();
    let control = capture.grab(rect)?.to_vec();
    ui.test_set_capturable(false);
    let shown = frame_boxes
        .iter()
        .map(|b| count_diff(&baseline, &control, rect, Some(*b), Some(ink_box), 3))
        .sum::<usize>();
    report.check(
        hidden <= tolerance && shown > tolerance * 4,
        format_args!("outline and pill not in the capture: {hidden} pixels differ under them (tolerance {tolerance}); control with the exclusion off: {shown}"),
    );
    settle(&ui, &mut events, 60);

    // 7. Undo, clear, put down.
    ui.ink_key(InkKey::Undo);
    let undone = wait_state(&ui, &mut events, 1000, |s| s.strokes == 0 && !s.content);
    flush();
    let after_undo = capture.grab(rect)?.to_vec();
    let left = count_diff(&baseline, &after_undo, rect, Some(ink_box), None, 3);
    report.check(
        undone && left <= 4,
        format_args!("undo takes the stroke back ({left} pixels still differ)"),
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
    flush();
    let after_clear = capture.grab(rect)?.to_vec();
    let both = bbox(
        &[line.clone(), second.clone()].concat(),
        (20.0 * scale) as i32,
    );
    let left = count_diff(&baseline, &after_clear, rect, Some(both), None, 3);
    let state = ui.ink_state();
    report.check(
        two && cleared && left <= 4 && state.tool == Some(InkTool::Pen),
        format_args!("clear wipes both strokes and keeps the pen ({left} pixels still differ)"),
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
    flush();
    let after_fade = capture.grab(rect)?.to_vec();
    // Where the stroke was: the capture is back to the baseline. (The rest of the region is
    // whatever the person has on screen there and may change by itself; it is reported.)
    let left = count_diff(&baseline, &after_fade, rect, Some(ink_box), None, 3);
    let elsewhere = count_diff(&baseline, &after_fade, rect, None, Some(ink_box), 3);
    let animated = frame::animation_effects();
    report.check(
        faded && left <= 4 && (!animated || lowest < 200),
        format_args!(
            "the ink fades (lowest alpha seen {lowest}) and ends fully transparent and hidden after {:.0} ms ({left} pixels differ where the stroke was)",
            fade_time * 1000.0
        ),
    );
    report.info(format_args!(
        "after the fade, {elsewhere} pixels of the rest of the region differ from the baseline (the desktop under it)"
    ));
    let gone = run_probe(spot, &ui, &mut events)?;
    report.check(
        gone.0 != windows.ink,
        "after put-down a second process no longer finds the ink window",
    );

    // 8. Idle: nothing moving, no timer but the pill's.
    settle(&ui, &mut events, 300);
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
        format_args!("idle for 3 s: the UI thread woke {woke} times (the pill's once-a-second update), process CPU {:.1} ms", spent * 1000.0),
    );

    // 9. Stop: gates at once, then the fade, then everything hidden.
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
    let all_hidden = ours.iter().all(|&h| {
        // SAFETY: reads a flag of a window handle.
        !unsafe { IsWindowVisible(HWND(h as *mut c_void)) }.as_bool()
    });
    report.check(
        hidden_event && all_hidden,
        format_args!(
            "stop fades out and hides every window ({:.0} ms)",
            stopped_at.elapsed().as_secs_f64() * 1000.0
        ),
    );
    let followed_off = events.contains(&UiEvent::ToolChanged(None));
    report.check(followed_off, "stop puts the tool down (ToolChanged(None))");

    // 10. Shutdown within its bound.
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

    // 11. Timing on 3840x2160, at 1x and 2x.
    for scale in [1.0, 2.0] {
        timing(&mut report, &monitor, scale)?;
    }
    Ok(report.pass)
}

/// Draws a scripted 400-point stroke on a 3840x2160 ink window, one posted move at a time,
/// and reports the frame times.
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

static WATCHED: [AtomicIsize; 6] = [const { AtomicIsize::new(0) }; 6];
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

/// Renders the pill, the outline and the ink off screen into `crates/app/examples/`.
fn pngs() -> Result<()> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let fonts = Fonts::new();
    let labels = Labels::default();
    for (dark, theme) in [(false, "light"), (true, "dark")] {
        for (scale, percent) in [(1.0, 100), (1.25, 125)] {
            let text = pill_text(42.0, None, &labels, ZoomModifier::Alt);
            let pill = render_pill(&fonts, &text, scale, dark);
            let page = on_desktop(&pill, dark, 24);
            save(
                &dir.join(format!("recording-pill-{theme}-{percent}.png")),
                &page,
            )?;
        }
    }
    let pen = pill_text(75.0, Some(InkTool::Pen), &labels, ZoomModifier::Alt);
    save(
        &dir.join("recording-pill-pen-light-100.png"),
        &on_desktop(&render_pill(&fonts, &pen, 1.0, false), false, 24),
    )?;

    // The outline around a 2x zoom on a 1280x720 "screen", with the pill.
    let mut screen = text_page(1280, 720);
    let screen_rect = Rect::new(0.0, 0.0, 1280.0, 720.0);
    let area = Rect::new(420.0, 230.0, 640.0, 360.0);
    if let Some(outline) = outline_rect(area, 1.0) {
        frame::draw_outline(&mut screen, outline, (0, 0), 1.0);
    }
    let pill = render_pill(
        &fonts,
        &pill_text(42.0, None, &labels, ZoomModifier::Alt),
        1.0,
        false,
    );
    let at = pill_origin(
        &MonitorBounds {
            bounds: screen_rect,
            work_area: screen_rect,
            scale: 1.0,
        },
        (pill.width(), pill.height()),
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

/// The pill on a plain desktop-like backdrop, with `pad` pixels around it.
fn on_desktop(pill: &Pixmap, dark: bool, pad: u32) -> Pixmap {
    let mut page = Pixmap::new(pill.width() + 2 * pad, pill.height() + 2 * pad).expect("page");
    page.fill(if dark {
        Color::from_rgba8(0x1b, 0x1f, 0x27, 255)
    } else {
        Color::from_rgba8(0xdf, 0xe6, 0xee, 255)
    });
    page.draw_pixmap(
        pad as i32,
        pad as i32,
        pill.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    page
}

/// A light page with a heading and lines of "text".
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
