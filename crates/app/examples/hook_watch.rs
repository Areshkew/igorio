//! Prints what `igorio_app::hook` sees, for a person to try: moves (at most 10 lines a
//! second), clicks with double and triple clicks marked, and the wheel. Runs 20 seconds,
//! or the number of seconds given as the first argument.

use std::time::{Duration, Instant};

use igorio_app::hook::{MouseEvent, MouseHook};
use windows::Win32::System::Threading::WaitForSingleObject;

fn main() -> windows::core::Result<()> {
    let seconds = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let mut hook = MouseHook::start()?;
    println!("Watching the mouse for {seconds} s.");
    let started = Instant::now();
    let mut last_move_print = 0.0f64;
    let mut skipped_moves = 0u32;
    let mut events = Vec::new();
    let mut last_check = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        // SAFETY: the wake handle stays open while `hook` lives.
        unsafe { WaitForSingleObject(hook.wake_handle(), 100) };
        hook.drain(&mut events);
        for event in events.drain(..) {
            match event {
                MouseEvent::Move {
                    x,
                    y,
                    time,
                    injected,
                } => {
                    if time - last_move_print < 0.1 {
                        skipped_moves += 1;
                        continue;
                    }
                    last_move_print = time;
                    let tag = if injected { " injected" } else { "" };
                    println!("{time:9.3} move  ({x}, {y}){tag}  (+{skipped_moves} not printed)");
                    skipped_moves = 0;
                }
                MouseEvent::Down {
                    button,
                    x,
                    y,
                    time,
                    click_count,
                    injected,
                } => {
                    let mark = match click_count {
                        1 => String::new(),
                        2 => "  DOUBLE".into(),
                        3 => "  TRIPLE".into(),
                        n => format!("  x{n}"),
                    };
                    let tag = if injected { " injected" } else { "" };
                    println!("{time:9.3} down  {button:?} ({x}, {y}){mark}{tag}");
                }
                MouseEvent::Up {
                    button,
                    x,
                    y,
                    time,
                    injected,
                } => {
                    let tag = if injected { " injected" } else { "" };
                    println!("{time:9.3} up    {button:?} ({x}, {y}){tag}");
                }
                MouseEvent::Wheel {
                    delta,
                    horizontal,
                    x,
                    y,
                    time,
                    ..
                } => {
                    let axis = if horizontal { "h" } else { "v" };
                    println!("{time:9.3} wheel {axis} {delta} ({x}, {y})");
                }
            }
        }
        if last_check.elapsed() >= Duration::from_secs(1) {
            last_check = Instant::now();
            if !hook.is_alive() || hook.seems_stalled() {
                println!("hook stalled or dead; restarting");
                hook.restart()?;
            }
        }
    }
    let stats = hook.stats();
    println!(
        "{} callbacks, longest {:?}, dropped {}",
        stats.callbacks, stats.max_callback, stats.dropped
    );
    Ok(())
}
