mod classify;
mod clipboard;
mod folder;
mod observation;

use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler},
            DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener},
            LibraryLoader::GetModuleHandleW,
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, Error, Result, w},
};

use classify::{Verdict, classify};
use clipboard::{PixelSize, Snapshot, format_size};
use observation::{KeyEvent, WindowInfo};

const IMMEDIATE: Duration = Duration::from_millis(1_500);
const SHORTCUT_WINDOW: Duration = Duration::from_secs(15);
const OVERLAY_GRACE: Duration = Duration::from_secs(3);
const MATCH_WINDOW: Duration = Duration::from_secs(4);
// File header reads can take several seconds after the notification.
const MATCH_RETENTION: Duration = Duration::from_secs(8);
const POLL_MS: u32 = 100;
static STOP: AtomicBool = AtomicBool::new(false);

thread_local! {
    static STATE: RefCell<Watcher> = RefCell::default();
}

#[derive(Default)]
struct Watcher {
    key: Option<KeyEvent>,
    overlay: HWND,
    overlay_seen: Option<Instant>,
    overlay_consumed: bool,
    last_verdict: Option<(u32, Verdict)>,
    pairs: Pairs,
}

impl Watcher {
    fn keys(&mut self) {
        for key in observation::take_key_events() {
            println!(
                "t_ms={} KEY shortcut={:?} injected={} age_ms={}",
                timestamp_ms(),
                key.shortcut,
                key.injected,
                key.at.elapsed().as_millis()
            );
            if !key.injected {
                self.key = Some(key);
            }
        }
    }

    fn overlay_seen(&mut self, window: &WindowInfo, at: Instant) {
        if self.overlay != window.hwnd {
            println!(
                "t_ms={} OVERLAY visible=true window={window}",
                timestamp_ms()
            );
            self.overlay = window.hwnd;
            self.overlay_consumed = false;
        }
        self.overlay_seen = Some(at);
    }

    fn overlay_hidden(&mut self, at: Instant) {
        if !self.overlay.is_invalid() {
            println!(
                "t_ms={} OVERLAY visible=false hwnd={:p}",
                timestamp_ms(),
                self.overlay.0
            );
            self.overlay = HWND::default();
            self.overlay_seen = Some(at);
        }
    }

    fn on_clipboard(
        &mut self,
        snapshot: &Snapshot,
        foreground: &WindowInfo,
        at: Instant,
        stamp: u128,
    ) {
        let key = self
            .key
            .and_then(|key| Some((key.shortcut, at.checked_duration_since(key.at)?)));
        let overlay_age = self
            .overlay_seen
            .and_then(|seen| at.checked_duration_since(seen));
        let mut signals = snapshot.signals();
        signals.recent_overlay =
            !self.overlay_consumed && overlay_age.is_some_and(|age| age <= OVERLAY_GRACE);
        signals.fresh_shortcut = key
            .filter(|(_, age)| *age <= SHORTCUT_WINDOW)
            .map(|(key, _)| key);
        signals.instant_shortcut = key.filter(|(_, age)| *age <= IMMEDIATE).map(|(key, _)| key);

        // Posted notifications can describe the same current snapshot if writes happened in a burst.
        let observed = self
            .last_verdict
            .filter(|(seq, _)| snapshot.error.is_none() && *seq == snapshot.sequence);
        let verdict = observed.map_or_else(|| classify(signals), |(_, verdict)| verdict);
        println!(
            "t_ms={stamp} CLIPBOARD {snapshot} size={} foreground={foreground} {} shortcut={:?} overlay_hwnd={:p} overlay_age_ms={:?} repeat={} verdict={} reason={:?}",
            format_size(snapshot.dimensions),
            snapshot.markers(),
            key.map(|(key, age)| (key, age.as_millis())),
            self.overlay.0,
            overlay_age.map(|age| age.as_millis()),
            observed.is_some(),
            verdict.label(),
            verdict.reason(),
        );
        if observed.is_some() {
            return;
        }
        if snapshot.error.is_none() {
            self.last_verdict = Some((snapshot.sequence, verdict));
        }
        if matches!(verdict, Verdict::Screenshot(_)) {
            self.key = None;
            self.overlay_consumed = true;
        }
        if snapshot.error.is_none()
            && let Some(size) = snapshot.dimensions
        {
            self.pairs.clipboard((snapshot.sequence, verdict), at, size);
        }
    }
}

#[derive(Default)]
struct Pairs {
    clipboard: Vec<(Instant, PixelSize, (u32, Verdict))>,
    files: Vec<(Instant, PixelSize, PathBuf)>,
}

impl Pairs {
    fn clipboard(&mut self, clip: (u32, Verdict), at: Instant, size: PixelSize) {
        self.prune();
        if let Some(index) = closest(&self.files, at, size) {
            let (file_at, _, path) = self.files.swap_remove(index);
            print_match(clip, at, &path, file_at, size);
        } else {
            self.clipboard.push((at, size, clip));
        }
    }

    fn file(&mut self, path: PathBuf, at: Instant, size: PixelSize) {
        self.prune();
        if let Some(index) = closest(&self.clipboard, at, size) {
            let (clipboard_at, _, clip) = self.clipboard.swap_remove(index);
            print_match(clip, clipboard_at, &path, at, size);
        } else {
            self.files.push((at, size, path));
        }
    }

    fn prune(&mut self) {
        self.files
            .retain(|(at, _, _)| at.elapsed() <= MATCH_RETENTION);
        self.clipboard
            .retain(|(at, _, _)| at.elapsed() <= MATCH_RETENTION);
    }
}

fn closest<T>(items: &[(Instant, PixelSize, T)], at: Instant, size: PixelSize) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, (time, dimensions, _))| {
            let gap = at.max(*time).duration_since(at.min(*time));
            (*dimensions == size && gap <= MATCH_WINDOW).then_some((gap, index))
        })
        .min()
        .map(|(_, index)| index)
}

fn print_match(
    (sequence, verdict): (u32, Verdict),
    clipboard_at: Instant,
    path: &std::path::Path,
    file_at: Instant,
    size: PixelSize,
) {
    let gap = clipboard_at
        .max(file_at)
        .duration_since(clipboard_at.min(file_at));
    println!(
        "t_ms={} MATCH same_event=likely seq={sequence} clipboard_verdict={} path={path:?} size={} delta_ms={}",
        timestamp_ms(),
        verdict.label(),
        format_size(Some(size)),
        gap.as_millis()
    );
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn poll_overlay() {
    let now = Instant::now();
    let visible = observation::visible_overlay();
    STATE.with_borrow_mut(|state| {
        state.keys();
        if let Some(window) = &visible {
            state.overlay_seen(window, now);
        } else {
            state.overlay_hidden(now);
        }
    });
}

fn poll_folder(events: &Receiver<folder::Event>) {
    for event in events.try_iter() {
        match event {
            folder::Event::Status(status) => println!("t_ms={} WATCH {status}", timestamp_ms()),
            folder::Event::Image {
                path,
                at,
                dimensions,
            } => {
                println!(
                    "t_ms={} FILE path={path:?} size={} age_ms={}",
                    timestamp_ms(),
                    format_size(dimensions),
                    at.elapsed().as_millis()
                );
                if let Some(size) = dimensions {
                    STATE.with_borrow_mut(|state| state.pairs.file(path, at, size));
                }
            }
        }
    }
}

unsafe extern "system" fn foreground_event(
    _: HWINEVENTHOOK,
    _: u32,
    hwnd: HWND,
    _: i32,
    _: i32,
    _: u32,
    event_tick: u32,
) {
    let window = WindowInfo::read(hwnd);
    let overlay = observation::is_overlay(&window);
    let now = Instant::now();
    println!(
        "t_ms={} FOREGROUND event_tick={event_tick} window={window} overlay_candidate={overlay}",
        timestamp_ms()
    );
    if overlay {
        STATE.with_borrow_mut(|state| state.overlay_seen(&window, now));
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLIPBOARDUPDATE {
        let stamp = timestamp_ms();
        poll_overlay();
        let at = Instant::now();
        let foreground = WindowInfo::read(unsafe { GetForegroundWindow() });
        let snapshot = Snapshot::read(Some(window), true);
        STATE.with_borrow_mut(|state| state.on_clipboard(&snapshot, &foreground, at, stamp));
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

unsafe extern "system" fn console_handler(event: u32) -> BOOL {
    if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        STOP.store(true, Ordering::Relaxed);
        return true.into();
    }
    false.into()
}

fn run() -> Result<()> {
    let (stop, stop_receiver) = mpsc::channel();
    let (events, event_receiver) = mpsc::channel();
    let worker = match folder::known_path() {
        Ok(path) => Some(std::thread::spawn(move || {
            folder::watch(path, stop_receiver, events)
        })),
        Err(error) => {
            println!(
                "t_ms={} WATCH Screenshots folder unavailable: {error}",
                timestamp_ms()
            );
            None
        }
    };
    let result = run_message_loop(&event_receiver);
    drop(stop);
    if let Some(worker) = worker {
        worker.join().expect("folder watcher panicked");
    }
    poll_folder(&event_receiver);
    println!("t_ms={} STOP", timestamp_ms());
    result
}

fn run_message_loop(events: &Receiver<folder::Event>) -> Result<()> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let instance = HINSTANCE(module.0);
        let name = w!("SnipDetectMessageWindow");
        let class = WNDCLASSW {
            hInstance: instance,
            lpszClassName: name,
            lpfnWndProc: Some(window_proc),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(Error::from_thread());
        }
        let window = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            name,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )?;
        let result = listen(window, instance, events);
        let _ = DestroyWindow(window);
        let _ = UnregisterClassW(name, Some(instance));
        result
    }
}

fn listen(window: HWND, instance: HINSTANCE, events: &Receiver<folder::Event>) -> Result<()> {
    unsafe {
        AddClipboardFormatListener(window)?;
        if SetTimer(Some(window), 1, POLL_MS, None) == 0 {
            return Err(Error::from_thread());
        }
        observation::prime_modifiers();
        let keyboard = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(observation::keyboard_hook),
            Some(instance),
            0,
        );
        let foreground = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let console = SetConsoleCtrlHandler(Some(console_handler), true);
        println!(
            "t_ms={} START clipboard=true keyboard={keyboard:?} foreground_hook={} ctrl_handler={console:?} foreground={}",
            timestamp_ms(),
            !foreground.is_invalid(),
            WindowInfo::read(GetForegroundWindow())
        );
        let result = loop {
            let mut message = MSG::default();
            match GetMessageW(&mut message, None, 0, 0).0 {
                -1 => break Err(Error::from_thread()),
                0 => break Ok(()),
                _ => {}
            }
            if message.hwnd == window && message.message == WM_TIMER {
                poll_overlay();
                poll_folder(events);
                if STOP.load(Ordering::Relaxed) {
                    break Ok(());
                }
            } else {
                DispatchMessageW(&message);
            }
        };
        let _ = KillTimer(Some(window), 1);
        let _ = RemoveClipboardFormatListener(window);
        if let Ok(keyboard) = keyboard {
            let _ = UnhookWindowsHookEx(keyboard);
        }
        if !foreground.is_invalid() {
            let _ = UnhookWinEvent(foreground);
        }
        if console.is_ok() {
            let _ = SetConsoleCtrlHandler(Some(console_handler), false);
        }
        result
    }
}

fn main() -> std::process::ExitCode {
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => match run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                std::process::ExitCode::FAILURE
            }
        },
        [arg] if arg == "--once-dump" => {
            let snapshot = Snapshot::read(None, false);
            println!("t_ms={} DUMP {snapshot}", timestamp_ms());
            if snapshot.error.is_none() {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            }
        }
        _ => {
            eprintln!("Usage: snip-detect [--once-dump]");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_nearest_same_size_in_either_order_within_the_window() {
        let at = Instant::now();
        let events = [
            (at + Duration::from_secs(7), (20, 30), ()),
            (at + Duration::from_secs(1), (20, 40), ()),
            (at + Duration::from_secs(1), (20, 30), ()),
            (at - Duration::from_millis(200), (20, 30), ()),
        ];
        assert_eq!(closest(&events, at, (20, 30)), Some(3));
        assert_eq!(closest(&events, at, (40, 50)), None);
        assert_eq!(closest(&events[..1], at, (20, 30)), None);
    }

    #[test]
    #[ignore = "live desktop smoke test"]
    fn watcher_starts_and_shuts_down() {
        let stop = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            STOP.store(true, Ordering::Relaxed);
        });
        run().unwrap();
        stop.join().unwrap();
    }
}
