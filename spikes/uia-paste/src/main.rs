mod clipboard;
mod policy;
mod target;
mod uia;

use std::cell::RefCell;
use std::error::Error;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_ACCESSDENIED, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use policy::{Facts, Verdict};
use uia::{Snapshot, Worker, print_node};

const MOUSE_UP_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MOUSE_UP_TIMEOUT: Duration = Duration::from_secs(1);
const AFTER_MOUSE_UP_DELAY: Duration = Duration::from_millis(80);
const TERMINAL_AFTER_MOUSE_UP: Duration = Duration::from_millis(140);
const TERMINAL_CLIPBOARD_QUIET: Duration = Duration::from_millis(90);
const TERMINAL_SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
const RESTORE_NO_READ_DELAY: Duration = Duration::from_secs(8);

static MOUSE_SERIAL: AtomicU32 = AtomicU32::new(0);
static PRIMARY_UP_SERIAL: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy)]
struct Gesture {
    point: POINT,
    serial: u32,
    started: Instant,
}

struct HookState {
    sender: SyncSender<Gesture>,
    previous: Option<(POINT, u32)>,
    primary_down: Option<u32>,
    max_time: u32,
    max_x: u32,
    max_y: u32,
    down_message: u32,
    up_message: u32,
}

thread_local! {
    static HOOK: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let event = unsafe { &*(data.0 as *const MSLLHOOKSTRUCT) };
        if event.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0 {
            HOOK.with_borrow_mut(|state| {
                let Some(state) = state else { return };
                let message = message.0 as u32;
                if message == state.down_message {
                    let serial = MOUSE_SERIAL.fetch_add(1, Relaxed).wrapping_add(1);
                    if state.primary_down.replace(serial).is_some() {
                        state.previous = None;
                        return;
                    }
                    if state.previous.take().is_some_and(|(point, time)| {
                        event.time.wrapping_sub(time) <= state.max_time
                            && point.x.abs_diff(event.pt.x) <= state.max_x
                            && point.y.abs_diff(event.pt.y) <= state.max_y
                    }) {
                        let _ = state.sender.try_send(Gesture {
                            point: event.pt,
                            serial,
                            started: Instant::now(),
                        });
                    } else {
                        state.previous = Some((event.pt, event.time));
                    }
                } else if message == state.up_message {
                    if let Some(serial) = state.primary_down.take() {
                        PRIMARY_UP_SERIAL.store(serial, Relaxed);
                    }
                } else if matches!(
                    message,
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                ) {
                    MOUSE_SERIAL.fetch_add(1, Relaxed);
                    state.previous = None;
                }
            });
        }
    }
    unsafe { CallNextHookEx(None, code, message, data) }
}

fn wait_for_release(gesture: Gesture) -> bool {
    loop {
        if MOUSE_SERIAL.load(Relaxed) != gesture.serial
            || gesture.started.elapsed() >= MOUSE_UP_TIMEOUT
        {
            return false;
        }
        if PRIMARY_UP_SERIAL.load(Relaxed) == gesture.serial {
            return true;
        }
        thread::sleep(MOUSE_UP_POLL_INTERVAL);
    }
}

fn settle_terminal(released: Instant) {
    let mut seq = clipboard::sequence();
    let mut last_change = Instant::now();
    while released.elapsed() < TERMINAL_SETTLE_TIMEOUT {
        let current = clipboard::sequence();
        if current != seq {
            seq = current;
            last_change = Instant::now();
        }
        if released.elapsed() >= TERMINAL_AFTER_MOUSE_UP
            && last_change.elapsed() >= TERMINAL_CLIPBOARD_QUIET
        {
            break;
        }
        thread::sleep(MOUSE_UP_POLL_INTERVAL);
    }
}

fn valid_target(gesture: Gesture, root: HWND) -> bool {
    MOUSE_SERIAL.load(Relaxed) == gesture.serial
        && !root.is_invalid()
        && target::window_at(gesture.point).1 == root
        && unsafe {
            GetAncestor(GetForegroundWindow(), GA_ROOT) == root
                && [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
                    .into_iter()
                    .all(|key| GetAsyncKeyState(i32::from(key.0)) >= 0)
        }
}

fn send_paste() -> Result<(), String> {
    fn key(vk: VIRTUAL_KEY, dw_flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    dwFlags: dw_flags,
                    ..KEYBDINPUT::default()
                },
            },
        }
    }
    let v = VIRTUAL_KEY(0x56);
    let input = [
        key(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
        key(v, KEYBD_EVENT_FLAGS(0)),
        key(v, KEYEVENTF_KEYUP),
        key(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    let sent = unsafe { SendInput(&input, size_of::<INPUT>() as i32) } as usize;
    if sent == input.len() {
        return Ok(());
    }
    let err = windows::core::Error::from_thread();
    // A partial send can leave Ctrl or V down.
    if sent > 0 {
        let releases = if sent == 2 { &input[2..] } else { &input[3..] };
        unsafe { SendInput(releases, size_of::<INPUT>() as i32) };
    }
    Err(format!(
        "SendInput sent {sent}/{} events ({}); UIPI is possible.",
        input.len(),
        error(&err)
    ))
}

fn paste(gesture: Gesture, root: HWND, owner: HWND, terminal: bool) -> Result<(), Box<dyn Error>> {
    if !valid_target(gesture, root) {
        println!(
            "Paste skipped because the foreground window, mouse gesture, or modifiers changed."
        );
        return Ok(());
    }
    let payload = clipboard::payload(terminal)?;
    let restore = clipboard::stage(owner, &payload)?;
    if !restore.still_current() {
        return Ok(());
    }
    if valid_target(gesture, root) {
        match send_paste() {
            Ok(()) => {
                println!(
                    "Sent Ctrl+V. Waiting {} seconds to restore.",
                    RESTORE_NO_READ_DELAY.as_secs()
                );
                thread::sleep(RESTORE_NO_READ_DELAY);
            }
            Err(err) => eprintln!("{err}"),
        }
    } else {
        println!("Paste cancelled after staging because the target or input state changed.");
    }
    restore.restore(owner)?;
    Ok(())
}

fn print_snapshot(info: &Snapshot) {
    if info.chain.is_empty() {
        println!("Hit unavailable");
    }
    for (depth, node) in info.chain.iter().enumerate() {
        let label = if depth == 0 {
            "Hit".into()
        } else {
            format!("Ancestor {depth}")
        };
        print_node(&label, node);
    }
    if let Some(focus) = &info.focus {
        print_node("Focus", focus);
    } else {
        println!("Focus unavailable");
    }
    for err in &info.errors {
        println!("UIA {err}");
    }
}

fn controller(worker: Worker, gestures: Receiver<Gesture>, clipboard_owner: Option<isize>) {
    let own_level = target::own_integrity().map_err(|err| error(&err));
    if let Err(err) = &own_level {
        eprintln!("Own integrity lookup: {err}");
    }
    while let Ok(gesture) = gestures.recv() {
        let (child, root, window_pid) = target::window_at(gesture.point);
        let window = target::process(window_pid);
        let is_paste = clipboard_owner.is_some();
        let mut release = None;
        if is_paste {
            if !wait_for_release(gesture) {
                println!(
                    "Paste skipped because the second mouse press did not end within the gesture."
                );
                continue;
            }
            release = Some(Instant::now());
            thread::sleep(AFTER_MOUSE_UP_DELAY);
        }
        let info = worker.lookup(gesture.point);
        println!("Double-click ({}, {})", gesture.point.x, gesture.point.y);
        print_snapshot(&info);

        let hit_pid = info
            .chain
            .first()
            .and_then(|hit| hit.pid)
            .unwrap_or(window_pid);
        let hit_process = (hit_pid != window_pid).then(|| target::process(hit_pid));
        let owner = hit_process.as_ref().unwrap_or(&window);
        let own = own_level.as_ref().ok().copied();
        let elevated = |process: &target::Process| {
            process
                .integrity
                .zip(own)
                .map(|(theirs, ours)| theirs > ours)
        };
        println!(
            "Hit owner PID={hit_pid} exe={:?} integrity={:?} our_integrity={own:?} elevated_vs_ours={:?}",
            owner.exe,
            owner.integrity,
            elevated(owner)
        );
        if hit_pid != window_pid {
            println!(
                "Window owner PID={window_pid} exe={:?} integrity={:?} elevated_vs_ours={:?}",
                window.exe,
                window.integrity,
                elevated(&window)
            );
        }
        for err in window
            .errors
            .iter()
            .chain(hit_process.iter().flat_map(|process| &process.errors))
        {
            println!("Process lookup {err}");
        }

        let mut facts = Facts {
            point: (gesture.point.x, gesture.point.y),
            chain: &info.chain,
            focus: info.focus.as_ref(),
            hit_exe: owner.exe.as_deref(),
            window_exe: window.exe.as_deref(),
            shell_view: None,
        };
        let mut path = None;
        if let Some(kind) = target::shell_kind(root) {
            match target::shell_path(kind, child, root) {
                Ok(Some(folder)) => {
                    facts.shell_view = Some(kind);
                    path = Some(folder);
                }
                Ok(None) => (),
                Err(err) => println!("Shell view lookup: {}", error(&err)),
            }
        }
        let (verdict, reason) = policy::verdict(&facts);
        println!("Verdict {} ({reason})", verdict.label());
        if matches!(
            verdict,
            Verdict::Desktop | Verdict::ExplorerFolderBackground
        ) {
            match path {
                Some(Ok(path)) => println!("Folder path: {}", path.display()),
                Some(Err(err)) => println!("Folder path unavailable: {err}"),
                None => (),
            }
        }

        if let Some(owner_hwnd) =
            clipboard_owner.filter(|_| matches!(verdict, Verdict::Editable | Verdict::Terminal))
        {
            if elevated(owner) == Some(true)
                || elevated(&window) == Some(true)
                || info
                    .errors
                    .iter()
                    .chain(window.errors.iter())
                    .chain(hit_process.iter().flat_map(|process| &process.errors))
                    .any(|err| err.contains("access denied (possible UIPI)"))
            {
                println!(
                    "Paste skipped because access is denied or the target runs at higher integrity (UIPI)."
                );
                continue;
            }
            let terminal = verdict == Verdict::Terminal;
            if let Some(released) = release.filter(|_| terminal) {
                settle_terminal(released);
            }
            if let Err(err) = paste(gesture, root, HWND(owner_hwnd as *mut _), terminal) {
                eprintln!("Paste failed: {err}");
            }
        }
    }
}

fn run_hook(paste: bool) -> Result<(), Box<dyn Error>> {
    let worker = Worker::start()?;
    let (sender, receiver) = mpsc::sync_channel(0);
    unsafe {
        let module = GetModuleHandleW(PCWSTR::null())?;
        let instance = HINSTANCE(module.0);
        let owner = if paste {
            Some(CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("uia-paste"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )?)
        } else {
            None
        };
        let swapped = GetSystemMetrics(SM_SWAPBUTTON) != 0;
        HOOK.with_borrow_mut(|slot| {
            *slot = Some(HookState {
                sender,
                previous: None,
                primary_down: None,
                max_time: GetDoubleClickTime(),
                max_x: (GetSystemMetrics(SM_CXDOUBLECLK) / 2).max(0) as u32,
                max_y: (GetSystemMetrics(SM_CYDOUBLECLK) / 2).max(0) as u32,
                down_message: if swapped {
                    WM_RBUTTONDOWN
                } else {
                    WM_LBUTTONDOWN
                },
                up_message: if swapped { WM_RBUTTONUP } else { WM_LBUTTONUP },
            });
        });
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(instance), 0)?;
        let owner_handle = owner.map(|hwnd| hwnd.0 as isize);
        thread::spawn(move || controller(worker, receiver, owner_handle));

        println!(
            "{} mode ready. Double-click in the target app. Press Ctrl+C here to stop.",
            if paste { "Paste" } else { "Inspect" }
        );
        let mut msg = MSG::default();
        let loop_result = loop {
            match GetMessageW(&mut msg, None, 0, 0).0 {
                -1 => break Err(windows::core::Error::from_thread()),
                0 => break Ok(()),
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        };
        let _ = UnhookWindowsHookEx(hook);
        if let Some(owner) = owner {
            let _ = DestroyWindow(owner);
        }
        loop_result?;
    }
    Ok(())
}

fn selftest() -> Result<(), Box<dyn Error>> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point)? };
    let worker = Worker::start()?;
    println!(
        "Self-test: IUIAutomation ready. Cursor ({}, {})",
        point.x, point.y
    );
    let info = worker.lookup(point);
    print_snapshot(&info);
    if info.chain.is_empty() || info.focus.is_none() {
        return Err("self-test could not read both the hit and focused UIA elements".into());
    }
    if !clipboard::selftest()? {
        return Err("in-memory HGLOBAL clipboard round-trip failed".into());
    }
    println!("Self-test: in-memory HGLOBAL clipboard round-trip passed (2 formats, 1 skipped).");
    Ok(())
}

fn error(err: &windows::core::Error) -> String {
    if err.code() == E_ACCESSDENIED {
        format!("access denied (possible UIPI), {}", err.code())
    } else {
        err.to_string()
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // Align WindowFromPoint with the physical screen coordinates used by UIA and the hook.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)? };
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode] if mode == "inspect" => run_hook(false),
        [mode] if mode == "paste" => run_hook(true),
        [mode] if mode == "--selftest" => selftest(),
        _ => Err("usage: uia-paste inspect | paste | --selftest".into()),
    }
}
