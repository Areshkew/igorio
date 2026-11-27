//! Non-interactive self-check of `igorio_app::notes::summoner`. Sends no input, moves
//! nothing, launches nothing. Shows the tool menu (two rows) at the cursor for about three
//! seconds, without the gesture and without arming the click gate. Prints PASS or FAIL per
//! check; exits 0 only if all pass.
//!
//! Checks of the menu window: its styles (layered, click-through, topmost, no-activate, tool
//! window), that the first row is under the cursor and the card inside the work area, that
//! it is excluded from screen capture, that the whole window is click-through seen from
//! another process (the click gate in the mouse hook takes the click on a row, never the
//! window), that it never becomes the foreground window, and that it costs no CPU while
//! hidden or shown and still. Whether the foreground window changed while the menu showed is
//! printed as INFO (the person at the desktop may switch windows meanwhile).
//!
//! What it cannot check: the hook-level swallow of a click, and the gesture itself (both
//! need real input). A message posted to our own window does not pass through the low-level
//! mouse hook, so check (b) only shows that our own window receives every posted click while
//! the gate is off; the swallow is covered by the unit tests of `hook::ClickGate`, the
//! gesture by the unit tests of `notes::summoner` and by `SUMMON_TEST.md`.
//!
//! `summon_selftest --probe X Y HWND` is the helper mode: run as a separate process, it
//! reports whether `WindowFromPoint` at (X, Y) finds the window HWND.

use std::fmt::Display;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use igorio_app::hook::CLICK_GATE;
use igorio_app::notes::summoner::{Bounds, SummonConfig, Summoner, ToolId};
use igorio_app::overlay::{cursor_pos, ensure_dpi_awareness, monitor_at};
use igorio_app::uia::Uia;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GA_ROOT, GWL_EXSTYLE,
    GetAncestor, GetForegroundWindow, GetWindowDisplayAffinity, GetWindowLongPtrW, GetWindowRect,
    IsWindowVisible, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, PostMessageW, QS_ALLINPUT, RegisterClassW, SetWindowLongPtrW,
    WDA_EXCLUDEFROMCAPTURE, WM_LBUTTONDOWN, WM_LBUTTONUP, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_OVERLAPPED,
    WindowFromPoint,
};
use windows::core::w;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Test clicks posted to our own window.
const CLICKS: u32 = 20;
/// Most CPU the idle summoner may use, as a share of one core.
const IDLE_CPU: f64 = 0.01;

static DOWNS: AtomicU32 = AtomicU32::new(0);
static UPS: AtomicU32 = AtomicU32::new(0);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [flag, x, y, hwnd] if flag == "--probe" => probe(x, y, hwnd).map(|()| true),
        [] => run(),
        _ => Err("usage: summon_selftest [--probe X Y HWND]".into()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("summon_selftest: {err}");
            ExitCode::FAILURE
        }
    }
}

fn report(ok: bool, what: impl Display) -> bool {
    println!("{} {what}", if ok { "PASS" } else { "FAIL" });
    ok
}

fn run() -> Result<bool> {
    ensure_dpi_awareness();
    let mut pass = true;
    let since = Instant::now();

    // a. Start.
    let started = Instant::now();
    let uia = Uia::start()?;
    let mut summoner = Summoner::start(SummonConfig::default(), uia.requester()?)?;
    pass &= report(
        summoner.is_alive() && !CLICK_GATE.is_armed(),
        format_args!(
            "a. starts in {} ms with its keyboard hook set and the click gate off",
            started.elapsed().as_millis()
        ),
    );

    // b. Gate off: every posted click reaches our own test window.
    let window = test_window()?;
    for _ in 0..CLICKS {
        // SAFETY: our own window, value arguments only.
        unsafe {
            PostMessageW(Some(window), WM_LBUTTONDOWN, WPARAM(1), LPARAM(0))?;
            PostMessageW(Some(window), WM_LBUTTONUP, WPARAM(0), LPARAM(0))?;
        }
    }
    pump_for(Duration::from_millis(200));
    let (downs, ups) = (DOWNS.load(Ordering::SeqCst), UPS.load(Ordering::SeqCst));
    pass &= report(
        downs == CLICKS && ups == CLICKS,
        format_args!(
            "b. gate off: {downs}/{CLICKS} presses and {ups}/{CLICKS} releases posted to our window arrived (posted messages skip the low-level hook; the swallow is covered by unit tests)"
        ),
    );
    // SAFETY: created on this thread above.
    unsafe { DestroyWindow(window)? };

    // c. Idle CPU with the menu hidden.
    pump_for(Duration::from_millis(300));
    let hidden = cpu_share(Duration::from_secs(2));
    pass &= report(
        hidden < IDLE_CPU,
        format_args!(
            "c. idle CPU, menu hidden: {:.3}% of one core",
            hidden * 100.0
        ),
    );

    // d. The menu window: shown at the cursor with two rows (the test mode does not arm the
    // gate).
    // SAFETY: no arguments.
    let foreground_before = unsafe { GetForegroundWindow() };
    summoner.test_menu(Some(vec![ToolId::NewNote, ToolId::RecordScreen]));
    let hwnd = HWND(summoner.menu_window() as *mut _);
    let mut foreground_hits = 0;
    let mut foreground_changed = 0;
    let shown = Instant::now();
    while shown.elapsed() < Duration::from_millis(600) {
        pump_for(Duration::from_millis(20));
        // SAFETY: no arguments.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground == hwnd {
            foreground_hits += 1;
        }
        if foreground != foreground_before {
            foreground_changed += 1;
        }
    }
    // SAFETY: reads flags of a window of this process.
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    // SAFETY: as above.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let wanted = WS_EX_LAYERED.0
        | WS_EX_TRANSPARENT.0
        | WS_EX_TOPMOST.0
        | WS_EX_NOACTIVATE.0
        | WS_EX_TOOLWINDOW.0;
    pass &= report(
        visible && style & wanted == wanted && !CLICK_GATE.is_armed(),
        format_args!(
            "d. menu shown: layered, click-through, topmost, no-activate, tool window (style {style:#x}); gate still off"
        ),
    );
    let cursor = cursor_pos();
    let cursor = (cursor.x.round() as i32, cursor.y.round() as i32);
    let area = monitor_at(cursor_pos()).work_area;
    let work = Bounds {
        left: area.x.round() as i32,
        top: area.y.round() as i32,
        right: (area.x + area.width).round() as i32,
        bottom: (area.y + area.height).round() as i32,
    };
    let placed = summoner.menu_layout();
    let mut window_rect = RECT::default();
    // SAFETY: writes into a local.
    unsafe { GetWindowRect(hwnd, &mut window_rect)? };
    pass &= report(
        placed.is_some_and(|l| {
            l.rows == 2
                && l.row_at(cursor) == Some(0)
                && l.card.inside(&work)
                && (
                    window_rect.left,
                    window_rect.top,
                    window_rect.right,
                    window_rect.bottom,
                ) == (l.window.left, l.window.top, l.window.right, l.window.bottom)
        }),
        format_args!(
            "d. two rows, the first under the cursor at {},{}, card {:?} inside the work area {:?}, window where placed",
            cursor.0,
            cursor.1,
            placed.map(|l| l.card),
            work
        ),
    );
    let mut affinity = 0;
    // SAFETY: writes into a local.
    let affinity_ok = unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }.is_ok();
    pass &= report(
        affinity_ok && affinity == WDA_EXCLUDEFROMCAPTURE.0,
        format_args!("d. menu excluded from capture (affinity {affinity:#x})"),
    );

    // e. Click-through seen from another process (the gate takes a click on a row, not the
    // window); the control run without WS_EX_TRANSPARENT shows the probe finds the menu when
    // it should.
    let mut rect = RECT::default();
    // SAFETY: writes into a local.
    unsafe { GetWindowRect(hwnd, &mut rect)? };
    let center = POINT {
        x: (rect.left + rect.right) / 2,
        y: (rect.top + rect.bottom) / 2,
    };
    let normal = run_probe(center, hwnd)?;
    // SAFETY: our own window; the style is restored right after the control probe.
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (style & !WS_EX_TRANSPARENT.0) as isize) };
    let control = run_probe(center, hwnd);
    // SAFETY: as above.
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize) };
    let control = control?;
    pass &= report(
        !normal && control,
        format_args!(
            "e. click-through from another process (WindowFromPoint at {},{} finds the menu: {normal}; control without WS_EX_TRANSPARENT: {control})",
            center.x, center.y
        ),
    );

    // f. Idle CPU with the menu shown and still.
    let still = cpu_share(Duration::from_secs(2));
    // SAFETY: no arguments.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground == hwnd {
        foreground_hits += 1;
    }
    if foreground != foreground_before {
        foreground_changed += 1;
    }
    pass &= report(
        still < IDLE_CPU,
        format_args!(
            "f. idle CPU, menu shown and still: {:.3}% of one core",
            still * 100.0
        ),
    );
    pass &= report(
        foreground_hits == 0,
        format_args!("f. the menu never became the foreground window ({foreground_hits} samples)"),
    );
    println!(
        "INFO f. the foreground window changed in {foreground_changed} samples while the menu showed (0 expected unless someone switched windows)"
    );
    summoner.test_menu(None);
    pump_for(Duration::from_millis(300));
    // SAFETY: reads a flag of a window of this process.
    let gone = !unsafe { IsWindowVisible(hwnd) }.as_bool();
    pass &= report(
        gone && summoner.menu_layout().is_none(),
        "f. the menu hides again",
    );

    // g. The keyboard hook's worst callback so far (the whole run, over two seconds).
    let (calls, worst) = summoner.key_hook_stats();
    pass &= report(
        worst < Duration::from_millis(1),
        format_args!(
            "g. keyboard hook: {calls} callbacks in {:.1} s, worst {} us{}",
            since.elapsed().as_secs_f64(),
            worst.as_micros(),
            if calls == 0 {
                " (no key was pressed during the run)"
            } else {
                ""
            }
        ),
    );

    // h. Restart.
    summoner.restart()?;
    pump_for(Duration::from_millis(300));
    pass &= report(
        summoner.is_alive() && !CLICK_GATE.is_armed(),
        "h. restart: alive again, gate off",
    );

    // i. Shutdown joins within a second; no key is left down; the gate is off.
    let stopping = Instant::now();
    let ended = summoner.shutdown();
    let took = stopping.elapsed();
    // SAFETY: plain value arguments; the high bit is set while a key is down.
    let (alt, ctrl) = unsafe {
        (
            GetAsyncKeyState(i32::from(VK_MENU.0)) < 0,
            GetAsyncKeyState(i32::from(VK_CONTROL.0)) < 0,
        )
    };
    pass &= report(
        ended && took < Duration::from_secs(1),
        format_args!("i. shutdown joined in {} ms", took.as_millis()),
    );
    pass &= report(
        !alt && !ctrl && !CLICK_GATE.is_armed(),
        format_args!(
            "i. afterwards Alt down: {alt}, Ctrl down: {ctrl}, gate armed: {}",
            CLICK_GATE.is_armed()
        ),
    );
    let _ = uia.shutdown();
    Ok(pass)
}

/// The process's CPU time over `span` as a share of one core, pumping meanwhile.
fn cpu_share(span: Duration) -> f64 {
    let before = process_cpu();
    let started = Instant::now();
    pump_for(span);
    let wall = started.elapsed().as_secs_f64();
    (process_cpu() - before) / wall
}

fn process_cpu() -> f64 {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: writes into locals; the pseudo handle needs no closing.
    let _ = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    (ticks(kernel) + ticks(user)) as f64 / 1e7
}

unsafe extern "system" fn test_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            DOWNS.fetch_add(1, Ordering::SeqCst);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            UPS.fetch_add(1, Ordering::SeqCst);
            LRESULT(0)
        }
        // SAFETY: the arguments come unchanged from the system.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// A hidden window of this thread that counts clicks.
fn test_window() -> Result<HWND> {
    // SAFETY: a null module name returns the exe's own module.
    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSW {
        lpfnWndProc: Some(test_proc),
        hInstance: instance.into(),
        lpszClassName: w!("IgorioSummonSelftest"),
        ..Default::default()
    };
    // SAFETY: `class` points only to static data and a 'static function.
    unsafe { RegisterClassW(&class) };
    // SAFETY: the class is registered above; never shown.
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("IgorioSummonSelftest"),
            w!("summon selftest"),
            WS_OVERLAPPED,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };
    Ok(hwnd)
}

fn pump_for(span: Duration) {
    let until = Instant::now() + span;
    loop {
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid MSG for the calls.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // SAFETY: no handles; waits for messages or the timeout.
        unsafe {
            MsgWaitForMultipleObjectsEx(
                None,
                left.as_millis().max(1) as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }
    }
}

fn run_probe(point: POINT, hwnd: HWND) -> Result<bool> {
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
        pump_for(Duration::from_millis(5));
    }
    let output = child.wait_with_output()?;
    let text = String::from_utf8(output.stdout)?;
    match text.trim() {
        "hit=true" => Ok(true),
        "hit=false" => Ok(false),
        other => Err(format!("probe said {other:?}").into()),
    }
}

fn probe(x: &str, y: &str, hwnd: &str) -> Result<()> {
    ensure_dpi_awareness();
    let point = POINT {
        x: x.parse()?,
        y: y.parse()?,
    };
    let menu = hwnd.parse::<isize>()?;
    // SAFETY: plain value arguments.
    let hit = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    println!("hit={}", hit.0 as isize == menu);
    Ok(())
}
