//! Self-check for `igorio_app::hook`. Non-interactive; a person may use the mouse meanwhile.
//! The only input it sends is a relative mouse move of zero distance, which does not move
//! the cursor. Prints PASS, FAIL or INFO per check; exits 0 only if nothing failed.

use std::mem::size_of;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use igorio_app::hook::{MouseEvent, MouseHook};
use windows::Win32::Foundation::{CloseHandle, POINT, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{OpenThread, THREAD_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        self.failed |= !ok;
    }

    fn info(&self, name: &str, detail: impl std::fmt::Display) {
        println!("INFO {name}: {detail}");
    }
}

fn zero_move() -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: one valid INPUT, size given correctly.
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) == 1 }
}

fn cursor_pos() -> POINT {
    let mut point = POINT::default();
    // SAFETY: `point` outlives the call.
    let _ = unsafe { GetCursorPos(&mut point) };
    point
}

fn signalled(hook: &MouseHook, timeout_ms: u32) -> bool {
    // SAFETY: the wake handle stays open while `hook` lives.
    unsafe { WaitForSingleObject(hook.wake_handle(), timeout_ms) == WAIT_OBJECT_0 }
}

fn main() -> ExitCode {
    let mut r = Report { failed: false };

    // a. start
    let hook = match MouseHook::start() {
        Ok(hook) => hook,
        Err(error) => {
            r.check("a. start", false, error);
            return ExitCode::FAILURE;
        }
    };
    r.check(
        "a. start and is_alive",
        hook.is_alive(),
        format!("is_alive = {}", hook.is_alive()),
    );

    // b. zero-distance injected moves
    let mut events = Vec::new();
    hook.drain(&mut events);
    events.clear();
    let mut woke = 0;
    let mut injected_times = Vec::new();
    for _ in 0..5 {
        if !zero_move() {
            r.check("b. SendInput", false, "SendInput sent nothing");
            break;
        }
        if signalled(&hook, 200) {
            woke += 1;
        }
        hook.drain(&mut events);
        for event in events.drain(..) {
            if let MouseEvent::Move {
                time,
                injected: true,
                ..
            } = event
            {
                injected_times.push(time);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if injected_times.is_empty() {
        r.info(
            "b. zero-distance moves",
            "Windows did not deliver them to the hook",
        );
    } else {
        let increasing = injected_times.windows(2).all(|w| w[1] > w[0]);
        r.check(
            "b. injected moves arrive, time increasing",
            increasing,
            format!(
                "{} of 5 arrived, times {:?}",
                injected_times.len(),
                injected_times
            ),
        );
        r.check(
            "b. wake signal fired",
            woke > 0,
            format!("{woke} of 5 waits woke"),
        );
    }

    // c. two seconds of whatever the person does
    let before = hook.stats();
    let started = Instant::now();
    let mut drained = 0usize;
    let mut kinds = [0usize; 4];
    let mut wakes = 0usize;
    let mut best_rate = 0.0f64;
    let mut bucket_start = Instant::now();
    let mut bucket_callbacks = before.callbacks;
    let mut cursor = cursor_pos();
    let mut cursor_moves = 0usize;
    while started.elapsed() < Duration::from_secs(2) {
        let now = cursor_pos();
        if now != cursor {
            cursor_moves += 1;
            cursor = now;
        }
        if signalled(&hook, 20) {
            wakes += 1;
        }
        hook.drain(&mut events);
        drained += events.len();
        for event in events.drain(..) {
            let i = match event {
                MouseEvent::Move { .. } => 0,
                MouseEvent::Down { .. } => 1,
                MouseEvent::Up { .. } => 2,
                MouseEvent::Wheel { .. } => 3,
            };
            kinds[i] += 1;
        }
        let elapsed = bucket_start.elapsed();
        if elapsed >= Duration::from_millis(100) {
            let now = hook.stats().callbacks;
            best_rate = best_rate.max((now - bucket_callbacks) as f64 / elapsed.as_secs_f64());
            bucket_callbacks = now;
            bucket_start = Instant::now();
        }
    }
    let after = hook.stats();
    r.info(
        "c. 2 s window",
        format!(
            "{} callbacks, {drained} events drained (move {}, down {}, up {}, wheel {}), {wakes} wakes, highest callback rate {:.0}/s, dropped {}",
            after.callbacks - before.callbacks,
            kinds[0], kinds[1], kinds[2], kinds[3],
            best_rate,
            after.dropped,
        ),
    );
    let callbacks = after.callbacks - before.callbacks;
    if callbacks == 0 {
        r.info("c. longest callback", "no callbacks to measure");
    } else {
        r.check(
            "c. longest callback under 1 ms",
            after.max_callback < Duration::from_millis(1),
            format!("{:?}", after.max_callback),
        );
    }
    if cursor_moves == 0 {
        r.info(
            "c. real input",
            "the cursor did not move; nobody used the mouse",
        );
    } else {
        r.check(
            "c. real input reaches the hook",
            callbacks > 0,
            format!("cursor moved in {cursor_moves} polls, {callbacks} callbacks"),
        );
    }
    r.check(
        "c. still alive",
        hook.is_alive(),
        format!("is_alive = {}", hook.is_alive()),
    );

    // d. drop joins the thread
    let thread_id = hook.thread_id().unwrap_or(0);
    // SAFETY: opens a handle to a thread of this process; closed below.
    let thread = unsafe { OpenThread(THREAD_SYNCHRONIZE, false, thread_id) };
    let dropped_at = Instant::now();
    drop(hook);
    let drop_time = dropped_at.elapsed();
    r.check(
        "d. drop within 1 s",
        drop_time < Duration::from_secs(1),
        format!("{drop_time:?}"),
    );
    match thread {
        Ok(thread) => {
            // SAFETY: `thread` is the handle opened above, closed once.
            let ended = unsafe {
                let ended = WaitForSingleObject(thread, 0) == WAIT_OBJECT_0;
                let _ = CloseHandle(thread);
                ended
            };
            r.check(
                "d. hook thread ended",
                ended,
                format!("thread {thread_id} handle signalled = {ended}"),
            );
        }
        Err(error) => r.check(
            "d. hook thread ended",
            false,
            format!("OpenThread: {error}"),
        ),
    }

    // e. start again, and restart
    match MouseHook::start() {
        Ok(mut hook) => {
            r.check(
                "e. start after drop",
                hook.is_alive(),
                format!("is_alive = {}", hook.is_alive()),
            );
            let old = hook.thread_id();
            let restarted = hook.restart().is_ok() && hook.is_alive() && hook.thread_id() != old;
            r.check(
                "e. restart",
                restarted,
                format!("thread {old:?} -> {:?}", hook.thread_id()),
            );
        }
        Err(error) => r.check("e. start after drop", false, error),
    }

    if r.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
