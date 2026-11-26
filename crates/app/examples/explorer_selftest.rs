//! Self-check for `igorio_app::explorer`. Non-interactive: it sends no input, does not move
//! the cursor, and does not open, close, navigate or select anything in Explorer. Prints
//! PASS or FAIL per check (INFO for what it cannot check here) and exits 0 only if every
//! check passes. The only file it writes is an empty zip under `target/explorer_selftest`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use igorio_app::explorer::{
    DEFAULT_DEADLINE, Explorer, FolderOutcome, FolderRequest, RequestId, outcome_for_parsing_name,
    tabs,
};
use igorio_app::overlay::ensure_dpi_awareness;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::Shell::{FOLDERID_Desktop, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetDesktopWindow, IsWindowVisible};
use windows::core::w;

/// Slack over the deadline for thread wake-ups.
const SLACK: Duration = Duration::from_millis(50);

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
    let explorer = match Explorer::start() {
        Ok(explorer) => explorer,
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

    // 1. The desktop.
    let known = desktop_known_folder();
    let (outcome, took) = ask(&explorer, FolderRequest::Desktop, DEFAULT_DEADLINE);
    let ok = matches!(&outcome, Some(FolderOutcome::Folder(p)) if p.is_dir() && Some(p) == known.as_ref());
    check(
        "desktop folder",
        ok && took <= DEFAULT_DEADLINE + SLACK,
        format!("{outcome:?} in {:.1} ms (known folder {known:?})", ms(took)),
    );

    // 2. The Explorer windows open right now (none is opened here).
    let windows = explorer_windows();
    let visible: Vec<isize> = windows.iter().filter(|w| w.1).map(|w| w.0).collect();
    for (window, _) in windows.iter().filter(|w| !w.1) {
        println!("INFO hidden CabinetWClass {window:#x} (Explorer's preloaded window): skipped");
    }
    if visible.is_empty() {
        println!("INFO no File Explorer window is open: window lookups skipped");
    }
    for window in visible {
        let tabs = tabs(window);
        let (outcome, took) = ask(&explorer, FolderRequest::Window(window), DEFAULT_DEADLINE);
        let ok = matches!(
            &outcome,
            Some(FolderOutcome::Folder(_) | FolderOutcome::NotAFolder { .. })
        );
        check(
            &format!("Explorer window {window:#x}"),
            ok && took <= DEFAULT_DEADLINE + SLACK,
            format!(
                "{} tab(s), active {:?}: {outcome:?} in {:.1} ms",
                tabs.windows.len(),
                tabs.active.map(|t| format!("{t:#x}")),
                ms(took)
            ),
        );
    }

    // 3. A 0 ms deadline times out; the next request is still answered.
    let (outcome, took) = ask(&explorer, FolderRequest::Desktop, Duration::ZERO);
    check(
        "0 ms deadline",
        outcome == Some(FolderOutcome::TimedOut),
        format!("{outcome:?} in {:.1} ms", ms(took)),
    );
    let (outcome, took) = ask(&explorer, FolderRequest::Desktop, DEFAULT_DEADLINE);
    check(
        "next request answered",
        matches!(outcome, Some(FolderOutcome::Folder(_))),
        format!("{outcome:?} in {:.1} ms", ms(took)),
    );

    // 4. Windows that are not Explorer fail cleanly.
    // SAFETY: no arguments; returns the desktop window handle.
    let root = unsafe { GetDesktopWindow() }.0 as isize;
    for (name, window) in [("desktop root window", root), ("stale handle", 0x7ffe_fff0)] {
        let (outcome, took) = ask(&explorer, FolderRequest::Window(window), DEFAULT_DEADLINE);
        check(
            &format!("not Explorer: {name}"),
            matches!(outcome, Some(FolderOutcome::Failed(_))),
            format!("{outcome:?} in {:.1} ms", ms(took)),
        );
    }

    // 5. A burst of 30 requests runs only a few.
    let started = Instant::now();
    let mut last = RequestId(0);
    for _ in 0..30 {
        last = explorer.request(FolderRequest::Desktop, Duration::from_secs(2));
    }
    let answers = wait_all(&explorer, last, Duration::from_secs(3));
    let last_ok = matches!(answers.last(), Some((id, FolderOutcome::Folder(_))) if *id == last);
    check(
        "burst of 30",
        last_ok && answers.len() <= 3,
        format!(
            "{} answered (the rest replaced), last {:?}, {:.1} ms",
            answers.len(),
            answers.last().map(|a| &a.1),
            ms(started.elapsed())
        ),
    );

    // 6. Reveal against nothing: windows that are not Explorer are dropped, and the worker
    // still answers afterwards. (Reveal into a real window would change the user's
    // selection, so it is not run here.)
    explorer.reveal(root, PathBuf::from(r"C:\nothing\here.png"));
    explorer.reveal(0, PathBuf::from(r"C:\nothing\here.png"));
    let (outcome, took) = ask(&explorer, FolderRequest::Desktop, DEFAULT_DEADLINE);
    check(
        "reveal against nothing is harmless",
        matches!(outcome, Some(FolderOutcome::Folder(_))),
        format!("{outcome:?} in {:.1} ms", ms(took)),
    );

    // 7. The folder rules on real Shell items (no window involved), on this thread.
    classify_items(&mut check, known.as_deref());

    let started = Instant::now();
    let joined = explorer.shutdown();
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

/// Name, Shell parsing name, hard check (else INFO when the item cannot be made), expected.
type Item = (&'static str, String, bool, fn(&FolderOutcome) -> bool);

fn classify_items(check: &mut impl FnMut(&str, bool, String), desktop: Option<&Path>) {
    // SAFETY: no reserved pointer; this thread owns no windows. Left initialised until exit.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/explorer_selftest");
    let zip = dir.join("empty.zip");
    // An empty zip: just the end-of-central-directory record.
    let mut bytes = b"PK\x05\x06".to_vec();
    bytes.resize(22, 0);
    let wrote = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&zip, bytes));
    let dir = dir.canonicalize().unwrap_or(dir);
    let plain = |p: &Path| p.to_string_lossy().trim_start_matches(r"\\?\").to_string();

    let folder = |o: &FolderOutcome| matches!(o, FolderOutcome::Folder(_));
    let not_folder = |o: &FolderOutcome| matches!(o, FolderOutcome::NotAFolder { .. });
    let mut items: Vec<Item> = vec![
        (
            "This PC",
            "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}".into(),
            true,
            not_folder,
        ),
        (
            "Recycle Bin",
            "shell:RecycleBinFolder".into(),
            true,
            not_folder,
        ),
        (
            "Home",
            "::{f874310e-b6b7-47dc-bc84-b9e6b38f5903}".into(),
            false,
            not_folder,
        ),
        (
            "Documents library",
            r"::{031E4825-7B94-4dc3-B131-E946B44C8DD5}\Documents.library-ms".into(),
            false,
            not_folder,
        ),
        (
            "search result",
            "search-ms:query=png&crumb=location:C%3A%5C".into(),
            false,
            not_folder,
        ),
        (
            "Network",
            "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}".into(),
            false,
            not_folder,
        ),
        ("plain folder", plain(&dir), true, folder),
    ];
    if wrote.is_ok() {
        items.push((
            "zip file",
            plain(&zip.canonicalize().unwrap_or(zip)),
            true,
            not_folder,
        ));
    }
    if let Some(desktop) = desktop {
        items.push(("Desktop folder", plain(desktop), true, folder));
    }
    for (name, parsing, hard, expected) in items {
        let started = Instant::now();
        let outcome = outcome_for_parsing_name(&parsing);
        let detail = format!("{outcome:?} in {:.1} ms", ms(started.elapsed()));
        if hard || !matches!(outcome, FolderOutcome::Failed(_)) {
            check(&format!("item: {name}"), expected(&outcome), detail);
        } else {
            println!("INFO item: {name}: {detail}");
        }
    }
}

/// Sends one request and waits for its answer.
fn ask(
    explorer: &Explorer,
    request: FolderRequest,
    deadline: Duration,
) -> (Option<FolderOutcome>, Duration) {
    let started = Instant::now();
    let id = explorer.request(request, deadline);
    let answer = wait_all(explorer, id, deadline + Duration::from_secs(2))
        .into_iter()
        .find(|(got, _)| *got == id)
        .map(|(_, outcome)| outcome);
    (answer, started.elapsed())
}

/// Every answer until `id` is answered or `limit` passes.
fn wait_all(
    explorer: &Explorer,
    id: RequestId,
    limit: Duration,
) -> Vec<(RequestId, FolderOutcome)> {
    let until = Instant::now() + limit;
    let mut seen = Vec::new();
    while Instant::now() < until {
        // SAFETY: the wake handle is alive while `explorer` is.
        unsafe { WaitForSingleObject(explorer.wake_handle(), 20) };
        while let Some(answer) = explorer.poll() {
            let done = answer.0 == id;
            seen.push(answer);
            if done {
                return seen;
            }
        }
    }
    seen
}

/// Top-level `CabinetWClass` windows and whether each is visible.
fn explorer_windows() -> Vec<(isize, bool)> {
    let mut found = Vec::new();
    let mut after = None;
    while found.len() < 256 {
        // SAFETY: plain window queries; no title, so no message is sent.
        let Ok(window) = (unsafe { FindWindowExW(None, after, w!("CabinetWClass"), None) }) else {
            break;
        };
        // SAFETY: as above.
        let visible = unsafe { IsWindowVisible(window) }.as_bool();
        found.push((window.0 as isize, visible));
        after = Some(window);
    }
    found
}

fn desktop_known_folder() -> Option<PathBuf> {
    // SAFETY: constant folder id; the string is freed right after it is copied.
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
