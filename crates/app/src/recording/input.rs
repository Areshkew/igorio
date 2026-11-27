//! What the user does during a recording, turned into commands. From `RecordingInput.swift`.
//! No recording logic lives here: [`Machine`] turns keys, taken wheel events and the tool
//! shortcuts into [`Action`]s, and the recording UI (`ink_overlay::RecordingUi`) carries them
//! out and sends the viewport ones on to the app.
//!
//! # The zoom gesture on Windows
//!
//! macOS: Option + scroll zooms, holding Option moves the zoomed view with the cursor, letting
//! go leaves the view, a double tap of Option returns to the whole screen.
//!
//! Windows: **Alt** by default ([`ZoomModifier::Alt`], one config value; `CtrlAlt` is the
//! alternative). Alt is what the user already holds for the note gesture (which is blocked
//! while a recording runs), and it is the key in the same place as Option.
//!
//! The problem with Alt is the lone Alt: an Alt press and release with no other key in
//! between activates the foreground window's menu bar (or Office key tips, or the menu of VS
//! Code and Chromium). A zoom (Alt + wheel) or a hold (Alt, move, let go) would end in exactly
//! that, and a double tap would be two lone Alts, which turns the menu on and off again: a
//! visible flicker in the video. The note gesture's answer works here too (see
//! `notes::summoner`): **while Alt is still down, inject one unassigned key (`0xE8`)**. The
//! release that follows is then not a lone Alt. Nothing is ever swallowed for it.
//!
//! When to inject: on **every** press of Alt on its own while a recording runs, because the
//! first tap of a double tap cannot be told apart from a lone tap until the second comes, and
//! by then the first one would have toggled the menu. So during a recording a lone Alt tap
//! does not open menus (F10 still does; Alt+letter, Alt+Tab and Alt+F4 work as always, Alt is
//! really down). The key goes out from the recording UI thread right after it reads the Alt
//! press, and only if, right before `SendInput` (rule 7): no key changed since that press,
//! Alt is down, and Ctrl, Shift and Win are up. A tap shorter than that check (a few
//! milliseconds) is not masked and behaves as a lone Alt.
//!
//! Ctrl+Alt needs no mask: a release of Ctrl+Alt is never a lone Alt. Its cost is a
//! two-key chord to hold and to double tap. AltGr (right Alt on many layouts, which Windows
//! sends as a fake left Ctrl plus right Alt) never counts as the modifier in either mode.
//!
//! # What is swallowed, and why nothing can stay stuck
//!
//! - **Wheel events** while a recording runs and the modifier is held, decided in the mouse
//!   hook from `hook::WHEEL_GATE` (an atomic armed by the recording, a held bit kept by the
//!   keyboard hook below, and the real key state read in the callback). A wheel event
//!   without the modifier is never swallowed. No key or button state is involved, so nothing
//!   can stay down.
//! - **Drawing keys** (Ctrl+Z, Backspace, Delete, Esc) while a tool is in hand, decided in
//!   the keyboard hook below from [`DRAW_KEYS`]. Only key-downs (and their auto-repeats) are
//!   swallowed, never a key-up: a key whose press Windows never saw cannot be left down, and
//!   a lone key-up reaching an app is ignored by it. Modifiers are never swallowed.
//! - **Injected**: the mask key `0xE8` down and up in one `SendInput` call, as above, marked
//!   with `input::INJECTED_MARKER` so the app's own hooks skip it.
//!
//! Both gates are released at stop, on shutdown, and by a guard when the recording UI thread
//! ends in any way. A killed process takes its hooks with it: Windows removes them, so nothing
//! stays swallowed.
//!
//! # The drawing keys (A.2)
//!
//! `InkOverlay.swift` never lets its panel become key (`canBecomeKey` false): the recorded app
//! keeps the keyboard focus and keeps looking active in the video, and the global event tap
//! takes the drawing keys. The same here: the ink window never activates and never takes the
//! focus (`WS_EX_NOACTIVATE`), and a low-level keyboard hook on its own thread (rule 1)
//! swallows exactly the drawing keys while a tool is in hand. Taking the focus instead would
//! need no hook, but the recorded app would show as inactive (grey title bar, no caret) in the
//! video while drawing.
//!
//! # Threads
//!
//! The keyboard hook ([`KeyHook`]) only pumps messages. Its callback copies the event, updates
//! a small modifier state, sets the wheel gate's held bit, decides the drawing keys from one
//! atomic, pushes a small event into a preallocated queue, sets an event and returns. It never
//! allocates, logs or calls out.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use igorio_core::ink::{Ink, InkKey, InkTool};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThreadId, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
    PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use crate::hook::{self, WHEEL_GATE};
use crate::input::INJECTED_MARKER;
pub use crate::notes::summoner::Modifier as ZoomModifier;
use crate::notes::summoner::{MASK_KEY, is_altgr_ctrl};

// ---------------------------------------------------------------------------------------
// Constants (the Swift values)
// ---------------------------------------------------------------------------------------

/// Two presses of the modifier this close together are a double tap (`doubleTapWindow`).
pub const DOUBLE_TAP_WINDOW: f64 = 0.35;
/// Following starts this long after the modifier goes down, so the two taps of a double tap
/// do not tug the view toward the cursor first (`followDelay`). A wheel event starts it at
/// once.
pub const FOLLOW_DELAY: f64 = 0.15;
/// Zoom per wheel notch, in `RecordingViewport::zoom_by` units (`zoomAmount`: one line
/// notch is 0.12).
pub const ZOOM_PER_NOTCH: f64 = 0.12;
/// `WHEEL_DELTA`: one notch.
pub const WHEEL_DELTA: f64 = 120.0;

const VK_BACK: u32 = 0x08;
const VK_ESCAPE: u32 = 0x1B;
const VK_DELETE: u32 = 0x2E;
const VK_Z: u32 = 0x5A;

/// Key events the hook can queue between two drains.
const KEY_QUEUE_CAPACITY: usize = 256;
/// How long `KeyHook::drop` waits for its thread.
const STOP_WAIT: Duration = Duration::from_secs(1);
/// Thread message that ends the hook thread.
const WM_STOP: u32 = WM_APP + 1;

// ---------------------------------------------------------------------------------------
// Toggling the recording
// ---------------------------------------------------------------------------------------

/// Where the recording stands (`RecordingCapability.State`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    Idle,
    Starting,
    Recording,
    Stopping,
}

/// What the ToggleRecording shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Start,
    Stop,
    /// Starting or stopping already: wait.
    Ignore,
}

/// The ToggleRecording shortcut (`RecordingCapability.toggle()`). The wiring calls this when
/// `shortcuts::Hotkeys` reports the press.
pub fn toggle(state: RecordingState) -> Toggle {
    match state {
        RecordingState::Idle => Toggle::Start,
        RecordingState::Recording => Toggle::Stop,
        RecordingState::Starting | RecordingState::Stopping => Toggle::Ignore,
    }
}

// ---------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------

/// The modifier keys down, and whether any other key is down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub left_alt: bool,
    pub right_alt: bool,
    pub left_ctrl: bool,
    pub right_ctrl: bool,
    /// AltGr's fake left Ctrl.
    pub altgr: bool,
    pub shift: bool,
    pub win: bool,
    /// Any other key, by virtual key.
    others: [u64; 4],
}

impl Mods {
    /// The modifiers as `GetAsyncKeyState` reports them now (after a hook restart). Other
    /// keys are taken as up.
    pub fn now() -> Mods {
        let down = |vk: VIRTUAL_KEY| key_down(vk);
        Mods {
            left_alt: down(VK_LMENU),
            right_alt: down(VK_RMENU),
            left_ctrl: down(VK_LCONTROL),
            right_ctrl: down(VK_RCONTROL),
            altgr: false,
            shift: down(VK_LSHIFT) || down(VK_RSHIFT),
            win: down(VK_LWIN) || down(VK_RWIN),
            others: [0; 4],
        }
    }

    /// Applies a change; false when nothing changed (an auto-repeat).
    pub fn apply(&mut self, vk: u32, down: bool, altgr: bool) -> bool {
        let flag = match vk {
            0xA4 | 0x12 => &mut self.left_alt,
            0xA5 => &mut self.right_alt,
            0xA2 | 0x11 if altgr => &mut self.altgr,
            0xA2 | 0x11 => &mut self.left_ctrl,
            0xA3 => &mut self.right_ctrl,
            0xA0 | 0xA1 | 0x10 => &mut self.shift,
            0x5B | 0x5C => &mut self.win,
            vk => {
                let (word, bit) = ((vk as usize & 0xFF) / 64, 1u64 << (vk % 64));
                let was = self.others[word] & bit != 0;
                if down {
                    self.others[word] |= bit;
                } else {
                    self.others[word] &= !bit;
                }
                return was != down;
            }
        };
        let changed = *flag != down;
        *flag = down;
        // AltGr's release clears its fake Ctrl with a plain left Ctrl up: clear both.
        if !down && matches!(vk, 0xA2 | 0x11) {
            let was = self.altgr || self.left_ctrl;
            self.altgr = false;
            self.left_ctrl = false;
            return changed || was;
        }
        changed
    }

    pub fn alt(&self) -> bool {
        self.left_alt || self.right_alt
    }

    pub fn ctrl(&self) -> bool {
        self.left_ctrl || self.right_ctrl
    }

    fn others(&self) -> bool {
        self.others.iter().any(|&word| word != 0)
    }

    /// Exactly the zoom modifier is down, nothing else, and not AltGr.
    pub fn combo(&self, modifier: ZoomModifier) -> bool {
        let clean = !self.altgr && !self.shift && !self.win && !self.others();
        match modifier {
            ZoomModifier::Alt => clean && self.alt() && !self.ctrl(),
            ZoomModifier::CtrlAlt => clean && self.alt() && self.ctrl(),
        }
    }
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: plain value argument; the high bit is set while the key is down.
    (unsafe { GetAsyncKeyState(i32::from(vk.0)) }) < 0
}

/// Which drawing key a press is, given the modifiers down (`RecordingInput.handle`): Ctrl+Z
/// (Cmd+Z on macOS) undoes, Backspace or Delete clears, Esc puts the pen down. Any other
/// modifier makes it someone else's shortcut. Pen and highlighter are global shortcuts and
/// do not come through here.
pub fn drawing_key(vk: u32, mods: &Mods) -> Option<InkKey> {
    let ctrl = mods.ctrl() && !mods.altgr;
    let others = mods.alt() || mods.shift || mods.win || mods.altgr;
    match vk {
        VK_Z if ctrl && !others => Some(InkKey::Undo),
        VK_ESCAPE if !ctrl && !others => Some(InkKey::Done),
        VK_BACK | VK_DELETE if !ctrl && !others => Some(InkKey::Clear),
        _ => None,
    }
}

/// One key change as the recording's keyboard hook saw it. Auto-repeats are reported only
/// when the hook took them (a held Ctrl+Z undoes again, as on macOS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyInput {
    pub vk: u32,
    pub down: bool,
    pub repeat: bool,
    /// AltGr's fake left Ctrl.
    pub altgr: bool,
    /// The hook swallowed it (a drawing key while a tool was in hand).
    pub taken: bool,
    /// The hook's count of key changes, this one included (for the mask key's rule 7).
    pub changes: u64,
    /// The clock of `hook::now`.
    pub time: f64,
}

/// What the hook decided for one key event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    /// Swallow it: return nonzero without calling `CallNextHookEx`.
    pub swallow: bool,
    /// The zoom modifier is held after this event (the wheel gate's held bit).
    pub held: bool,
    /// What to queue for the recording UI, if anything.
    pub event: Option<KeyInput>,
}

/// The keyboard hook's state, pure so the decisions can be tested without a hook.
#[derive(Debug, Clone)]
pub struct HookState {
    /// Keys down as the hook saw them, to tell auto-repeats.
    down: [u64; 4],
    mods: Mods,
    changes: u64,
}

impl HookState {
    pub fn new(mods: Mods) -> HookState {
        HookState {
            down: [0; 4],
            mods,
            changes: 0,
        }
    }

    pub fn mods(&self) -> Mods {
        self.mods
    }

    /// One key event. `ours`: it carries `INJECTED_MARKER` (the mask key): not looked at.
    /// `injected`: synthetic input from anyone else: tracked, never swallowed. `draw`: the
    /// value of [`DRAW_KEYS`].
    #[allow(clippy::too_many_arguments)]
    pub fn on_key(
        &mut self,
        vk: u32,
        scan_code: u32,
        down: bool,
        injected: bool,
        ours: bool,
        draw: bool,
        modifier: ZoomModifier,
        time: f64,
    ) -> Verdict {
        if ours {
            return Verdict {
                swallow: false,
                held: self.mods.combo(modifier),
                event: None,
            };
        }
        let vk = vk & 0xFF;
        let (word, bit) = (vk as usize / 64, 1u64 << (vk % 64));
        let repeat = down && self.down[word] & bit != 0;
        if down {
            self.down[word] |= bit;
        } else {
            self.down[word] &= !bit;
        }
        let altgr = is_altgr_ctrl(vk, scan_code);
        if !repeat {
            self.mods.apply(vk, down, altgr);
            self.changes += 1;
        }
        let taken = down && !injected && draw && drawing_key(vk, &self.mods).is_some();
        let event = (!repeat || taken).then_some(KeyInput {
            vk,
            down,
            repeat,
            altgr,
            taken,
            changes: self.changes,
            time,
        });
        Verdict {
            swallow: taken,
            held: self.mods.combo(modifier),
            event,
        }
    }
}

// ---------------------------------------------------------------------------------------
// The state machine
// ---------------------------------------------------------------------------------------

/// What happens to the machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// The recording runs from now on.
    Start,
    /// The recording stopped.
    Stop,
    Key(KeyInput),
    /// Wheel events the gate took since the last step, summed (`WHEEL_DELTA` units;
    /// positive is away from the user or to the right).
    Wheel(i64),
    /// Time passed (deadlines).
    Tick,
    /// The keyboard hook restarted: these modifiers are down now.
    KeysReset(Mods),
    /// The Pen or Highlighter shortcut was pressed.
    ToolShortcut(InkTool),
    /// A key for the ink from somewhere other than the keyboard (a menu, the self-test).
    InkKey(InkKey),
}

/// What the machine asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// `RecordingViewport::zoom_by` this amount (positive zooms in).
    Zoom(f64),
    /// The area follows the cursor (true) or stays where it is (false).
    Follow(bool),
    /// Back to the whole screen.
    ResetZoom,
    /// The tool in hand changed.
    Tool(Option<InkTool>),
    /// The ink changed (undo, clear, a tool switch): draw it again.
    Redraw,
    /// The pen was put down: the ink fades away.
    FadeAway,
    /// Inject the mask key now, if the hook counted no key change since `changes`.
    Mask { changes: u64 },
}

/// The input rules of `RecordingInput.swift`. Pure: time comes in, actions go out. The ink
/// itself is passed to [`step`](Self::step) (one `Ink`, owned by the ink window).
#[derive(Debug, Clone)]
pub struct Machine {
    modifier: ZoomModifier,
    mask: bool,
    recording: bool,
    mods: Mods,
    held: bool,
    /// When the modifier last went down on its own, to pair a second tap with.
    last_tap: Option<f64>,
    follow_due: Option<f64>,
    following: bool,
    /// The hook restarted during this hold: no following until the modifier is released.
    stale: bool,
}

impl Machine {
    /// `mask`: request the mask key in Alt mode (off for tests that must not inject).
    pub fn new(modifier: ZoomModifier, mask: bool) -> Machine {
        Machine {
            modifier,
            mask,
            recording: false,
            mods: Mods::default(),
            held: false,
            last_tap: None,
            follow_due: None,
            following: false,
            stale: false,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording
    }

    pub fn is_following(&self) -> bool {
        self.following
    }

    pub fn modifier(&self) -> ZoomModifier {
        self.modifier
    }

    /// The wheel gate should be armed.
    pub fn wheel_armed(&self) -> bool {
        self.recording
    }

    /// The drawing keys should be swallowed ([`DRAW_KEYS`]).
    pub fn keys_armed(&self, ink: &Ink) -> bool {
        self.recording && ink.is_drawing()
    }

    /// When [`Input::Tick`] is next needed.
    pub fn next_deadline(&self) -> Option<f64> {
        self.follow_due
    }

    pub fn step(&mut self, input: Input, now: f64, ink: &mut Ink) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::Start => {
                self.recording = true;
                self.last_tap = None;
                self.stale = self.held;
            }
            Input::Stop => {
                self.recording = false;
                self.follow_due = None;
                self.last_tap = None;
                self.stop_following(&mut out);
                // `InkOverlay.close`: a pen in hand is put down, and its ink fades.
                if ink.is_drawing() {
                    ink.put_down();
                    out.push(Action::Tool(None));
                    out.push(Action::FadeAway);
                }
            }
            Input::Key(key) => self.key(key, now, ink, &mut out),
            Input::Wheel(delta) => {
                if self.recording && delta != 0 {
                    // A scroll makes it a hold at once (`reportFollowing`).
                    if self.held {
                        self.start_following(&mut out);
                    }
                    out.push(Action::Zoom(delta as f64 / WHEEL_DELTA * ZOOM_PER_NOTCH));
                }
            }
            Input::Tick => {
                if self.follow_due.is_some_and(|due| now >= due) && self.held {
                    self.start_following(&mut out);
                }
            }
            Input::KeysReset(mods) => {
                self.mods = mods;
                self.held = mods.combo(self.modifier);
                self.last_tap = None;
                self.follow_due = None;
                self.stop_following(&mut out);
                self.stale = self.held;
            }
            Input::ToolShortcut(tool) => {
                if self.recording {
                    let key = match tool {
                        InkTool::Pen => InkKey::Pen,
                        InkTool::Highlighter => InkKey::Highlighter,
                    };
                    press(ink, key, &mut out);
                }
            }
            Input::InkKey(key) => {
                if self.recording {
                    press(ink, key, &mut out);
                }
            }
        }
        out
    }

    fn key(&mut self, key: KeyInput, now: f64, ink: &mut Ink, out: &mut Vec<Action>) {
        if key.taken {
            // The hook swallowed it: a drawing key while the gate said a tool was in hand.
            // If the tool went down in between, the key is simply used up.
            if self.recording
                && ink.is_drawing()
                && let Some(ink_key) = drawing_key(key.vk, &self.mods)
            {
                press(ink, ink_key, out);
            }
            if key.repeat {
                return;
            }
        }
        if key.repeat {
            return;
        }
        self.mods.apply(key.vk, key.down, key.altgr);
        if key.down && !is_modifier(key.vk) {
            // The modifier then a key is typing, not half a double tap.
            self.last_tap = None;
        }
        let held = self.mods.combo(self.modifier);
        if held == self.held {
            return;
        }
        self.held = held;
        if !held {
            self.follow_due = None;
            self.stale = false;
            self.stop_following(out);
            return;
        }
        if !self.recording {
            return;
        }
        if self.modifier == ZoomModifier::Alt && self.mask {
            out.push(Action::Mask {
                changes: key.changes,
            });
        }
        match self.last_tap {
            Some(tap) if now - tap < DOUBLE_TAP_WINDOW => {
                self.last_tap = None;
                out.push(Action::ResetZoom);
            }
            _ => self.last_tap = Some(now),
        }
        self.follow_due = Some(now + FOLLOW_DELAY);
    }

    /// The modifier has been held long enough (or a scroll came): it is a hold, not a tap.
    fn start_following(&mut self, out: &mut Vec<Action>) {
        self.follow_due = None;
        if self.following || self.stale {
            return;
        }
        self.following = true;
        self.last_tap = None;
        out.push(Action::Follow(true));
    }

    fn stop_following(&mut self, out: &mut Vec<Action>) {
        if self.following {
            self.following = false;
            out.push(Action::Follow(false));
        }
    }
}

/// `InkOverlay.press`: applies a key to the ink and says what changed.
fn press(ink: &mut Ink, key: InkKey, out: &mut Vec<Action>) {
    let before = ink.tool();
    if !ink.press(key) {
        return;
    }
    let after = ink.tool();
    if key == InkKey::Done || (before.is_some() && after.is_none()) {
        out.push(Action::FadeAway);
    } else {
        out.push(Action::Redraw);
    }
    if before != after {
        out.push(Action::Tool(after));
    }
}

fn is_modifier(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5
    )
}

// ---------------------------------------------------------------------------------------
// The keyboard hook (rule 1)
// ---------------------------------------------------------------------------------------

/// Drawing keys are swallowed while this is set: a tool is in hand during a recording. The
/// recording UI sets it after every change of the tool and clears it at stop and shutdown.
pub static DRAW_KEYS: AtomicBool = AtomicBool::new(false);

/// An auto-reset event handle, closed on drop.
struct Event(HANDLE);

// SAFETY: a kernel event handle may be used and closed from any thread.
unsafe impl Send for Event {}
// SAFETY: `SetEvent` and waits on the same handle from several threads are allowed.
unsafe impl Sync for Event {}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: created by `CreateEventW`, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct Shared {
    keys: Mutex<VecDeque<KeyInput>>,
    dropped: AtomicU64,
    wake: Event,
    installed: AtomicBool,
    /// The modifier mode: 1 for Ctrl+Alt.
    ctrl_alt: AtomicBool,
    callbacks: AtomicU64,
    max_ns: AtomicU64,
    swallowed: AtomicU64,
    /// The hook's key change count, for the mask key's last check.
    changes: AtomicU64,
    /// Alt (bit 0) and Ctrl (bit 1) as the hook last saw them, for the health check.
    seen: AtomicU32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Context {
    shared: Arc<Shared>,
    state: HookState,
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

unsafe extern "system" fn key_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let started = Instant::now();
        // SAFETY: for HC_ACTION, `lparam` points to a KBDLLHOOKSTRUCT valid for this call;
        // it is copied out at once.
        let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let swallow = catch_unwind(AssertUnwindSafe(|| {
            key_event(wparam.0 as u32, &info, started)
        }));
        if matches!(swallow, Ok(true)) {
            // A drawing key while a tool is in hand: the app underneath never sees it.
            return LRESULT(1);
        }
    }
    // SAFETY: passes the hook's own arguments on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn key_event(message: u32, info: &KBDLLHOOKSTRUCT, started: Instant) -> bool {
    CONTEXT.with(|context| {
        let Ok(mut context) = context.try_borrow_mut() else {
            return false;
        };
        let Some(context) = context.as_mut() else {
            return false;
        };
        let shared = &context.shared;
        let modifier = if shared.ctrl_alt.load(Relaxed) {
            ZoomModifier::CtrlAlt
        } else {
            ZoomModifier::Alt
        };
        let verdict = context.state.on_key(
            info.vkCode,
            info.scanCode,
            message == WM_KEYDOWN || message == WM_SYSKEYDOWN,
            info.flags.0 & LLKHF_INJECTED.0 != 0,
            info.dwExtraInfo == INJECTED_MARKER,
            DRAW_KEYS.load(SeqCst),
            modifier,
            hook::now(),
        );
        WHEEL_GATE.set_held(verdict.held);
        let mods = context.state.mods();
        shared.seen.store(
            u32::from(mods.alt()) | u32::from(mods.ctrl() || mods.altgr) << 1,
            Relaxed,
        );
        if let Some(event) = verdict.event {
            shared.changes.store(event.changes, SeqCst);
            {
                let mut keys = lock(&shared.keys);
                if keys.len() < KEY_QUEUE_CAPACITY {
                    keys.push_back(event);
                } else {
                    shared.dropped.fetch_add(1, Relaxed);
                }
            }
            // SAFETY: the handle lives as long as `shared`.
            let _ = unsafe { SetEvent(shared.wake.0) };
        }
        if verdict.swallow {
            shared.swallowed.fetch_add(1, Relaxed);
        }
        shared.callbacks.fetch_add(1, Relaxed);
        let spent = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        shared.max_ns.fetch_max(spent, Relaxed);
        verdict.swallow
    })
}

/// Counters of the keyboard hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHookStats {
    pub callbacks: u64,
    pub max_callback: Duration,
    pub swallowed: u64,
    pub dropped: u64,
}

/// The recording's keyboard hook: its own thread, `WH_KEYBOARD_LL`. Started with the recording
/// UI and dropped with it; while it is gone nothing is swallowed and the wheel gate's held bit
/// is clear.
pub struct KeyHook {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

impl KeyHook {
    /// Starts the hook thread. Returns once the hook is set or failed.
    pub fn start(modifier: ZoomModifier) -> Result<KeyHook, String> {
        hook::now();
        // SAFETY: no pointers; auto-reset, not signalled, unnamed. Owned by `Event`.
        let wake = Event(
            unsafe { CreateEventW(None, false, false, None) }.map_err(|e| format!("event: {e}"))?,
        );
        let shared = Arc::new(Shared {
            keys: Mutex::new(VecDeque::with_capacity(KEY_QUEUE_CAPACITY)),
            dropped: AtomicU64::new(0),
            wake,
            installed: AtomicBool::new(false),
            ctrl_alt: AtomicBool::new(modifier == ZoomModifier::CtrlAlt),
            callbacks: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            swallowed: AtomicU64::new(0),
            changes: AtomicU64::new(0),
            seen: AtomicU32::new(0),
        });
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
        let thread_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("recording-key-hook".into())
            .spawn(move || run_hook(thread_shared, modifier, ready_tx))
            .map_err(|e| format!("keyboard hook thread: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(KeyHook {
                shared,
                thread: Some(thread),
                thread_id,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err("keyboard hook thread ended".into()),
        }
    }

    /// Appends the key events since the last call, oldest first.
    pub fn drain(&self, out: &mut Vec<KeyInput>) {
        out.extend(lock(&self.shared.keys).drain(..));
    }

    /// An auto-reset event set when key events are waiting.
    pub fn wake_handle(&self) -> HANDLE {
        self.shared.wake.0
    }

    /// The thread runs and the hook was set (Windows may still have removed it: see
    /// [`disagrees`](Self::disagrees)).
    pub fn is_alive(&self) -> bool {
        self.shared.installed.load(Relaxed)
            && self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// The modifiers Windows reports now differ from what the hook saw last. Twice in a row,
    /// a second apart, means the hook is gone (Windows removes slow hooks without notice).
    pub fn disagrees(&self) -> bool {
        let seen = self.shared.seen.load(Relaxed);
        let system = Mods::now();
        (seen & 1 != 0) != system.alt() || (seen & 2 != 0) != system.ctrl()
    }

    /// The hook's count of key changes so far.
    pub fn changes(&self) -> u64 {
        self.shared.changes.load(SeqCst)
    }

    pub fn stats(&self) -> KeyHookStats {
        KeyHookStats {
            callbacks: self.shared.callbacks.load(Relaxed),
            max_callback: Duration::from_nanos(self.shared.max_ns.load(Relaxed)),
            swallowed: self.shared.swallowed.load(Relaxed),
            dropped: self.shared.dropped.load(Relaxed),
        }
    }
}

impl Drop for KeyHook {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        // SAFETY: posts a message with no pointers to a thread of this process.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_STOP, WPARAM(0), LPARAM(0)) };
        let handle = HANDLE(thread.as_raw_handle());
        // SAFETY: the live handle owned by `thread`, which outlives the wait.
        let ended =
            unsafe { WaitForSingleObject(handle, STOP_WAIT.as_millis() as u32) } == WAIT_OBJECT_0;
        if ended {
            let _ = thread.join();
        }
        WHEEL_GATE.set_held(false);
    }
}

fn run_hook(shared: Arc<Shared>, modifier: ZoomModifier, ready: mpsc::Sender<Result<u32, String>>) {
    let mut msg = MSG::default();
    // SAFETY: plain calls on this thread; `PeekMessageW` creates its queue so `WM_STOP` can
    // be posted as soon as `ready` is sent.
    let (thread_id, module) = unsafe {
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        (GetCurrentThreadId(), GetModuleHandleW(None))
    };
    let mods = Mods::now();
    WHEEL_GATE.set_held(mods.combo(modifier));
    CONTEXT.set(Some(Context {
        shared: shared.clone(),
        state: HookState::new(mods),
    }));
    // SAFETY: `key_proc` matches HOOKPROC and lives for the whole program.
    let hook = module.and_then(|module| unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), Some(module.into()), 0)
    });
    let hook = match hook {
        Ok(hook) => hook,
        Err(error) => {
            CONTEXT.set(None);
            WHEEL_GATE.set_held(false);
            let _ = ready.send(Err(format!("keyboard hook: {error}")));
            return;
        }
    };
    shared.installed.store(true, Relaxed);
    let _ = ready.send(Ok(thread_id));
    drop(ready);
    // SAFETY: `msg` is a valid MSG; this thread owns no windows.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.hwnd.is_invalid() && msg.message == WM_STOP {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    shared.installed.store(false, Relaxed);
    // SAFETY: set by this thread above, removed once (an error if Windows already removed
    // it is ignored).
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    CONTEXT.set(None);
    WHEEL_GATE.set_held(false);
}

// ---------------------------------------------------------------------------------------
// The mask key
// ---------------------------------------------------------------------------------------

/// Sends the mask key (`0xE8` down and up, one `SendInput` call, marked as ours), after the
/// last checks (rule 7): the hook counted no key change since `changes` (`current`), Alt is
/// down, and Ctrl, Shift and Win are up.
pub fn send_mask(changes: u64, current: u64) -> Result<(), &'static str> {
    if changes != current {
        return Err("a key changed since Alt went down");
    }
    let mods = Mods::now();
    if !mods.alt() || !key_down(VK_MENU) {
        return Err("Alt is already up");
    }
    if mods.ctrl() || mods.shift || mods.win {
        return Err("another modifier is down");
    }
    let key = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(MASK_KEY),
                // No layout maps this key to a scan code.
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: INJECTED_MARKER,
            },
        },
    };
    let inputs = [key(false), key(true)];
    // SAFETY: `inputs` is a local array of fully initialised keyboard inputs.
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent == inputs.len() as u32 {
        Ok(())
    } else {
        Err("SendInput refused it")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use igorio_core::geom::{Point, Rect};
    use igorio_core::viewport::RecordingViewport;

    const LALT: u32 = 0xA4;
    const RALT: u32 = 0xA5;
    const LCTRL: u32 = 0xA2;
    const LSHIFT: u32 = 0xA0;
    const TAB: u32 = 0x09;
    const CTRL_SCAN: u32 = 0x1D;
    const ALTGR_CTRL_SCAN: u32 = 0x21D;

    /// The hook and the machine together, as the recording UI runs them.
    struct Script {
        hook: HookState,
        machine: Machine,
        ink: Ink,
        draw: bool,
        actions: Vec<(f64, Action)>,
        swallowed: Vec<u32>,
        held: bool,
    }

    impl Script {
        fn new(modifier: ZoomModifier) -> Script {
            let mut s = Script {
                hook: HookState::new(Mods::default()),
                machine: Machine::new(modifier, true),
                ink: Ink::new(),
                draw: false,
                actions: Vec::new(),
                swallowed: Vec::new(),
                held: false,
            };
            s.input(0.0, Input::Start);
            s
        }

        fn input(&mut self, t: f64, input: Input) -> &mut Script {
            let actions = self.machine.step(input, t, &mut self.ink);
            self.draw = self.machine.keys_armed(&self.ink);
            self.actions.extend(actions.into_iter().map(|a| (t, a)));
            self
        }

        fn raw(&mut self, t: f64, vk: u32, scan: u32, down: bool) -> &mut Script {
            let modifier = self.machine.modifier();
            let verdict = self
                .hook
                .on_key(vk, scan, down, false, false, self.draw, modifier, t);
            self.held = verdict.held;
            if verdict.swallow {
                self.swallowed.push(vk);
            }
            if let Some(event) = verdict.event {
                self.input(t, Input::Key(event));
            }
            self
        }

        fn key(&mut self, t: f64, vk: u32, down: bool) -> &mut Script {
            let scan = if vk == LCTRL { CTRL_SCAN } else { 0 };
            self.raw(t, vk, scan, down)
        }

        fn tap(&mut self, t: f64, vk: u32) -> &mut Script {
            self.key(t, vk, true).key(t + 0.05, vk, false)
        }

        /// A wheel event as the mouse hook would decide it, with the keys down per the hook.
        fn wheel(&mut self, t: f64, delta: i32) -> bool {
            let gate = hook::WheelGate::new();
            if self.machine.wheel_armed() {
                gate.arm(self.machine.modifier() == ZoomModifier::CtrlAlt);
            }
            gate.set_held(self.held);
            let mods = self.hook.mods();
            let taken = gate.on_wheel(delta, false, |ctrl| mods.alt() && (!ctrl || mods.ctrl()));
            if taken {
                let (sum, _) = gate.take();
                self.input(t, Input::Wheel(sum));
            }
            taken
        }

        fn count(&self, f: impl Fn(&Action) -> bool) -> usize {
            self.actions.iter().filter(|(_, a)| f(a)).count()
        }

        fn zooms(&self) -> Vec<f64> {
            self.actions
                .iter()
                .filter_map(|(_, a)| match a {
                    Action::Zoom(z) => Some(*z),
                    _ => None,
                })
                .collect()
        }

        fn follows(&self) -> Vec<bool> {
            self.actions
                .iter()
                .filter_map(|(_, a)| match a {
                    Action::Follow(on) => Some(*on),
                    _ => None,
                })
                .collect()
        }

        fn resets(&self) -> usize {
            self.count(|a| *a == Action::ResetZoom)
        }

        fn masks(&self) -> usize {
            self.count(|a| matches!(a, Action::Mask { .. }))
        }
    }

    #[test]
    fn modifier_and_wheel_zooms_by_the_swift_step_clamped_by_the_viewport() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.key(0.0, LALT, true);
        assert!(s.wheel(0.05, 120));
        assert!(s.wheel(0.06, -240));
        assert_eq!(s.zooms(), vec![0.12, -0.24]);
        // A wheel event starts following at once.
        assert_eq!(s.follows(), vec![true]);
        let display = Rect::new(0.0, 0.0, 2560.0, 1440.0);
        let mut viewport = RecordingViewport::new(display);
        for _ in 0..100 {
            viewport.zoom_by(0.12);
        }
        assert_eq!(viewport.target_zoom(), 4.0);
        viewport.zoom_by(-0.12 * 1000.0);
        assert_eq!(viewport.target_zoom(), 1.0);
    }

    #[test]
    fn holding_the_modifier_follows_and_letting_go_keeps_the_view() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.key(1.0, LALT, true);
        assert_eq!(s.machine.next_deadline(), Some(1.0 + FOLLOW_DELAY));
        s.input(1.1, Input::Tick);
        assert!(s.follows().is_empty(), "not before the follow delay");
        s.input(1.16, Input::Tick);
        assert_eq!(s.follows(), vec![true]);
        s.key(2.0, LALT, false);
        assert_eq!(s.follows(), vec![true, false]);
        // The viewport stays where it is once following stops.
        let display = Rect::new(0.0, 0.0, 2560.0, 1440.0);
        let mut viewport = RecordingViewport::new(display);
        viewport.zoom_by(1.0);
        for _ in 0..60 {
            viewport.step(Point::new(2400.0, 1300.0), true, 1.0 / 60.0);
        }
        let held = viewport.area();
        for _ in 0..60 {
            viewport.step(Point::new(100.0, 100.0), false, 1.0 / 60.0);
        }
        assert_eq!(viewport.area(), held);
    }

    #[test]
    fn a_quick_tap_does_not_follow() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.tap(1.0, LALT);
        s.input(1.2, Input::Tick);
        assert!(s.follows().is_empty());
        assert_eq!(s.machine.next_deadline(), None);
    }

    #[test]
    fn a_double_tap_resets_and_a_slow_one_does_not() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.tap(1.0, LALT).tap(1.3, LALT);
        assert_eq!(s.resets(), 1);
        assert!(s.follows().is_empty(), "the taps never tug the view");
        // A third tap right after starts a new pair, it does not reset again.
        s.tap(1.5, LALT);
        assert_eq!(s.resets(), 1);

        let mut s = Script::new(ZoomModifier::Alt);
        s.tap(1.0, LALT).tap(1.36, LALT);
        assert_eq!(s.resets(), 0, "0.36 s apart is too slow");

        // A key in between is typing, not a double tap.
        let mut s = Script::new(ZoomModifier::Alt);
        s.tap(1.0, LALT).tap(1.1, TAB).tap(1.2, LALT);
        assert_eq!(s.resets(), 0);

        // A hold that followed is not a tap.
        let mut s = Script::new(ZoomModifier::Alt);
        s.key(1.0, LALT, true);
        s.input(1.16, Input::Tick);
        s.key(1.2, LALT, false).tap(1.3, LALT);
        assert_eq!(s.resets(), 0);
    }

    #[test]
    fn every_alt_press_during_a_recording_asks_for_the_mask_key_and_nothing_else_does() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.tap(1.0, LALT).tap(1.3, LALT);
        assert_eq!(s.masks(), 2);
        // The mask carries the hook's change count of the Alt press.
        let changes: Vec<u64> = s
            .actions
            .iter()
            .filter_map(|(_, a)| match a {
                Action::Mask { changes } => Some(*changes),
                _ => None,
            })
            .collect();
        assert_eq!(changes, vec![1, 3]);
        // Alt+Shift, Ctrl+Alt: not the gesture, no mask.
        let mut s = Script::new(ZoomModifier::Alt);
        s.key(1.0, LSHIFT, true).key(1.1, LALT, true);
        s.key(1.2, LALT, false).key(1.3, LSHIFT, false);
        s.key(2.0, LCTRL, true).key(2.1, LALT, true);
        assert_eq!(s.masks(), 0);
        // Ctrl+Alt mode needs none.
        let mut s = Script::new(ZoomModifier::CtrlAlt);
        s.key(1.0, LCTRL, true).key(1.05, LALT, true);
        assert!(s.wheel(1.1, 120));
        assert_eq!(s.masks(), 0);
        // Without the mask option nothing is asked.
        let mut m = Machine::new(ZoomModifier::Alt, false);
        let mut ink = Ink::new();
        m.step(Input::Start, 0.0, &mut ink);
        let mut hook = HookState::new(Mods::default());
        let v = hook.on_key(LALT, 0, true, false, false, false, ZoomModifier::Alt, 1.0);
        let out = m.step(Input::Key(v.event.unwrap()), 1.0, &mut ink);
        assert!(!out.iter().any(|a| matches!(a, Action::Mask { .. })));
    }

    #[test]
    fn the_wheel_without_the_modifier_is_not_taken() {
        let mut s = Script::new(ZoomModifier::Alt);
        assert!(!s.wheel(1.0, 120));
        // Alt with Shift, Ctrl or another key: not the modifier.
        s.key(1.1, LSHIFT, true).key(1.2, LALT, true);
        assert!(!s.wheel(1.3, 120));
        s.key(1.4, LSHIFT, false);
        assert!(s.wheel(1.5, 120), "Alt alone again once Shift is up");
        s.key(1.6, TAB, true);
        assert!(!s.wheel(1.7, 120));
        s.key(1.8, TAB, false).key(1.9, LALT, false);
        assert!(!s.wheel(2.0, 120));
        assert_eq!(s.zooms(), vec![0.12]);
        // Ctrl+Alt mode: Alt alone is not enough.
        let mut s = Script::new(ZoomModifier::CtrlAlt);
        s.key(1.0, LALT, true);
        assert!(!s.wheel(1.1, 120));
        s.key(1.2, LCTRL, true);
        assert!(s.wheel(1.3, 120));
    }

    #[test]
    fn nothing_is_taken_when_no_recording_runs() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.input(0.5, Input::Stop);
        s.actions.clear();
        s.key(1.0, LALT, true);
        assert!(!s.wheel(1.1, 120));
        s.input(1.3, Input::Tick);
        s.key(1.4, LALT, false).tap(1.5, LALT);
        s.input(1.6, Input::ToolShortcut(InkTool::Pen));
        assert!(s.ink.tool().is_none());
        s.key(1.7, VK_ESCAPE, true);
        assert!(s.swallowed.is_empty());
        assert!(s.actions.is_empty(), "{:?}", s.actions);
        assert!(!s.machine.wheel_armed() && !s.machine.keys_armed(&s.ink));
    }

    #[test]
    fn drawing_keys_are_taken_only_with_a_tool_in_hand() {
        let mut s = Script::new(ZoomModifier::Alt);
        // No tool: Esc, Backspace and Ctrl+Z belong to the app.
        s.tap(1.0, VK_ESCAPE).tap(1.1, VK_BACK);
        s.key(1.2, LCTRL, true)
            .tap(1.3, VK_Z)
            .key(1.4, LCTRL, false);
        assert!(s.swallowed.is_empty());

        s.input(2.0, Input::ToolShortcut(InkTool::Pen));
        assert!(s.draw);
        for x in [10.0, 20.0, 30.0] {
            s.ink.begin_stroke(Point::new(x, 10.0));
            s.ink.extend_stroke(Point::new(x, 40.0));
            s.ink.end_stroke();
        }
        // Ctrl+Z undoes; a held Ctrl+Z repeats; only the Z presses are taken.
        s.key(3.0, LCTRL, true)
            .key(3.1, VK_Z, true)
            .key(3.2, VK_Z, true);
        assert_eq!(s.ink.strokes().len(), 1);
        s.key(3.3, VK_Z, false).key(3.4, LCTRL, false);
        assert_eq!(s.swallowed, vec![VK_Z, VK_Z]);
        // Ctrl+Shift+Z, Alt+Backspace: someone else's shortcut.
        s.key(3.5, LSHIFT, true)
            .key(3.5, LCTRL, true)
            .tap(3.6, VK_Z);
        s.key(3.7, LCTRL, false).key(3.7, LSHIFT, false);
        s.key(3.8, LALT, true)
            .tap(3.9, VK_BACK)
            .key(4.0, LALT, false);
        assert_eq!(s.swallowed.len(), 2);
        assert_eq!(s.ink.strokes().len(), 1);
        // Delete clears and keeps the pen; Esc puts it down and the gate opens.
        s.tap(5.0, VK_DELETE);
        assert!(s.ink.strokes().is_empty() && s.ink.tool() == Some(InkTool::Pen));
        s.tap(5.1, VK_ESCAPE);
        assert!(s.ink.tool().is_none() && !s.draw);
        assert!(s.count(|a| *a == Action::FadeAway) == 1);
        assert_eq!(s.swallowed, vec![VK_Z, VK_Z, VK_DELETE, VK_ESCAPE]);
        // The next Esc goes to the app.
        s.tap(5.2, VK_ESCAPE);
        assert_eq!(s.swallowed.len(), 4);
        // Key-ups are never swallowed.
        let mut hook = HookState::new(Mods::default());
        let v = hook.on_key(
            VK_ESCAPE,
            1,
            false,
            false,
            false,
            true,
            ZoomModifier::Alt,
            0.0,
        );
        assert!(!v.swallow);
    }

    #[test]
    fn a_tool_shortcut_picks_up_switches_and_puts_down_as_ink_press_says() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.input(1.0, Input::ToolShortcut(InkTool::Pen));
        assert_eq!(s.ink.tool(), Some(InkTool::Pen));
        s.ink.begin_stroke(Point::new(1.0, 1.0));
        s.ink.extend_stroke(Point::new(9.0, 9.0));
        s.ink.end_stroke();
        s.input(2.0, Input::ToolShortcut(InkTool::Highlighter));
        assert_eq!(s.ink.tool(), Some(InkTool::Highlighter));
        assert_eq!(s.ink.strokes().len(), 1, "switching keeps the ink");
        s.input(3.0, Input::ToolShortcut(InkTool::Highlighter));
        assert_eq!(s.ink.tool(), None);
        assert!(s.ink.strokes().is_empty());
        let tools: Vec<Option<InkTool>> = s
            .actions
            .iter()
            .filter_map(|(_, a)| match a {
                Action::Tool(t) => Some(*t),
                _ => None,
            })
            .collect();
        assert_eq!(
            tools,
            vec![Some(InkTool::Pen), Some(InkTool::Highlighter), None]
        );
        assert_eq!(s.count(|a| *a == Action::FadeAway), 1);
        assert_eq!(s.count(|a| *a == Action::Redraw), 2);
    }

    #[test]
    fn altgr_never_counts_as_the_modifier() {
        for modifier in [ZoomModifier::Alt, ZoomModifier::CtrlAlt] {
            let mut s = Script::new(modifier);
            // AltGr: a fake left Ctrl (scan 0x21D), then right Alt.
            s.raw(1.0, LCTRL, ALTGR_CTRL_SCAN, true)
                .key(1.0, RALT, true);
            assert!(!s.wheel(1.1, 120), "{modifier:?}");
            s.input(1.3, Input::Tick);
            s.key(1.4, RALT, false).raw(1.4, LCTRL, CTRL_SCAN, false);
            s.raw(1.5, LCTRL, ALTGR_CTRL_SCAN, true)
                .key(1.5, RALT, true);
            s.key(1.55, RALT, false).raw(1.55, LCTRL, CTRL_SCAN, false);
            assert!(s.follows().is_empty() && s.resets() == 0 && s.masks() == 0);
            // AltGr+Z is a character, not Ctrl+Z.
            s.input(2.0, Input::ToolShortcut(InkTool::Pen));
            s.raw(2.1, LCTRL, ALTGR_CTRL_SCAN, true)
                .key(2.1, RALT, true)
                .tap(2.2, VK_Z);
            assert!(s.swallowed.is_empty());
        }
        // Right Alt alone (layouts without AltGr) is Alt.
        let mut s = Script::new(ZoomModifier::Alt);
        s.key(1.0, RALT, true);
        assert!(s.wheel(1.1, 120));
    }

    #[test]
    fn the_hook_restarting_mid_gesture_ends_following_until_release() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.key(1.0, LALT, true);
        s.input(1.2, Input::Tick);
        assert_eq!(s.follows(), vec![true]);
        // The hook restarted; Alt is still down per GetAsyncKeyState.
        let mods = Mods {
            left_alt: true,
            ..Mods::default()
        };
        s.hook = HookState::new(mods);
        s.input(1.5, Input::KeysReset(mods));
        assert_eq!(s.follows(), vec![true, false]);
        // Still held: zoom works, following does not come back in this hold.
        s.held = s.hook.mods().combo(ZoomModifier::Alt);
        assert!(s.wheel(1.6, 120));
        s.input(2.0, Input::Tick);
        assert_eq!(s.follows(), vec![true, false]);
        // Released and pressed again: all normal.
        s.key(2.1, LALT, false).key(3.0, LALT, true);
        s.input(3.2, Input::Tick);
        assert_eq!(s.follows(), vec![true, false, true]);
        assert_eq!(s.resets(), 0, "a restart never makes a double tap");
    }

    #[test]
    fn stopping_mid_gesture_releases_every_gate() {
        let mut s = Script::new(ZoomModifier::Alt);
        s.input(0.5, Input::ToolShortcut(InkTool::Pen));
        s.key(1.0, LALT, true);
        s.input(1.2, Input::Tick);
        assert!(s.machine.wheel_armed() && s.machine.keys_armed(&s.ink));
        s.input(1.3, Input::Stop);
        assert!(!s.machine.wheel_armed() && !s.machine.keys_armed(&s.ink));
        assert_eq!(s.follows(), vec![true, false]);
        assert!(s.ink.tool().is_none());
        assert_eq!(s.count(|a| *a == Action::FadeAway), 1);
        assert_eq!(s.machine.next_deadline(), None);
        // Alt is still down after the stop: the wheel passes, Esc passes.
        assert!(!s.wheel(1.4, 120));
        s.tap(1.5, VK_ESCAPE);
        assert!(s.swallowed.is_empty());
    }

    #[test]
    fn our_own_injected_keys_are_ignored_and_others_are_never_swallowed() {
        let mut hook = HookState::new(Mods::default());
        let held = hook.on_key(LALT, 0, true, false, false, false, ZoomModifier::Alt, 0.0);
        assert!(held.held);
        // Our mask key in between: not seen, Alt is still held alone.
        let mask = hook.on_key(0xE8, 0, true, true, true, false, ZoomModifier::Alt, 0.0);
        assert!(mask.held && mask.event.is_none());
        // Someone else's injected Esc while drawing: passes.
        let mut hook = HookState::new(Mods::default());
        let esc = hook.on_key(
            VK_ESCAPE,
            1,
            true,
            true,
            false,
            true,
            ZoomModifier::Alt,
            0.0,
        );
        assert!(!esc.swallow);
    }

    #[test]
    fn toggle_follows_the_recording_state() {
        assert_eq!(toggle(RecordingState::Idle), Toggle::Start);
        assert_eq!(toggle(RecordingState::Recording), Toggle::Stop);
        assert_eq!(toggle(RecordingState::Starting), Toggle::Ignore);
        assert_eq!(toggle(RecordingState::Stopping), Toggle::Ignore);
    }
}
