//! For a person: every 500 ms for 30 seconds, if the window under the cursor is a File
//! Explorer window or the desktop, prints the folder it shows (or `NotAFolder` with its
//! name), the tab count and the timing. No hooks, no input.
//!
//! `explorer_watch [SECONDS]` changes the duration.

use std::time::{Duration, Instant};

use igorio_app::explorer::{DEFAULT_DEADLINE, Explorer, FolderOutcome, FolderRequest, tabs};
use igorio_app::overlay::{cursor_pos, ensure_dpi_awareness};
use igorio_app::uia::{Shell, shell_kind};
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetClassNameW, WindowFromPoint,
};

fn main() -> windows::core::Result<()> {
    ensure_dpi_awareness();
    let seconds = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let explorer = Explorer::start()?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        let tick = Instant::now();
        let cursor = cursor_pos();
        let point = POINT {
            x: cursor.x.round() as i32,
            y: cursor.y.round() as i32,
        };
        // SAFETY: plain window queries; `WindowFromPoint` sends messages only to windows of
        // this thread, which owns none.
        let root = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
        let mut name = [0u16; 64];
        // SAFETY: the buffer is valid for its length.
        let len = unsafe { GetClassNameW(root, &mut name) } as usize;
        let class = String::from_utf16_lossy(&name[..len]);
        let at = format!(
            "{:5.1}s ({:5}, {:5})",
            started.elapsed().as_secs_f64(),
            point.x,
            point.y
        );
        let (request, label) = match shell_kind(&class) {
            Some(Shell::Explorer) => {
                let tabs = tabs(root.0 as isize);
                (
                    FolderRequest::Window(root.0 as isize),
                    format!("Explorer, {} tab(s)", tabs.windows.len()),
                )
            }
            Some(Shell::Desktop) => (FolderRequest::Desktop, "desktop".to_string()),
            None => {
                println!("{at} not Explorer ({class})");
                std::thread::sleep(Duration::from_millis(500).saturating_sub(tick.elapsed()));
                continue;
            }
        };
        let id = explorer.request(request, DEFAULT_DEADLINE);
        let mut answer = None;
        while answer.is_none() && tick.elapsed() < Duration::from_secs(1) {
            // SAFETY: the wake handle is alive while `explorer` is.
            unsafe { WaitForSingleObject(explorer.wake_handle(), 100) };
            while let Some((got, outcome)) = explorer.poll() {
                if got == id {
                    answer = Some(outcome);
                }
            }
        }
        let took = tick.elapsed().as_secs_f64() * 1000.0;
        let text = match answer {
            Some(FolderOutcome::Folder(path)) => format!("folder {}", path.display()),
            Some(FolderOutcome::NotAFolder { display_name }) => {
                format!("not a folder: {display_name:?}")
            }
            Some(FolderOutcome::TimedOut) => "timed out".into(),
            Some(FolderOutcome::Failed(err)) => format!("failed: {err}"),
            None => "no answer".into(),
        };
        println!("{at} {label}: {text} ({took:.1} ms)");
        std::thread::sleep(Duration::from_millis(500).saturating_sub(tick.elapsed()));
    }
    explorer.shutdown();
    Ok(())
}
