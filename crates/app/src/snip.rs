//! Windows screenshot detection. Wait on [Snip::wake_handle], then drain events on the
//! UI thread. Clipboard reads are requested from the existing clipboard thread. The
//! keyboard thread only observes input, the WinEvent thread tracks the overlay and the
//! folder thread watches and decodes PNGs. The controller correlates signals, decodes
//! clipboard images and writes the optional log.
//!
//! An image-only clipboard update is evidence, never proof. Cancellation, file pairing,
//! process names and monitor coverage are heuristics. See examples/SNIP_TEST.md.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use igorio_kit::image::Image;
use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_OBJECT_0, WAIT_TIMEOUT,
    WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ACTION_ADDED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_READ,
    GetFileInformationByHandle, ReadDirectoryChangesW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_S,
    VK_SHIFT, VK_SNAPSHOT,
};
use windows::Win32::UI::Shell::{FOLDERID_Screenshots, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_SYSTEM_FOREGROUND, GA_ROOT, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetForegroundWindow,
    GetPhysicalCursorPos, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, HC_ACTION,
    IsIconic, IsWindowVisible, IsZoomed, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, LLKHF_INJECTED,
    LLKHF_LOWER_IL_INJECTED, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, OBJID_WINDOW,
    PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW, QS_ALLINPUT, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP,
    WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WS_EX_TOPMOST,
};
use windows::core::{Error, PWSTR, Result};

use crate::clipboard::{
    self, Clipboard, ClipboardChange, ImageFormat, ImageMetadata, ImageRead, ScreenshotClipboard,
};
use crate::hook::{HookStats, now};

const INSTANT_CAPTURE: f64 = 1.5;
const OWNER_SHORTCUT: f64 = 15.0;
const OVERLAY_GRACE: f64 = 3.0;
const UNKNOWN_OWNER_GRACE: f64 = 0.25;
const SNIP_FILE_WAIT: Duration = Duration::from_millis(400);
const SIGNAL_SETTLE: Duration = Duration::from_millis(100);
const PAIR_WINDOW: f64 = 4.0;
const PAIR_RETENTION: f64 =
    PAIR_WINDOW + FILE_TIMEOUT.as_secs_f64() + CLIPBOARD_DEADLINE.as_secs_f64();
const OVERLAY_COVERAGE_PERCENT: i64 = 90;
const MAX_OVERLAY_WINDOWS: usize = 16;
const BASE_DPI: f64 = 96.0;

const CLIPBOARD_DEADLINE: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(25);
const STOP_TIMEOUT: Duration = Duration::from_millis(1800);
const OVERLAY_CHECK_MS: u32 = 250;
const SIGNAL_CAP: usize = 64;
const OUTPUT_CAP: usize = 32;
const PAIR_CAP: usize = 32;
const WM_PROBE: u32 = WM_APP + 1;
const STALL_STRIKES: u8 = 2;
const FIRST_KEY: u8 = 8;
const KEY_COUNT: usize = 256 - FIRST_KEY as usize;
const IMAGE_CAP: usize = 128 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = clipboard::SNAPSHOT_CAP as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Snip,
    PrintScreen,
    WindowPrintScreen,
    FullScreenFile,
    /// An image copied in another app, carried when "Also carry copied images" is on. It is
    /// not a screenshot, so it is never kept.
    Copied,
}

pub enum SnipEvent {
    CaptureUiShown {
        time: f64,
    },
    CaptureUiGone {
        time: f64,
    },
    Captured {
        image: Arc<Image>,
        scale: f64,
        captured_at: SystemTime,
        source: Source,
        saved_file: Option<PathBuf>,
    },
    Ignored {
        reason: String,
    },
}

pub struct Snip {
    clipboard: ScreenshotClipboard,
    folder: Option<PathBuf>,
    output: Arc<Output>,
    copies: Arc<AtomicBool>,
    run: Option<Run>,
    stall: StallCheck,
}

impl Snip {
    /// The app must keep the clipboard thread running until this detector is shut down.
    pub fn start(clipboard: &Clipboard) -> Result<Self> {
        Self::starter(clipboard)()
    }

    /// Take a reader on the UI thread, then run the bounded hook startup on a worker.
    pub fn starter(clipboard: &Clipboard) -> impl FnOnce() -> Result<Self> + Send + use<> {
        let reader = clipboard.screenshot_reader();
        move || Self::start_with_folder(reader, None)
    }

    /// Watch an explicit directory, for isolated tests.
    pub fn start_in_folder(clipboard: &Clipboard, path: PathBuf) -> Result<Self> {
        Self::start_with_folder(clipboard.screenshot_reader(), Some(path))
    }

    fn start_with_folder(clipboard: ScreenshotClipboard, folder: Option<PathBuf>) -> Result<Self> {
        now(); // Initialise the epoch before any callback.
        let mut detector = Self {
            clipboard,
            folder,
            output: Arc::new(Output {
                wake: Event::new(false)?,
                queue: Mutex::new(VecDeque::with_capacity(OUTPUT_CAP)),
            }),
            copies: Arc::default(),
            run: None,
            stall: StallCheck::default(),
        };
        detector.restart()?;
        Ok(detector)
    }

    /// "Also carry copied images": carry an image copied in another app too. Off at start,
    /// kept across restart, read at each clipboard update.
    pub fn set_carry_copies(&self, on: bool) {
        self.copies.store(on, Relaxed);
    }

    /// Auto-reset event. It stays open and has the same handle across restart.
    pub fn wake_handle(&self) -> HANDLE {
        self.output.wake.0
    }

    pub fn drain(&self, out: &mut Vec<SnipEvent>) {
        out.reserve(OUTPUT_CAP);
        out.extend(
            self.output
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..),
        );
    }

    /// Whether all worker threads are running. A silently removed hook still needs
    /// [Self::seems_stalled] to detect it.
    pub fn is_alive(&self) -> bool {
        self.run
            .as_ref()
            .is_some_and(|run| run.threads.iter().all(|t| !t.is_finished()))
    }

    /// Call about once a second. A missing pump acknowledgement or two sampled changes
    /// in held keys without any keyboard callbacks suggests a stall. Short key presses
    /// between samples can go undetected. Elevated windows can cause false alarms.
    pub fn seems_stalled(&mut self) -> bool {
        let Some(run) = &self.run else { return true };
        let callbacks = run.keyboard.callbacks.load(Relaxed);
        let probes = run.keyboard.probes.load(Relaxed);
        let mut held = [false; KEY_COUNT];
        for (key, state) in (FIRST_KEY..=u8::MAX).zip(&mut held) {
            *state = key_down(VIRTUAL_KEY(u16::from(key)));
        }
        let changed = self.stall.held.is_some_and(|last| last != held);
        if callbacks != self.stall.callbacks {
            self.stall.strikes = 0;
        } else if changed {
            self.stall.strikes = self.stall.strikes.saturating_add(1);
        }
        let pump_stalled = self.stall.probe_pending && self.stall.probes == probes;
        self.stall.held = Some(held);
        self.stall.callbacks = callbacks;
        self.stall.probes = probes;
        // SAFETY: a scalar-only message to the hook thread, whose queue exists at startup.
        self.stall.probe_pending =
            unsafe { PostThreadMessageW(run.keyboard_id, WM_PROBE, WPARAM(0), LPARAM(0)) }.is_ok();
        pump_stalled || !self.stall.probe_pending || self.stall.strikes >= STALL_STRIKES
    }

    pub fn hook_stats(&self) -> HookStats {
        self.run.as_ref().map_or(
            HookStats {
                callbacks: 0,
                max_callback: Duration::ZERO,
                dropped: 0,
            },
            |run| HookStats {
                callbacks: run.keyboard.callbacks.load(Relaxed),
                max_callback: Duration::from_nanos(run.keyboard.max_ns.load(Relaxed)),
                dropped: run.keyboard.dropped.load(Relaxed),
            },
        )
    }

    /// Replace the worker set. Pending signals are dropped and an active overlay is released.
    pub fn restart(&mut self) -> Result<()> {
        if !self.stop() {
            return Err(Error::new(E_FAIL, "screenshot workers did not stop"));
        }
        self.stall = StallCheck::default();
        self.run = Some(Run::start(
            self.clipboard.clone(),
            self.folder.clone(),
            self.output.clone(),
            self.copies.clone(),
        )?);
        Ok(())
    }

    /// True if every detector thread joined successfully within the shared shutdown budget.
    pub fn shutdown(mut self) -> bool {
        self.stop()
    }

    fn stop(&mut self) -> bool {
        self.run.take().is_none_or(|mut run| run.stop())
    }
}

impl Drop for Snip {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct StallCheck {
    held: Option<[bool; KEY_COUNT]>,
    callbacks: u64,
    probes: u64,
    probe_pending: bool,
    strikes: u8,
}

struct Event(HANDLE);

// SAFETY: kernel events can be signalled, waited on and closed from any thread.
unsafe impl Send for Event {}
// SAFETY: the handle is owned, and simultaneous SetEvent and wait are allowed.
unsafe impl Sync for Event {}

impl Event {
    fn new(manual_reset: bool) -> Result<Self> {
        // SAFETY: no borrowed pointers. The caller owns the resulting handle.
        unsafe { CreateEventW(None, manual_reset, false, None) }.map(Self)
    }

    fn set(&self) {
        // SAFETY: the handle is live for the duration of this borrow.
        let _ = unsafe { SetEvent(self.0) };
    }

    fn wait(&self, duration: Duration) -> bool {
        // SAFETY: the event outlives the wait.
        unsafe { WaitForSingleObject(self.0, millis(duration)) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: exclusively owned, created by Event::new.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn millis(duration: Duration) -> u32 {
    duration.as_millis().min(u32::MAX as u128 - 1) as u32
}

struct Output {
    wake: Event,
    queue: Mutex<VecDeque<SnipEvent>>,
}

impl Output {
    fn push(&self, event: SnipEvent) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() == OUTPUT_CAP {
            if matches!(event, SnipEvent::Ignored { .. }) {
                return;
            }
            let evict = queue
                .iter()
                .position(|e| matches!(e, SnipEvent::Ignored { .. }))
                .or_else(|| {
                    queue
                        .iter()
                        .position(|e| matches!(e, SnipEvent::Captured { .. }))
                })
                .unwrap_or(0);
            queue.remove(evict);
        }
        let wake = queue.is_empty();
        queue.push_back(event);
        drop(queue);
        if wake {
            self.wake.set();
        }
    }
}

struct Run {
    stop_event: Arc<Event>,
    threads: Vec<JoinHandle<()>>,
    keyboard: Arc<KeyboardStats>,
    keyboard_id: u32,
}

impl Run {
    fn start(
        clipboard: ScreenshotClipboard,
        folder: Option<PathBuf>,
        output: Arc<Output>,
        copies: Arc<AtomicBool>,
    ) -> Result<Self> {
        // A writer can create the first file before the new worker is scheduled.
        // Establish the recovery scan cutoff before starting any workers.
        let started = SystemTime::now();
        let last_change = clipboard.latest();
        let (tx, rx) = mpsc::sync_channel(SIGNAL_CAP);
        let mut run = Self {
            stop_event: Arc::new(Event::new(true)?),
            threads: Vec::with_capacity(4),
            keyboard: Arc::new(KeyboardStats::default()),
            keyboard_id: 0,
        };
        let stats = run.keyboard.clone();
        let keys = tx.clone();
        let (ready, installed) = mpsc::channel();
        run.spawn("snip-keyboard", move |stop| {
            keyboard_thread(stop, keys, stats, ready)
        })?;
        run.keyboard_id = installed
            .recv_timeout(STOP_TIMEOUT)
            .map_err(|_| Error::new(E_FAIL, "keyboard hook startup timed out"))??;

        let observed = tx.clone();
        let (ready, installed) = mpsc::channel();
        run.spawn("snip-windows", move |stop| {
            overlay_thread(stop, observed, ready)
        })?;
        installed
            .recv_timeout(STOP_TIMEOUT)
            .map_err(|_| Error::new(E_FAIL, "WinEvent hook startup timed out"))??;

        let test_folder = folder.is_some();
        run.spawn("snip-folder", move |stop| {
            folder_thread(&stop, &tx, folder.as_deref(), started)
        })?;
        run.spawn("snip-controller", move |stop| {
            let mut controller = Controller::new(output, test_folder);
            controller.copies = copies;
            controller.run(&stop, rx, &clipboard, last_change);
        })?;
        Ok(run)
    }

    fn spawn(&mut self, name: &str, run: impl FnOnce(Arc<Event>) + Send + 'static) -> Result<()> {
        let stop = self.stop_event.clone();
        self.threads.push(
            thread::Builder::new()
                .name(name.into())
                .spawn(move || run(stop))
                .map_err(|e| Error::new(E_FAIL, e.to_string()))?,
        );
        Ok(())
    }

    fn stop(&mut self) -> bool {
        self.stop_event.set();
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut joined = true;
        for thread in self.threads.drain(..) {
            let handle = HANDLE(thread.as_raw_handle());
            // SAFETY: JoinHandle owns the thread handle through the wait.
            let done = unsafe {
                WaitForSingleObject(
                    handle,
                    millis(deadline.saturating_duration_since(Instant::now())),
                )
            };
            joined &= done == WAIT_OBJECT_0 && thread.join().is_ok();
        }
        joined
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            self.stop();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shortcut {
    Print,
    AltPrint,
    WinPrint,
    Snip,
    Escape,
}

#[derive(Debug, Clone, Copy)]
struct Key {
    shortcut: Shortcut,
    time: f64,
    point: Option<POINT>,
    injected: bool,
}

enum Signal {
    Key(Key),
    Overlay {
        shown: bool,
        time: f64,
        scale: f64,
        class: String,
        process: String,
    },
    OverlayRejected {
        time: f64,
        event: u32,
        class: String,
        process: String,
    },
    File(Capture),
    WatchingFolder(PathBuf),
    Folder(String),
}

#[derive(Default)]
struct KeyboardStats {
    callbacks: AtomicU64,
    max_ns: AtomicU64,
    dropped: AtomicU64,
    probes: AtomicU64,
}

struct Keyboard {
    tx: SyncSender<Signal>,
    stats: Arc<KeyboardStats>,
    print_down: Cell<bool>,
    s_down: Cell<bool>,
}

thread_local! {
    static KEYBOARD: RefCell<Option<Keyboard>> = const { RefCell::new(None) };
}

fn key_down(key: VIRTUAL_KEY) -> bool {
    // SAFETY: scalar virtual key. Only the high (currently held) bit matters.
    unsafe { GetAsyncKeyState(i32::from(key.0)) < 0 }
}

fn cursor() -> Option<POINT> {
    let mut point = POINT::default();
    // SAFETY: writable local POINT.
    unsafe { GetPhysicalCursorPos(&mut point) }
        .ok()
        .map(|()| point)
}

unsafe extern "system" fn keyboard_callback(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let started = Instant::now();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: for HC_ACTION, l points to a keyboard hook record until return.
            let info = unsafe { *(l.0 as *const KBDLLHOOKSTRUCT) };
            KEYBOARD.with_borrow(|context| {
                let Some(ctx) = context else { return };
                let pressed = matches!(w.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                let vk = VIRTUAL_KEY(info.vkCode as u16);
                let shortcut = key_shortcut(ctx, vk, pressed, info);
                if let Some(shortcut) = shortcut {
                    let event = Key {
                        shortcut,
                        time: now(),
                        point: if shortcut == Shortcut::Escape {
                            None
                        } else {
                            cursor()
                        },
                        injected: info.flags.0 & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED).0 != 0,
                    };
                    if ctx.tx.try_send(Signal::Key(event)).is_err() {
                        ctx.stats.dropped.fetch_add(1, Relaxed);
                    }
                }
                ctx.stats.callbacks.fetch_add(1, Relaxed);
                ctx.stats.max_ns.fetch_max(
                    started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                    Relaxed,
                );
            });
        }));
    }
    // SAFETY: forward the system's arguments unchanged, even after a panic.
    unsafe { CallNextHookEx(None, code, w, l) }
}

fn key_shortcut(
    ctx: &Keyboard,
    vk: VIRTUAL_KEY,
    pressed: bool,
    info: KBDLLHOOKSTRUCT,
) -> Option<Shortcut> {
    match vk {
        VK_ESCAPE => return pressed.then_some(Shortcut::Escape),
        // Some keyboards send only a Print Screen release. Emit on the first observed
        // edge. Track repeat for these keys alone, never for a modifier.
        VK_SNAPSHOT if ctx.print_down.replace(pressed) => return None,
        VK_S if ctx.s_down.replace(pressed) || !pressed => return None,
        VK_SNAPSHOT | VK_S => {}
        _ => return None,
    }
    // The async state for the triggering key is updated after the callback. These are
    // other keys. The Alt bit also covers a Print Screen system-key event.
    let win = key_down(VK_LWIN) || key_down(VK_RWIN);
    let alt = key_down(VK_MENU) || info.flags.contains(LLKHF_ALTDOWN);
    let shift = key_down(VK_SHIFT);
    if key_down(VK_CONTROL) {
        return None;
    }
    match (vk, win, alt, shift) {
        (VK_S, true, false, true) => Some(Shortcut::Snip),
        (VK_SNAPSHOT, true, false, false) => Some(Shortcut::WinPrint),
        (VK_SNAPSHOT, false, true, false) => Some(Shortcut::AltPrint),
        (VK_SNAPSHOT, false, false, false) => Some(Shortcut::Print),
        _ => None,
    }
}

fn keyboard_thread(
    stop: Arc<Event>,
    tx: SyncSender<Signal>,
    stats: Arc<KeyboardStats>,
    ready: mpsc::Sender<Result<u32>>,
) {
    let id = make_message_queue();
    KEYBOARD.set(Some(Keyboard {
        tx,
        stats: stats.clone(),
        print_down: Cell::new(false),
        s_down: Cell::new(false),
    }));
    // SAFETY: own module, system ABI callback, global low-level hook installed on this thread.
    let installed = unsafe { GetModuleHandleW(None) }.and_then(|module| {
        // SAFETY: callback lives for the entire program, installed and removed on this thread.
        unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_callback),
                Some(module.into()),
                0,
            )
        }
    });
    let hook = match installed {
        Ok(hook) => hook,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _ = ready.send(Ok(id));
    pump(
        &stop,
        u32::MAX,
        |message| {
            if message.message == WM_PROBE {
                stats.probes.fetch_add(1, Relaxed);
            }
        },
        || {},
    );
    // SAFETY: the hook was installed on this thread, and is removed before callback context clears.
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    KEYBOARD.set(None);
}

fn make_message_queue() -> u32 {
    let mut message = MSG::default();
    // SAFETY: writable local. Peek creates this thread's queue before startup acknowledges.
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: no arguments.
    unsafe { GetCurrentThreadId() }
}

fn pump(
    stop: &Event,
    timeout: u32,
    mut on_message: impl FnMut(&MSG),
    mut maintenance: impl FnMut(),
) {
    loop {
        // SAFETY: live stop event, single handle carried through the wait.
        let wake = unsafe {
            MsgWaitForMultipleObjectsEx(Some(&[stop.0]), timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
        if wake == WAIT_OBJECT_0 {
            return;
        }
        if wake == WAIT_TIMEOUT {
            maintenance();
            continue;
        }
        if wake.0 != WAIT_OBJECT_0.0 + 1 {
            return;
        }
        let mut message = MSG::default();
        // SAFETY: writes a message to a local. Callback reentrancy is handled by the hook.
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            if message.message == WM_QUIT || stop.wait(Duration::ZERO) {
                return;
            }
            on_message(&message);
            // SAFETY: dispatch a message read on this thread.
            unsafe { DispatchMessageW(&message) };
        }
        maintenance();
    }
}

struct OverlayObserver {
    tx: SyncSender<Signal>,
    windows: Vec<(HWND, String, String)>,
    published: bool,
    scale: f64,
    debug: bool,
}

thread_local! {
    static OVERLAY: RefCell<Option<OverlayObserver>> = const { RefCell::new(None) };
}

unsafe extern "system" fn window_event(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if child == 0 && object == OBJID_WINDOW.0 && !hwnd.is_invalid() {
            OVERLAY.with_borrow_mut(|observer| {
                if let Some(observer) = observer {
                    observer.observe(event, hwnd);
                }
            });
        }
    }));
}

impl OverlayObserver {
    fn observe(&mut self, event: u32, hwnd: HWND) {
        if matches!(event, EVENT_OBJECT_HIDE | EVENT_OBJECT_DESTROY) {
            self.windows.retain(|(window, _, _)| *window != hwnd);
        } else if self.windows.len() < MAX_OVERLAY_WINDOWS
            && !self.windows.iter().any(|(window, _, _)| *window == hwnd)
        {
            let geometry = overlay_geometry(hwnd);
            if !geometry && !(self.debug && event == EVENT_SYSTEM_FOREGROUND) {
                self.refresh();
                return;
            }
            // SAFETY: writes the owning PID to a local, if the window still exists.
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            let process = process_name(pid);
            if capture_process(&process) {
                let mut class = [0u16; 256];
                // SAFETY: class points to a writable local buffer.
                let len = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
                let class = String::from_utf16_lossy(&class[..len]);
                if geometry {
                    if self.windows.is_empty() {
                        self.scale = monitor_scale(cursor());
                    }
                    self.windows.push((hwnd, class, process));
                } else {
                    let _ = self.tx.try_send(Signal::OverlayRejected {
                        time: now(),
                        event,
                        class,
                        process,
                    });
                }
            }
        }
        self.refresh();
    }

    fn refresh(&mut self) {
        self.windows
            .retain(|(window, _, _)| overlay_geometry(*window));
        let shown = !self.windows.is_empty();
        if shown == self.published {
            return;
        }
        let (class, process) = self
            .windows
            .first()
            .map(|(_, class, process)| (class.clone(), process.clone()))
            .unwrap_or_default();
        if self
            .tx
            .try_send(Signal::Overlay {
                shown,
                time: now(),
                scale: self.scale,
                class,
                process,
            })
            .is_ok()
        {
            self.published = shown;
        }
    }
}

fn overlay_thread(stop: Arc<Event>, tx: SyncSender<Signal>, ready: mpsc::Sender<Result<()>>) {
    make_message_queue();
    OVERLAY.set(Some(OverlayObserver {
        tx,
        windows: Vec::new(),
        published: false,
        scale: 1.0,
        debug: debug_log_path().is_some(),
    }));
    let mut hooks = Vec::new();
    for (first, last) in [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
    ] {
        // SAFETY: the callback is static. Out-of-context callbacks run on this thread.
        let hook = unsafe {
            SetWinEventHook(
                first,
                last,
                None,
                Some(window_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_invalid() {
            let _ = ready.send(Err(Error::new(E_FAIL, "SetWinEventHook failed")));
            break;
        }
        hooks.push(hook);
    }
    if hooks.len() == 2 {
        let _ = ready.send(Ok(()));
        OVERLAY.with_borrow_mut(|observer| {
            if let Some(observer) = observer {
                // SAFETY: no arguments. Null is a valid "no foreground window" result.
                let hwnd = unsafe { GetForegroundWindow() };
                if !hwnd.is_invalid() {
                    observer.observe(EVENT_SYSTEM_FOREGROUND, hwnd);
                }
            }
        });
        // Recheck only handles already identified as capture UI, for missed hide events.
        pump(
            &stop,
            OVERLAY_CHECK_MS,
            |_| {},
            || {
                OVERLAY.with_borrow_mut(|observer| {
                    if let Some(observer) = observer {
                        observer.refresh();
                    }
                });
            },
        );
    }
    for hook in hooks {
        // SAFETY: unregister each hook on the thread that registered it.
        let _ = unsafe { UnhookWinEvent(hook) };
    }
    OVERLAY.set(None);
}

fn overlay_geometry(hwnd: HWND) -> bool {
    // SAFETY: scalar HWND queries. A vanished window fails the visibility or bounds check.
    let visible = unsafe {
        GetAncestor(hwnd, GA_ROOT) == hwnd
            && IsWindowVisible(hwnd).as_bool()
            && !IsIconic(hwnd).as_bool()
            && !IsZoomed(hwnd).as_bool()
    };
    // SAFETY: only reads the extended style; a vanished window has no topmost bit.
    if !visible || unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0 == 0 {
        return false;
    }
    let mut rect = RECT::default();
    // SAFETY: writes only to rect.
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }
    // SAFETY: the HWND is a scalar and the buffer is suitably sized and initialized.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: cbSize matches the writable MONITORINFO at &mut info.
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return false;
    }
    let screen = info.rcMonitor;
    let width = i64::from(rect.right.min(screen.right)) - i64::from(rect.left.max(screen.left));
    let height = i64::from(rect.bottom.min(screen.bottom)) - i64::from(rect.top.max(screen.top));
    if width * 100 < (i64::from(screen.right) - i64::from(screen.left)) * OVERLAY_COVERAGE_PERCENT
        || height * 100
            < (i64::from(screen.bottom) - i64::from(screen.top)) * OVERLAY_COVERAGE_PERCENT
    {
        return false;
    }
    let mut cloaked = 0u32;
    // SAFETY: DWMWA_CLOAKED writes a DWORD into the correctly sized local buffer.
    let _ = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
    };
    cloaked == 0
}

fn monitor_scale(point: Option<POINT>) -> f64 {
    let Some(point) = point else { return 1.0 };
    // SAFETY: scalar arguments, work area chosen from the physical cursor position.
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let (mut x, mut y) = (0, 0);
    // SAFETY: writable local UINTs for the two DPI values.
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_ok() && x != 0 {
        f64::from(x) / BASE_DPI
    } else {
        1.0
    }
}

fn process_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    // SAFETY: scalar PID and minimum query permissions. The returned handle is owned.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    // SAFETY: len describes the live writable path buffer.
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
    };
    // SAFETY: close the owned process handle exactly once.
    let _ = unsafe { CloseHandle(handle) };
    if result.is_err() {
        return String::new();
    }
    let path = String::from_utf16_lossy(&path[..len as usize]);
    path.rsplit('\\').next().unwrap_or_default().to_owned()
}

fn capture_process(name: &str) -> bool {
    [
        "ScreenClippingHost.exe",
        "SnippingTool.exe",
        "ScreenSketch.exe",
    ]
    .iter()
    .any(|exe| name.eq_ignore_ascii_case(exe))
}

const FOLDER_POLL_MS: u32 = 50;
const FOLDER_RECHECK: Duration = Duration::from_secs(1);
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(1);
const FILE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PENDING_FILES: usize = 32;
const RECENT_FILES: usize = 64;
const RECOVERY_SCAN_LIMIT: usize = 1024;

fn screenshots_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_owned());
    }
    // SAFETY: the known folder GUID is static; no caller-supplied token.
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Screenshots, KF_FLAG_DONT_VERIFY, None) }?;
    // SAFETY: successful result is a NUL-terminated shell allocation.
    let result = OsString::from_wide(unsafe { path.as_wide() }).into();
    // SAFETY: free the shell allocation exactly once.
    unsafe { CoTaskMemFree(Some(path.0.cast())) };
    Ok(result)
}

fn open_directory(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OVERLAPPED.0)
        .open(path)
}

fn identity(file: &File) -> Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file owns its handle and info is a writable local.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

fn folder_thread(
    stop: &Event,
    tx: &SyncSender<Signal>,
    override_path: Option<&Path>,
    started: SystemTime,
) {
    let mut backoff = RETRY_MIN;
    let mut last_error = String::new();
    let mut seen = VecDeque::new();
    while !stop.wait(Duration::ZERO) {
        let attempt = Instant::now();
        let result = (|| {
            let path = screenshots_path(override_path)?;
            let directory = open_directory(&path).map_err(folder_error)?;
            let mut watch = FolderWatch {
                stop,
                tx,
                override_path,
                seen: &mut seen,
                started,
                pending: HashMap::new(),
            };
            watch.run(&path, &directory)
        })();
        if stop.wait(Duration::ZERO) {
            return;
        }
        if let Err(error) = result {
            let message = error.to_string();
            if message != last_error {
                let _ = tx.try_send(Signal::Folder(format!("retrying: {message}")));
                last_error = message;
            }
        }
        if attempt.elapsed() >= FOLDER_RECHECK {
            backoff = RETRY_MIN;
        }
        if stop.wait(backoff) {
            return;
        }
        backoff = (backoff * 2).min(RETRY_MAX);
    }
}

fn folder_error(error: std::io::Error) -> Error {
    Error::new(E_FAIL, error.to_string())
}

struct DirectoryRead {
    handle: HANDLE,
    request: *mut OVERLAPPED,
    active: bool,
}

impl DirectoryRead {
    fn finish(&mut self) -> Result<usize> {
        let mut length = 0;
        // SAFETY: request belongs to this live handle. The completion event was signalled.
        unsafe { GetOverlappedResult(self.handle, self.request, &mut length, false) }?;
        self.active = false;
        Ok(length as usize)
    }
}

impl Drop for DirectoryRead {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: the buffer, OVERLAPPED and handle are still live. Wait for cancellation
            // to complete even when the original request completes before CancelIoEx.
            unsafe {
                let _ = CancelIoEx(self.handle, Some(self.request));
                let _ = GetOverlappedResult(self.handle, self.request, &mut 0, true);
            }
        }
    }
}

struct FolderWatch<'a> {
    stop: &'a Event,
    tx: &'a SyncSender<Signal>,
    override_path: Option<&'a Path>,
    seen: &'a mut VecDeque<(PathBuf, FileVersion)>,
    started: SystemTime,
    pending: HashMap<PathBuf, FileArrival>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileVersion {
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    length: u64,
}

struct FileArrival {
    time: f64,
    captured_at: SystemTime,
    deadline: Instant,
}

impl FolderWatch<'_> {
    fn run(&mut self, path: &Path, directory: &File) -> Result<()> {
        let directory_id = identity(directory)?;
        let handle = HANDLE(directory.as_raw_handle());
        let completion = Event::new(false)?;
        let mut buffer = [0u32; 4096]; // FILE_NOTIFY_INFORMATION requires DWORD alignment.
        let mut rescan = true;
        let mut first = true;
        let mut recheck = Instant::now() + FOLDER_RECHECK;
        loop {
            let mut request = OVERLAPPED {
                hEvent: completion.0,
                ..Default::default()
            };
            let request = &raw mut request;
            // SAFETY: the DWORD-aligned buffer and OVERLAPPED stay in scope and untouched
            // through each read. DirectoryRead cancels and joins an outstanding request.
            unsafe {
                ReadDirectoryChangesW(
                    handle,
                    buffer.as_mut_ptr().cast(),
                    size_of_val(&buffer) as u32,
                    false,
                    FILE_NOTIFY_CHANGE_FILE_NAME,
                    None,
                    Some(request),
                    None,
                )
            }?;
            let mut read = DirectoryRead {
                handle,
                request,
                active: true,
            };
            if first {
                let _ = self.tx.try_send(Signal::WatchingFolder(path.to_owned()));
                first = false;
            }
            if rescan {
                self.scan(path);
            }
            let length = loop {
                let handles = [self.stop.0, completion.0];
                let recheck_ms = millis(recheck.saturating_duration_since(Instant::now())).max(1);
                let timeout = if self.pending.is_empty() {
                    recheck_ms
                } else {
                    recheck_ms.min(FOLDER_POLL_MS)
                };
                // SAFETY: both live events outlast the wait.
                let ready = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
                if ready == WAIT_OBJECT_0 {
                    return Ok(());
                }
                if ready.0 == WAIT_OBJECT_0.0 + 1 {
                    break read.finish()?;
                }
                if ready != WAIT_TIMEOUT {
                    return Err(Error::from_thread());
                }
                if Instant::now() >= recheck {
                    let current_path = screenshots_path(self.override_path)?;
                    let current = open_directory(&current_path).map_err(folder_error)?;
                    if current_path != path || identity(&current)? != directory_id {
                        return Err(Error::new(E_FAIL, "Screenshots folder moved or replaced"));
                    }
                    recheck = Instant::now() + FOLDER_RECHECK;
                }
                self.report_ready();
            };
            drop(read);
            rescan = length == 0;
            if rescan {
                let _ = self
                    .tx
                    .try_send(Signal::Folder("overflow; scanning for recent files".into()));
            }
            if length <= size_of_val(&buffer) {
                // SAFETY: an initialized buffer of at least length bytes, no I/O pending.
                let bytes =
                    unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length) };
                for (action, name) in directory_changes(bytes) {
                    let file = path.join(name);
                    if action == FILE_ACTION_ADDED.0 || action == FILE_ACTION_RENAMED_NEW_NAME.0 {
                        self.queue(file, SystemTime::now(), now());
                    } else if action == FILE_ACTION_REMOVED.0
                        || action == FILE_ACTION_RENAMED_OLD_NAME.0
                    {
                        self.pending.remove(&file);
                        self.seen.retain(|(p, _)| *p != file);
                    }
                }
            }
            self.report_ready();
        }
    }

    fn scan(&mut self, path: &Path) {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.take(RECOVERY_SCAN_LIMIT).flatten() {
                if self.stop.wait(Duration::ZERO) {
                    break;
                }
                let file = entry.path();
                if !is_png(&file) {
                    continue;
                }
                if let Ok(meta) = entry.metadata()
                    && let Some(stamp) = meta
                        .created()
                        .ok()
                        .into_iter()
                        .chain(meta.modified().ok())
                        .max()
                    && let Ok(age) = SystemTime::now().duration_since(stamp)
                    && stamp >= self.started
                    && age.as_secs_f64() <= PAIR_WINDOW
                {
                    self.queue(file, stamp, now() - age.as_secs_f64());
                }
            }
        }
    }

    fn queue(&mut self, file: PathBuf, captured_at: SystemTime, time: f64) {
        if self.pending.len() < MAX_PENDING_FILES && is_png(&file) {
            self.pending.entry(file).or_insert(FileArrival {
                time,
                captured_at,
                deadline: Instant::now() + FILE_TIMEOUT,
            });
        }
    }

    fn report_ready(&mut self) {
        self.pending.retain(|path, arrival| {
            if self.stop.wait(Duration::ZERO) {
                return false;
            }
            if Instant::now() >= arrival.deadline {
                let _ = self.tx.try_send(Signal::Folder(format!(
                    "file did not become readable: {path:?}"
                )));
                return false;
            }
            // Reject handles with write access already open, and prevent new writers
            // during the read. This tests Windows sharing, not merely readable headers.
            let Ok(file) = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ.0)
                .open(path)
            else {
                return true;
            };
            let Ok(meta) = file.metadata() else {
                return true;
            };
            let version = FileVersion {
                created: meta.created().ok(),
                modified: meta.modified().ok(),
                length: meta.len(),
            };
            if self
                .seen
                .iter()
                .any(|(seen, v)| path == seen && *v == version)
            {
                return false;
            }
            let mut bytes = Vec::new();
            let data = if version.length > MAX_FILE_BYTES {
                Err("file exceeds byte cap")
            } else if file
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .is_err()
            {
                return true;
            } else {
                decode_png(&bytes)
            };
            match data {
                Ok(image) => {
                    let event = Capture {
                        time: arrival.time,
                        captured_at: arrival.captured_at,
                        image,
                        source: Source::FullScreenFile,
                        scale: monitor_scale(cursor()),
                        saved_file: Some(path.clone()),
                        session: None,
                    };
                    if self.tx.try_send(Signal::File(event)).is_err() {
                        return true;
                    }
                }
                Err(reason) => {
                    let _ = self
                        .tx
                        .try_send(Signal::Folder(format!("invalid PNG {path:?}: {reason}")));
                }
            }
            if self.seen.len() == RECENT_FILES {
                self.seen.pop_front();
            }
            self.seen.push_back((path.clone(), version));
            false
        });
    }
}

fn is_png(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
}

/// The file a snip offers on the clipboard is the Snipping Tool's temporary copy
/// (`...\Packages\<package>\TempState\ScreenClip\{guid}.png`), which Windows deletes.
/// It must not mark the card as already saved.
fn is_temp_copy(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case("TempState"))
}

fn directory_changes(mut bytes: &[u8]) -> Vec<(u32, OsString)> {
    let mut changes = Vec::new();
    while let (Some(next), Some(action), Some(length)) =
        (u32_at(bytes, 0), u32_at(bytes, 4), u32_at(bytes, 8))
    {
        let next = next as usize;
        let length = length as usize;
        let end = if next == 0 {
            bytes.len()
        } else {
            next.min(bytes.len())
        };
        if !length.is_multiple_of(2) || end < 12 || length > end - 12 {
            break;
        }
        let name: Vec<_> = bytes[12..12 + length]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u16::from_le_bytes(*p))
            .collect();
        let name = OsString::from_wide(&name);
        if matches!(Path::new(&name).components().next(), Some(std::path::Component::Normal(part)) if part == name)
        {
            changes.push((action, name));
        }
        if next == 0 || next >= bytes.len() {
            break;
        }
        bytes = &bytes[next..];
    }
    changes
}

#[derive(Debug, Clone, Copy)]
enum Input {
    Clipboard {
        own: bool,
        image: bool,
        files: bool,
        /// HTML, rich text or plain text.
        rich: bool,
        /// Plain text, a part of `rich`.
        text: bool,
        capture_owner: bool,
    },
    File {
        test_folder: bool,
    },
}

#[derive(Clone, Copy, Debug)]
struct KeyEvidence {
    shortcut: Shortcut,
    time: f64,
    scale: f64,
    clipboard_used: bool,
    file_used: bool,
}

#[derive(Clone, Copy, Debug)]
struct OverlayEvidence {
    shown: f64,
    gone: Option<f64>,
    scale: f64,
    used: bool,
}

#[derive(Debug)]
struct Signals {
    input: Input,
    time: f64,
    key: Option<KeyEvidence>,
    overlay: Option<OverlayEvidence>,
    escape: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Carry {
        source: Source,
        reason: &'static str,
        overlay: bool,
    },
    SnipFollowUp,
    Ignore(&'static str),
}

fn classify(signals: &Signals) -> Verdict {
    let Signals {
        input,
        time,
        key,
        overlay,
        escape,
    } = *signals;
    let cancelled = |started| escape.is_some_and(|escape| escape >= started && escape <= time);
    let recent_key = key.filter(|key| time - key.time >= -SIGNAL_SETTLE.as_secs_f64());
    let carry = |source, reason| Verdict::Carry {
        source,
        reason,
        overlay: false,
    };

    let Input::Clipboard {
        own,
        image,
        files,
        rich,
        capture_owner,
        ..
    } = input
    else {
        if recent_key.is_some_and(|key| {
            key.shortcut == Shortcut::WinPrint && !key.file_used && time - key.time <= PAIR_WINDOW
        }) {
            return carry(Source::FullScreenFile, "PNG after Win+PrtScn");
        }
        return if matches!(input, Input::File { test_folder: true }) {
            carry(Source::FullScreenFile, "PNG in test folder")
        } else {
            Verdict::Ignore("PNG without recent Win+PrtScn")
        };
    };
    if own {
        return Verdict::Ignore("own clipboard update");
    }
    if !image {
        return Verdict::Ignore(if files {
            "file-transfer format"
        } else {
            "no supported image format"
        });
    }
    if rich {
        return Verdict::Ignore("image accompanied by HTML or text");
    }
    if let Some(overlay) = overlay
        && !cancelled(overlay.shown)
        && !recent_key.is_some_and(|key| {
            matches!(key.shortcut, Shortcut::Snip | Shortcut::Print)
                && key.time <= overlay.shown
                && overlay.shown - key.time <= OWNER_SHORTCUT
                && cancelled(key.time)
        })
        && time - overlay.shown >= -SIGNAL_SETTLE.as_secs_f64()
        && overlay.gone.is_none_or(|gone| {
            time - gone
                <= if capture_owner || overlay.used {
                    OVERLAY_GRACE
                } else {
                    UNKNOWN_OWNER_GRACE
                }
        })
        && !recent_key.is_some_and(|key| {
            matches!(key.shortcut, Shortcut::AltPrint | Shortcut::WinPrint)
                && key.time >= overlay.shown
        })
    {
        return if overlay.used {
            Verdict::SnipFollowUp
        } else {
            Verdict::Carry {
                source: Source::Snip,
                reason: "image during or just after snipping overlay",
                overlay: true,
            }
        };
    }
    if let Some(key) = recent_key.filter(|key| {
        !key.clipboard_used
            && (!matches!(key.shortcut, Shortcut::Snip | Shortcut::Print) || !cancelled(key.time))
    }) {
        let age = time - key.time;
        let showed_overlay =
            overlay.is_some_and(|overlay| overlay.shown >= key.time - SIGNAL_SETTLE.as_secs_f64());
        if age <= INSTANT_CAPTURE {
            match key.shortcut {
                Shortcut::AltPrint => {
                    return carry(
                        Source::WindowPrintScreen,
                        "image immediately after Alt+PrtScn",
                    );
                }
                Shortcut::WinPrint => {
                    return carry(Source::PrintScreen, "image immediately after Win+PrtScn");
                }
                Shortcut::Print if !showed_overlay => {
                    return carry(Source::PrintScreen, "image immediately after PrtScn");
                }
                _ => {}
            }
        }
        if capture_owner
            && !showed_overlay
            && age <= OWNER_SHORTCUT
            && matches!(key.shortcut, Shortcut::Snip | Shortcut::Print)
        {
            return carry(
                Source::Snip,
                "capture owner after shortcut without detected overlay",
            );
        }
    }
    Verdict::Ignore(if files {
        "file-transfer format"
    } else {
        "image without recent capture evidence"
    })
}

/// Whether an update the screenshot rules ignored is an image the user copied in another
/// app, carried when "Also carry copied images" is on. Image data with HTML counts (a
/// browser's "Copy image"); image data with plain text does not (a cell range, a slide or
/// a document selection), nor files, our own writes or an update a screenshot handled.
fn copied_image(enabled: bool, input: &Input, verdict: Verdict) -> bool {
    enabled
        && matches!(verdict, Verdict::Ignore(_))
        && matches!(
            *input,
            Input::Clipboard {
                own: false,
                image: true,
                files: false,
                text: false,
                ..
            }
        )
}

/// (files, rich, text): file-transfer formats, HTML or any text, plain text.
fn image_flags(formats: &[(u32, Option<String>)]) -> (bool, bool, bool) {
    let mut files = false;
    let mut rich = false;
    let mut text = false;
    for (id, name) in formats {
        files |= *id == 15; // CF_HDROP
        text |= matches!(id, 1 | 7 | 13); // CF_TEXT, CF_OEMTEXT, CF_UNICODETEXT
        rich |= text;
        if let Some(name) = name {
            files |= [
                "Shell IDList Array",
                "FileName",
                "FileNameW",
                "Preferred DropEffect",
                "FileGroupDescriptor",
                "FileGroupDescriptorW",
                "FileContents",
            ]
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known));
            rich |= [
                "HTML Format",
                "text/html",
                "Rich Text Format",
                "Rich Text Format Without Objects",
            ]
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known));
        }
    }
    (files, rich, text)
}

struct Capture {
    time: f64,
    captured_at: SystemTime,
    image: Image,
    source: Source,
    scale: f64,
    saved_file: Option<PathBuf>,
    session: Option<f64>,
}

impl Capture {
    fn event(self) -> SnipEvent {
        SnipEvent::Captured {
            image: Arc::new(self.image),
            scale: self.scale,
            captured_at: self.captured_at,
            source: self.source,
            saved_file: self.saved_file,
        }
    }
}

struct PairRecord {
    time: f64,
    session: Option<f64>,
    dimensions: (u32, u32),
    file: bool,
    waiting: Option<Capture>,
}

impl PairRecord {
    fn matches(&self, other: &Self) -> bool {
        // Different known sessions beat size and time. A file can fall back to those
        // signals if we missed its shortcut, for example after a restart.
        self.file != other.file
            && self.dimensions == other.dimensions
            && (self.time - other.time).abs() <= PAIR_WINDOW
            && self.session.zip(other.session).is_none_or(|(a, b)| a == b)
    }
}

struct ClipboardJob {
    change: ClipboardChange,
    deadline: Instant,
    stage: ClipboardStage,
}

enum ClipboardStage {
    Inspect(Receiver<std::result::Result<ImageMetadata, clipboard::ClipError>>),
    Settle(ImageMetadata),
    Read {
        reply: Receiver<std::result::Result<ImageRead, clipboard::ClipError>>,
        candidate: ClipboardCandidate,
    },
}

#[derive(Clone, Copy)]
struct ClipboardCandidate {
    format: ImageFormat,
    source: Source,
    key: Option<f64>,
    overlay: Option<f64>,
    scale: f64,
    wait_for_file: bool,
    follow_up: bool,
}

struct SnipReceipt {
    overlay: f64,
    time: f64,
    dimensions: (u32, u32),
    pending: Option<(Instant, Capture)>,
}

const KEY_HISTORY: usize = 16;
const MAX_DEFERRED: usize = 4;

struct Controller {
    output: Arc<Output>,
    log: Option<File>,
    overlay: Option<OverlayEvidence>,
    keys: VecDeque<KeyEvidence>,
    escape: Option<f64>,
    clipboard_job: Option<ClipboardJob>,
    snip_receipt: Option<SnipReceipt>,
    watched_folder: Option<PathBuf>,
    pairs: VecDeque<PairRecord>,
    own_pid: u32,
    test_folder: bool,
    copies: Arc<AtomicBool>,
}

impl Controller {
    fn new(output: Arc<Output>, test_folder: bool) -> Self {
        Self {
            output,
            log: open_log(),
            overlay: None,
            keys: VecDeque::new(),
            escape: None,
            clipboard_job: None,
            snip_receipt: None,
            watched_folder: None,
            pairs: VecDeque::new(),
            // SAFETY: no arguments.
            own_pid: unsafe { GetCurrentProcessId() },
            test_folder,
            copies: Arc::default(),
        }
    }

    fn run(
        mut self,
        stop: &Event,
        rx: Receiver<Signal>,
        clipboard: &ScreenshotClipboard,
        mut last: Option<ClipboardChange>,
    ) {
        while !stop.wait(Duration::ZERO) {
            let wait = self
                .snip_receipt
                .as_ref()
                .and_then(|receipt| receipt.pending.as_ref())
                .map_or(TICK, |(at, _)| {
                    TICK.min(at.saturating_duration_since(Instant::now()))
                });
            match rx.recv_timeout(wait) {
                Ok(signal) => {
                    self.signal(signal);
                    for signal in rx.try_iter().take(SIGNAL_CAP) {
                        self.signal(signal);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.expire_pairs(false);
            self.poll_clipboard(clipboard);
            self.flush_snip(false);
            if self.clipboard_job.is_none()
                && let Some(change) = clipboard.latest().filter(|change| Some(*change) != last)
            {
                last = Some(change);
                self.record(
                    change.time,
                    format_args!("CLIPBOARD seq={}", change.sequence),
                );
                let deadline = Instant::now()
                    + CLIPBOARD_DEADLINE
                        .saturating_sub(Duration::from_secs_f64((now() - change.time).max(0.0)));
                match clipboard.inspect(change, deadline) {
                    Ok(rx) => {
                        self.clipboard_job = Some(ClipboardJob {
                            change,
                            deadline,
                            stage: ClipboardStage::Inspect(rx),
                        })
                    }
                    Err(err) => self.ignored(change.time, err.to_string()),
                }
            }
        }
        if self.overlay.is_some_and(|overlay| overlay.gone.is_none()) {
            self.output.push(SnipEvent::CaptureUiGone { time: now() });
        }
        self.expire_pairs(true);
        self.flush_snip(true);
    }

    fn record(&mut self, time: f64, message: std::fmt::Arguments<'_>) {
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "t={time:.6} {message}");
        }
    }

    fn ignored(&mut self, time: f64, reason: impl Into<String>) {
        let reason = reason.into();
        self.record(time, format_args!("VERDICT ignored reason={reason:?}"));
        self.output.push(SnipEvent::Ignored { reason });
    }

    fn signal(&mut self, signal: Signal) {
        match signal {
            Signal::Key(key) => {
                if key.shortcut == Shortcut::Escape
                    && !self.overlay.is_some_and(|overlay| overlay.gone.is_none())
                    && !self
                        .keys
                        .back()
                        .is_some_and(|last| key.time - last.time <= OWNER_SHORTCUT)
                {
                    return;
                }
                let scale = monitor_scale(key.point);
                self.record(
                    key.time,
                    format_args!(
                        "KEY shortcut={:?} injected={} scale={scale}",
                        key.shortcut, key.injected
                    ),
                );
                if !key.injected {
                    if key.shortcut == Shortcut::Escape {
                        self.escape = Some(key.time);
                    } else {
                        if self.keys.len() == KEY_HISTORY {
                            self.keys.pop_front();
                        }
                        self.keys.push_back(KeyEvidence {
                            shortcut: key.shortcut,
                            time: key.time,
                            scale,
                            clipboard_used: false,
                            file_used: false,
                        });
                    }
                }
            }
            Signal::Overlay {
                shown,
                time,
                scale,
                class,
                process,
            } => {
                self.record(
                    time,
                    format_args!(
                        "OVERLAY shown={shown} scale={scale} class={class:?} process={process:?}"
                    ),
                );
                if shown {
                    self.overlay = Some(OverlayEvidence {
                        shown: time,
                        gone: None,
                        scale,
                        used: false,
                    });
                    self.output.push(SnipEvent::CaptureUiShown { time });
                } else if let Some(overlay) = &mut self.overlay {
                    overlay.gone = Some(time);
                    self.output.push(SnipEvent::CaptureUiGone { time });
                }
            }
            Signal::OverlayRejected {
                time,
                event,
                class,
                process,
            } => {
                self.record(time, format_args!(
                    "OVERLAY rejected geometry event={event:#x} class={class:?} process={process:?}"
                ));
            }
            Signal::File(mut capture) => {
                let time = capture.time;
                let dimensions = (capture.image.width, capture.image.height);
                self.record(
                    time,
                    format_args!(
                        "FILE path={:?} size={}x{}",
                        capture.saved_file, dimensions.0, dimensions.1
                    ),
                );
                let signals = self.evidence(
                    time,
                    Input::File {
                        test_folder: self.test_folder,
                    },
                );
                let verdict = classify(&signals);
                self.record(
                    time,
                    format_args!("CLASSIFY {signals:?} verdict={verdict:?}"),
                );
                match verdict {
                    Verdict::Carry { source, .. } => {
                        capture.source = source;
                        if let Some(key) =
                            signals.key.filter(|key| key.shortcut == Shortcut::WinPrint)
                        {
                            capture.session = Some(key.time);
                            capture.scale = key.scale;
                            if let Some(key) = self.keys.iter_mut().find(|k| k.time == key.time) {
                                key.file_used = true;
                            }
                        }
                        self.deliver(capture, false);
                    }
                    Verdict::Ignore(reason) => self.ignored(time, reason),
                    Verdict::SnipFollowUp => {
                        unreachable!("file observations cannot be snip follow-ups")
                    }
                }
            }
            Signal::WatchingFolder(path) => {
                self.record(now(), format_args!("FOLDER watching {path:?}"));
                self.watched_folder = Some(path);
            }
            Signal::Folder(status) => self.record(now(), format_args!("FOLDER {status}")),
        }
    }

    fn evidence(&self, time: f64, input: Input) -> Signals {
        let latest = time + SIGNAL_SETTLE.as_secs_f64();
        Signals {
            input,
            time,
            key: self
                .keys
                .iter()
                .rev()
                .find(|key| key.time <= latest)
                .copied(),
            overlay: self.overlay.filter(|overlay| overlay.shown <= latest),
            escape: self.escape,
        }
    }

    fn poll_clipboard(&mut self, clipboard: &ScreenshotClipboard) {
        let Some(mut job) = self.clipboard_job.take() else {
            return;
        };
        let time = job.change.time;
        if Instant::now() >= job.deadline {
            self.ignored(time, "clipboard request deadline");
            return;
        }
        if let ClipboardStage::Inspect(rx) = &job.stage {
            match rx.try_recv() {
                Ok(Ok(metadata)) => job.stage = ClipboardStage::Settle(metadata),
                Ok(Err(err)) => {
                    self.ignored(time, err.to_string());
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    self.ignored(time, "clipboard worker stopped");
                    return;
                }
                Err(TryRecvError::Empty) => {
                    self.clipboard_job = Some(job);
                    return;
                }
            }
        }
        match job.stage {
            ClipboardStage::Settle(ref metadata) => {
                // Once the overlay is known, don't spend the reordering allowance
                // waiting. The producer can replace this sequence at any moment.
                let overlay_observed = self.overlay.is_some_and(|overlay| {
                    overlay.shown <= time + SIGNAL_SETTLE.as_secs_f64()
                        && overlay.gone.is_none_or(|gone| time - gone <= OVERLAY_GRACE)
                });
                if overlay_observed || now() - time >= SIGNAL_SETTLE.as_secs_f64() {
                    let Some(read) = self.classify_clipboard(clipboard, &job, metadata) else {
                        return;
                    };
                    job.stage = read;
                }
            }
            ClipboardStage::Read {
                ref reply,
                candidate,
            } => match reply.try_recv() {
                Ok(result) => {
                    self.clipboard_read(job.change, candidate, result);
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    self.ignored(time, "clipboard worker stopped");
                    return;
                }
                Err(TryRecvError::Empty) => {}
            },
            ClipboardStage::Inspect(_) => unreachable!("pending inspection returned above"),
        }
        self.clipboard_job = Some(job);
    }

    fn classify_clipboard(
        &mut self,
        clipboard: &ScreenshotClipboard,
        job: &ClipboardJob,
        metadata: &ImageMetadata,
    ) -> Option<ClipboardStage> {
        let time = job.change.time;
        let (files, rich, text) = image_flags(&metadata.formats);
        let format = ImageFormat::preferred(&metadata.formats);
        let owner = process_name(metadata.owner_pid);
        let signals = self.evidence(
            time,
            Input::Clipboard {
                own: metadata.owner_pid == self.own_pid,
                image: format.is_some(),
                files,
                rich,
                text,
                capture_owner: capture_process(&owner),
            },
        );
        let mut verdict = classify(&signals);
        if copied_image(self.copies.load(Relaxed), &signals.input, verdict) {
            verdict = Verdict::Carry {
                source: Source::Copied,
                reason: "image copied in another app",
                overlay: false,
            };
        }
        self.record(time, format_args!("CLASSIFY seq={} owner={owner:?} pid={} formats={:?} {signals:?} verdict={verdict:?}",
            job.change.sequence, metadata.owner_pid, metadata.formats));
        let (source, use_overlay, follow_up) = match verdict {
            Verdict::Carry {
                source, overlay, ..
            } => (source, overlay, false),
            Verdict::SnipFollowUp
                if metadata.has_file_drop()
                    && self.snip_receipt.as_ref().is_some_and(|receipt| {
                        signals.overlay.is_some_and(|o| o.shown == receipt.overlay)
                            && (0.0..=OVERLAY_GRACE).contains(&(time - receipt.time))
                            && (receipt.pending.is_some() || self.log.is_some())
                    }) =>
            {
                (Source::Snip, true, true)
            }
            Verdict::SnipFollowUp => {
                self.ignored(time, "snip already captured");
                return None;
            }
            Verdict::Ignore(reason) => {
                self.ignored(time, reason);
                return None;
            }
        };
        let format = format?;
        let overlay = signals.overlay.filter(|_| use_overlay);
        // A copied image owes nothing to a shortcut and leaves it unused.
        let key = signals.key.filter(|key| match source {
            Source::Snip => {
                time - key.time <= OWNER_SHORTCUT
                    && matches!(key.shortcut, Shortcut::Snip | Shortcut::Print)
            }
            Source::Copied => false,
            _ => true,
        });
        let scale = overlay
            .map(|o| o.scale)
            .or(key.map(|k| k.scale))
            .unwrap_or_else(|| monitor_scale(cursor()));
        let with_file = source == Source::Snip && metadata.has_file_drop();
        match clipboard.read(job.change, format, with_file, job.deadline) {
            Ok(reply) => Some(ClipboardStage::Read {
                reply,
                candidate: ClipboardCandidate {
                    format,
                    source,
                    scale,
                    key: key.map(|k| k.time),
                    overlay: overlay.map(|o| o.shown),
                    wait_for_file: key.is_some_and(|k| k.shortcut == Shortcut::WinPrint),
                    follow_up,
                },
            }),
            Err(err) => {
                self.ignored(time, err.to_string());
                None
            }
        }
    }

    fn clipboard_read(
        &mut self,
        change: ClipboardChange,
        candidate: ClipboardCandidate,
        result: std::result::Result<ImageRead, clipboard::ClipError>,
    ) {
        let time = change.time;
        let ImageRead { bytes, file } = match result {
            Ok(read) => read,
            Err(err) => {
                self.ignored(time, err.to_string());
                return;
            }
        };
        if let (Some(path), Some(log)) = (&file, &mut self.log) {
            let folder = path.parent();
            let watched = self.watched_folder.as_deref();
            let same_folder = folder.zip(watched).map(|(a, b)| a == b);
            let _ = writeln!(
                log,
                "t={time:.6} HDROP path={path:?} folder={folder:?} watching={watched:?} same_folder={same_folder:?}"
            );
        }
        let file = file.filter(|path| is_png(path) && !is_temp_copy(path));
        if candidate.follow_up {
            // A follow-up can only add metadata. Inspect the size without decoding
            // all the pixels again while the first image waits for its file path.
            let dimensions = match candidate.format {
                ImageFormat::Png => png::Decoder::new(bytes.as_slice())
                    .read_header_info()
                    .ok()
                    .map(|info| info.size()),
                ImageFormat::DibV5 | ImageFormat::Dib => dib_dimensions(&bytes),
            };
            let receipt = self.snip_receipt.as_mut().filter(|receipt| {
                Some(receipt.overlay) == candidate.overlay && Some(receipt.dimensions) == dimensions
            });
            if let Some(receipt) = receipt {
                if let Some(path) = file
                    && let Some((_, mut capture)) =
                        receipt.pending.take_if(|(at, _)| Instant::now() < *at)
                {
                    capture.saved_file = Some(path);
                    self.emit(capture);
                }
                self.ignored(time, "duplicate clipboard update for snip");
            } else {
                self.ignored(time, "image changed after snip was already captured");
            }
            return;
        }

        let image = match candidate.format {
            ImageFormat::Png => decode_png(&bytes),
            ImageFormat::DibV5 | ImageFormat::Dib => decode_dib(&bytes),
        };
        let image = match image {
            Ok(image) => image,
            Err(reason) => {
                self.ignored(time, reason);
                return;
            }
        };
        let dimensions = (image.width, image.height);
        // A stale sequence or a bad decode never reaches this point.
        if let Some(key) = self.keys.iter_mut().find(|k| Some(k.time) == candidate.key) {
            key.clipboard_used = true;
        }
        let capture = Capture {
            time,
            captured_at: change.captured_at,
            image,
            source: candidate.source,
            scale: candidate.scale,
            saved_file: file,
            session: candidate.key.or(candidate.overlay),
        };
        if let Some(overlay) = self
            .overlay
            .as_mut()
            .filter(|o| Some(o.shown) == candidate.overlay)
        {
            overlay.used = true;
            let shown = overlay.shown;
            self.flush_snip(true);
            let mut receipt = SnipReceipt {
                overlay: shown,
                time,
                dimensions,
                pending: None,
            };
            if capture.saved_file.is_none() {
                receipt.pending = Some((Instant::now() + SNIP_FILE_WAIT, capture));
            } else {
                self.emit(capture);
            }
            self.snip_receipt = Some(receipt);
        } else if capture.source == Source::Copied {
            // Never paired with a screenshot file of the same size.
            self.emit(capture);
        } else {
            self.deliver(capture, candidate.wait_for_file);
        }
    }

    fn flush_snip(&mut self, stopping: bool) {
        if let Some(receipt) = &mut self.snip_receipt
            && let Some((_, capture)) = receipt
                .pending
                .take_if(|(at, _)| stopping || Instant::now() >= *at)
        {
            self.emit(capture);
        }
    }

    fn expire_pairs(&mut self, stopping: bool) {
        let time = now();
        for index in 0..self.pairs.len() {
            if (stopping || time - self.pairs[index].time > PAIR_WINDOW)
                && let Some(capture) = self.pairs[index].waiting.take()
            {
                self.emit(capture);
            }
        }
        self.pairs
            .retain(|record| time - record.time <= PAIR_RETENTION);
    }

    fn deliver(&mut self, capture: Capture, defer: bool) {
        let time = capture.time;
        let mut record = PairRecord {
            time,
            session: capture.session,
            dimensions: (capture.image.width, capture.image.height),
            file: capture.source == Source::FullScreenFile,
            waiting: None,
        };
        if let Some((index, _)) = self
            .pairs
            .iter()
            .enumerate()
            .filter(|(_, previous)| previous.matches(&record))
            .min_by(|(_, a), (_, b)| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
        {
            let pair = self.pairs.remove(index).expect("index from pairing list");
            self.record(
                time,
                format_args!(
                    "PAIR matched time={:.6} size={:?} session={:?}",
                    pair.time, record.dimensions, record.session
                ),
            );
            if pair.waiting.is_some() {
                self.emit(capture);
            } else {
                self.ignored(time, "clipboard and file are one screenshot");
            }
            return;
        }
        if self.pairs.len() == PAIR_CAP
            && let Some(capture) = self.pairs.pop_front().and_then(|record| record.waiting)
        {
            self.emit(capture);
        }
        if defer
            && self
                .pairs
                .iter()
                .filter(|record| record.waiting.is_some())
                .count()
                < MAX_DEFERRED
        {
            self.record(
                time,
                format_args!(
                    "VERDICT deferred source={:?} pairing with file",
                    capture.source
                ),
            );
            record.waiting = Some(capture);
        } else {
            self.emit(capture);
        }
        self.pairs.push_back(record);
    }

    fn emit(&mut self, capture: Capture) {
        self.record(
            capture.time,
            format_args!(
                "VERDICT captured source={:?} size={}x{} scale={} file={:?}",
                capture.source,
                capture.image.width,
                capture.image.height,
                capture.scale,
                capture.saved_file
            ),
        );
        self.output.push(capture.event());
    }
}

/// Location of the optional tuning log. The detector appends only with FAIRY_DEBUG=1.
pub fn debug_log_path() -> Option<PathBuf> {
    if std::env::var_os("FAIRY_DEBUG").as_deref() == Some(std::ffi::OsStr::new("1")) {
        Some(
            PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
                .join("Igorio")
                .join("snip.log"),
        )
    } else {
        None
    }
}

fn open_log() -> Option<File> {
    let path = debug_log_path()?;
    fs::create_dir_all(path.parent()?).ok()?;
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn decode_png(bytes: &[u8]) -> std::result::Result<Image, &'static str> {
    if bytes.len() > MAX_FILE_BYTES as usize {
        return Err("PNG exceeds encoded byte cap");
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|_| "invalid PNG header")?;
    let (width, height) = reader.info().size();
    let len = rgba_len(width, height).ok_or("PNG dimensions exceed byte cap")?;
    let buffer_size = reader.output_buffer_size();
    if buffer_size > len {
        return Err("unsupported PNG pixel layout");
    }
    let mut pixels = vec![0; buffer_size];
    let output = reader
        .next_frame(&mut pixels)
        .map_err(|_| "invalid PNG pixels")?;
    reader.finish().map_err(|_| "incomplete PNG")?;
    if output.width != width || output.height != height {
        return Err("PNG frame does not fill the image");
    }
    pixels.truncate(output.buffer_size());
    use png::ColorType::{Grayscale, GrayscaleAlpha, Indexed, Rgba};
    let rgba = match output.color_type {
        Rgba => pixels,
        Indexed => return Err("unexpanded indexed PNG"),
        format => {
            let mut rgba = Vec::with_capacity(len);
            for pixel in pixels.chunks_exact(format.samples()) {
                rgba.extend(match format {
                    Grayscale => [pixel[0], pixel[0], pixel[0], 255],
                    GrayscaleAlpha => [pixel[0], pixel[0], pixel[0], pixel[1]],
                    _ => [pixel[0], pixel[1], pixel[2], 255],
                });
            }
            rgba
        }
    };
    Image::new(width, height, rgba).ok_or("invalid PNG dimensions")
}

fn rgba_len(width: u32, height: u32) -> Option<usize> {
    let bytes = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    (bytes > 0 && bytes <= IMAGE_CAP).then_some(bytes)
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn dib_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let width = u32::try_from(u32_at(bytes, 4)? as i32).ok()?;
    let height = (u32_at(bytes, 8)? as i32).unsigned_abs();
    rgba_len(width, height)?;
    Some((width, height))
}

fn decode_dib(bytes: &[u8]) -> std::result::Result<Image, &'static str> {
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    let get = |offset| u32_at(bytes, offset).ok_or("truncated DIB header");
    let header = get(0)? as usize;
    if !matches!(header, 40 | 52 | 56 | 108 | 124) || header > bytes.len() {
        return Err("unsupported DIB header");
    }
    let width = u32::try_from(get(4)? as i32).map_err(|_| "negative DIB width")?;
    let signed_height = get(8)? as i32;
    let height = signed_height.unsigned_abs();
    let len = rgba_len(width, height).ok_or("DIB dimensions exceed byte cap")?;

    let layout = get(12)?;
    let bits = layout >> 16;
    if layout & 0xffff != 1 || !matches!(bits, 24 | 32) {
        return Err("unsupported DIB planes or depth");
    }
    let compression = get(16)?;
    let (masks, external_masks) = match compression {
        BI_RGB => (
            [
                0xff0000,
                0xff00,
                0xff,
                if bits == 32 { 0xff000000 } else { 0 },
            ],
            0usize,
        ),
        BI_BITFIELDS => (
            [
                get(40)?,
                get(44)?,
                get(48)?,
                if header >= 56 { get(52)? } else { 0 },
            ],
            if header == 40 { 12 } else { 0 },
        ),
        _ => return Err("unsupported DIB compression"),
    };
    let mut occupied = 0;
    for (index, mask) in masks.into_iter().enumerate() {
        if mask == 0 && index == 3 {
            continue;
        }
        if mask == 0 || mask & occupied != 0 || u64::from(mask) >> bits != 0 {
            return Err("invalid DIB mask");
        }
        let normalized = mask >> mask.trailing_zeros();
        if normalized & normalized.wrapping_add(1) != 0 {
            return Err("noncontiguous DIB mask");
        }
        occupied |= mask;
    }

    let colors = (get(32)? as usize)
        .checked_mul(4)
        .ok_or("DIB palette overflow")?;
    let offset = (header + external_masks)
        .checked_add(colors)
        .ok_or("DIB offset overflow")?;
    let step = (bits / 8) as usize;
    let row_bytes = width as usize * step;
    let stride = row_bytes.next_multiple_of(4);
    let pixel_bytes = stride
        .checked_mul(height as usize)
        .ok_or("DIB stride overflow")?;
    let end = offset.checked_add(pixel_bytes).ok_or("DIB size overflow")?;
    let pixels = bytes.get(offset..end).ok_or("truncated DIB pixels")?;

    let mut rgba = Vec::with_capacity(len);
    // Convert BGR/BGRA directly for the common formats.
    let standard = masks[..3] == [0xff0000, 0xff00, 0xff];
    let mut alpha_nonzero = false;
    for y in 0..height as usize {
        let row = if signed_height < 0 {
            y
        } else {
            height as usize - y - 1
        };
        for pixel in pixels[row * stride..row * stride + row_bytes].chunks_exact(step) {
            let rgba_pixel = if standard && (masks[3] == 0 || masks[3] == 0xff000000) {
                [
                    pixel[2],
                    pixel[1],
                    pixel[0],
                    if masks[3] == 0 { 255 } else { pixel[3] },
                ]
            } else {
                let packed =
                    u32::from_le_bytes([pixel[0], pixel[1], pixel[2], *pixel.get(3).unwrap_or(&0)]);
                masks.map(|mask| {
                    if mask == 0 {
                        return 255;
                    }
                    let shift = mask.trailing_zeros();
                    let max = u64::from(mask >> shift);
                    let value = u64::from((packed & mask) >> shift);
                    ((value * 255 + max / 2) / max) as u8
                })
            };
            alpha_nonzero |= rgba_pixel[3] != 0;
            rgba.extend(rgba_pixel);
        }
    }
    if !alpha_nonzero {
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
    Image::new(width, height, rgba).ok_or("invalid DIB dimensions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_task_can_leave_the_clipboard_owner_thread() {
        fn spawnable<T: Send + 'static>(_: impl FnOnce() -> Result<T> + Send + 'static) {}
        let _: fn(&Clipboard) = |clipboard| spawnable(Snip::starter(clipboard));
    }

    #[test]
    fn the_snipping_tools_temp_copy_is_not_a_saved_file() {
        let temp = Path::new(
            r"C:\Users\a\AppData\Local\Packages\MicrosoftWindows.Client.Core_cw5n1h2txyewy\TempState\ScreenClip\{DBB1584A}.png",
        );
        assert!(is_temp_copy(temp));
        assert!(!is_temp_copy(Path::new(
            r"C:\Users\a\Pictures\Screenshots\Screenshot 1.png"
        )));
    }

    fn clipboard_signals() -> Signals {
        Signals {
            input: Input::Clipboard {
                own: false,
                image: true,
                files: false,
                rich: false,
                text: false,
                capture_owner: false,
            },
            time: 6.0,
            key: None,
            overlay: None,
            escape: None,
        }
    }

    fn key(shortcut: Shortcut, time: f64) -> Option<KeyEvidence> {
        Some(KeyEvidence {
            shortcut,
            time,
            scale: 1.25,
            clipboard_used: false,
            file_used: false,
        })
    }

    fn overlay(shown: f64, gone: Option<f64>) -> Option<OverlayEvidence> {
        Some(OverlayEvidence {
            shown,
            gone,
            scale: 1.5,
            used: false,
        })
    }

    fn source(signals: &Signals) -> Option<Source> {
        match classify(signals) {
            Verdict::Carry { source, .. } => Some(source),
            Verdict::Ignore(_) | Verdict::SnipFollowUp => None,
        }
    }

    #[test]
    fn win_shift_s_with_overlay_and_capture_owner_fallback() {
        let mut signals = clipboard_signals();
        signals.key = key(Shortcut::Snip, 1.0);
        signals.overlay = overlay(1.2, Some(5.9));
        assert_eq!(source(&signals), Some(Source::Snip));
        signals.overlay = None;
        assert_eq!(source(&signals), None);
        let Input::Clipboard {
            ref mut capture_owner,
            ..
        } = signals.input
        else {
            unreachable!()
        };
        *capture_owner = true;
        assert_eq!(source(&signals), Some(Source::Snip));
        signals.time = 16.1;
        assert_eq!(source(&signals), None);
    }

    #[test]
    fn abandoned_snip_followed_by_ordinary_image_copy() {
        let mut signals = clipboard_signals();
        signals.key = key(Shortcut::Snip, 1.0);
        signals.overlay = overlay(1.2, Some(5.9));
        signals.escape = Some(5.8);
        assert_eq!(source(&signals), None);
        signals.escape = None;
        signals.time = 6.2;
        assert_eq!(source(&signals), None);
    }

    #[test]
    fn print_screen_with_setting_off_or_on_and_alt_print_screen() {
        let mut signals = clipboard_signals();
        signals.key = key(Shortcut::Print, 5.0);
        assert_eq!(source(&signals), Some(Source::PrintScreen));
        signals.overlay = overlay(5.1, None);
        assert_eq!(source(&signals), Some(Source::Snip));
        signals.overlay.as_mut().unwrap().used = true;
        assert_eq!(source(&signals), None);
        signals.key = key(Shortcut::AltPrint, 5.5);
        assert_eq!(source(&signals), Some(Source::WindowPrintScreen));
        signals.time = 7.1;
        assert_eq!(source(&signals), None);
    }

    #[test]
    fn win_print_screen_file_and_consumed_shortcut() {
        let mut signals = clipboard_signals();
        signals.key = key(Shortcut::WinPrint, 5.0);
        assert_eq!(source(&signals), Some(Source::PrintScreen));
        signals.input = Input::File { test_folder: false };
        assert_eq!(source(&signals), Some(Source::FullScreenFile));
        signals.time = 9.01;
        assert_eq!(source(&signals), None);
        signals.time = 6.0;
        signals.key.as_mut().unwrap().file_used = true;
        assert_eq!(source(&signals), None);
    }

    #[test]
    fn browser_file_transfer_editor_text_and_own_writes() {
        let mut signals = clipboard_signals();
        assert_eq!(source(&signals), None); // editor or image-only browser
        signals.key = key(Shortcut::Print, 5.0);
        for (files, rich, image, own, expected) in [
            (
                false,
                true,
                true,
                false,
                "image accompanied by HTML or text",
            ), // browser
            (true, false, false, false, "file-transfer format"), // Explorer
            (false, true, false, false, "no supported image format"), // text
            (false, false, true, true, "own clipboard update"),  // stage
            (false, true, true, true, "own clipboard update"),   // restore
        ] {
            signals.input = Input::Clipboard {
                own,
                image,
                files,
                rich,
                text: false,
                capture_owner: false,
            };
            assert_eq!(classify(&signals), Verdict::Ignore(expected));
        }
        assert_eq!(
            image_flags(&[(15, None), (0xc001, Some("HTML Format".into()))]),
            (true, true, false)
        );
    }

    fn observed_formats(standard: &[u32], registered: &[&str]) -> Vec<(u32, Option<String>)> {
        standard
            .iter()
            .map(|&id| (id, None))
            .chain(
                registered
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (0xc000 + i as u32, Some((*name).to_owned()))),
            )
            .collect()
    }

    fn snip_formats(with_file: bool) -> Vec<(u32, Option<String>)> {
        if with_file {
            observed_formats(
                &[2, 15, 8, 17],
                &[
                    "DataObject",
                    "Preferred DropEffect",
                    "Shell IDList Array",
                    "FileContents",
                    "FileGroupDescriptorW",
                    "PNG",
                    "Ole Private Data",
                ],
            )
        } else {
            observed_formats(
                &[2, 8, 17],
                &[
                    "DataObject",
                    "Preferred DropEffect",
                    "PNG",
                    "Ole Private Data",
                ],
            )
        }
    }

    fn observed_input(owner: &str, formats: &[(u32, Option<String>)]) -> Input {
        let (files, rich, text) = image_flags(formats);
        Input::Clipboard {
            own: false,
            image: ImageFormat::preferred(formats).is_some(),
            files,
            rich,
            text,
            capture_owner: capture_process(owner),
        }
    }

    #[test]
    fn observed_svchost_snip_formats_require_capture_evidence_and_prefer_png() {
        for shortcut in [Shortcut::Snip, Shortcut::Print] {
            for with_file in [false, true] {
                let formats = snip_formats(with_file);
                assert_eq!(formats.len(), if with_file { 11 } else { 7 });
                assert!(matches!(
                    ImageFormat::preferred(&formats),
                    Some(ImageFormat::Png)
                ));
                for gone in [None, Some(5.9)] {
                    let signals = Signals {
                        input: observed_input("svchost.exe", &formats),
                        key: key(shortcut, 2.6),
                        overlay: overlay(3.5, gone),
                        ..clipboard_signals()
                    };
                    assert_eq!(source(&signals), Some(Source::Snip));
                    assert_eq!(
                        classify(&Signals {
                            key: None,
                            overlay: None,
                            ..signals
                        }),
                        Verdict::Ignore("file-transfer format")
                    );
                }
            }
        }
    }

    #[test]
    fn observed_alt_print_browser_explorer_and_text_formats() {
        let alt = observed_formats(&[2, 8, 17], &[]);
        assert!(matches!(
            ImageFormat::preferred(&alt),
            Some(ImageFormat::DibV5)
        ));
        let mut signals = Signals {
            input: observed_input("", &alt),
            key: key(Shortcut::AltPrint, 5.8),
            ..clipboard_signals()
        };
        assert_eq!(source(&signals), Some(Source::WindowPrintScreen));
        let browser = observed_formats(&[17], &["PNG", "HTML Format", "UniformResourceLocatorW"]);
        signals.input = observed_input("vivaldi.exe", &browser);
        assert_eq!(
            classify(&signals),
            Verdict::Ignore("image accompanied by HTML or text")
        );
        signals.key = None;
        let explorer = observed_formats(
            &[15],
            &[
                "DataObject",
                "Shell IDList Array",
                "FileName",
                "FileContents",
            ],
        );
        signals.input = observed_input("explorer.exe", &explorer);
        assert_eq!(classify(&signals), Verdict::Ignore("file-transfer format"));
        signals.input = observed_input("", &observed_formats(&[13, 16, 1, 7], &[]));
        assert_eq!(
            classify(&signals),
            Verdict::Ignore("no supported image format")
        );
    }

    #[test]
    fn copied_images_carry_only_image_data_without_text_files_own_writes_or_snips() {
        let copied = |enabled, signals: &Signals| {
            copied_image(enabled, &signals.input, classify(signals))
        };
        let mut signals = clipboard_signals();
        for (owner, formats, expected) in [
            ("mspaint.exe", observed_formats(&[2, 8, 17], &[]), true), // image only
            ("chrome.exe", observed_formats(&[2, 8, 17], &["HTML Format"]), true), // image + HTML
            (
                "vivaldi.exe",
                observed_formats(&[17], &["PNG", "HTML Format", "UniformResourceLocatorW"]),
                true,
            ),
            (
                "EXCEL.EXE",
                observed_formats(&[2, 8, 17, 13, 1, 7], &["PNG", "HTML Format", "Rich Text Format"]),
                false,
            ), // image + text
            (
                "explorer.exe",
                observed_formats(&[15], &["Shell IDList Array", "FileNameW"]),
                false,
            ), // file drop
            ("svchost.exe", snip_formats(false), false), // snip data, even without evidence
            ("notepad.exe", observed_formats(&[13, 1, 7], &[]), false), // text only
        ] {
            signals.input = observed_input(owner, &formats);
            assert_eq!(copied(true, &signals), expected, "{owner}");
            assert!(!copied(false, &signals), "{owner} with the setting off");
        }

        // Our own staging and restore.
        signals.input = observed_input("", &observed_formats(&[2, 8, 17], &[]));
        assert!(copied(true, &signals));
        let Input::Clipboard { ref mut own, .. } = signals.input else { unreachable!() };
        *own = true;
        assert!(!copied(true, &signals));

        // An image the screenshot rules carried, or a later update of that snip.
        let mut signals = Signals {
            input: observed_input("", &observed_formats(&[2, 8, 17], &[])),
            key: key(Shortcut::AltPrint, 5.8),
            ..clipboard_signals()
        };
        assert_eq!(source(&signals), Some(Source::WindowPrintScreen));
        assert!(!copied(true, &signals));
        signals.key = key(Shortcut::Snip, 2.6);
        signals.overlay = Some(OverlayEvidence {
            used: true,
            ..overlay(3.5, Some(5.9)).unwrap()
        });
        assert_eq!(classify(&signals), Verdict::SnipFollowUp);
        assert!(!copied(true, &signals));
    }

    fn snip_controller() -> (Arc<Output>, Controller) {
        let output = Arc::new(Output {
            wake: Event::new(false).unwrap(),
            queue: Mutex::default(),
        });
        let mut controller = Controller::new(output.clone(), false);
        controller.log = None;
        controller.overlay = overlay(3.5, None);
        controller.keys.extend(key(Shortcut::Snip, 2.6));
        (output, controller)
    }

    #[test]
    fn observed_four_updates_carry_once_and_attach_the_hdrop_path() {
        let (output, mut controller) = snip_controller();
        let captured_at = SystemTime::now();
        let candidate = ClipboardCandidate {
            format: ImageFormat::Png,
            source: Source::Snip,
            key: Some(2.6),
            overlay: Some(3.5),
            scale: 1.25,
            wait_for_file: false,
            follow_up: false,
        };
        let bytes = crate::saver::encode_png(&sample_capture(false, None).image, 1.25).unwrap();
        let change = ClipboardChange {
            sequence: 100,
            time: 5.8,
            captured_at,
        };
        controller.clipboard_read(change, candidate, Err(clipboard::ClipError::Changed));
        assert!(!controller.overlay.unwrap().used);
        assert!(!controller.keys[0].clipboard_used);
        let signals =
            controller.evidence(5.85, observed_input("svchost.exe", &snip_formats(false)));
        assert_eq!(source(&signals), Some(Source::Snip));
        controller.clipboard_read(
            ClipboardChange {
                sequence: 105,
                time: 5.85,
                ..change
            },
            candidate,
            Ok(ImageRead {
                bytes: bytes.clone(),
                file: None,
            }),
        );
        assert!(controller.overlay.unwrap().used);
        assert!(controller.keys[0].clipboard_used);
        assert!(controller.snip_receipt.as_ref().unwrap().pending.is_some());

        let follow_up = ClipboardCandidate {
            follow_up: true,
            ..candidate
        };
        controller.clipboard_read(
            ClipboardChange {
                sequence: 117,
                time: 5.9,
                ..change
            },
            follow_up,
            Err(clipboard::ClipError::Changed),
        );
        controller.overlay.as_mut().unwrap().gone = Some(5.95);
        let signals = controller.evidence(6.05, observed_input("svchost.exe", &snip_formats(true)));
        assert_eq!(classify(&signals), Verdict::SnipFollowUp);
        let file = PathBuf::from(r"C:\Screenshots\Shot.png");
        controller.clipboard_read(
            ClipboardChange {
                sequence: 125,
                time: 6.05,
                ..change
            },
            follow_up,
            Ok(ImageRead {
                bytes: bytes.clone(),
                file: Some(file.clone()),
            }),
        );
        controller.clipboard_read(
            ClipboardChange {
                sequence: 126,
                time: 6.1,
                ..change
            },
            follow_up,
            Ok(ImageRead {
                bytes,
                file: Some(file.clone()),
            }),
        );
        controller.flush_snip(true);

        let out = output.queue.lock().unwrap();
        let carries: Vec<_> = out
            .iter()
            .filter_map(|event| match event {
                SnipEvent::Captured {
                    captured_at,
                    saved_file,
                    ..
                } => Some((*captured_at, saved_file.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(carries, [(captured_at, Some(file.as_path()))]);
    }

    #[test]
    fn snip_without_hdrop_expires_and_late_metadata_does_not_carry_again() {
        let (output, mut controller) = snip_controller();
        let change = ClipboardChange {
            sequence: 5,
            time: 5.85,
            captured_at: SystemTime::now(),
        };
        let candidate = ClipboardCandidate {
            format: ImageFormat::Png,
            source: Source::Snip,
            key: Some(2.6),
            overlay: Some(3.5),
            scale: 1.25,
            wait_for_file: false,
            follow_up: false,
        };
        controller.clipboard_read(
            change,
            candidate,
            Ok(ImageRead {
                bytes: vec![],
                file: None,
            }),
        );
        assert!(!controller.overlay.unwrap().used);
        let bytes = crate::saver::encode_png(&sample_capture(false, None).image, 1.25).unwrap();
        controller.clipboard_read(
            change,
            candidate,
            Ok(ImageRead {
                bytes: bytes.clone(),
                file: None,
            }),
        );
        let (at, _) = controller
            .snip_receipt
            .as_mut()
            .unwrap()
            .pending
            .as_mut()
            .unwrap();
        assert!(*at <= Instant::now() + SNIP_FILE_WAIT);
        *at = Instant::now();
        controller.flush_snip(false);
        controller.clipboard_read(
            change,
            ClipboardCandidate {
                follow_up: true,
                ..candidate
            },
            Ok(ImageRead {
                bytes,
                file: Some(r"C:\Screenshots\Late.png".into()),
            }),
        );
        let out = output.queue.lock().unwrap();
        let captures: Vec<_> = out
            .iter()
            .filter(|event| matches!(event, SnipEvent::Captured { .. }))
            .collect();
        assert!(matches!(
            captures.as_slice(),
            [SnipEvent::Captured {
                saved_file: None,
                ..
            }]
        ));
    }

    fn sample_capture(file: bool, session: Option<f64>) -> Capture {
        Capture {
            time: now(),
            captured_at: SystemTime::now(),
            image: Image::new(1, 1, vec![1, 2, 3, 255]).unwrap(),
            source: if file {
                Source::FullScreenFile
            } else {
                Source::PrintScreen
            },
            scale: 1.0,
            saved_file: file.then(|| "fixture.png".into()),
            session,
        }
    }

    #[test]
    fn pairs_file_and_clipboard_once_in_either_order() {
        for file_first in [false, true] {
            let output = Arc::new(Output {
                wake: Event::new(false).unwrap(),
                queue: Mutex::default(),
            });
            let mut controller = Controller::new(output.clone(), false);
            controller.log = None;
            controller.deliver(sample_capture(file_first, Some(1.0)), !file_first);
            controller.deliver(sample_capture(!file_first, Some(1.0)), file_first);
            let out = output.queue.lock().unwrap();
            let captured = out
                .iter()
                .filter(|e| matches!(e, SnipEvent::Captured { .. }))
                .count();
            assert_eq!(captured, 1);
            assert!(matches!(
                out[0],
                SnipEvent::Captured {
                    source: Source::FullScreenFile,
                    saved_file: Some(_),
                    ..
                }
            ));
        }
    }

    #[test]
    fn different_known_sessions_do_not_pair_even_with_same_dimensions() {
        let a = PairRecord {
            time: 6.0,
            session: Some(1.0),
            dimensions: (2, 1),
            file: true,
            waiting: None,
        };
        let mut b = PairRecord {
            time: 7.0,
            session: Some(2.0),
            dimensions: (2, 1),
            file: false,
            waiting: None,
        };
        assert!(!a.matches(&b));
        b.session = Some(1.0);
        assert!(a.matches(&b));
        b.time = 10.01;
        assert!(!a.matches(&b));
    }

    fn dib_header(header: u32, width: i32, height: i32, bits: u32, compression: u32) -> Vec<u8> {
        let mut dib = vec![0; header as usize];
        for (offset, value) in [
            (0, header),
            (4, width as u32),
            (8, height as u32),
            (12, bits << 16 | 1),
            (16, compression),
        ] {
            dib[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        dib
    }

    #[test]
    fn bottom_up_rgb24_observes_row_padding() {
        let mut dib = dib_header(40, 1, 2, 24, 0);
        dib.extend([1, 2, 3, 99, 4, 5, 6, 99]);
        let image = decode_dib(&dib).unwrap();
        assert_eq!((image.width, image.height), (1, 2));
        assert_eq!(image.rgba, [6, 5, 4, 255, 3, 2, 1, 255]);
    }

    #[test]
    fn top_down_rgb32_zero_alpha_becomes_opaque() {
        let mut dib = dib_header(40, 1, -2, 32, 0);
        dib.extend([3, 2, 1, 0, 6, 5, 4, 0]);
        assert_eq!(decode_dib(&dib).unwrap().rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
        dib[47] = 17;
        assert_eq!(decode_dib(&dib).unwrap().rgba, [1, 2, 3, 0, 4, 5, 6, 17]);
    }

    #[test]
    fn bitfields_with_external_masks_and_v5_inline_alpha() {
        let mut dib = dib_header(40, 1, -1, 32, 3);
        for mask in [0x3ffu32, 0xffc00, 0x3ff00000] {
            dib.extend(mask.to_le_bytes());
        }
        dib.extend((0x3ffu32 | (0x200 << 10)).to_le_bytes());
        assert_eq!(decode_dib(&dib).unwrap().rgba, [255, 128, 0, 255]);
        let mut dib = dib_header(124, 1, 1, 32, 3);
        for (offset, mask) in [
            (40, 0xffu32),
            (44, 0xff00),
            (48, 0xff0000),
            (52, 0xff000000),
        ] {
            dib[offset..offset + 4].copy_from_slice(&mask.to_le_bytes());
        }
        dib.extend([1, 2, 3, 42]);
        assert_eq!(decode_dib(&dib).unwrap().rgba, [1, 2, 3, 42]);
        dib.truncate(127);
        assert!(decode_dib(&dib).is_err());
    }

    #[test]
    fn png_expands_palette_with_straight_alpha_and_requires_iend() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::One);
            encoder.set_palette(vec![200, 40, 30, 60, 50, 40]);
            encoder.set_trns(vec![33, 255]);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[0b0100_0000])
                .unwrap();
        }
        assert_eq!(
            decode_png(&bytes).unwrap().rgba,
            [200, 40, 30, 33, 60, 50, 40, 255]
        );
        assert!(decode_png(&bytes[..bytes.len() - 12]).is_err());
    }
}
