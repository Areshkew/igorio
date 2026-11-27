//! Non-interactive check of the ways the app ends (`cargo run -p igorio-app --example
//! quit_selftest`). Builds the app binary in the same profile, then for each case starts it as
//! a child process with its data folders in a scratch folder under `target/`, and checks:
//!
//! 1. WM_CLOSE posted to its hidden tray window: it exits by itself with code 0, and its debug
//!    log shows the quit path in order.
//! 2. WM_QUERYENDSESSION returns TRUE; WM_ENDSESSION(TRUE) returns only after the session-end
//!    quit path ran (the log shows it, within its bound). Nothing really signs out.
//! 3. Ctrl+Break to its own process group (debug build only: the release build has no
//!    console): it exits through the quit path. This case runs in a helper copy of this
//!    program started with a hidden console of its own, which the app shares.
//! 4. In each case no temp file is left in the scratch folders, the open note's text is on
//!    disk, and its window position was written again.
//!
//! The child's windows are found by its process id only; no other process is touched.

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetConsoleWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    ENDSESSION_LOGOFF, EnumWindows, GetClassNameW, GetWindowThreadProcessId, PostMessageW,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, WM_CLOSE, WM_ENDSESSION,
    WM_QUERYENDSESSION,
};
use windows::core::BOOL;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const NOTE_ID: &str = "6F9A1B2C-3D4E-4F5A-8B7C-8D9EAFB0C1D2";
const NOTE_TEXT: &str = "quit selftest note text";
/// How long the child may take to start and to exit by itself.
const START_WAIT: Duration = Duration::from_secs(20);
const EXIT_WAIT: Duration = Duration::from_secs(20);
/// The session-end steps are bounded to 4.0 s; a little margin for the message round trip. (No
/// recording runs here: one in progress adds up to 5 s, under its shutdown block reason.)
const END_SESSION_BOUND: Duration = Duration::from_millis(4500);

const NORMAL_LINES: &[&str] = &[
    "quit: start",
    "quit: settings window closed",
    "quit: recording stopped",
    "quit: shortcuts stopped",
    "quit: notes stopped",
    "quit: screenshot detection stopped",
    "quit: clipboard checked, workers stopped",
    "quit: done in",
];
const SESSION_END_LINES: &[&str] = &[
    "quit: Windows asks to end the session: yes",
    "quit: start (session end)",
    "quit: clipboard checked",
    "quit: notes stopped",
    "quit: clipboard thread stopped",
    "quit: recording stopped",
    "quit: done in",
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, app, root] = args.as_slice()
        && flag == "--break"
    {
        let mut failures = 0;
        if let Err(error) = break_case(Path::new(app), Path::new(root), &mut failures) {
            println!("FAIL  break: {error}");
            failures += 1;
        }
        std::process::exit(failures as i32);
    }
    let mut failures = 0;
    let result = run(&mut failures);
    if let Err(error) = result {
        println!("FAIL  setup: {error}");
        failures += 1;
    }
    if failures == 0 {
        println!("all checks passed");
    } else {
        println!("{failures} check(s) failed");
        std::process::exit(1);
    }
}

fn check(failures: &mut usize, name: &str, ok: bool, detail: &str) {
    if ok {
        println!("PASS  {name}");
    } else {
        println!("FAIL  {name}: {detail}");
        *failures += 1;
    }
}

fn run(failures: &mut usize) -> Result<()> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.join("..").join("..").join("target");
    let release = std::env::current_exe()?
        .components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case("release"));
    let app = build_app(&manifest, &target, release)?;
    let scratch = target.join("quit-selftest");
    let _ = fs::remove_dir_all(&scratch);
    // SAFETY: no arguments; reads this process's console window handle.
    let has_console = !unsafe { GetConsoleWindow() }.is_invalid();
    println!(
        "app: {} ({} build), scratch: {}",
        app.display(),
        if release { "release" } else { "debug" },
        scratch.display()
    );

    // 1. WM_CLOSE to the hidden window.
    {
        let case = Case::new(&scratch.join("close"))?;
        let mut child = case.start(&app, has_console)?;
        let tray = case.wait_until_up(&mut child)?;
        case.forget_window_positions()?;
        // SAFETY: a message without pointers to the child's own tray window.
        unsafe { PostMessageW(Some(tray), WM_CLOSE, WPARAM(0), LPARAM(0)) }?;
        let code = wait_exit(&mut child, EXIT_WAIT)?;
        check(failures, "close: exits by itself with code 0", code == Some(0), &format!("{code:?}"));
        let log = case.log();
        check(failures, "close: quit path lines in order", in_order(&log, NORMAL_LINES), &tail(&log));
        case.check_files(failures, "close");
    }

    // 2. WM_QUERYENDSESSION, then WM_ENDSESSION(TRUE).
    {
        let case = Case::new(&scratch.join("session"))?;
        let mut child = case.start(&app, has_console)?;
        let tray = case.wait_until_up(&mut child)?;
        case.forget_window_positions()?;
        let answer = send(tray, WM_QUERYENDSESSION, 0, ENDSESSION_LOGOFF as isize);
        check(failures, "session: WM_QUERYENDSESSION returns TRUE", answer == Some(1), &format!("{answer:?}"));
        let started = Instant::now();
        let answer = send(tray, WM_ENDSESSION, 1, ENDSESSION_LOGOFF as isize);
        let took = started.elapsed();
        // Read right away: what the log shows now ran before WM_ENDSESSION returned.
        let log = case.log();
        check(failures, "session: WM_ENDSESSION answered", answer.is_some(), "timed out");
        check(
            failures,
            &format!("session: WM_ENDSESSION returned in {:.3} s (bound {:.1} s)", took.as_secs_f64(), END_SESSION_BOUND.as_secs_f64()),
            took <= END_SESSION_BOUND,
            "too slow",
        );
        check(
            failures,
            "session: the quit path ran before WM_ENDSESSION returned",
            in_order(&log, SESSION_END_LINES),
            &tail(&log),
        );
        // What Windows would find when it ends the process now.
        case.check_files(failures, "session");
        let code = match wait_exit(&mut child, EXIT_WAIT)? {
            Some(code) => format!("exited by itself, code {code}"),
            None => {
                child.kill()?;
                child.wait()?;
                "ended by the test".to_string()
            }
        };
        println!("      session: afterwards the process {code}");
        let log = case.log();
        let starts = log.lines().filter(|line| line.contains("quit: start")).count();
        check(failures, "session: the quit path ran once", starts == 1, &format!("{starts} starts"));
    }

    // 3. Ctrl+Break, in a helper with a hidden console of its own (this process may have
    // none, and its console is not disturbed either way).
    if release {
        println!("SKIP  break: the release build has no console (run the debug example)");
    } else {
        let mut helper = Command::new(std::env::current_exe()?)
            .arg("--break")
            .arg(&app)
            .arg(scratch.join("break"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let code = wait_exit(&mut helper, START_WAIT + EXIT_WAIT)?;
        let mut output = String::new();
        if let Some(mut stdout) = helper.stdout.take() {
            use std::io::Read;
            let _ = stdout.read_to_string(&mut output);
        }
        print!("{output}");
        let reported = output.lines().filter(|line| line.starts_with("FAIL")).count();
        *failures += reported;
        if code != Some(0) && reported == 0 {
            check(failures, "break: helper", false, &format!("exit {code:?}"));
        }
    }
    Ok(())
}

/// Case 3, in the helper: the app shares this helper's console and gets Ctrl+Break.
fn break_case(app: &Path, root: &Path, failures: &mut usize) -> Result<()> {
    let case = Case::new(root)?;
    let mut child = case.start(app, true)?;
    case.wait_until_up(&mut child)?;
    case.forget_window_positions()?;
    // SAFETY: plain values; the group is the child's own (CREATE_NEW_PROCESS_GROUP).
    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) }?;
    let code = wait_exit(&mut child, EXIT_WAIT)?;
    check(failures, "break: exits by itself with code 0", code == Some(0), &format!("{code:?}"));
    let log = case.log();
    check(failures, "break: quit path lines in order", in_order(&log, NORMAL_LINES), &tail(&log));
    case.check_files(failures, "break");
    Ok(())
}

/// Builds the app binary with the cargo that runs this example, in the same profile.
fn build_app(manifest: &Path, target: &Path, release: bool) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = Command::new(cargo);
    build.args(["build", "-q", "-p", "igorio-app", "--bin", "igorio"]).current_dir(manifest);
    if release {
        build.arg("--release");
    }
    if !build.status()?.success() {
        return Err("could not build the app".into());
    }
    let app = target.join(if release { "release" } else { "debug" }).join("igorio.exe");
    if !app.exists() {
        return Err(format!("{} is missing", app.display()).into());
    }
    Ok(app)
}

/// One child run with its own scratch folders.
struct Case {
    roaming: PathBuf,
    local: PathBuf,
    notes: PathBuf,
    captures: PathBuf,
}

impl Case {
    fn new(root: &Path) -> Result<Case> {
        let case = Case {
            roaming: root.join("Roaming"),
            local: root.join("Local"),
            notes: root.join("Notes"),
            captures: root.join("Captures"),
        };
        for folder in [&case.roaming.join("Igorio"), &case.local, &case.notes, &case.captures] {
            fs::create_dir_all(folder)?;
        }
        // Screenshot detection off and no global shortcuts: the child stays out of the way of
        // the app the user is running.
        let shortcuts: serde_json::Map<String, serde_json::Value> = [
            "restore_last", "discard_top", "discard_all", "open_settings", "new_note",
            "toggle_recording", "pen", "highlighter",
        ]
        .iter()
        .map(|id| (id.to_string(), serde_json::Value::Null))
        .collect();
        let settings = serde_json::json!({
            "capabilities": { "fairy.screenshots": false },
            "notes_folder": case.notes,
            "captures_folder": case.captures,
            "keep_every_screenshot": false,
            "launch_at_login": false,
            "shortcuts": shortcuts,
        });
        fs::write(case.roaming.join("Igorio").join("settings.json"), settings.to_string())?;
        // One note, open at start (shown without taking focus).
        let note = format!(
            "---\nid: {NOTE_ID}\ncreated: 2026-11-26T10:00:00Z\nfrom: \"Desktop\"\nkind: desktop\n---\n\n{NOTE_TEXT}"
        );
        fs::write(case.notes.join("Selftest.md"), note)?;
        fs::write(case.windows_file(), windows_file(true))?;
        Ok(case)
    }

    fn windows_file(&self) -> PathBuf {
        self.roaming.join("Igorio").join("note-windows.json")
    }

    fn log_file(&self) -> PathBuf {
        self.local.join("Igorio").join("debug.log")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.log_file()).unwrap_or_default()
    }

    fn start(&self, app: &Path, share_console: bool) -> Result<Child> {
        let stderr = fs::File::create(self.local.join("stderr.txt"))?;
        let mut flags = CREATE_NEW_PROCESS_GROUP;
        if !share_console {
            flags |= CREATE_NO_WINDOW;
        }
        let child = Command::new(app)
            .env("APPDATA", &self.roaming)
            .env("LOCALAPPDATA", &self.local)
            .env("FAIRY_DEBUG", "1")
            .env_remove("FAIRY_CAPTURABLE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .creation_flags(flags)
            .spawn()?;
        Ok(child)
    }

    /// Waits for the child's tray window and for its notes (the note is open by then).
    fn wait_until_up(&self, child: &mut Child) -> Result<HWND> {
        let until = Instant::now() + START_WAIT;
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(format!("the app exited at start: {status}").into());
            }
            if let Some(tray) = tray_window(child.id())
                && self.log().contains("notes: on")
            {
                // Let the restored note window finish opening.
                thread::sleep(Duration::from_millis(500));
                return Ok(tray);
            }
            if Instant::now() > until {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the app did not start within 20 s".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The quit must write the note's window state again: the file the child read at start
    /// is replaced by one that says the note is closed.
    fn forget_window_positions(&self) -> Result<()> {
        fs::write(self.windows_file(), windows_file(false))?;
        Ok(())
    }

    fn check_files(&self, failures: &mut usize, case: &str) {
        let mut temps = Vec::new();
        for folder in [&self.roaming, &self.local, &self.notes, &self.captures] {
            find_temps(folder, &mut temps);
        }
        check(failures, &format!("{case}: no temp files left"), temps.is_empty(), &format!("{temps:?}"));
        // The store names the file after the note's first line, so look for it by its id.
        let on_disk = fs::read_dir(&self.notes).is_ok_and(|entries| {
            entries.flatten().filter_map(|entry| fs::read_to_string(entry.path()).ok()).any(
                |text| text.contains(&format!("id: {NOTE_ID}")) && text.contains(NOTE_TEXT),
            )
        });
        check(failures, &format!("{case}: the note's text is on disk"), on_disk, "no note file with its id and text");
        let windows: serde_json::Value = fs::read(self.windows_file())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let open = windows[NOTE_ID]["isOpen"].as_bool();
        check(
            failures,
            &format!("{case}: the note's window state was written (open)"),
            open == Some(true),
            &windows.to_string(),
        );
    }
}

fn windows_file(open: bool) -> String {
    serde_json::json!({
        NOTE_ID: { "frame": { "x": 80.0, "y": 80.0, "width": 280.0, "height": 220.0 }, "isOpen": open }
    })
    .to_string()
}

fn find_temps(folder: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_temps(&path, out);
        } else if entry.file_name().to_string_lossy().to_ascii_lowercase().contains("tmp") {
            out.push(path);
        }
    }
}

/// True when every line appears in the log, each after the one before.
fn in_order(log: &str, lines: &[&str]) -> bool {
    let mut rest = log;
    for line in lines {
        match rest.find(line) {
            Some(at) => rest = &rest[at + line.len()..],
            None => return false,
        }
    }
    true
}

fn tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().filter(|line| line.contains("quit")).collect();
    format!("log quit lines: {lines:#?}")
}

fn wait_exit(child: &mut Child, within: Duration) -> Result<Option<i32>> {
    let until = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status.code().unwrap_or(-1)));
        }
        if Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Sends a message without pointers to the child's window and returns its answer.
fn send(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> Option<isize> {
    let mut answer = 0usize;
    // SAFETY: scalar arguments; `answer` outlives the call.
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            msg,
            WPARAM(wparam),
            LPARAM(lparam),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            10_000,
            Some(&mut answer),
        )
    };
    (sent.0 != 0).then_some(answer as isize)
}

/// The child's hidden tray window, found by its process id and class name.
fn tray_window(pid: u32) -> Option<HWND> {
    struct Search {
        pid: u32,
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Search` below, alive for the whole EnumWindows call.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut pid = 0;
        // SAFETY: `pid` is a live out-pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid != search.pid {
            return true.into();
        }
        let mut class = [0u16; 64];
        // SAFETY: the buffer is live and its length is passed with it.
        let len = unsafe { GetClassNameW(hwnd, &mut class) };
        if String::from_utf16_lossy(&class[..len.max(0) as usize]) == "IgorioTrayWindow" {
            search.found = Some(hwnd);
            return false.into();
        }
        true.into()
    }
    let mut search = Search { pid, found: None };
    // SAFETY: `visit` only uses `search` during the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.found
}
