//! The global mouse hook (`WH_MOUSE_LL`). It feeds shake detection (moves) and paste on
//! double-click (button events) with a view of the mouse across all apps.
//!
//! ## Threads
//!
//! [`MouseHook::start`] spawns a hook thread. It sets the hook, then only pumps messages. The
//! callback runs on that thread and does the minimum (rule 1 in `ROADMAP.md`): it copies
//! the `MSLLHOOKSTRUCT`, counts clicks, pushes one small event into a preallocated queue,
//! signals an event handle, and calls `CallNextHookEx`. No logging, COM, clipboard,
//! process queries or heap allocation happen on that thread.
//!
//! It swallows exactly two kinds of event: the press the [`CLICK_GATE`] is armed for (inside
//! its rectangle: the tool menu at the cursor), and the release of that press; and, while a
//! recording runs and its zoom
//! modifier is held, wheel events ([`WHEEL_GATE`]). Each decision reads one atomic and
//! compares ([`ClickGate::on_press`], [`WheelGate::on_wheel`], which also reads the async
//! key state); everything else goes on to `CallNextHookEx`. A swallowed event is not queued:
//! to the rest of the app it did not happen.
//! A panic inside the callback is caught, so it cannot unwind across the `extern "system"`
//! boundary, and `CallNextHookEx` still runs.
//!
//! The thread that owns the [`MouseHook`] (the UI thread) waits on [`MouseHook::wake_handle`]
//! together with its message queue, and calls [`MouseHook::drain`] after every wait.
//!
//! ## Coalescing
//!
//! One mutex-protected `VecDeque`, preallocated to [`QUEUE_CAPACITY`]. A `Move` that
//! arrives while the newest queued event is also a `Move` replaces it; every other event
//! is appended. So moves collapse to the latest position between two button or wheel
//! events, while the order of all events stays as it happened and each button event keeps
//! its own position and time. A separate "latest move" atomic slot would lose the order of
//! a move relative to a click, and a single queue under a lock held for a few
//! instructions costs the hook thread nothing it can notice. If the UI thread stops
//! draining and the queue fills up, new events that cannot replace a queued move are counted in
//! [`HookStats::dropped`] instead of allocating more memory.
//!
//! The wake is coalesced the same way: the auto-reset event is set only when an event lands
//! in an empty queue. An event handle is used instead of `PostThreadMessageW`, because
//! thread messages are lost while a modal loop (a tray menu, a window drag) runs on the UI
//! thread, and a posted message per move would flood the queue.
//!
//! Rate seen by the UI thread: at most one `Move` per drain, and the UI thread drains once
//! per loop pass. With a pass that only drains, it sees nearly every move up to the mouse's
//! own rate (125 to 1000 Hz). While it also renders at 8 ms frames, it sees about 125 per
//! second. `ShakeDetector` needs gaps below `max_pause` (0.3 s) and strokes of 30 points,
//! so 60 Hz or more is enough; each `Move` carries the time it was read on the hook thread,
//! so late draining does not distort speeds.
//!
//! ## Time
//!
//! `time` in every event is seconds since a process-wide epoch, read with `Instant` (which
//! is `QueryPerformanceCounter` on Windows) when the callback runs. [`now`] reads the same
//! clock. Click counting uses the hook's own millisecond `time` field instead, with
//! wrap-around, because that is the clock `GetDoubleClickTime` is meant for.
//!
//! ## Coordinates and buttons
//!
//! Positions are physical pixels in virtual-screen coordinates (what the hook reports for
//! a Per-Monitor V2 aware thread; the hook thread sets that awareness itself). Buttons are
//! the physical buttons; with swapped buttons (`SM_SWAPBUTTON`), the primary is `Right`.
//!
//! ## Dead hooks
//!
//! Windows removes a low-level hook without notice when a callback takes longer than
//! `LowLevelHooksTimeout`. [`MouseHook::is_alive`] only says that the thread runs and the
//! hook was set. [`MouseHook::seems_stalled`] is the cheap self-check: called about once a
//! second, it reports a stall when the cursor moved but no callback ran, twice in a row.
//! Then call [`MouseHook::restart`].
//!
//! ## Limits
//!
//! - Without UI access, Windows can skip the hook for input to a higher-integrity window.
//!   While such a window is in front, `seems_stalled` can report a stall; a restart then is
//!   harmless. The installed `uiAccess` build can receive that input.
//! - The secure desktop (UAC prompt, Ctrl+Alt+Del) is not seen at all.
//! - If the hook is restarted (or the process ends) between a swallowed press and its
//!   release, the release reaches the window below without its press. Windows never saw
//!   the press, so no button is left down.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::windows::io::AsRawHandle;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicIsize, AtomicU64, Ordering::Relaxed, Ordering::SeqCst,
};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, HANDLE, LPARAM, LRESULT, POINT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThreadId, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetSystemMetricsForDpi,
    MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetDoubleClickTime, VK_CONTROL, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, HC_ACTION, HHOOK, LLMHF_INJECTED,
    LLMHF_LOWER_IL_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    SM_CXDOUBLECLK, SM_CYDOUBLECLK, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    WH_MOUSE_LL, WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN,
    WM_XBUTTONUP, XBUTTON1,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, KBDLLHOOKSTRUCT, LLKHF_INJECTED, WH_KEYBOARD_LL, WM_KEYDOWN,
    WM_SYSKEYDOWN, WindowFromPoint,
};
use windows::core::{Error, Result};

/// Most events the queue holds between two drains. Preallocated, so the callback never
/// allocates. About 160 KB.
pub const QUEUE_CAPACITY: usize = 4096;

/// How long `Drop` and `restart` wait for the hook thread to end.
const STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// Thread message that ends the hook thread's loop.
const WM_STOP: u32 = WM_APP + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// One mouse event. Positions are physical pixels in virtual-screen coordinates; `time` is
/// seconds on the clock of [`now`]. `injected` is true for synthetic input (`SendInput`
/// and similar, `LLMHF_INJECTED` or `LLMHF_LOWER_IL_INJECTED`), so callers can skip their
/// own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseEvent {
    Move {
        x: i32,
        y: i32,
        time: f64,
        injected: bool,
    },
    /// `click_count` is 1 for a single click, 2 for the second of a double-click, 3 for a
    /// triple-click, and so on. Injected, pen and touch presses are always 1 and do not take
    /// part in counting. `window` is the top-level window under the point when the press
    /// happened (`WindowFromPoint`, 0 for none); presses on different windows never pair.
    /// `touch` marks a press Windows promoted from pen or touch input.
    Down {
        button: Button,
        x: i32,
        y: i32,
        time: f64,
        click_count: u32,
        injected: bool,
        window: isize,
        touch: bool,
    },
    Up {
        button: Button,
        x: i32,
        y: i32,
        time: f64,
        injected: bool,
    },
    /// `delta` is in `WHEEL_DELTA` units (120 per notch); positive is away from the user
    /// or to the right.
    Wheel {
        delta: i32,
        horizontal: bool,
        x: i32,
        y: i32,
        time: f64,
        injected: bool,
    },
}

/// Counters kept by the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookStats {
    /// Callbacks with `HC_ACTION`, coalesced or not.
    pub callbacks: u64,
    /// Longest time spent in our part of the callback (before `CallNextHookEx`).
    pub max_callback: Duration,
    /// Events dropped because the queue was full.
    pub dropped: u64,
}

/// Seconds since the process-wide epoch: the clock of every event's `time`.
pub fn now() -> f64 {
    epoch().elapsed().as_secs_f64()
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// The size of the double-click rectangle and the double-click time at one click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickLimits {
    pub time_ms: u32,
    /// Full width and height of the rectangle centred on the previous click, physical pixels.
    pub width: i32,
    pub height: i32,
}

/// Counts consecutive presses of one button, as Windows does for `WM_LBUTTONDBLCLK`: the
/// press must come within `time_ms` of the previous press of the same button and inside the
/// rectangle centred on it. A press of another button starts over.
#[derive(Debug, Clone, Default)]
pub struct ClickCounter {
    last: Option<LastPress>,
}

#[derive(Debug, Clone, Copy)]
struct LastPress {
    button: Button,
    x: i32,
    y: i32,
    tick: u32,
    count: u32,
    limits: ClickLimits,
    window: isize,
}

impl ClickCounter {
    /// Records a press and returns its click count. `tick` is the hook's millisecond time,
    /// which wraps around after 49.7 days. `limits` are those at this press; the next press
    /// is compared with them. `window` is the top-level window under the press: a press on
    /// another window starts over.
    pub fn down(
        &mut self,
        button: Button,
        x: i32,
        y: i32,
        tick: u32,
        limits: ClickLimits,
        window: isize,
    ) -> u32 {
        let count = match self.last {
            Some(last)
                if last.button == button
                    && last.window == window
                    && tick.wrapping_sub(last.tick) <= last.limits.time_ms
                    && x.abs_diff(last.x) <= last.limits.width.max(0) as u32 / 2
                    && y.abs_diff(last.y) <= last.limits.height.max(0) as u32 / 2 =>
            {
                last.count.saturating_add(1)
            }
            _ => 1,
        };
        self.last = Some(LastPress {
            button,
            x,
            y,
            tick,
            count,
            limits,
            window,
        });
        count
    }
}

impl ClickCounter {
    /// A press as the hook sees it. Synthetic, pen and touch presses (`skip`) neither count
    /// nor reset the count: they are 1 and the counter is left as it was. `limits` is only
    /// read for presses that count.
    pub fn press(
        &mut self,
        button: Button,
        (x, y): (i32, i32),
        tick: u32,
        limits: impl FnOnce() -> ClickLimits,
        window: isize,
        skip: bool,
    ) -> u32 {
        if skip {
            return 1;
        }
        self.down(button, x, y, tick, limits(), window)
    }
}

/// `dwExtraInfo` of mouse messages Windows promoted from pen or touch input
/// (`MI_WP_SIGNATURE`), under `SIGNATURE_MASK`.
pub const PEN_OR_TOUCH_SIGNATURE: usize = 0xFF51_5700;
const SIGNATURE_MASK: usize = 0xFFFF_FF00;

/// Was this mouse event promoted from pen or touch input?
pub fn is_pen_or_touch(extra_info: usize) -> bool {
    extra_info & SIGNATURE_MASK == PEN_OR_TOUCH_SIGNATURE
}

/// An auto-reset event handle, closed on drop.
struct Event(HANDLE);

// SAFETY: a kernel event handle may be used and closed from any thread.
unsafe impl Send for Event {}
// SAFETY: `SetEvent` and waits on the same handle from several threads are allowed.
unsafe impl Sync for Event {}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `CreateEventW` and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// State shared by the hook thread and the owner. Lives as long as either holds it.
struct Shared {
    queue: Mutex<VecDeque<MouseEvent>>,
    wake: Event,
    callbacks: AtomicU64,
    max_callback_ns: AtomicU64,
    dropped: AtomicU64,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, VecDeque<MouseEvent>> {
        // A panic while the lock is held cannot leave the queue half-changed, so a
        // poisoned lock is still usable.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Called on the hook thread.
    fn push(&self, event: MouseEvent) {
        let wake = {
            let mut queue = self.queue();
            let was_empty = queue.is_empty();
            let full = queue.len() >= QUEUE_CAPACITY;
            match queue.back_mut() {
                Some(last @ MouseEvent::Move { .. })
                    if matches!(event, MouseEvent::Move { .. }) =>
                {
                    *last = event;
                }
                _ if full => {
                    self.dropped.fetch_add(1, Relaxed);
                }
                _ => queue.push_back(event),
            }
            was_empty
        };
        if wake {
            // SAFETY: the handle stays open while `self` lives.
            let _ = unsafe { SetEvent(self.wake.0) };
        }
    }
}

// MARK: Click gate

/// Swallows one press of one button inside one screen rectangle, and its release, on request:
/// the click on the tool menu at the cursor (`notes::summoner`).
///
/// The summoner arms the gate only while the menu shows, with the epoch it read when the
/// modifier went down and the menu card's rectangle ([`arm_in`](Self::arm_in)). The keyboard hook of the summoner calls [`disarm`](Self::disarm) on
/// every key change, which also moves the epoch on: an arm based on an older key state
/// fails, and a released modifier or a typed key disarms at once, on the hook thread,
/// before Windows passes the next input on (low-level hooks are called in input order).
///
/// The mouse hook asks [`on_press`](Self::on_press) on every press: one load, and a
/// compare-and-swap and two more loads only when armed. Any press while armed (that is not
/// injected) disarms the gate. It is swallowed only when it is of the armed button, not pen
/// or touch, and inside the rectangle; any other press, including one outside the rectangle,
/// passes on with its release, so the app below gets the whole click. The matching release
/// of a swallowed press is swallowed by the hook's own state. The taken click's point and
/// time are kept here and [`wake_handle`](Self::wake_handle) is set.
pub struct ClickGate {
    /// Bits 0 to 7: the armed button ([`button_code`]), 0 when not armed. Bits 8 and up:
    /// the epoch.
    state: AtomicU64,
    /// The armed rectangle, physical pixels, right and bottom exclusive: left in the high 32
    /// bits and top in the low of `area_lt`, right and bottom likewise in `area_rb`. Written
    /// before the arm's compare-and-swap, so a press that sees the gate armed sees it.
    area_lt: AtomicU64,
    area_rb: AtomicU64,
    /// Clicks taken since start.
    taken: AtomicU64,
    /// Point of the last taken click: x in the high 32 bits, y in the low.
    taken_point: AtomicU64,
    /// Time of the last taken click (`f64` bits, the clock of [`now`]).
    taken_time: AtomicU64,
    /// An auto-reset event handle (0 until first asked for). Never closed, so the hook can
    /// set it without a lock.
    wake: AtomicIsize,
}

/// The process's one gate, read by the process's one mouse hook.
pub static CLICK_GATE: ClickGate = ClickGate::new();

/// A click the gate swallowed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TakenClick {
    /// Counts up with every taken click; compare with the last one seen.
    pub count: u64,
    pub x: i32,
    pub y: i32,
    pub time: f64,
}

/// The gate's code for a button: 1 to 5.
pub fn button_code(button: Button) -> u64 {
    match button {
        Button::Left => 1,
        Button::Right => 2,
        Button::Middle => 3,
        Button::X1 => 4,
        Button::X2 => 5,
    }
}

const ARM_BITS: u64 = 0xFF;

fn pack(a: i32, b: i32) -> u64 {
    ((a as u32 as u64) << 32) | b as u32 as u64
}

fn unpack(v: u64) -> (i32, i32) {
    ((v >> 32) as u32 as i32, v as u32 as i32)
}

impl ClickGate {
    pub const fn new() -> ClickGate {
        ClickGate {
            state: AtomicU64::new(0),
            area_lt: AtomicU64::new(0),
            area_rb: AtomicU64::new(0),
            taken: AtomicU64::new(0),
            taken_point: AtomicU64::new(0),
            taken_time: AtomicU64::new(0),
            wake: AtomicIsize::new(0),
        }
    }

    /// The current epoch. It moves on with every [`disarm`](Self::disarm) and every taken
    /// click.
    pub fn epoch(&self) -> u64 {
        self.state.load(SeqCst) >> 8
    }

    /// Arms the gate for one press of `button` anywhere on the screen. See
    /// [`arm_in`](Self::arm_in).
    pub fn arm(&self, epoch: u64, button: Button) -> bool {
        self.arm_in(epoch, button, (i32::MIN, i32::MIN, i32::MAX, i32::MAX))
    }

    /// Arms the gate for one press of `button` inside `area` (left, top, right, bottom;
    /// physical pixels, right and bottom exclusive), if the epoch is still `epoch` and the
    /// gate is not armed. False when a key changed since (the caller must not show the menu)
    /// or it is armed already. The gate has one arming caller (the summoner thread).
    pub fn arm_in(&self, epoch: u64, button: Button, area: (i32, i32, i32, i32)) -> bool {
        if self.is_armed() {
            return false;
        }
        self.area_lt.store(pack(area.0, area.1), SeqCst);
        self.area_rb.store(pack(area.2, area.3), SeqCst);
        self.state
            .compare_exchange(epoch << 8, epoch << 8 | button_code(button), SeqCst, SeqCst)
            .is_ok()
    }

    /// Disarms and moves the epoch on. Returns the new epoch. Cheap enough for a hook
    /// callback: a compare-and-swap loop on one word.
    pub fn disarm(&self) -> u64 {
        let old = self
            .state
            .try_update(SeqCst, SeqCst, |v| Some(((v >> 8) + 1) << 8))
            .unwrap_or(0);
        (old >> 8) + 1
    }

    /// Disarms without moving the epoch: the summoner hides the pill but the hold goes on,
    /// so it may arm again with the same epoch when the pill comes back.
    pub fn withdraw(&self) {
        self.state.fetch_and(!ARM_BITS, SeqCst);
    }

    pub fn is_armed(&self) -> bool {
        self.state.load(SeqCst) & ARM_BITS != 0
    }

    /// The decision for one press at (`x`, `y`), made in the hook callback. True: swallow it
    /// (the gate was armed for this button, the press is inside its rectangle, and the gate
    /// is now taken). Injected presses never touch the gate; any other press while armed
    /// disarms it and passes.
    pub fn on_press(&self, button: Button, injected: bool, touch: bool, x: i32, y: i32) -> bool {
        if injected {
            return false;
        }
        let state = self.state.load(SeqCst);
        let armed = state & ARM_BITS;
        if armed == 0 {
            return false;
        }
        let next = ((state >> 8) + 1) << 8;
        let taken = self
            .state
            .compare_exchange(state, next, SeqCst, SeqCst)
            .is_ok();
        // A disarm that raced with us already did what the else branch would.
        if !taken || armed != button_code(button) || touch {
            return false;
        }
        let (left, top) = unpack(self.area_lt.load(SeqCst));
        let (right, bottom) = unpack(self.area_rb.load(SeqCst));
        x >= left && x < right && y >= top && y < bottom
    }

    /// Records a taken click and wakes the waiter. Called by the hook after `on_press` said
    /// true.
    fn record(&self, x: i32, y: i32, time: f64) {
        self.taken_point.store(pack(x, y), SeqCst);
        self.taken_time.store(time.to_bits(), SeqCst);
        self.taken.fetch_add(1, SeqCst);
        let wake = self.wake.load(SeqCst);
        if wake != 0 {
            // SAFETY: the handle was created by `wake_handle` and is never closed.
            let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
        }
    }

    /// The last taken click (count 0: none yet).
    pub fn taken(&self) -> TakenClick {
        let count = self.taken.load(SeqCst);
        let (x, y) = unpack(self.taken_point.load(SeqCst));
        TakenClick {
            count,
            x,
            y,
            time: f64::from_bits(self.taken_time.load(SeqCst)),
        }
    }

    /// An auto-reset event set after every taken click. Created on the first call (not in
    /// the hook), never closed. Invalid if it could not be created.
    pub fn wake_handle(&self) -> HANDLE {
        let current = self.wake.load(SeqCst);
        if current != 0 {
            return HANDLE(current as *mut _);
        }
        // SAFETY: no pointers; auto-reset, not signalled, unnamed. Kept for the process's life.
        let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
            return HANDLE::default();
        };
        match self
            .wake
            .compare_exchange(0, event.0 as isize, SeqCst, SeqCst)
        {
            Ok(_) => event,
            Err(existing) => {
                // SAFETY: our own new handle, which nobody else saw.
                let _ = unsafe { CloseHandle(event) };
                HANDLE(existing as *mut _)
            }
        }
    }
}

impl Default for ClickGate {
    fn default() -> Self {
        ClickGate::new()
    }
}

// MARK: Wheel gate

/// Swallows wheel events while a recording runs and its zoom modifier is held: the zoom
/// gesture of `recording::input`. The app under the cursor never sees those events.
///
/// Three facts decide, all read in the hook callback without a lock:
/// - armed: a recording runs (`recording::input` arms the gate at start and disarms it at
///   stop, on shutdown and when its thread ends however it ends);
/// - held: the recording's keyboard hook saw exactly the modifier go down and stay down
///   (it sets this bit on its own thread in input order, so it is current when the wheel
///   event that follows a key change reaches the mouse hook);
/// - the modifier keys are really down right now (`GetAsyncKeyState`). This check makes a
///   stale `held` bit harmless: if the keyboard hook died without clearing it, a wheel event
///   without the modifier still passes.
///
/// Injected wheel events are never taken. A taken event is not queued for the rest of the
/// app (to it, the event did not happen); its delta is added up here instead and
/// [`wake_handle`](Self::wake_handle) is set. The recording reads the sum with
/// [`take`](Self::take).
pub struct WheelGate {
    /// [`WHEEL_ARMED`], [`WHEEL_HELD`], [`WHEEL_CTRL`].
    state: AtomicU64,
    /// Sum of the deltas taken and not yet read, in `WHEEL_DELTA` units (120 per notch).
    delta: AtomicI64,
    /// Events taken since start.
    taken: AtomicU64,
    /// An auto-reset event handle (0 until first asked for). Never closed.
    wake: AtomicIsize,
}

/// A recording runs.
const WHEEL_ARMED: u64 = 1;
/// The zoom modifier is held, as the recording's keyboard hook saw it.
const WHEEL_HELD: u64 = 2;
/// The modifier is Ctrl+Alt: Ctrl must be down too.
const WHEEL_CTRL: u64 = 4;

/// The process's one wheel gate, read by the process's one mouse hook.
pub static WHEEL_GATE: WheelGate = WheelGate::new();

impl WheelGate {
    pub const fn new() -> WheelGate {
        WheelGate {
            state: AtomicU64::new(0),
            delta: AtomicI64::new(0),
            taken: AtomicU64::new(0),
            wake: AtomicIsize::new(0),
        }
    }

    /// A recording started. `ctrl_too`: the modifier is Ctrl+Alt rather than Alt. Clears
    /// `held` and any delta left from before.
    pub fn arm(&self, ctrl_too: bool) {
        self.delta.store(0, SeqCst);
        self.state
            .store(WHEEL_ARMED | if ctrl_too { WHEEL_CTRL } else { 0 }, SeqCst);
    }

    /// The recording stopped: nothing is taken any more, and the held bit is cleared.
    pub fn disarm(&self) {
        self.state.store(0, SeqCst);
        self.delta.store(0, SeqCst);
    }

    /// Set by the recording's keyboard hook when the modifier goes down (alone) or up.
    pub fn set_held(&self, held: bool) {
        if held {
            self.state.fetch_or(WHEEL_HELD, SeqCst);
        } else {
            self.state.fetch_and(!WHEEL_HELD, SeqCst);
        }
    }

    pub fn is_armed(&self) -> bool {
        self.state.load(SeqCst) & WHEEL_ARMED != 0
    }

    pub fn is_held(&self) -> bool {
        self.state.load(SeqCst) & WHEEL_HELD != 0
    }

    /// The decision for one wheel event, made in the hook callback. True: swallow it (its
    /// delta is recorded). `keys_down(ctrl_too)` says whether Alt (and Ctrl when `ctrl_too`)
    /// are down right now; it is asked only when armed and held.
    pub fn on_wheel(
        &self,
        delta: i32,
        injected: bool,
        keys_down: impl FnOnce(bool) -> bool,
    ) -> bool {
        if injected {
            return false;
        }
        let state = self.state.load(SeqCst);
        if state & (WHEEL_ARMED | WHEEL_HELD) != WHEEL_ARMED | WHEEL_HELD {
            return false;
        }
        if !keys_down(state & WHEEL_CTRL != 0) {
            return false;
        }
        self.delta.fetch_add(i64::from(delta), SeqCst);
        self.taken.fetch_add(1, SeqCst);
        let wake = self.wake.load(SeqCst);
        if wake != 0 {
            // SAFETY: the handle was created by `wake_handle` and is never closed.
            let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
        }
        true
    }

    /// The delta taken since the last call (`WHEEL_DELTA` units), and the number of events
    /// taken since start.
    pub fn take(&self) -> (i64, u64) {
        (self.delta.swap(0, SeqCst), self.taken.load(SeqCst))
    }

    /// An auto-reset event set after every taken event. Created on the first call (not in
    /// the hook), never closed. Invalid if it could not be created.
    pub fn wake_handle(&self) -> HANDLE {
        let current = self.wake.load(SeqCst);
        if current != 0 {
            return HANDLE(current as *mut _);
        }
        // SAFETY: no pointers; auto-reset, not signalled, unnamed. Kept for the process's life.
        let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
            return HANDLE::default();
        };
        match self
            .wake
            .compare_exchange(0, event.0 as isize, SeqCst, SeqCst)
        {
            Ok(_) => event,
            Err(existing) => {
                // SAFETY: our own new handle, which nobody else saw.
                let _ = unsafe { CloseHandle(event) };
                HANDLE(existing as *mut _)
            }
        }
    }
}

impl Default for WheelGate {
    fn default() -> Self {
        WheelGate::new()
    }
}

/// Alt (and Ctrl when `ctrl_too`) down right now. Cheap: reads the async key state.
fn zoom_keys_down(ctrl_too: bool) -> bool {
    // SAFETY: plain value arguments; the high bit is set while the key is down.
    unsafe {
        GetAsyncKeyState(i32::from(VK_MENU.0)) < 0
            && (!ctrl_too || GetAsyncKeyState(i32::from(VK_CONTROL.0)) < 0)
    }
}

/// What the callback reaches. Only the hook thread touches it.
struct Context {
    shared: Arc<Shared>,
    clicks: ClickCounter,
    /// The button whose press the gate took: its release is swallowed too.
    swallow_release: Option<Button>,
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let started = Instant::now();
        // SAFETY: for HC_ACTION, `lparam` points to an MSLLHOOKSTRUCT that is valid for the
        // duration of this call; it is copied out at once.
        let info = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
        let swallow = catch_unwind(AssertUnwindSafe(|| handle(wparam.0 as u32, &info, started)));
        if matches!(swallow, Ok(true)) {
            // Taken by the click gate: the press (or its release) goes nowhere.
            return LRESULT(1);
        }
    }
    // SAFETY: passes the hook's own arguments on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Whether to swallow a release: the one of the press the gate took. Clears the mark.
pub fn swallows_release(pending: &mut Option<Button>, button: Button, injected: bool) -> bool {
    if !injected && *pending == Some(button) {
        *pending = None;
        return true;
    }
    false
}

/// Returns true when the event must be swallowed.
fn handle(message: u32, info: &MSLLHOOKSTRUCT, started: Instant) -> bool {
    CONTEXT.with(|context| {
        let Ok(mut context) = context.try_borrow_mut() else {
            return false;
        };
        let Some(context) = context.as_mut() else {
            return false;
        };
        let (x, y) = (info.pt.x, info.pt.y);
        let time = started.duration_since(epoch()).as_secs_f64();
        let injected = info.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0;
        let xbutton = if (info.mouseData >> 16) as u16 == XBUTTON1 {
            Button::X1
        } else {
            Button::X2
        };
        let event = match message {
            WM_MOUSEMOVE => Some(MouseEvent::Move {
                x,
                y,
                time,
                injected,
            }),
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                let button = match message {
                    WM_LBUTTONDOWN => Button::Left,
                    WM_RBUTTONDOWN => Button::Right,
                    WM_MBUTTONDOWN => Button::Middle,
                    _ => xbutton,
                };
                let touch = is_pen_or_touch(info.dwExtraInfo);
                if CLICK_GATE.on_press(button, injected, touch, x, y) {
                    context.swallow_release = Some(button);
                    CLICK_GATE.record(x, y, time);
                    finish(&context.shared, started);
                    return true;
                }
                let window = top_window_at(info.pt);
                let click_count = context.clicks.press(
                    button,
                    (x, y),
                    info.time,
                    || click_limits(info.pt),
                    window,
                    injected || touch,
                );
                Some(MouseEvent::Down {
                    button,
                    x,
                    y,
                    time,
                    click_count,
                    injected,
                    window,
                    touch,
                })
            }
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
                let button = match message {
                    WM_LBUTTONUP => Button::Left,
                    WM_RBUTTONUP => Button::Right,
                    WM_MBUTTONUP => Button::Middle,
                    _ => xbutton,
                };
                if swallows_release(&mut context.swallow_release, button, injected) {
                    finish(&context.shared, started);
                    return true;
                }
                Some(MouseEvent::Up {
                    button,
                    x,
                    y,
                    time,
                    injected,
                })
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = (info.mouseData >> 16) as u16 as i16 as i32;
                if WHEEL_GATE.on_wheel(delta, injected, zoom_keys_down) {
                    // Taken by the recording's zoom: not queued.
                    finish(&context.shared, started);
                    return true;
                }
                Some(MouseEvent::Wheel {
                    delta,
                    horizontal: message == WM_MOUSEHWHEEL,
                    x,
                    y,
                    time,
                    injected,
                })
            }
            _ => None,
        };
        if let Some(event) = event {
            context.shared.push(event);
        }
        finish(&context.shared, started);
        false
    })
}

/// Counts the callback and its time.
fn finish(shared: &Shared, started: Instant) {
    shared.callbacks.fetch_add(1, Relaxed);
    let spent = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    shared.max_callback_ns.fetch_max(spent, Relaxed);
}

/// The top-level window at `point`. `WindowFromPoint` reads window manager state and only
/// sends `WM_NCHITTEST` to windows of the calling thread, and this thread owns none, so it
/// never waits on another process; it costs microseconds, and only presses pay for it.
fn top_window_at(point: POINT) -> isize {
    // SAFETY: value arguments only.
    unsafe {
        let child = WindowFromPoint(point);
        if child.is_invalid() {
            return 0;
        }
        let root = GetAncestor(child, GA_ROOT);
        if root.is_invalid() {
            child.0 as isize
        } else {
            root.0 as isize
        }
    }
}

/// The double-click time and rectangle at `point`, the rectangle scaled for the DPI of the
/// monitor there, so it matches the physical pixels of the hook.
fn click_limits(point: POINT) -> ClickLimits {
    // SAFETY: plain value arguments; `dpi_x`/`dpi_y` outlive the call.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let (mut dpi_x, mut dpi_y) = (96, 96);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err() {
            (dpi_x, dpi_y) = (96, 96);
        }
        ClickLimits {
            time_ms: GetDoubleClickTime(),
            width: GetSystemMetricsForDpi(SM_CXDOUBLECLK, dpi_x),
            height: GetSystemMetricsForDpi(SM_CYDOUBLECLK, dpi_y),
        }
    }
}

/// The body of the hook thread. Sends the outcome of setting the hook through `ready`.
fn run(shared: Arc<Shared>, installed: Arc<AtomicBool>, ready: mpsc::Sender<Result<u32>>) {
    let mut msg = MSG::default();
    // SAFETY: plain calls on this thread. The DPI context makes hook points and monitor DPI
    // physical. `PeekMessageW` creates the message queue, so `WM_STOP` can be posted as soon
    // as `ready` is sent.
    let (thread_id, module) = unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        (GetCurrentThreadId(), GetModuleHandleW(None))
    };
    // The first call loads and initialises what the DPI functions need (about 1 ms); do it
    // here so the first click's callback does not pay for it.
    click_limits(POINT::default());
    CONTEXT.set(Some(Context {
        shared,
        clicks: ClickCounter::default(),
        swallow_release: None,
    }));
    // SAFETY: `hook_proc` matches HOOKPROC and lives for the whole program.
    let hook: Result<HHOOK> = module.and_then(|module| unsafe {
        SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), Some(module.into()), 0)
    });
    let hook = match hook {
        Ok(hook) => hook,
        Err(error) => {
            CONTEXT.set(None);
            let _ = ready.send(Err(error));
            return;
        }
    };
    installed.store(true, Relaxed);
    let _ = ready.send(Ok(thread_id));
    drop(ready);

    // SAFETY: `msg` is a valid MSG for the calls to fill and read. This thread owns no
    // windows; dispatching keeps any message other code might post harmless.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.hwnd.is_invalid() && msg.message == WM_STOP {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    installed.store(false, Relaxed);
    // SAFETY: `hook` was set by this thread and is removed once. It may already be gone
    // if Windows removed it; the error is ignored.
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    CONTEXT.set(None);
}

/// One hook thread.
struct Worker {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
    installed: Arc<AtomicBool>,
}

impl Worker {
    fn spawn(shared: &Arc<Shared>) -> Result<Worker> {
        let installed = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = Arc::clone(shared);
        let thread_installed = Arc::clone(&installed);
        let thread = thread::Builder::new()
            .name("mouse-hook".into())
            .spawn(move || run(thread_shared, thread_installed, ready_tx))
            .map_err(|_| Error::from(E_FAIL))?;
        // The thread always sends before it ends, unless it panicked.
        let thread_id = match ready_rx.recv() {
            Ok(Ok(id)) => id,
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(Error::from(E_FAIL));
            }
        };
        Ok(Worker {
            thread: Some(thread),
            thread_id,
            installed,
        })
    }

    fn is_alive(&self) -> bool {
        self.installed.load(Relaxed) && self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Worker {
    /// Asks the thread to unhook and end, and joins it. If it does not end within
    /// `STOP_TIMEOUT`, it is left running detached rather than blocking the caller; it only
    /// holds an `Arc` to the shared state, so nothing it uses is freed under it.
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        for _ in 0..10 {
            // SAFETY: posts a message with no pointers to a thread of this process.
            if unsafe { PostThreadMessageW(self.thread_id, WM_STOP, WPARAM(0), LPARAM(0)) }.is_ok()
                || thread.is_finished()
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let handle = HANDLE(thread.as_raw_handle());
        // SAFETY: `handle` is the live thread handle owned by `thread`, which outlives the wait.
        let ended = unsafe { WaitForSingleObject(handle, STOP_TIMEOUT.as_millis() as u32) }
            == WAIT_OBJECT_0;
        if ended {
            let _ = thread.join();
        }
    }
}

/// Stall self-check state.
#[derive(Default)]
struct StallCheck {
    cursor: Option<POINT>,
    callbacks: u64,
    strikes: u32,
}

/// The global mouse hook. Belongs to the thread that started it, which drains it.
pub struct MouseHook {
    // Dropped first, so the thread is stopped before the owner's handle to `shared` goes.
    worker: Option<Worker>,
    shared: Arc<Shared>,
    stall: StallCheck,
}

impl MouseHook {
    /// Starts the hook thread and sets the hook. Returns once the hook is set or failed.
    pub fn start() -> Result<MouseHook> {
        epoch();
        // SAFETY: no pointers are passed; the returned handle is owned by `Event`.
        let wake = Event(unsafe { CreateEventW(None, false, false, None)? });
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::with_capacity(QUEUE_CAPACITY)),
            wake,
            callbacks: AtomicU64::new(0),
            max_callback_ns: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let worker = Worker::spawn(&shared)?;
        Ok(MouseHook {
            worker: Some(worker),
            shared,
            stall: StallCheck::default(),
        })
    }

    /// An auto-reset event, set when an event arrives in an empty queue. Add it to the
    /// handles of `MsgWaitForMultipleObjectsEx` and call [`drain`](Self::drain) after every
    /// wait. Stays the same across [`restart`](Self::restart); do not close it.
    pub fn wake_handle(&self) -> HANDLE {
        self.shared.wake.0
    }

    /// Appends the events received so far to `out`, oldest first. Reuse `out` to avoid
    /// allocating while the lock is held.
    pub fn drain(&self, out: &mut Vec<MouseEvent>) {
        out.extend(self.shared.queue().drain(..));
    }

    /// True if the hook thread runs and the hook was set. Does not notice a hook that
    /// Windows removed for being slow; see [`seems_stalled`](Self::seems_stalled).
    pub fn is_alive(&self) -> bool {
        self.worker.as_ref().is_some_and(Worker::is_alive)
    }

    /// Cheap self-check, to call about once a second: true when the cursor moved since the
    /// last call but no callback ran, twice in a row. A cursor moved by `SetCursorPos` or
    /// over an elevated foreground window can cause a false alarm, which costs only a
    /// restart.
    pub fn seems_stalled(&mut self) -> bool {
        let mut cursor = POINT::default();
        // SAFETY: `cursor` outlives the call. Fails on the secure desktop; skip then.
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return false;
        }
        let callbacks = self.shared.callbacks.load(Relaxed);
        let moved = self.stall.cursor.is_some_and(|last| last != cursor);
        let silent = callbacks == self.stall.callbacks;
        self.stall.strikes = if moved && silent {
            self.stall.strikes + 1
        } else {
            0
        };
        self.stall.cursor = Some(cursor);
        self.stall.callbacks = callbacks;
        self.stall.strikes >= 2
    }

    /// Stops the hook thread and starts a new one. Queued events and the wake handle stay.
    pub fn restart(&mut self) -> Result<()> {
        self.worker = None;
        self.stall = StallCheck::default();
        self.worker = Some(Worker::spawn(&self.shared)?);
        Ok(())
    }

    /// Id of the current hook thread, for diagnostics.
    pub fn thread_id(&self) -> Option<u32> {
        self.worker.as_ref().map(|w| w.thread_id)
    }

    pub fn stats(&self) -> HookStats {
        HookStats {
            callbacks: self.shared.callbacks.load(Relaxed),
            max_callback: Duration::from_nanos(self.shared.max_callback_ns.load(Relaxed)),
            dropped: self.shared.dropped.load(Relaxed),
        }
    }
}

// MARK: Key presses

/// Counts the user's key presses (`WH_KEYBOARD_LL` on its own thread, rule 1): the drop
/// router stops a delivery when the user starts typing. Only a counter is kept, never which
/// key. Injected keys (ours and others') and the modifier keys do not count; modifiers are
/// waited out by the paste itself.
pub struct KeyWatch {
    presses: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

thread_local! {
    static KEY_PRESSES: RefCell<Option<Arc<AtomicU64>>> = const { RefCell::new(None) };
}

/// Virtual keys that only modify others (Shift, Ctrl, Alt, Win, left and right).
const MODIFIER_KEYS: &[u32] = &[
    0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5,
];

/// Does a low-level key event count as the user typing?
pub fn counts_as_typing(message: u32, vk: u32, flags: u32, extra_info: usize) -> bool {
    (message == WM_KEYDOWN || message == WM_SYSKEYDOWN)
        && flags & LLKHF_INJECTED.0 == 0
        && extra_info != crate::input::INJECTED_MARKER
        && !MODIFIER_KEYS.contains(&vk)
}

unsafe extern "system" fn key_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, `lparam` points to a KBDLLHOOKSTRUCT valid for this call.
        let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if counts_as_typing(wparam.0 as u32, info.vkCode, info.flags.0, info.dwExtraInfo) {
            let _ = catch_unwind(|| {
                KEY_PRESSES.with(|cell| {
                    if let Ok(cell) = cell.try_borrow()
                        && let Some(presses) = cell.as_ref()
                    {
                        presses.fetch_add(1, Relaxed);
                    }
                })
            });
        }
    }
    // SAFETY: passes the hook's own arguments on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

impl KeyWatch {
    /// Starts the keyboard hook thread. Returns once the hook is set or failed.
    pub fn start() -> Result<KeyWatch> {
        let presses = Arc::new(AtomicU64::new(0));
        let thread_presses = presses.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();
        let thread = thread::Builder::new()
            .name("key-hook".into())
            .spawn(move || {
                let mut msg = MSG::default();
                // SAFETY: plain calls on this thread; `PeekMessageW` creates its queue.
                let (thread_id, module) = unsafe {
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                    (GetCurrentThreadId(), GetModuleHandleW(None))
                };
                KEY_PRESSES.with(|cell| *cell.borrow_mut() = Some(thread_presses));
                // SAFETY: `key_proc` matches HOOKPROC and lives for the whole program.
                let hook = module.and_then(|module| unsafe {
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), Some(module.into()), 0)
                });
                let hook = match hook {
                    Ok(hook) => hook,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(thread_id));
                // SAFETY: as in `run`.
                unsafe {
                    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                        if msg.hwnd.is_invalid() && msg.message == WM_STOP {
                            break;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    // SAFETY: set by this thread above, removed once.
                    let _ = UnhookWindowsHookEx(hook);
                }
            })
            .map_err(|_| Error::from(E_FAIL))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(KeyWatch {
                presses,
                thread: Some(thread),
                thread_id,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(Error::from(E_FAIL)),
        }
    }

    /// Key presses counted since start.
    pub fn presses(&self) -> u64 {
        self.presses.load(Relaxed)
    }
}

impl Drop for KeyWatch {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        // SAFETY: posts a message with no pointers to a thread of this process.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_STOP, WPARAM(0), LPARAM(0)) };
        let until = Instant::now() + STOP_TIMEOUT;
        while !thread.is_finished() && Instant::now() < until {
            thread::sleep(Duration::from_millis(2));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: ClickLimits = ClickLimits {
        time_ms: 500,
        width: 4,
        height: 4,
    };

    #[test]
    fn presses_on_different_windows_never_pair() {
        let mut c = ClickCounter::default();
        assert_eq!(c.down(Button::Left, 0, 0, 1000, LIMITS, 0x10), 1);
        // The first click closed a dialog: the second lands on the window it exposed.
        assert_eq!(c.down(Button::Left, 0, 0, 1100, LIMITS, 0x20), 1);
        assert_eq!(c.down(Button::Left, 0, 0, 1200, LIMITS, 0x20), 2);
    }

    #[test]
    fn injected_and_touch_presses_do_not_take_part_in_counting() {
        let mut c = ClickCounter::default();
        assert_eq!(
            c.press(Button::Left, (0, 0), 1000, || LIMITS, 0x10, false),
            1
        );
        // A synthetic press in between neither counts nor resets.
        assert_eq!(
            c.press(Button::Left, (0, 0), 1050, || LIMITS, 0x10, true),
            1
        );
        assert_eq!(
            c.press(Button::Left, (0, 0), 1100, || LIMITS, 0x10, false),
            2
        );
        // Two synthetic presses are never a double-click.
        let mut c = ClickCounter::default();
        assert_eq!(
            c.press(Button::Left, (0, 0), 1000, || LIMITS, 0x10, true),
            1
        );
        assert_eq!(
            c.press(Button::Left, (0, 0), 1100, || LIMITS, 0x10, true),
            1
        );
    }

    #[test]
    fn pen_and_touch_presses_are_recognised() {
        assert!(is_pen_or_touch(0xFF51_5700));
        assert!(is_pen_or_touch(0xFF51_5780));
        assert!(!is_pen_or_touch(0));
        assert!(!is_pen_or_touch(crate::input::INJECTED_MARKER));
    }

    #[test]
    fn only_the_users_non_modifier_keys_count_as_typing() {
        assert!(counts_as_typing(WM_KEYDOWN, 0x41, 0, 0));
        assert!(counts_as_typing(WM_SYSKEYDOWN, 0x09, 0, 0));
        // Ours, other injected keys, key-ups and modifiers do not.
        assert!(!counts_as_typing(
            WM_KEYDOWN,
            0x41,
            0,
            crate::input::INJECTED_MARKER
        ));
        assert!(!counts_as_typing(WM_KEYDOWN, 0x41, LLKHF_INJECTED.0, 0));
        assert!(!counts_as_typing(0x0101, 0x41, 0, 0));
        for vk in [0x10, 0xA0, 0x11, 0xA3, 0x12, 0x5B] {
            assert!(!counts_as_typing(WM_KEYDOWN, vk, 0, 0), "{vk:#x}");
        }
    }

    #[test]
    fn counts_double_and_triple_clicks() {
        let mut c = ClickCounter::default();
        assert_eq!(c.down(Button::Left, 100, 100, 1000, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Left, 101, 99, 1200, LIMITS, 0), 2);
        assert_eq!(c.down(Button::Left, 102, 98, 1700, LIMITS, 0), 3);
    }

    #[test]
    fn late_click_starts_over() {
        let mut c = ClickCounter::default();
        assert_eq!(c.down(Button::Left, 0, 0, 1000, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Left, 0, 0, 1501, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Left, 0, 0, 2001, LIMITS, 0), 2);
    }

    #[test]
    fn far_click_starts_over() {
        let mut c = ClickCounter::default();
        assert_eq!(c.down(Button::Left, 0, 0, 1000, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Left, 3, 0, 1100, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Left, 3, -3, 1200, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Left, 1, -1, 1300, LIMITS, 0), 2);
    }

    #[test]
    fn rectangle_is_that_of_the_previous_click() {
        let mut c = ClickCounter::default();
        let wide = ClickLimits {
            width: 20,
            ..LIMITS
        };
        assert_eq!(c.down(Button::Left, 0, 0, 1000, wide, 0), 1);
        assert_eq!(c.down(Button::Left, 10, 0, 1100, LIMITS, 0), 2);
    }

    #[test]
    fn another_button_starts_over() {
        let mut c = ClickCounter::default();
        assert_eq!(c.down(Button::Left, 0, 0, 1000, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Right, 0, 0, 1100, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Left, 0, 0, 1200, LIMITS, 0), 1);
        assert_eq!(c.down(Button::X1, 0, 0, 1300, LIMITS, 0), 1);
        assert_eq!(c.down(Button::X2, 0, 0, 1400, LIMITS, 0), 1);
    }

    #[test]
    fn tick_wraps_around() {
        let mut c = ClickCounter::default();
        assert_eq!(c.down(Button::Left, 0, 0, u32::MAX - 100, LIMITS, 0), 1);
        assert_eq!(c.down(Button::Left, 0, 0, 200, LIMITS, 0), 2);
        // A tick before the previous one (out of order) reads as far in the future.
        assert_eq!(c.down(Button::Left, 0, 0, 100, LIMITS, 0), 1);
    }

    fn shared() -> Shared {
        Shared {
            queue: Mutex::new(VecDeque::with_capacity(QUEUE_CAPACITY)),
            // SAFETY: no pointers are passed; the handle is owned by `Event`.
            wake: Event(unsafe { CreateEventW(None, false, false, None) }.unwrap()),
            callbacks: AtomicU64::new(0),
            max_callback_ns: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    fn mv(x: i32) -> MouseEvent {
        MouseEvent::Move {
            x,
            y: 0,
            time: x as f64,
            injected: false,
        }
    }

    #[test]
    fn moves_coalesce_but_keep_their_order_with_buttons() {
        let s = shared();
        let down = MouseEvent::Down {
            button: Button::Left,
            x: 2,
            y: 0,
            time: 2.5,
            click_count: 1,
            injected: false,
            window: 0,
            touch: false,
        };
        let up = MouseEvent::Up {
            button: Button::Left,
            x: 4,
            y: 0,
            time: 4.5,
            injected: false,
        };
        for event in [mv(1), mv(2), down, mv(3), mv(4), up, mv(5), mv(6), mv(7)] {
            s.push(event);
        }
        let mut out = Vec::new();
        out.extend(s.queue().drain(..));
        assert_eq!(out, vec![mv(2), down, mv(4), up, mv(7)]);
    }

    #[test]
    fn wake_fires_only_on_empty_queue() {
        let s = shared();
        let signalled = |s: &Shared| unsafe { WaitForSingleObject(s.wake.0, 0) } == WAIT_OBJECT_0;
        s.push(mv(1));
        s.push(mv(2));
        assert!(signalled(&s));
        assert!(!signalled(&s));
        s.queue().clear();
        s.push(mv(3));
        assert!(signalled(&s));
    }

    #[test]
    fn full_queue_drops_new_events_and_counts_them() {
        let s = shared();
        let wheel = MouseEvent::Wheel {
            delta: 120,
            horizontal: false,
            x: 0,
            y: 0,
            time: 0.0,
            injected: false,
        };
        for _ in 0..QUEUE_CAPACITY + 3 {
            s.push(wheel);
        }
        assert_eq!(s.queue().len(), QUEUE_CAPACITY);
        assert_eq!(s.dropped.load(Relaxed), 3);
    }

    // --- Click gate ---

    #[test]
    fn unarmed_gate_lets_every_press_through() {
        let gate = ClickGate::new();
        for button in [Button::Left, Button::Right, Button::Middle] {
            assert!(!gate.on_press(button, false, false, 0, 0));
        }
        assert!(!gate.is_armed());
    }

    #[test]
    fn armed_gate_takes_one_press_of_its_button_and_its_release() {
        let gate = ClickGate::new();
        let epoch = gate.epoch();
        assert!(gate.arm(epoch, Button::Left));
        assert!(gate.on_press(Button::Left, false, false, 0, 0));
        // Only that one: the next press passes.
        assert!(!gate.is_armed());
        assert!(!gate.on_press(Button::Left, false, false, 0, 0));
        let mut pending = Some(Button::Left);
        assert!(!swallows_release(&mut pending, Button::Right, false));
        assert!(!swallows_release(&mut pending, Button::Left, true));
        assert!(swallows_release(&mut pending, Button::Left, false));
        assert!(!swallows_release(&mut pending, Button::Left, false));
    }

    #[test]
    fn other_buttons_touch_and_injected_presses_are_never_taken() {
        let gate = ClickGate::new();
        assert!(gate.arm(gate.epoch(), Button::Left));
        // Injected presses do not even look at the gate.
        assert!(!gate.on_press(Button::Left, true, false, 0, 0));
        assert!(gate.is_armed());
        // Another button disarms.
        assert!(!gate.on_press(Button::Right, false, false, 0, 0));
        assert!(!gate.is_armed());
        // A pen or touch press of the armed button disarms and passes.
        assert!(gate.arm(gate.epoch(), Button::Left));
        assert!(!gate.on_press(Button::Left, false, true, 0, 0));
        assert!(!gate.is_armed());
    }

    #[test]
    fn a_key_change_disarms_and_stale_arms_fail() {
        let gate = ClickGate::new();
        let held = gate.disarm(); // the modifier went down
        assert!(gate.arm(held, Button::Left));
        let after = gate.disarm(); // the modifier went up (keyboard hook)
        assert_eq!(after, held + 1);
        assert!(!gate.on_press(Button::Left, false, false, 0, 0));
        // The summoner, a step behind, tries to arm with the old epoch: refused.
        assert!(!gate.arm(held, Button::Left));
        assert!(!gate.on_press(Button::Left, false, false, 0, 0));
        // Arming twice is refused too.
        assert!(gate.arm(after, Button::Left));
        assert!(!gate.arm(after, Button::Right));
    }

    #[test]
    fn withdrawing_keeps_the_epoch_so_the_same_hold_can_arm_again() {
        let gate = ClickGate::new();
        let held = gate.disarm();
        assert!(gate.arm(held, Button::Left));
        gate.withdraw(); // the cursor left empty space
        assert!(!gate.is_armed());
        assert!(!gate.on_press(Button::Left, false, false, 0, 0));
        assert!(gate.arm(held, Button::Left)); // and came back
        assert!(gate.on_press(Button::Left, false, false, 0, 0));
        // After a key change, withdrawing does not bring an old epoch back.
        let held = gate.epoch();
        assert!(gate.arm(held, Button::Left));
        let after = gate.disarm();
        gate.withdraw();
        assert_eq!(gate.epoch(), after);
        assert!(!gate.arm(held, Button::Left));
    }

    // --- Wheel gate ---

    #[test]
    fn wheel_gate_takes_only_while_armed_held_and_the_keys_are_down() {
        let gate = WheelGate::new();
        // Not recording: nothing is taken, the keys are not even asked.
        assert!(!gate.on_wheel(120, false, |_| panic!("asked")));
        gate.set_held(true);
        assert!(!gate.on_wheel(120, false, |_| panic!("asked")));
        // Recording, modifier not held: a plain wheel event passes.
        gate.arm(false);
        assert!(!gate.on_wheel(120, false, |_| panic!("asked")));
        gate.set_held(true);
        // Held, but the keys are up now (a stale bit): passes.
        assert!(!gate.on_wheel(120, false, |_| false));
        // Held and down: taken, and the delta adds up.
        assert!(gate.on_wheel(120, false, |ctrl| !ctrl));
        assert!(gate.on_wheel(-240, false, |_| true));
        assert_eq!(gate.take(), (-120, 2));
        assert_eq!(gate.take(), (0, 2));
        // Injected wheel events are never taken.
        assert!(!gate.on_wheel(120, true, |_| true));
        // Released: passes again.
        gate.set_held(false);
        assert!(!gate.on_wheel(120, false, |_| true));
    }

    #[test]
    fn wheel_gate_disarm_releases_everything() {
        let gate = WheelGate::new();
        gate.arm(true);
        gate.set_held(true);
        assert!(gate.on_wheel(120, false, |ctrl| ctrl));
        gate.disarm();
        assert!(!gate.is_armed() && !gate.is_held());
        assert!(!gate.on_wheel(120, false, |_| true));
        assert_eq!(gate.take().0, 0);
        // Ctrl+Alt mode asks for Ctrl too.
        gate.arm(true);
        gate.set_held(true);
        let mut asked = None;
        gate.on_wheel(120, false, |ctrl| {
            asked = Some(ctrl);
            true
        });
        assert_eq!(asked, Some(true));
    }

    #[test]
    fn a_press_outside_the_armed_area_disarms_and_passes_with_its_release() {
        let gate = ClickGate::new();
        let area = (-200, 100, 50, 180);
        // Outside (right and bottom are exclusive): passes, and the gate is gone.
        for (x, y) in [(50, 120), (0, 180), (-201, 120), (0, 99)] {
            assert!(gate.arm_in(gate.epoch(), Button::Left, area));
            assert!(!gate.on_press(Button::Left, false, false, x, y), "{x},{y}");
            assert!(!gate.is_armed());
        }
        // So its release passes too: the hook marks a release only for a swallowed press.
        let mut pending = None;
        assert!(!swallows_release(&mut pending, Button::Left, false));
        // Inside, corners included: swallowed.
        for (x, y) in [(-200, 100), (49, 179), (0, 150)] {
            assert!(gate.arm_in(gate.epoch(), Button::Left, area));
            assert!(gate.on_press(Button::Left, false, false, x, y), "{x},{y}");
            assert!(!gate.is_armed());
        }
        // Another button or a touch inside still only disarms.
        assert!(gate.arm_in(gate.epoch(), Button::Left, area));
        assert!(!gate.on_press(Button::Right, false, false, 0, 150));
        assert!(gate.arm_in(gate.epoch(), Button::Left, area));
        assert!(!gate.on_press(Button::Left, false, true, 0, 150));
        // A new arm replaces the area; an armed gate refuses another arm and keeps its own.
        assert!(gate.arm_in(gate.epoch(), Button::Left, (0, 0, 10, 10)));
        assert!(!gate.arm_in(gate.epoch(), Button::Left, (100, 100, 200, 200)));
        assert!(gate.on_press(Button::Left, false, false, 5, 5));
        // `arm` alone is the whole screen.
        assert!(gate.arm(gate.epoch(), Button::Left));
        assert!(gate.on_press(Button::Left, false, false, i32::MIN, i32::MAX - 1));
    }

    #[test]
    fn taken_clicks_are_recorded_with_negative_coordinates() {
        let gate = ClickGate::new();
        assert_eq!(gate.taken().count, 0);
        gate.record(-1920, -5, 12.5);
        let taken = gate.taken();
        assert_eq!(
            (taken.count, taken.x, taken.y, taken.time),
            (1, -1920, -5, 12.5)
        );
    }
}
