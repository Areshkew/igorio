//! For a person: every 500 ms for 30 seconds, prints what UI Automation finds under the
//! cursor: mapped role, control type, the editable verdict of `editable_target`, the process
//! and the timing. No hooks, no input.
//!
//! `uia_watch [SECONDS]` changes the duration.

use std::time::{Duration, Instant};

use igorio_app::overlay::{cursor_pos, ensure_dpi_awareness};
use igorio_app::uia::{Lookup, Outcome, Uia, control_type_name};
use igorio_core::editable::{AccessibilityNode, editable_target};
use igorio_core::geom::Point;
use windows::Win32::System::Threading::WaitForSingleObject;

fn main() -> windows::core::Result<()> {
    ensure_dpi_awareness();
    let seconds = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let uia = Uia::start()?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        let tick = Instant::now();
        let cursor = cursor_pos();
        let point = (cursor.x.round() as i32, cursor.y.round() as i32);
        let id = uia.request(Lookup::at(point));
        let mut answer = None;
        while answer.is_none() && tick.elapsed() < Duration::from_secs(1) {
            // SAFETY: the wake handle is alive while `uia` is.
            unsafe { WaitForSingleObject(uia.wake_handle(), 100) };
            while let Some((got, outcome)) = uia.poll() {
                if got == id {
                    answer = Some(outcome);
                }
            }
        }
        let took = tick.elapsed().as_secs_f64() * 1000.0;
        let at = format!(
            "{:5.1}s ({:5}, {:5})",
            started.elapsed().as_secs_f64(),
            point.0,
            point.1
        );
        match answer {
            Some(Outcome::Found(s)) => {
                let e = s.hit.element();
                let verdict =
                    editable_target(Point::new(f64::from(point.0), f64::from(point.1)), &s.hit)
                        .map_or("no".to_string(), |n| format!("yes ({})", n.element().role));
                let focused = s.focused.as_ref().map_or("-".to_string(), |f| {
                    format!("{} {:?}", f.element().role, clip(&f.element().name))
                });
                let parents: Vec<String> = std::iter::successors(s.hit.parent(), |n| n.parent())
                    .take(4)
                    .map(|n| n.element().role.to_string())
                    .collect();
                println!(
                    "{at} {}{} <- {} {:?} | editable: {verdict} | {} pid {}{}{} | {} calls, {:.1} ms worker, {took:.1} ms total | up: {} | focus: {focused}",
                    e.role,
                    e.subrole.map(|r| format!("/{r}")).unwrap_or_default(),
                    control_type_name(e.control_type),
                    clip(&e.name),
                    s.target.exe.as_deref().unwrap_or("?"),
                    s.target.pid,
                    if s.target.is_terminal {
                        " terminal"
                    } else {
                        ""
                    },
                    s.target
                        .shell
                        .map(|k| format!(" {k:?}"))
                        .unwrap_or_default(),
                    s.calls,
                    s.elapsed.as_secs_f64() * 1000.0,
                    parents.join(" > "),
                );
            }
            Some(Outcome::OwnProcess) => println!("{at} own process ({took:.1} ms)"),
            Some(Outcome::Elevated { pid }) => {
                println!("{at} elevated window, pid {pid}: can't reach this window")
            }
            Some(Outcome::TimedOut { target }) => println!(
                "{at} timed out: {} pid {}{} ({took:.1} ms)",
                target.exe.as_deref().unwrap_or("?"),
                target.pid,
                if target.is_terminal {
                    " terminal (silent terminal: paste)"
                } else {
                    ""
                },
            ),
            Some(Outcome::Failed(err)) => println!("{at} failed: {err}"),
            None => println!("{at} no answer"),
        }
        std::thread::sleep(Duration::from_millis(500).saturating_sub(tick.elapsed()));
    }
    uia.shutdown();
    Ok(())
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(40) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}
