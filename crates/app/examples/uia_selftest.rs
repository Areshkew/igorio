//! Self-check for `igorio_app::uia`. Non-interactive: it sends no input and does not move the
//! cursor. Prints PASS or FAIL per check and exits 0 only if every check passes.
//!
//! For a moment it shows a small window of its own in the top-right screen corner (no
//! taskbar button, never activated) to check the own-process answer.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use igorio_app::overlay::{cursor_pos, ensure_dpi_awareness};
use igorio_app::uia::{
    DEFAULT_DEADLINE, Lookup, MAX_CALLS, Outcome, RequestId, Snapshot, Uia, UiaNode,
    control_type_name,
};
use igorio_core::editable::{AccessibilityNode, editable_target};
use igorio_core::geom::Point;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetSystemMetrics, MSG,
    MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    RegisterClassW, SM_CXSCREEN, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

const SIZE: i32 = 24;

fn main() -> ExitCode {
    ensure_dpi_awareness();
    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures += 1;
        }
    };

    let started = Instant::now();
    let uia = match Uia::start() {
        Ok(uia) => uia,
        Err(err) => {
            check("start worker", false, format!("{err}"));
            return ExitCode::FAILURE;
        }
    };
    check(
        "start worker",
        true,
        format!("{:.1} ms", ms(started.elapsed())),
    );

    // 1. What is under the cursor, and the focused element.
    let cursor = cursor_pos();
    let point = (cursor.x.round() as i32, cursor.y.round() as i32);
    let calls_before = uia.calls_made();
    let started = Instant::now();
    let id = uia.request(Lookup::at(point));
    let answer = wait_for(&uia, id, Duration::from_secs(2));
    let took = started.elapsed();
    let detail = format!("({}, {}) answered in {:.1} ms", point.0, point.1, ms(took));
    match &answer {
        Some(Outcome::Found(snapshot)) => {
            print_snapshot(snapshot, point);
            check(
                "lookup under the cursor",
                took <= DEFAULT_DEADLINE + Duration::from_millis(50),
                detail,
            );
            check(
                "call count within the cap",
                snapshot.calls <= MAX_CALLS
                    && uia.calls_made() - calls_before == u64::from(snapshot.calls),
                format!("{} calls (cap {MAX_CALLS})", snapshot.calls),
            );
        }
        Some(other) => {
            // A hung or elevated window under the cursor is a valid answer too.
            println!("  outcome: {}", describe(other));
            check(
                "lookup under the cursor",
                !matches!(other, Outcome::Failed(_))
                    && took <= DEFAULT_DEADLINE + Duration::from_millis(50),
                detail,
            );
        }
        None => check("lookup under the cursor", false, "no answer".into()),
    }

    // 2. A point over our own window: OwnProcess, without a UI Automation call.
    match OwnWindow::new() {
        Ok(window) => {
            pump(Duration::from_millis(50));
            let before = uia.calls_made();
            let started = Instant::now();
            let id = uia.request(Lookup::at(window.center));
            let answer = wait_for(&uia, id, Duration::from_secs(1));
            let calls = uia.calls_made() - before;
            check(
                "own window is OwnProcess without UI Automation",
                matches!(answer, Some(Outcome::OwnProcess)) && calls == 0,
                format!(
                    "{} at ({}, {}), {calls} calls, {:.1} ms",
                    answer.as_ref().map_or("no answer".into(), describe),
                    window.center.0,
                    window.center.1,
                    ms(started.elapsed()),
                ),
            );
        }
        Err(err) => check(
            "own window is OwnProcess without UI Automation",
            false,
            format!("{err}"),
        ),
    }

    // 3. A burst of 50: only a few run, the newest is answered.
    let mut ids = Vec::new();
    for _ in 0..50 {
        ids.push(uia.request(Lookup::at(point)));
    }
    let last = *ids.last().expect("50 ids");
    let started = Instant::now();
    let answers = collect_until(&uia, last, Duration::from_secs(3));
    let newest = answers.iter().any(|(id, _)| *id == last);
    check(
        "burst of 50: newest answered, few ran",
        newest && answers.len() <= 3,
        format!(
            "{} answers ({:?}), {:.1} ms",
            answers.len(),
            answers.iter().map(|(id, _)| id.0).collect::<Vec<_>>(),
            ms(started.elapsed())
        ),
    );

    // 4. A 0 ms deadline times out at once, and the next request is still answered.
    let started = Instant::now();
    let id = uia.request(Lookup {
        deadline: Duration::ZERO,
        ..Lookup::at(point)
    });
    let answer = wait_for(&uia, id, Duration::from_secs(1));
    check(
        "0 ms deadline is TimedOut",
        matches!(answer, Some(Outcome::TimedOut { .. })),
        format!(
            "{} in {:.1} ms",
            answer.as_ref().map_or("no answer".into(), describe),
            ms(started.elapsed())
        ),
    );
    // A generous deadline: the app under the cursor may be slow, and this check is about
    // the worker still working, not about speed.
    let id = uia.request(Lookup {
        deadline: Duration::from_secs(3),
        ..Lookup::at(point)
    });
    let answer = wait_for(&uia, id, Duration::from_secs(4));
    check(
        "next request still answered",
        matches!(
            answer,
            Some(Outcome::Found(_) | Outcome::Elevated { .. } | Outcome::OwnProcess)
        ),
        answer.as_ref().map_or("no answer".into(), describe),
    );

    // 5. Shutdown joins the worker.
    let started = Instant::now();
    let joined = uia.shutdown();
    check(
        "shutdown joins the worker",
        joined,
        format!("{:.1} ms", ms(started.elapsed())),
    );

    if failures == 0 {
        println!("all checks passed");
        ExitCode::SUCCESS
    } else {
        println!("{failures} check(s) failed");
        ExitCode::FAILURE
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Found(s) => format!("Found({} {:?})", s.hit.element().role, s.hit.element().name),
        Outcome::OwnProcess => "OwnProcess".into(),
        Outcome::Elevated { pid } => format!("Elevated(pid {pid})"),
        Outcome::TimedOut { target } => format!("TimedOut({:?} pid {})", target.exe, target.pid),
        Outcome::Failed(err) => format!("Failed({err})"),
    }
}

fn print_snapshot(s: &Snapshot, point: (i32, i32)) {
    let t = &s.target;
    println!(
        "  target: pid {} exe {:?} class {:?} terminal {} shell {:?}",
        t.pid, t.exe, t.class_name, t.is_terminal, t.shell
    );
    println!(
        "  worker time {:.1} ms, {} cross-process calls, truncated {}",
        ms(s.elapsed),
        s.calls,
        s.truncated
    );
    let mut node = Some(s.hit.clone());
    let mut below: Option<UiaNode> = None;
    let mut depth = 0;
    while let Some(n) = node {
        println!("  {}{}", "  ".repeat(depth), line(&n));
        // Children other than the chain are what the nearby search found.
        for child in n.children() {
            if !below.as_ref().is_some_and(|b| b.same(&child)) {
                println!("  {}  nearby: {}", "  ".repeat(depth), line(&child));
            }
        }
        node = n.parent();
        below = Some(n);
        depth += 1;
    }
    let verdict = editable_target(Point::new(f64::from(point.0), f64::from(point.1)), &s.hit);
    println!(
        "  editable target: {}",
        verdict.as_ref().map_or("none".into(), line)
    );
    match &s.focused {
        Some(f) => println!("  focused: {}", line(f)),
        None => println!("  focused: none"),
    }
}

fn line(n: &UiaNode) -> String {
    let e = n.element();
    let frame = e.frame.map_or("no frame".into(), |r| {
        format!("[{} {} {}x{}]", r.x, r.y, r.width, r.height)
    });
    format!(
        "{}{} <- {} class={:?} fw={:?} aria={:?} name={:?} {frame} pid={} editable_root={} value={:?} text={} textedit={} focusable={}",
        e.role,
        e.subrole.map(|s| format!("/{s}")).unwrap_or_default(),
        control_type_name(e.control_type),
        e.class_name,
        e.framework,
        e.aria_role,
        e.name,
        e.pid,
        e.editable_root,
        e.value_read_only.map(|ro| if ro { "ro" } else { "rw" }),
        e.text_pattern,
        e.text_edit_pattern,
        e.focusable,
    ) + &e
        .url
        .as_ref()
        .map(|u| format!(" url={u:?}"))
        .unwrap_or_default()
}

/// Waits for the answer to `id`, pumping messages like the UI loop.
fn wait_for(uia: &Uia, id: RequestId, limit: Duration) -> Option<Outcome> {
    collect_until(uia, id, limit)
        .into_iter()
        .find(|(i, _)| *i == id)
        .map(|(_, outcome)| outcome)
}

/// Every answer up to and including the one for `id`.
fn collect_until(uia: &Uia, id: RequestId, limit: Duration) -> Vec<(RequestId, Outcome)> {
    let until = Instant::now() + limit;
    let mut answers = Vec::new();
    while Instant::now() < until {
        let left = until.saturating_duration_since(Instant::now()).as_millis() as u32;
        // SAFETY: the wake handle is alive while `uia` is; the call only blocks this thread.
        unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&[uia.wake_handle()]),
                left,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        pump(Duration::ZERO);
        while let Some(answer) = uia.poll() {
            let done = answer.0 == id;
            answers.push(answer);
            if done {
                return answers;
            }
        }
    }
    answers
}

/// Dispatches queued messages, for `for_at_least` (or just once).
fn pump(for_at_least: Duration) {
    let until = Instant::now() + for_at_least;
    loop {
        let mut msg = MSG::default();
        // SAFETY: `msg` outlives the calls; messages of this thread only.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: `msg` was filled by PeekMessageW.
            unsafe { DispatchMessageW(&msg) };
        }
        if Instant::now() >= until {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A small topmost tool window of this process in the top-right corner.
struct OwnWindow {
    hwnd: HWND,
    center: (i32, i32),
}

impl OwnWindow {
    fn new() -> windows::core::Result<OwnWindow> {
        // SAFETY: a null module name returns the exe's own module.
        let instance = unsafe { GetModuleHandleW(None)? }.into();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("IgorioUiaSelftest"),
            ..Default::default()
        };
        // SAFETY: the class struct and its static strings outlive the call.
        unsafe { RegisterClassW(&class) };
        // SAFETY: value arguments only.
        let x = unsafe { GetSystemMetrics(SM_CXSCREEN) } - SIZE;
        // SAFETY: the class is registered above; all strings are static; no creation data.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                w!("IgorioUiaSelftest"),
                w!("uia selftest"),
                WS_POPUP | WS_VISIBLE,
                x,
                0,
                SIZE,
                SIZE,
                None,
                None,
                Some(instance),
                None,
            )?
        };
        Ok(OwnWindow {
            hwnd,
            center: (x + SIZE / 2, SIZE / 2),
        })
    }
}

impl Drop for OwnWindow {
    fn drop(&mut self) {
        // SAFETY: the window was created on this thread and is destroyed once.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: forwards the arguments the system passed in.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
