//! The gesture that summons a note: hold Alt on its own over empty space (the desktop, a
//! page margin, a window background) and a small pill rings the cursor; click while it
//! shows and a note is requested at that point. The click is swallowed, so the app
//! underneath never sees an Alt+click. From `NoteSummoner.swift` and the summon pill of
//! `NoteView.swift`.
//!
//! # The Alt key on Windows
//!
//! Pressing and releasing Alt with no other key in between ("a lone Alt") makes the
//! foreground window activate its menu bar (`WM_SYSCOMMAND`/`SC_KEYMENU`), show access-key
//! letters, or (Office) show its key tips; VS Code and Chromium do their own version. The
//! gesture must not leave that behind, and must not disturb any other use of Alt.
//!
//! Techniques considered:
//!
//! 1. Swallow the Alt press. Ruled out: Alt+Tab, Alt+F4 and access keys need it.
//! 2. Swallow the Alt release and inject `mask key down, mask key up, Alt up` from the
//!    keyboard hook (what AutoHotkey does for its hotkeys). It works, but a real key event
//!    is then withheld from the system: if the injection fails (UIPI refuses it because an
//!    elevated window came to the front, the hook thread dies between the two, the process
//!    is killed), Alt stays logically down for the whole session. It also puts `SendInput`
//!    inside a hook callback.
//! 3. Inject the mask key while Alt is still held, at the moment the pill appears, and
//!    swallow no key at all. The release that follows is then not a lone Alt: Windows (and
//!    apps that track Alt themselves) saw another key between press and release, exactly as
//!    with Alt+Tab. **Chosen.**
//!
//! The mask key is virtual key `0xE8`, which no layout assigns: it produces no character
//! (so no `WM_SYSCHAR`, no beep, no access key match), and apps ignore it. AutoHotkey uses
//! the same key for the same purpose. A Ctrl tap would work too, but Ctrl+Alt with a third
//! key is AltGr on many layouts, and some apps bind Ctrl+Alt chords.
//!
//! Exactly what this module swallows and injects:
//!
//! - **Keys: nothing is ever swallowed.** The keyboard hook only observes. So no key can be
//!   left down by this module in any path: not when the pill shows and Alt is released, not
//!   when Alt is released during a click, not on a focus change, not when Windows removes
//!   the hook, not when the app quits or is killed.
//! - **Injected: one `0xE8` down and up, in one `SendInput` call** (the system inserts the
//!   pair as one unit, nothing in between), with [`INJECTED_MARKER`], once per hold, only in
//!   Alt mode, only when the pill first appears in that hold, and only if right before the
//!   call Alt is still down, Ctrl, Shift and Win are up, and no key changed since the hold
//!   began (rule 7). The app's own hooks ignore it by the marker. A quick tap, a hold that
//!   never shows the pill, Alt+Tab, Alt+F4, Alt+letter and Alt+drag never inject anything.
//!   If the pill shows over the desktop and the user then presses Tab, Alt+Tab still works:
//!   Alt is still down.
//! - **Mouse: one press and its release**, of the primary button, and only while the pill
//!   shows: the summoner arms `hook::CLICK_GATE` when the pill appears and disarms it when
//!   the pill goes. The app's single mouse hook decides in its callback from that one atomic
//!   ([`crate::hook::ClickGate::on_press`]). The keyboard hook disarms the gate on every key
//!   change, on its own thread, before Windows passes on the next input, so a click right
//!   after Alt is released is never swallowed even if the summoner thread is late. Pen and
//!   touch presses, injected presses and other buttons are never taken (they disarm). An
//!   arm based on an older key state is refused (the gate's epoch).
//!
//! The mask is sent at show time, not at release, so when the user lets go without
//! clicking nothing more happens; the release is an ordinary Alt release. When the pill
//! never appeared (the hold was over a text field, or shorter than [`HOLD_DELAY`]), the
//! release is left alone and a lone Alt activates the menu as usual: the user may have meant
//! that. Holding Alt over a foreground window of an elevated app: Windows does not call our
//! keyboard hook then, so the gesture simply does not start.
//!
//! Verdict: plain Alt can be made safe this way, so it stays the default ([`Modifier::Alt`],
//! as on macOS). Ctrl+Alt ([`Modifier::CtrlAlt`]) is the fallback in the config: a Ctrl+Alt
//! release is never a lone Alt, so it needs no mask key at all. AltGr arrives as a fake left
//! Ctrl (scan code `0x21D`) plus right Alt; it never starts the gesture in either mode.
//!
//! What remains: the access-key underlines some apps draw while Alt is down appear (that
//! happens on the press, before we can know anything); and a hard kill between a swallowed
//! press and its release lets the release reach the app below without its press.
//!
//! # When the pill shows (the rules of `NoteSummoner.swift`)
//!
//! - The modifier went down on its own: no other key, no mouse button down (an Alt+drag
//!   already under way), not AltGr. In Ctrl+Alt mode, the hold starts when the second of the
//!   two goes down.
//! - It was held for [`HOLD_DELAY`] (0.3 s), so Alt shortcuts and quick taps never flash
//!   the pill. Any other key or button during the hold uses the hold up ([`Phase::Cancelled`]):
//!   no pill until the modifier is released and pressed again.
//! - The feature is enabled and nothing blocks it ([`Summoner::set_blocked`]: a paste
//!   delivery, a screenshot capture, a recording, the app paused).
//! - A UI Automation lookup at the cursor says the spot is empty
//!   ([`super::context::is_empty_space`]). The lookup runs once the hold delay has passed,
//!   not on every move. While the pill shows (or the hold waits for an empty spot) and the
//!   cursor moves, the spot is checked again at most every [`RECHECK_GAP`]; the pill hides
//!   when the cursor leaves empty space, and comes back when it returns. A lookup that
//!   times out means no pill.
//!
//! # Threads
//!
//! - The UI thread owns the [`Summoner`] and drives it with calls that never block:
//!   [`feed`](Summoner::feed) with the mouse events its own mouse hook delivers (there is
//!   still exactly one mouse hook in the process), [`set_blocked`](Summoner::set_blocked),
//!   [`set_enabled`](Summoner::set_enabled), [`apply_config`](Summoner::apply_config),
//!   [`summon_at_cursor`](Summoner::summon_at_cursor) for the NewNote hotkey, and
//!   [`drain`](Summoner::drain) after [`wake_handle`](Summoner::wake_handle) is signalled.
//! - The summoner thread owns the pill window and the state machine ([`Gesture`]), and asks
//!   UI Automation through its own lane of the app's one worker (`uia::Requester`). It
//!   never calls UI Automation itself and never waits on another process.
//! - The keyboard hook thread (rule 1) only pumps messages. Its callback copies the event,
//!   skips auto-repeats and our own injected keys, disarms the click gate, pushes a small
//!   event into a preallocated queue, sets an event, and always calls `CallNextHookEx`. A
//!   once-a-second check restarts it when it is gone or when the modifier keys Windows
//!   reports disagree with what it saw, twice in a row.
//!
//! # The pill
//!
//! A layered, click-through, topmost window excluded from capture (`overlay::Overlay`, the
//! same window the overlay uses: it never takes focus or input). A plate of
//! [`PILL_SIZE`] with corner radius [`PILL_RADIUS`], centred on the cursor's hot spot, in
//! the app's flat style, light or dark with the system's app mode. It grows out of a bead of
//! [`BEAD`] with a spring ([`SPRING_RESPONSE`], [`SPRING_DAMPING`]) and shrinks back in
//! [`DISAPPEAR`]. The animation clock ticks only while it changes; with reduced motion
//! ("Animation effects" off) it appears and goes at once. It follows the cursor while it
//! shows.
//!
//! # Manual test
//!
//! See `crates/app/examples/SUMMON_TEST.md` and `summon_demo`.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use igorio_core::geom::Point;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, LPARAM, LRESULT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThreadId, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_LBUTTON, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN,
    VK_MBUTTON, VK_RBUTTON, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, HC_ACTION,
    KBDLLHOOKSTRUCT, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE,
    PeekMessageW, PostThreadMessageW, QS_ALLINPUT, SM_SWAPBUTTON, SPI_GETCLIENTAREAANIMATION,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowsHookExW, SystemParametersInfoW,
    TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};
use windows::core::w;

use super::context::{self, AppNames, facts_at, is_empty_space};
use super::store::NoteOrigin;
use crate::hook::{self, Button, CLICK_GATE, MouseEvent};
use crate::input::INJECTED_MARKER;
use crate::overlay::{Overlay, monitor_at};
use crate::uia::{Lookup, Outcome, RequestId, Requester};

// ---------------------------------------------------------------------------------------
// Constants (the Swift values where there is one)
// ---------------------------------------------------------------------------------------

/// The modifier must be held this long first, so shortcuts and quick taps never flash the
/// pill (`holdDelay`).
pub const HOLD_DELAY: f64 = 0.3;
/// While the hold tracks the cursor, the spot is checked again at most this often (`0.12`
/// in `track()`).
pub const RECHECK_GAP: f64 = 0.12;
/// A move shorter than this (physical px) since the last check needs no new check.
pub const MOVE_SLOP: i32 = 3;
/// Deadline of one lookup. Longer than a frame, shorter than `uia::DEFAULT_DEADLINE`: a pill
/// that comes late is worse than none.
pub const LOOKUP_DEADLINE: Duration = Duration::from_millis(200);
/// A lookup with no answer after this long is forgotten (the worker always answers by its
/// deadline; this only guards against a lost answer).
pub const ASK_GIVE_UP: f64 = 1.0;
/// After a note is made and the modifier released, a new hold is ignored this long, so a
/// fumbled double press does not make two notes.
pub const COOLDOWN: f64 = 0.25;
/// The NewNote hotkey's lookup: the note is made from the window facts alone if no answer
/// came after this long.
pub const HOTKEY_GIVE_UP: f64 = 0.5;

/// The pill, in DIPs (`NoteLayout.summonSize`, `summonRadius`).
pub const PILL_SIZE: (f64, f64) = (46.0, 34.0);
pub const PILL_RADIUS: f64 = 12.0;
/// It grows out of a bead this wide (the fairy's idle orb).
pub const BEAD: f64 = 14.0;
/// Room around the pill for its shadow and the spring's overshoot, DIPs.
pub const PILL_MARGIN: f64 = 8.0;
/// The appear spring (`.spring(response: 0.24, dampingFraction: 0.7)`).
pub const SPRING_RESPONSE: f64 = 0.24;
pub const SPRING_DAMPING: f64 = 0.7;
/// The spring is treated as settled after this long.
pub const APPEAR: f64 = 0.36;
/// The disappear animation (`.easeOut(duration: 0.12)`).
pub const DISAPPEAR: f64 = 0.12;

/// The mask key: an unassigned virtual key.
pub const MASK_KEY: u16 = 0xE8;

/// Key events the hook can queue between two drains.
const KEY_QUEUE_CAPACITY: usize = 256;
/// Commands (mostly fed mouse events) queued for the summoner thread at most.
const COMMAND_CAPACITY: usize = 4096;
/// Animation frame interval.
const FRAME: Duration = Duration::from_millis(8);
/// The keyboard hook self-check interval, seconds.
const HEALTH_CHECK: f64 = 1.0;
/// How long `start` waits for the thread to be ready, and `shutdown` for it to end.
const START_WAIT: Duration = Duration::from_secs(3);
const STOP_WAIT: Duration = Duration::from_secs(1);
/// Thread message that ends the keyboard hook thread.
const WM_STOP: u32 = WM_APP + 1;

// ---------------------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------------------

/// The modifier held for the gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Modifier {
    /// Alt alone (either Alt key), as Option on macOS. The default.
    #[default]
    Alt,
    /// Ctrl and Alt together, nothing else.
    CtrlAlt,
}

impl Modifier {
    /// `alt` or `ctrl-alt`.
    pub fn parse(text: &str) -> Option<Modifier> {
        match text.trim().to_ascii_lowercase().as_str() {
            "alt" => Some(Modifier::Alt),
            "ctrl-alt" | "ctrl+alt" | "ctrlalt" => Some(Modifier::CtrlAlt),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Modifier::Alt => "Alt",
            Modifier::CtrlAlt => "Ctrl+Alt",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummonConfig {
    pub modifier: Modifier,
    /// The settings switch ("Hold Alt on empty space for a note").
    pub enabled: bool,
    /// Emit [`SummonerEvent::Log`] lines (`FAIRY_DEBUG=1`).
    pub debug: bool,
    /// Let screenshots see the pill (`FAIRY_CAPTURABLE=1`, for demos).
    pub capturable: bool,
}

impl Default for SummonConfig {
    fn default() -> Self {
        SummonConfig {
            modifier: Modifier::Alt,
            enabled: true,
            debug: false,
            capturable: false,
        }
    }
}

/// What holds the screen; any of them blocks the gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// A paste delivery is under way.
    Paste,
    /// A screenshot capture has the screen.
    Capture,
    /// A recording has the screen.
    Recording,
    /// The app is paused.
    Paused,
}

impl Block {
    fn bit(self) -> u8 {
        match self {
            Block::Paste => 1,
            Block::Capture => 2,
            Block::Recording => 4,
            Block::Paused => 8,
        }
    }
}

/// Make a note here.
#[derive(Debug, Clone, PartialEq)]
pub struct Summon {
    /// Physical pixels, virtual-screen coordinates: the click (or the cursor for the hotkey).
    pub point: (i32, i32),
    pub origin: NoteOrigin,
    /// The clock of `hook::now`.
    pub time: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SummonerEvent {
    Summon(Summon),
    /// A debug line (only with `SummonConfig::debug`).
    Log(String),
}

// ---------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------

/// One key change as the keyboard hook saw it (auto-repeats are not reported).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyInput {
    /// Virtual key (the hook reports left and right modifiers apart).
    pub vk: u32,
    pub down: bool,
    /// The fake left Ctrl that AltGr sends before its right Alt.
    pub altgr: bool,
    /// The click gate's epoch right after this event disarmed it.
    pub epoch: u64,
    pub time: f64,
}

/// Is this the left Ctrl that AltGr fakes? Its scan code carries the `0x200` bit (`0x21D`).
pub fn is_altgr_ctrl(vk: u32, scan_code: u32) -> bool {
    vk == u32::from(VK_LCONTROL.0) && scan_code & 0x200 != 0
}

/// The keys down, as far as the gesture cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Keys {
    pub left_alt: bool,
    pub right_alt: bool,
    pub left_ctrl: bool,
    pub right_ctrl: bool,
    /// AltGr's fake Ctrl.
    pub altgr: bool,
    pub shift: bool,
    pub win: bool,
    /// Any other key.
    others: [u64; 4],
}

impl Keys {
    /// Modifiers as `GetAsyncKeyState` reports them now (after a hook restart).
    pub fn now() -> Keys {
        let down = |vk: VIRTUAL_KEY| key_down(vk);
        Keys {
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

    /// Applies a change; false when nothing changed (a repeat).
    fn apply(&mut self, key: &KeyInput) -> bool {
        let flag = match key.vk {
            0xA4 | 0x12 => &mut self.left_alt,
            0xA5 => &mut self.right_alt,
            0xA2 | 0x11 if key.altgr => &mut self.altgr,
            0xA2 | 0x11 => &mut self.left_ctrl,
            0xA3 => &mut self.right_ctrl,
            0xA0 | 0xA1 | 0x10 => &mut self.shift,
            0x5B | 0x5C => &mut self.win,
            vk => {
                let (word, bit) = ((vk as usize & 0xFF) / 64, 1u64 << (vk % 64));
                let was = self.others[word] & bit != 0;
                if key.down {
                    self.others[word] |= bit;
                } else {
                    self.others[word] &= !bit;
                }
                return was != key.down;
            }
        };
        let changed = *flag != key.down;
        *flag = key.down;
        // AltGr's release clears its fake Ctrl with a plain left Ctrl up; clear both.
        if !key.down && matches!(key.vk, 0xA2 | 0x11) {
            let was = self.altgr || self.left_ctrl;
            self.altgr = false;
            self.left_ctrl = false;
            return changed || was;
        }
        changed
    }

    fn alt(&self) -> bool {
        self.left_alt || self.right_alt
    }

    fn ctrl(&self) -> bool {
        self.left_ctrl || self.right_ctrl
    }

    fn others(&self) -> bool {
        self.others.iter().any(|&w| w != 0)
    }

    /// Exactly the modifier of the gesture is down, nothing else.
    fn combo(&self, modifier: Modifier) -> bool {
        let clean = !self.altgr && !self.shift && !self.win && !self.others();
        match modifier {
            Modifier::Alt => clean && self.alt() && !self.ctrl(),
            Modifier::CtrlAlt => clean && self.alt() && self.ctrl(),
        }
    }

    /// Only keys of the combo are down (it may be incomplete: the first of Ctrl and Alt).
    fn partial(&self, modifier: Modifier) -> bool {
        let clean = !self.altgr && !self.shift && !self.win && !self.others();
        match modifier {
            Modifier::Alt => clean && !self.ctrl(),
            Modifier::CtrlAlt => clean,
        }
    }

    /// Any key of either combo still down: the hold is not over.
    fn holding(&self) -> bool {
        self.alt() || self.ctrl() || self.altgr
    }
}

/// Is `vk` one of the keys of the gesture's modifier?
fn is_combo_key(vk: u32, modifier: Modifier) -> bool {
    match modifier {
        Modifier::Alt => matches!(vk, 0xA4 | 0xA5 | 0x12),
        Modifier::CtrlAlt => matches!(vk, 0xA4 | 0xA5 | 0x12 | 0xA2 | 0xA3 | 0x11),
    }
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: plain value argument; the high bit is set while the key is down.
    (unsafe { GetAsyncKeyState(i32::from(vk.0)) }) < 0
}

/// Mouse buttons down now (`GetAsyncKeyState`), as the bits of [`button_bit`].
pub fn buttons_now() -> u8 {
    [
        (VK_LBUTTON, Button::Left),
        (VK_RBUTTON, Button::Right),
        (VK_MBUTTON, Button::Middle),
        (VK_XBUTTON1, Button::X1),
        (VK_XBUTTON2, Button::X2),
    ]
    .into_iter()
    .filter(|(vk, _)| key_down(*vk))
    .fold(0, |bits, (_, button)| bits | button_bit(button))
}

pub fn button_bit(button: Button) -> u8 {
    1 << (hook::button_code(button) - 1)
}

// ---------------------------------------------------------------------------------------
// The gesture: a pure state machine
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing held.
    Idle,
    /// The modifier is held alone; waiting for [`HOLD_DELAY`].
    Held,
    /// The delay passed; waiting for a lookup to say the spot is empty (no pill).
    Looking,
    /// The pill shows; the click gate is armed.
    Showing,
    /// A note was made; this hold is used up until the modifier is released.
    Clicked,
    /// This hold is used up (another key, a button, blocked); waiting for the release.
    Cancelled,
    /// A note was made and the modifier released; a new hold waits [`COOLDOWN`].
    Cooldown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    Key(KeyInput),
    Mouse(MouseEvent),
    /// The mouse buttons down now (`GetAsyncKeyState`), read before a hold may start, so a
    /// lost release in the feed cannot block the gesture for good.
    Buttons(u8),
    /// The answer to [`Action::Ask`]: is the spot empty?
    Answer {
        ask: u64,
        empty: bool,
    },
    /// The click gate swallowed a press here.
    ClickTaken {
        x: i32,
        y: i32,
    },
    Blocked(bool),
    Enabled(bool),
    Modifier(Modifier),
    /// The gate refused [`Action::Arm`]: a key changed meanwhile.
    ArmFailed,
    /// The keyboard hook was restarted: these keys are down now.
    KeysReset(Keys),
    /// Time passes.
    Tick,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Look up whether the spot at `point` is empty; answer with [`Input::Answer`].
    Ask {
        ask: u64,
        point: (i32, i32),
    },
    /// Arm the click gate for the primary button, with this hold's epoch.
    Arm {
        epoch: u64,
    },
    /// Inject the mask key (Alt mode), if the hold is still exactly as at `epoch`.
    Mask {
        epoch: u64,
    },
    Show {
        point: (i32, i32),
    },
    /// Disarm the click gate (keeping its epoch, so the same hold may arm it again).
    Disarm,
    Hide {
        animate: bool,
    },
    /// Make a note at `point`; `ask` is the lookup that found the spot empty.
    Summon {
        point: (i32, i32),
        ask: Option<u64>,
    },
}

/// The gesture's rules, driven by timed inputs. No Win32 calls.
#[derive(Debug, Clone)]
pub struct Gesture {
    modifier: Modifier,
    phase: Phase,
    keys: Keys,
    buttons: u8,
    blocked: bool,
    enabled: bool,
    /// This hold's gate epoch (from the key event that started it).
    epoch: u64,
    /// When the hold started, or when the cooldown ends.
    since: f64,
    until: f64,
    cursor: (i32, i32),
    next_ask: u64,
    pending: Option<u64>,
    asked_at: f64,
    asked_point: (i32, i32),
    /// The cursor moved since the spot was last asked about.
    dirty: bool,
    /// The latest lookup that found the spot empty.
    empty_ask: Option<u64>,
    /// The mask key was requested in this hold.
    masked: bool,
}

impl Gesture {
    pub fn new(modifier: Modifier) -> Gesture {
        Gesture {
            modifier,
            phase: Phase::Idle,
            keys: Keys::default(),
            buttons: 0,
            blocked: false,
            enabled: true,
            epoch: 0,
            since: 0.0,
            until: 0.0,
            cursor: (0, 0),
            next_ask: 0,
            pending: None,
            asked_at: f64::NEG_INFINITY,
            asked_point: (0, 0),
            dirty: false,
            empty_ask: None,
            masked: false,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn modifier(&self) -> Modifier {
        self.modifier
    }

    pub fn keys(&self) -> Keys {
        self.keys
    }

    /// Whether mouse moves matter now (the hold tracks the cursor).
    pub fn wants_moves(&self) -> bool {
        matches!(self.phase, Phase::Held | Phase::Looking | Phase::Showing)
    }

    /// When to send the next [`Input::Tick`], if anything waits on time.
    pub fn next_deadline(&self) -> Option<f64> {
        match self.phase {
            Phase::Held => Some(self.since + HOLD_DELAY),
            Phase::Looking | Phase::Showing => match self.pending {
                Some(_) => Some(self.asked_at + ASK_GIVE_UP),
                None if self.dirty => Some(self.asked_at + RECHECK_GAP),
                None => None,
            },
            Phase::Cooldown => Some(self.until),
            _ => None,
        }
    }

    pub fn step(&mut self, input: Input, now: f64) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::Key(key) => self.key(key, now, &mut out),
            Input::Mouse(event) => self.mouse(event, now, &mut out),
            Input::Buttons(bits) => self.buttons = bits,
            Input::Answer { ask, empty } => self.answer(ask, empty, now, &mut out),
            Input::ClickTaken { x, y } => {
                // The gate took a press while the pill showed: always make the note, the
                // click is gone either way.
                if self.phase == Phase::Showing {
                    out.push(Action::Hide { animate: false });
                }
                out.push(Action::Summon {
                    point: (x, y),
                    ask: self.empty_ask,
                });
                self.pending = None;
                self.phase = Phase::Clicked;
                self.after_hold(now);
            }
            Input::Blocked(blocked) => {
                self.blocked = blocked;
                if blocked {
                    self.spoil(&mut out, true);
                }
            }
            Input::Enabled(enabled) => {
                self.enabled = enabled;
                if !enabled {
                    self.spoil(&mut out, true);
                }
            }
            Input::Modifier(modifier) => {
                if modifier != self.modifier {
                    self.modifier = modifier;
                    self.spoil(&mut out, false);
                }
            }
            Input::ArmFailed => {
                if self.phase == Phase::Showing {
                    self.phase = Phase::Cancelled;
                    out.push(Action::Hide { animate: false });
                    self.after_hold(now);
                }
            }
            Input::KeysReset(keys) => {
                self.keys = keys;
                self.spoil(&mut out, false);
                self.after_hold(now);
            }
            Input::Tick => self.tick(now, &mut out),
            Input::Shutdown => {
                out.push(Action::Disarm);
                if self.phase == Phase::Showing {
                    out.push(Action::Hide { animate: false });
                }
                self.phase = Phase::Idle;
                self.pending = None;
            }
        }
        out
    }

    fn in_hold(&self) -> bool {
        matches!(self.phase, Phase::Held | Phase::Looking | Phase::Showing)
    }

    /// Ends a hold that is used up: hides the pill and disarms; then waits for the release.
    fn spoil(&mut self, out: &mut Vec<Action>, animate: bool) {
        if !self.in_hold() {
            return;
        }
        if self.phase == Phase::Showing {
            out.push(Action::Disarm);
            out.push(Action::Hide { animate });
        }
        self.pending = None;
        self.phase = Phase::Cancelled;
    }

    /// Out of a used-up hold once nothing of it is held any more.
    fn after_hold(&mut self, now: f64) {
        if self.keys.holding() {
            return;
        }
        self.phase = match self.phase {
            Phase::Clicked => {
                self.until = now + COOLDOWN;
                Phase::Cooldown
            }
            Phase::Cancelled => Phase::Idle,
            other => other,
        };
    }

    fn key(&mut self, key: KeyInput, now: f64, out: &mut Vec<Action>) {
        if !self.keys.apply(&key) {
            return; // a repeat
        }
        match self.phase {
            Phase::Idle | Phase::Cooldown => {
                let starts = key.down
                    && is_combo_key(key.vk, self.modifier)
                    && self.keys.combo(self.modifier);
                if starts {
                    let cooling = self.phase == Phase::Cooldown && now < self.until;
                    if cooling || self.buttons != 0 || self.blocked || !self.enabled {
                        self.phase = Phase::Cancelled;
                    } else {
                        self.phase = Phase::Held;
                        self.since = now;
                        self.epoch = key.epoch;
                        self.masked = false;
                        self.empty_ask = None;
                        self.pending = None;
                        self.asked_at = f64::NEG_INFINITY;
                    }
                } else if key.down
                    && self.keys.holding()
                    && !self.keys.partial(self.modifier)
                    && self.phase == Phase::Idle
                {
                    // A modifier is down together with something that is not the combo
                    // (AltGr, Ctrl in Alt mode, Shift): not a hold until all is released.
                    self.phase = Phase::Cancelled;
                }
            }
            Phase::Held | Phase::Looking | Phase::Showing => {
                // Any key change ends the hold: the release of the modifier, or another key
                // (Alt+Tab, Alt+F4, an access key), which uses it up.
                self.spoil(out, true);
                self.after_hold(now);
            }
            Phase::Clicked | Phase::Cancelled => self.after_hold(now),
        }
    }

    fn mouse(&mut self, event: MouseEvent, now: f64, out: &mut Vec<Action>) {
        match event {
            MouseEvent::Move { x, y, .. } => {
                self.cursor = (x, y);
                if matches!(self.phase, Phase::Looking | Phase::Showing) {
                    let (ax, ay) = self.asked_point;
                    if (x - ax).abs() > MOVE_SLOP || (y - ay).abs() > MOVE_SLOP {
                        self.dirty = true;
                    }
                    self.maybe_ask(now, out);
                }
            }
            MouseEvent::Down {
                button,
                injected: false,
                x,
                y,
                ..
            } => {
                // A press that reached us was not taken by the gate: the hold is used up
                // (an Alt+click, a pen or touch press, a click after the pill went).
                self.buttons |= button_bit(button);
                self.cursor = (x, y);
                self.spoil(out, true);
            }
            MouseEvent::Up {
                button,
                injected: false,
                ..
            } => self.buttons &= !button_bit(button),
            _ => {}
        }
    }

    fn answer(&mut self, ask: u64, empty: bool, now: f64, out: &mut Vec<Action>) {
        if self.pending != Some(ask) {
            return;
        }
        self.pending = None;
        match self.phase {
            Phase::Looking if empty => {
                self.empty_ask = Some(ask);
                out.push(Action::Arm { epoch: self.epoch });
                if self.modifier == Modifier::Alt && !self.masked {
                    self.masked = true;
                    out.push(Action::Mask { epoch: self.epoch });
                }
                out.push(Action::Show { point: self.cursor });
                self.phase = Phase::Showing;
            }
            Phase::Showing if empty => self.empty_ask = Some(ask),
            Phase::Showing => {
                // The cursor left empty space: the pill goes, and a click now is the app's.
                out.push(Action::Disarm);
                out.push(Action::Hide { animate: true });
                self.phase = Phase::Looking;
            }
            _ => {}
        }
        self.maybe_ask(now, out);
    }

    fn tick(&mut self, now: f64, out: &mut Vec<Action>) {
        match self.phase {
            Phase::Held if now >= self.since + HOLD_DELAY => {
                if self.buttons != 0 || self.blocked || !self.enabled {
                    self.phase = Phase::Cancelled;
                } else {
                    self.phase = Phase::Looking;
                    self.ask(now, out);
                }
            }
            Phase::Looking | Phase::Showing => self.maybe_ask(now, out),
            Phase::Cooldown if now >= self.until => self.phase = Phase::Idle,
            _ => {}
        }
    }

    fn maybe_ask(&mut self, now: f64, out: &mut Vec<Action>) {
        if self.pending.is_some() && now - self.asked_at > ASK_GIVE_UP {
            self.pending = None;
        }
        if self.pending.is_none() && self.dirty && now - self.asked_at >= RECHECK_GAP {
            self.ask(now, out);
        }
    }

    fn ask(&mut self, now: f64, out: &mut Vec<Action>) {
        self.next_ask += 1;
        self.pending = Some(self.next_ask);
        self.asked_at = now;
        self.asked_point = self.cursor;
        self.dirty = false;
        out.push(Action::Ask {
            ask: self.next_ask,
            point: self.cursor,
        });
    }
}

// ---------------------------------------------------------------------------------------
// The pill: drawing and animation (pure)
// ---------------------------------------------------------------------------------------

/// The pill window's size in physical pixels at `scale`.
pub fn pill_window_size(scale: f64) -> (u32, u32) {
    let w = ((PILL_SIZE.0 + 2.0 * PILL_MARGIN) * scale).ceil() as u32;
    let h = ((PILL_SIZE.1 + 2.0 * PILL_MARGIN) * scale).ceil() as u32;
    // Even sizes keep the hot spot on the centre pixel boundary.
    (w + w % 2, h + h % 2)
}

/// The appear spring at `t` seconds: 0 at the start, 1 at rest, overshooting slightly.
pub fn spring(t: f64) -> f64 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= APPEAR {
        return 1.0;
    }
    let omega = std::f64::consts::TAU / SPRING_RESPONSE;
    let zeta = SPRING_DAMPING;
    let damped = omega * (1.0 - zeta * zeta).sqrt();
    let decay = (-zeta * omega * t).exp();
    1.0 - decay * ((damped * t).cos() + zeta * omega / damped * (damped * t).sin())
}

fn ease_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Draws the pill at animation `value` (0: a hidden bead, 1: full size; may overshoot)
/// into `pixmap` (cleared first), centred.
pub fn render_pill(pixmap: &mut Pixmap, value: f64, scale: f64, dark: bool) {
    pixmap.fill(Color::TRANSPARENT);
    let alpha = value.clamp(0.0, 1.0) as f32;
    if alpha <= 0.0 {
        return;
    }
    let grow = value.max(0.0);
    let w = (BEAD + (PILL_SIZE.0 - BEAD) * grow) * scale;
    let h = (BEAD + (PILL_SIZE.1 - BEAD) * grow) * scale;
    let r = ((BEAD / 2.0 + (PILL_RADIUS - BEAD / 2.0) * grow) * scale).min(w.min(h) / 2.0);
    let (cx, cy) = (
        f64::from(pixmap.width()) / 2.0,
        f64::from(pixmap.height()) / 2.0,
    );
    let (fill, line, shadow): ([u8; 4], [u8; 4], [u8; 4]) = if dark {
        ([0x22, 0x27, 0x33, 240], [255, 255, 255, 40], [0, 0, 0, 90])
    } else {
        ([0xff, 0xff, 0xff, 245], [28, 38, 58, 64], [28, 38, 58, 44])
    };
    let s = scale as f32;
    // A soft shadow: rings that grow and fade, a little below the plate.
    for i in (1..=4).rev() {
        let spread = f64::from(i) * 1.2 * scale;
        let Some(path) = rounded(
            cx - w / 2.0 - spread,
            cy - h / 2.0 - spread + 1.0 * scale,
            w + 2.0 * spread,
            h + 2.0 * spread,
            r + spread,
        ) else {
            continue;
        };
        let a = f32::from(shadow[3]) / 255.0 * alpha * (0.35 - 0.07 * i as f32);
        fill_path(pixmap, &path, [shadow[0], shadow[1], shadow[2]], a);
    }
    let Some(plate) = rounded(cx - w / 2.0, cy - h / 2.0, w, h, r) else {
        return;
    };
    fill_path(
        pixmap,
        &plate,
        [fill[0], fill[1], fill[2]],
        f32::from(fill[3]) / 255.0 * alpha,
    );
    let inset = 0.5 * f64::from(s);
    if let Some(border) = rounded(
        cx - w / 2.0 + inset,
        cy - h / 2.0 + inset,
        w - 2.0 * inset,
        h - 2.0 * inset,
        (r - inset).max(0.0),
    ) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(
            line[0],
            line[1],
            line[2],
            (f32::from(line[3]) * alpha) as u8,
        );
        paint.anti_alias = true;
        let stroke = Stroke {
            width: s,
            ..Stroke::default()
        };
        pixmap.stroke_path(&border, &paint, &stroke, Transform::identity(), None);
    }
}

fn fill_path(pixmap: &mut Pixmap, path: &tiny_skia::Path, rgb: [u8; 3], alpha: f32) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(
        rgb[0],
        rgb[1],
        rgb[2],
        (alpha.clamp(0.0, 1.0) * 255.0) as u8,
    );
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn rounded(x: f64, y: f64, w: f64, h: f64, r: f64) -> Option<tiny_skia::Path> {
    let (x, y, w, h) = (x as f32, y as f32, w as f32, h as f32);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = (r as f32).clamp(0.0, w.min(h) / 2.0);
    let k = r * 0.552_284_8;
    let (l, t, rt, b) = (x, y, x + w, y + h);
    let mut p = PathBuilder::new();
    p.move_to(l + r, t);
    p.line_to(rt - r, t);
    p.cubic_to(rt - r + k, t, rt, t + r - k, rt, t + r);
    p.line_to(rt, b - r);
    p.cubic_to(rt, b - r + k, rt - r + k, b, rt - r, b);
    p.line_to(l + r, b);
    p.cubic_to(l + r - k, b, l, b - r + k, l, b - r);
    p.line_to(l, t + r);
    p.cubic_to(l, t + r - k, l + r - k, t, l + r, t);
    p.close();
    p.finish()
}

/// The pill's animation value over time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PillMotion {
    from: f64,
    start: f64,
    appearing: bool,
    /// False: jumps (reduced motion).
    animate: bool,
}

impl PillMotion {
    pub fn hidden() -> PillMotion {
        PillMotion {
            from: 0.0,
            start: 0.0,
            appearing: false,
            animate: false,
        }
    }

    pub fn value(&self, now: f64) -> f64 {
        let t = now - self.start;
        match (self.appearing, self.animate) {
            (true, false) => 1.0,
            (false, false) => 0.0,
            (true, true) => self.from + (1.0 - self.from) * spring(t),
            (false, true) => self.from * (1.0 - ease_out(t / DISAPPEAR)),
        }
    }

    /// Still changing at `now`.
    pub fn moving(&self, now: f64) -> bool {
        self.animate && now - self.start < if self.appearing { APPEAR } else { DISAPPEAR }
    }

    pub fn appear(&mut self, now: f64, animate: bool) {
        let from = self.value(now).clamp(0.0, 1.0);
        *self = PillMotion {
            from: if animate { from } else { 1.0 },
            start: now,
            appearing: true,
            animate,
        };
    }

    pub fn disappear(&mut self, now: f64, animate: bool) {
        let from = self.value(now).clamp(0.0, 1.0);
        *self = PillMotion {
            from,
            start: now,
            appearing: false,
            animate: animate && from > 0.0,
        };
    }

    pub fn is_shown(&self) -> bool {
        self.appearing
    }
}

// ---------------------------------------------------------------------------------------
// Shared state between the threads
// ---------------------------------------------------------------------------------------

/// An auto-reset event handle, closed on drop.
struct Event(HANDLE);

// SAFETY: a kernel event handle may be used and closed from any thread.
unsafe impl Send for Event {}
// SAFETY: `SetEvent` and waits on the same handle from several threads are allowed.
unsafe impl Sync for Event {}

impl Event {
    fn new() -> windows::core::Result<Event> {
        // SAFETY: no pointers; auto-reset, not signalled, unnamed. Owned by `Event`.
        unsafe { CreateEventW(None, false, false, None) }.map(Event)
    }

    fn set(&self) {
        // SAFETY: the handle lives as long as `self`.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: created by `CreateEventW`, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

enum Command {
    Mouse(MouseEvent),
    Blocked(bool),
    Enabled(bool),
    Config(SummonConfig),
    SummonAtCursor,
    RestartHook,
    /// For the self-test: show (or hide) the pill at the cursor without the gesture and
    /// without arming the gate.
    TestPill(bool),
    Shutdown,
}

struct Shared {
    commands: Mutex<VecDeque<Command>>,
    keys: Mutex<VecDeque<KeyInput>>,
    keys_dropped: AtomicU64,
    /// Wakes the summoner thread: commands or keys arrived.
    wake: Event,
    out: Mutex<VecDeque<SummonerEvent>>,
    out_wake: Event,
    wants_moves: AtomicBool,
    hook_installed: AtomicBool,
    key_callbacks: AtomicU64,
    key_max_ns: AtomicU64,
    pill_hwnd: AtomicIsize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while the lock is held cannot leave a queue half-changed.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn new() -> windows::core::Result<Shared> {
        Ok(Shared {
            commands: Mutex::new(VecDeque::new()),
            keys: Mutex::new(VecDeque::with_capacity(KEY_QUEUE_CAPACITY)),
            keys_dropped: AtomicU64::new(0),
            wake: Event::new()?,
            out: Mutex::new(VecDeque::new()),
            out_wake: Event::new()?,
            wants_moves: AtomicBool::new(false),
            hook_installed: AtomicBool::new(false),
            key_callbacks: AtomicU64::new(0),
            key_max_ns: AtomicU64::new(0),
            pill_hwnd: AtomicIsize::new(0),
        })
    }

    fn send(&self, command: Command) {
        {
            let mut commands = lock(&self.commands);
            if commands.len() >= COMMAND_CAPACITY && matches!(command, Command::Mouse(_)) {
                return;
            }
            commands.push_back(command);
        }
        self.wake.set();
    }

    fn emit(&self, event: SummonerEvent) {
        lock(&self.out).push_back(event);
        self.out_wake.set();
    }
}

// ---------------------------------------------------------------------------------------
// The keyboard hook (rule 1)
// ---------------------------------------------------------------------------------------

struct KeyContext {
    shared: Arc<Shared>,
    /// Keys down as this hook saw them, to drop auto-repeats.
    down: [u64; 4],
}

thread_local! {
    static KEY_CONTEXT: RefCell<Option<KeyContext>> = const { RefCell::new(None) };
}

unsafe extern "system" fn key_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let started = Instant::now();
        // SAFETY: for HC_ACTION, `lparam` points to a KBDLLHOOKSTRUCT valid for this call;
        // it is copied out at once.
        let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let _ = catch_unwind(AssertUnwindSafe(|| {
            key_event(wparam.0 as u32, &info, started)
        }));
    }
    // SAFETY: passes the hook's own arguments on unchanged. Never swallows.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn key_event(message: u32, info: &KBDLLHOOKSTRUCT, started: Instant) {
    // Our own mask key (and the paste's keys): not the user's.
    if info.dwExtraInfo == INJECTED_MARKER {
        return;
    }
    KEY_CONTEXT.with(|context| {
        let Ok(mut context) = context.try_borrow_mut() else {
            return;
        };
        let Some(context) = context.as_mut() else {
            return;
        };
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let vk = info.vkCode & 0xFF;
        let (word, bit) = (vk as usize / 64, 1u64 << (vk % 64));
        let was_down = context.down[word] & bit != 0;
        if down && was_down {
            return; // auto-repeat
        }
        if down {
            context.down[word] |= bit;
        } else {
            context.down[word] &= !bit;
        }
        let epoch = CLICK_GATE.disarm();
        let event = KeyInput {
            vk,
            down,
            altgr: is_altgr_ctrl(vk, info.scanCode),
            epoch,
            time: hook::now(),
        };
        let shared = &context.shared;
        {
            let mut keys = lock(&shared.keys);
            if keys.len() < KEY_QUEUE_CAPACITY {
                keys.push_back(event);
            } else {
                shared.keys_dropped.fetch_add(1, Relaxed);
            }
        }
        shared.wake.set();
        shared.key_callbacks.fetch_add(1, Relaxed);
        let spent = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        shared.key_max_ns.fetch_max(spent, Relaxed);
    });
}

struct KeyHook {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

impl KeyHook {
    fn start(shared: Arc<Shared>) -> Result<KeyHook, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
        let thread = thread::Builder::new()
            .name("summon-key-hook".into())
            .spawn(move || {
                let mut msg = MSG::default();
                // SAFETY: plain calls on this thread; `PeekMessageW` creates its queue so
                // `WM_STOP` can be posted as soon as `ready` is sent.
                let (thread_id, module) = unsafe {
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                    (GetCurrentThreadId(), GetModuleHandleW(None))
                };
                let installed = shared.clone();
                KEY_CONTEXT.set(Some(KeyContext {
                    shared,
                    down: [0; 4],
                }));
                // SAFETY: `key_proc` matches HOOKPROC and lives for the whole program.
                let hook = module.and_then(|module| unsafe {
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), Some(module.into()), 0)
                });
                let hook = match hook {
                    Ok(hook) => hook,
                    Err(error) => {
                        KEY_CONTEXT.set(None);
                        let _ = ready_tx.send(Err(format!("keyboard hook: {error}")));
                        return;
                    }
                };
                installed.hook_installed.store(true, Relaxed);
                let _ = ready_tx.send(Ok(thread_id));
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
                installed.hook_installed.store(false, Relaxed);
                // SAFETY: set by this thread above, removed once (an error if Windows
                // already removed it is ignored).
                let _ = unsafe { UnhookWindowsHookEx(hook) };
                KEY_CONTEXT.set(None);
            })
            .map_err(|e| format!("keyboard hook thread: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(KeyHook {
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

    fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
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
    }
}

// ---------------------------------------------------------------------------------------
// The summoner thread
// ---------------------------------------------------------------------------------------

/// What the thread starts with (kept by the owner for restarts).
#[derive(Debug, Clone, Copy)]
struct Start {
    config: SummonConfig,
    blocked: bool,
}

/// Disarms the gate however the thread ends, panics included.
struct DisarmOnExit;

impl Drop for DisarmOnExit {
    fn drop(&mut self) {
        CLICK_GATE.disarm();
    }
}

enum Purpose {
    Gesture(u64),
    Hotkey { point: (i32, i32), time: f64 },
}

struct Pill {
    window: Overlay,
    motion: PillMotion,
    pixmap: Option<Pixmap>,
    scale: f64,
    dark: bool,
    visible: bool,
    /// Rendered value last presented.
    drawn: f64,
}

impl Pill {
    fn new(capturable: bool) -> windows::core::Result<Pill> {
        let mut window = Overlay::new()?;
        window.set_capturable(capturable)?;
        Ok(Pill {
            window,
            motion: PillMotion::hidden(),
            pixmap: None,
            scale: 0.0,
            dark: false,
            visible: false,
            drawn: -1.0,
        })
    }

    fn show(&mut self, now: f64) {
        self.dark = system_dark();
        self.motion.appear(now, animation_effects());
        self.drawn = -1.0;
    }

    fn hide(&mut self, now: f64, animate: bool) {
        self.motion.disappear(now, animate && animation_effects());
        self.drawn = -1.0;
    }

    /// Presents the current frame at the cursor. Returns whether it still moves.
    fn frame(&mut self, now: f64) -> bool {
        let value = self.motion.value(now);
        let moving = self.motion.moving(now);
        if !self.motion.is_shown() && !moving {
            if self.visible {
                self.window.hide();
                self.visible = false;
            }
            return false;
        }
        let cursor = cursor();
        let scale = monitor_at(Point::new(f64::from(cursor.0), f64::from(cursor.1))).scale;
        if scale != self.scale || self.pixmap.is_none() {
            let (w, h) = pill_window_size(scale);
            self.pixmap = Pixmap::new(w, h);
            self.scale = scale;
            self.drawn = -1.0;
        }
        let Some(pixmap) = self.pixmap.as_mut() else {
            return false;
        };
        let origin = (
            cursor.0 - pixmap.width() as i32 / 2,
            cursor.1 - pixmap.height() as i32 / 2,
        );
        if (value - self.drawn).abs() > 1e-4 {
            render_pill(pixmap, value, scale, self.dark);
            self.drawn = value;
            let _ = self.window.present(pixmap, origin);
        } else {
            let _ = self.window.move_to(origin);
        }
        if !self.visible {
            self.window.show();
            self.visible = true;
        }
        if self.window.take_display_changed() {
            self.window.reassert();
        }
        moving
    }
}

struct Driver {
    shared: Arc<Shared>,
    requester: Arc<Mutex<Requester>>,
    gesture: Gesture,
    config: SummonConfig,
    pill: Pill,
    hook: Option<KeyHook>,
    asks: HashMap<RequestId, Purpose>,
    /// The latest lookup that found the spot empty, for the origin of a click.
    empty: Option<(u64, Outcome)>,
    names: AppNames,
    taken_seen: u64,
    next_health: f64,
    strikes: u32,
}

fn run(
    shared: Arc<Shared>,
    requester: Arc<Mutex<Requester>>,
    start: Start,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let _disarm = DisarmOnExit;
    let mut msg = MSG::default();
    // SAFETY: plain calls on this thread. Physical pixels for the pill and the cursor; the
    // message queue exists before anyone waits on us.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    }
    let gate_wake = CLICK_GATE.wake_handle();
    let pill = match Pill::new(start.config.capturable) {
        Ok(pill) => pill,
        Err(e) => {
            let _ = ready.send(Err(format!("pill window: {e}")));
            return;
        }
    };
    shared
        .pill_hwnd
        .store(pill.window.hwnd().0 as isize, Relaxed);
    let hook = match KeyHook::start(shared.clone()) {
        Ok(hook) => hook,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut gesture = Gesture::new(start.config.modifier);
    let now = hook::now();
    gesture.step(Input::Enabled(start.config.enabled), now);
    gesture.step(Input::Blocked(start.blocked), now);
    // Keys already down when we start count as a used-up hold.
    gesture.step(Input::KeysReset(Keys::now()), now);
    let mut driver = Driver {
        shared,
        requester,
        gesture,
        config: start.config,
        pill,
        hook: Some(hook),
        asks: HashMap::new(),
        empty: None,
        names: AppNames::default(),
        taken_seen: CLICK_GATE.taken().count,
        next_health: now + HEALTH_CHECK,
        strikes: 0,
    };
    let _ = ready.send(Ok(()));
    drop(ready);
    driver.log(&format!(
        "started: modifier {}, enabled {}",
        driver.config.modifier.label(),
        driver.config.enabled
    ));
    driver.run(gate_wake);
}

impl Driver {
    fn log(&self, text: &str) {
        if self.config.debug {
            self.shared.emit(SummonerEvent::Log(format!(
                "{:10.3} summon: {text}",
                hook::now()
            )));
        }
    }

    fn run(&mut self, gate_wake: HANDLE) {
        let mut animating = false;
        loop {
            let now = hook::now();
            let mut due = self.next_health;
            if let Some(at) = self.gesture.next_deadline() {
                due = due.min(at);
            }
            if animating {
                due = due.min(now + FRAME.as_secs_f64());
            }
            for purpose in self.asks.values() {
                if let Purpose::Hotkey { time, .. } = purpose {
                    due = due.min(time + HOTKEY_GIVE_UP);
                }
            }
            let timeout = ((due - now).max(0.0) * 1000.0).ceil() as u32;
            let mut handles = vec![self.shared.wake.0, lock(&self.requester).wake_handle()];
            if !gate_wake.is_invalid() {
                handles.push(gate_wake);
            }
            // SAFETY: every handle is alive for the call: ours, the requester's (owned by
            // `self.requester`) and the gate's (never closed).
            let result = unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&handles),
                    timeout,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
            if result == WAIT_FAILED {
                thread::sleep(Duration::from_millis(10));
            }
            pump_messages();
            if !self.turn() {
                break;
            }
            animating = self.pill.frame(hook::now());
        }
        // Shutdown: nothing armed, nothing shown, the hook removed.
        let actions = self.gesture.step(Input::Shutdown, hook::now());
        self.execute(actions);
        self.pill.motion = PillMotion::hidden();
        self.pill.frame(hook::now());
        self.hook = None;
        self.log("stopped");
    }

    /// One pass over everything that arrived. False on shutdown.
    fn turn(&mut self) -> bool {
        let commands: Vec<Command> = lock(&self.shared.commands).drain(..).collect();
        let keys: Vec<KeyInput> = lock(&self.shared.keys).drain(..).collect();
        let mut timed: Vec<(f64, Input)> = Vec::new();
        for command in commands {
            match command {
                Command::Mouse(event) => timed.push((event_time(&event), Input::Mouse(event))),
                Command::Blocked(blocked) => {
                    self.log(&format!("blocked {blocked}"));
                    self.step(Input::Blocked(blocked));
                }
                Command::Enabled(enabled) => {
                    self.log(&format!("enabled {enabled}"));
                    self.step(Input::Enabled(enabled));
                }
                Command::Config(config) => {
                    let capturable = config.capturable != self.config.capturable;
                    self.config = config;
                    if capturable {
                        let _ = self.pill.window.set_capturable(config.capturable);
                    }
                    self.step(Input::Modifier(config.modifier));
                    self.step(Input::Enabled(config.enabled));
                    self.log(&format!("config: modifier {}", config.modifier.label()));
                }
                Command::SummonAtCursor => self.summon_at_cursor(),
                Command::RestartHook => self.restart_hook("asked"),
                Command::TestPill(on) => {
                    let now = hook::now();
                    if on {
                        self.pill.show(now);
                    } else {
                        self.pill.hide(now, false);
                    }
                }
                Command::Shutdown => return false,
            }
        }
        for key in keys {
            timed.push((key.time, Input::Key(key)));
        }
        let taken = CLICK_GATE.taken();
        if taken.count != self.taken_seen {
            self.taken_seen = taken.count;
            self.log(&format!(
                "swallowed press at {},{} (the gate took it; its release is swallowed too)",
                taken.x, taken.y
            ));
            timed.push((
                taken.time,
                Input::ClickTaken {
                    x: taken.x,
                    y: taken.y,
                },
            ));
        }
        timed.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, input) in timed {
            if let Input::Key(key) = input
                && key.down
                && matches!(self.gesture.phase(), Phase::Idle | Phase::Cooldown)
            {
                self.gesture
                    .step(Input::Buttons(buttons_now()), hook::now());
            }
            self.step(input);
        }
        self.answers();
        if self.gesture.wants_moves() {
            // Moves before the hold began were not fed: give the gesture the cursor now.
            let (x, y) = cursor();
            self.step(Input::Mouse(MouseEvent::Move {
                x,
                y,
                time: hook::now(),
                injected: false,
            }));
        }
        self.step(Input::Tick);
        self.hotkey_give_up();
        self.health();
        self.shared
            .wants_moves
            .store(self.gesture.wants_moves(), Relaxed);
        true
    }

    fn step(&mut self, input: Input) {
        let before = self.gesture.phase();
        let actions = self.gesture.step(input, hook::now());
        let after = self.gesture.phase();
        if before != after {
            self.log(&format!("{before:?} -> {after:?}"));
        }
        self.execute(actions);
    }

    fn execute(&mut self, actions: Vec<Action>) {
        let now = hook::now();
        for action in actions {
            match action {
                Action::Ask { ask, point } => {
                    let id = lock(&self.requester).request(Lookup {
                        point,
                        want_focus: false,
                        deadline: LOOKUP_DEADLINE,
                        probe: false,
                    });
                    self.asks.insert(id, Purpose::Gesture(ask));
                }
                Action::Arm { epoch } => {
                    let button = primary_button();
                    if CLICK_GATE.arm(epoch, button) {
                        self.log(&format!("gate armed for {button:?} (epoch {epoch})"));
                    } else {
                        self.log("gate refused (a key changed): no pill");
                        let more = self.gesture.step(Input::ArmFailed, now);
                        self.execute(more);
                        return;
                    }
                }
                Action::Mask { epoch } => match send_mask(epoch) {
                    Ok(()) => self.log(&format!(
                        "injected mask key vk{MASK_KEY:02X} down+up (Alt held, epoch {epoch})"
                    )),
                    Err(why) => self.log(&format!("mask key not sent: {why}")),
                },
                Action::Show { point } => {
                    self.log(&format!("pill shown at {},{}", point.0, point.1));
                    self.pill.show(now);
                }
                Action::Disarm => {
                    CLICK_GATE.withdraw();
                    self.log("gate disarmed");
                }
                Action::Hide { animate } => {
                    self.log("pill hidden");
                    self.pill.hide(now, animate);
                }
                Action::Summon { point, ask } => {
                    let empty = self.empty.take();
                    let none = Outcome::Failed("no lookup".into());
                    let outcome = match (&empty, ask) {
                        (Some((id, outcome)), Some(ask)) if *id == ask => outcome,
                        _ => &none,
                    };
                    let origin = self.origin_at(point, outcome);
                    self.empty = empty;
                    self.emit_summon(point, origin, now);
                }
            }
        }
    }

    fn origin_at(&mut self, point: (i32, i32), outcome: &Outcome) -> NoteOrigin {
        let hit_pid = match outcome {
            Outcome::Found(s) => Some(s.hit.element().pid),
            _ => None,
        };
        let facts = facts_at(point, hit_pid, &mut self.names);
        context::origin(outcome, &facts)
    }

    fn emit_summon(&self, point: (i32, i32), origin: NoteOrigin, time: f64) {
        self.log(&format!(
            "summon at {},{}: {} | {}",
            point.0,
            point.1,
            origin.label(),
            origin.detail().replace('\n', " | ")
        ));
        self.shared.emit(SummonerEvent::Summon(Summon {
            point,
            origin,
            time,
        }));
    }

    fn answers(&mut self) {
        loop {
            let answer = lock(&self.requester).poll();
            let Some((id, outcome)) = answer else {
                break;
            };
            match self.asks.remove(&id) {
                Some(Purpose::Gesture(ask)) => {
                    let empty = is_empty_space(&outcome);
                    self.log(&format!(
                        "lookup {ask}: {} -> empty {empty}",
                        describe(&outcome)
                    ));
                    if empty {
                        // Read the window facts now (the exe's name is cached), so the
                        // click finds them ready.
                        if let Outcome::Found(s) = &outcome {
                            let _ = facts_at(s.point, Some(s.hit.element().pid), &mut self.names);
                        }
                        self.empty = Some((ask, outcome));
                    }
                    self.step(Input::Answer { ask, empty });
                }
                Some(Purpose::Hotkey { point, time }) => {
                    let origin = self.origin_at(point, &outcome);
                    self.emit_summon(point, origin, time);
                }
                None => {}
            }
        }
    }

    /// The NewNote hotkey: a note at the cursor, no gesture, nothing swallowed.
    fn summon_at_cursor(&mut self) {
        let point = cursor();
        let time = hook::now();
        let id = lock(&self.requester).request(Lookup {
            point,
            want_focus: false,
            deadline: LOOKUP_DEADLINE,
            probe: false,
        });
        self.log(&format!("hotkey: note at {},{}", point.0, point.1));
        self.asks.insert(id, Purpose::Hotkey { point, time });
    }

    /// A hotkey lookup that got no answer (a gesture lookup replaced it): the window facts
    /// alone name the origin.
    fn hotkey_give_up(&mut self) {
        let now = hook::now();
        let late: Vec<RequestId> = self
            .asks
            .iter()
            .filter(
                |(_, p)| matches!(p, Purpose::Hotkey { time, .. } if now - time > HOTKEY_GIVE_UP),
            )
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            if let Some(Purpose::Hotkey { point, time }) = self.asks.remove(&id) {
                let origin = self.origin_at(point, &Outcome::Failed("no answer".into()));
                self.emit_summon(point, origin, time);
            }
        }
        // Gesture lookups replaced before they ran never get an answer: forget old ones.
        if self.asks.len() > 64 {
            self.asks.clear();
        }
    }

    fn health(&mut self) {
        let now = hook::now();
        if now < self.next_health {
            return;
        }
        self.next_health = now + HEALTH_CHECK;
        let alive = self.hook.as_ref().is_some_and(KeyHook::is_alive)
            && self.shared.hook_installed.load(Relaxed);
        if !alive {
            self.restart_hook("the keyboard hook is gone");
            return;
        }
        // Windows removes a slow hook without notice. A hook that missed key changes shows
        // as a disagreement with the system's key state; twice in a row means restart.
        let system = Keys::now();
        let seen = self.gesture.keys();
        let disagree = system.alt() != seen.alt() || system.ctrl() != (seen.ctrl() || seen.altgr);
        self.strikes = if disagree { self.strikes + 1 } else { 0 };
        if self.strikes >= 2 {
            self.strikes = 0;
            self.restart_hook("its key state disagrees with the system's");
        }
    }

    fn restart_hook(&mut self, why: &str) {
        self.hook = None;
        CLICK_GATE.disarm();
        match KeyHook::start(self.shared.clone()) {
            Ok(hook) => {
                self.hook = Some(hook);
                self.log(&format!("keyboard hook restarted ({why})"));
            }
            Err(e) => self.log(&format!("keyboard hook restart failed ({why}): {e}")),
        }
        // Whatever was held is unknown now: a used-up hold until it is released.
        self.step(Input::KeysReset(Keys::now()));
    }
}

fn event_time(event: &MouseEvent) -> f64 {
    match *event {
        MouseEvent::Move { time, .. }
        | MouseEvent::Down { time, .. }
        | MouseEvent::Up { time, .. }
        | MouseEvent::Wheel { time, .. } => time,
    }
}

fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Found(s) => {
            let e = s.hit.element();
            format!(
                "{} {} ({}, {})",
                e.role,
                crate::uia::control_type_name(e.control_type),
                s.target.exe.as_deref().unwrap_or("?"),
                s.target.class_name
            )
        }
        Outcome::OwnProcess => "own window".into(),
        Outcome::Elevated { pid } => format!("elevated (pid {pid})"),
        Outcome::TimedOut { target } => format!("timed out ({})", target.class_name),
        Outcome::Failed(why) => format!("failed ({why})"),
    }
}

fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid MSG for the calls to fill and read; the pill's window
    // procedure is the overlay's, which touches no Rust state.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn cursor() -> (i32, i32) {
    let mut point = windows::Win32::Foundation::POINT::default();
    // SAFETY: writes into a local. Fails on the secure desktop: the origin then.
    let _ = unsafe { GetCursorPos(&mut point) };
    (point.x, point.y)
}

/// The physical button of the user's main click.
fn primary_button() -> Button {
    // SAFETY: plain value argument.
    if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 {
        Button::Right
    } else {
        Button::Left
    }
}

/// Sends the mask key, after the last checks (rule 7): no key changed since the hold
/// began, Alt is down, and Ctrl, Shift and Win are up.
fn send_mask(epoch: u64) -> Result<(), &'static str> {
    if CLICK_GATE.epoch() != epoch {
        return Err("a key changed since the hold began");
    }
    let keys = Keys::now();
    if !keys.alt() {
        return Err("Alt is already up");
    }
    if keys.ctrl() || keys.shift || keys.win {
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

fn system_dark() -> bool {
    let mut light: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: the value is restricted to a DWORD and both out buffers are locals.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut light as *mut u32).cast()),
            Some(&mut size),
        )
    };
    result.is_ok() && light == 0
}

/// False when the user turned off "Animation effects" (reduced motion).
fn animation_effects() -> bool {
    let mut enabled = windows::core::BOOL(0);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes exactly one BOOL into a local.
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut windows::core::BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok()
        && enabled.as_bool()
}

// ---------------------------------------------------------------------------------------
// Public API (the UI thread)
// ---------------------------------------------------------------------------------------

/// The note gesture. Owned by the UI thread; every call returns at once.
pub struct Summoner {
    shared: Arc<Shared>,
    requester: Arc<Mutex<Requester>>,
    thread: Option<JoinHandle<()>>,
    start: Start,
    blocks: u8,
}

impl Summoner {
    /// Starts the summoner thread (pill window, keyboard hook). `requester` is a lane of the
    /// app's UI Automation worker (`Uia::requester`).
    pub fn start(config: SummonConfig, requester: Requester) -> Result<Summoner, String> {
        hook::now();
        let shared = Arc::new(Shared::new().map_err(|e| format!("events: {e}"))?);
        let requester = Arc::new(Mutex::new(requester));
        let start = Start {
            config,
            blocked: false,
        };
        let thread = spawn(&shared, &requester, start)?;
        Ok(Summoner {
            shared,
            requester,
            thread: Some(thread),
            start,
            blocks: 0,
        })
    }

    /// The mouse events the app's hook delivered, in order. Moves are dropped at once while
    /// no hold is under way, so the summoner thread does not wake for them.
    pub fn feed(&self, events: &[MouseEvent]) {
        let moves = self.shared.wants_moves.load(Relaxed);
        let mut any = false;
        {
            let mut commands = lock(&self.shared.commands);
            for event in events {
                let is_move = matches!(event, MouseEvent::Move { .. });
                if (is_move && !moves) || matches!(event, MouseEvent::Wheel { .. }) {
                    continue;
                }
                if is_move
                    && let Some(Command::Mouse(last @ MouseEvent::Move { .. })) =
                        commands.back_mut()
                {
                    *last = *event;
                    any = true;
                    continue;
                }
                if commands.len() < COMMAND_CAPACITY {
                    commands.push_back(Command::Mouse(*event));
                    any = true;
                }
            }
        }
        if any {
            self.shared.wake.set();
        }
    }

    /// Something holds the screen (or no longer does). The gesture is blocked while any
    /// reason is set; a pill that shows goes at once.
    pub fn set_blocked(&mut self, reason: Block, blocked: bool) {
        let before = self.blocks != 0;
        if blocked {
            self.blocks |= reason.bit();
        } else {
            self.blocks &= !reason.bit();
        }
        let now = self.blocks != 0;
        self.start.blocked = now;
        if now != before {
            self.shared.send(Command::Blocked(now));
        }
    }

    /// The settings switch.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.start.config.enabled = enabled;
        self.shared.send(Command::Enabled(enabled));
    }

    pub fn apply_config(&mut self, config: SummonConfig) {
        self.start.config = config;
        self.shared.send(Command::Config(config));
    }

    /// The NewNote hotkey: looks up the origin at the cursor and emits a [`Summon`], with no
    /// gesture and nothing swallowed.
    pub fn summon_at_cursor(&self) {
        self.shared.send(Command::SummonAtCursor);
    }

    /// Appends every event since the last call.
    pub fn drain(&self, out: &mut Vec<SummonerEvent>) {
        out.extend(lock(&self.shared.out).drain(..));
    }

    /// An auto-reset event set when events are waiting. Stays the same across restarts.
    pub fn wake_handle(&self) -> HANDLE {
        self.shared.out_wake.0
    }

    /// The thread runs and its keyboard hook is set. The thread restarts the hook by itself
    /// when it finds it gone; [`restart`](Self::restart) covers a dead thread.
    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
            && self.shared.hook_installed.load(Relaxed)
    }

    /// Restarts the keyboard hook, or the whole thread if it ended.
    pub fn restart(&mut self) -> Result<(), String> {
        if self.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            self.shared.send(Command::RestartHook);
            return Ok(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        CLICK_GATE.disarm();
        self.thread = Some(spawn(&self.shared, &self.requester, self.start)?);
        Ok(())
    }

    /// Calls of the keyboard hook so far and the longest one.
    pub fn key_hook_stats(&self) -> (u64, Duration) {
        (
            self.shared.key_callbacks.load(Relaxed),
            Duration::from_nanos(self.shared.key_max_ns.load(Relaxed)),
        )
    }

    /// The pill window, for tests.
    pub fn pill_window(&self) -> isize {
        self.shared.pill_hwnd.load(Relaxed)
    }

    /// For the self-test only: shows the pill at the cursor (following it) without the
    /// gesture and without arming the gate, or hides it.
    pub fn test_pill(&self, show: bool) {
        self.shared.send(Command::TestPill(show));
    }

    /// Stops the thread: disarms the gate, hides the pill, removes the keyboard hook. True
    /// when the thread ended within a second.
    pub fn shutdown(mut self) -> bool {
        self.stop()
    }

    fn stop(&mut self) -> bool {
        CLICK_GATE.disarm();
        let Some(thread) = self.thread.take() else {
            return true;
        };
        self.shared.send(Command::Shutdown);
        let handle = HANDLE(thread.as_raw_handle());
        // SAFETY: the live handle owned by `thread`, which outlives the wait.
        let ended =
            unsafe { WaitForSingleObject(handle, STOP_WAIT.as_millis() as u32) } == WAIT_OBJECT_0;
        CLICK_GATE.disarm();
        if ended {
            let _ = thread.join();
        }
        ended
    }
}

impl Drop for Summoner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn(
    shared: &Arc<Shared>,
    requester: &Arc<Mutex<Requester>>,
    start: Start,
) -> Result<JoinHandle<()>, String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread_shared = shared.clone();
    let thread_requester = requester.clone();
    let thread = thread::Builder::new()
        .name("summoner".into())
        .spawn(move || run(thread_shared, thread_requester, start, ready_tx))
        .map_err(|e| format!("summoner thread: {e}"))?;
    match ready_rx.recv_timeout(START_WAIT) {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err("summoner thread did not start".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LALT: u32 = 0xA4;
    const RALT: u32 = 0xA5;
    const LCTRL: u32 = 0xA2;
    const TAB: u32 = 0x09;
    const F4: u32 = 0x73;
    const KEY_F: u32 = 0x46;

    /// A scripted run: inputs at times, every action recorded.
    struct Script {
        g: Gesture,
        actions: Vec<(f64, Action)>,
        epoch: u64,
    }

    impl Script {
        fn new(modifier: Modifier) -> Script {
            Script {
                g: Gesture::new(modifier),
                actions: Vec::new(),
                epoch: 0,
            }
        }

        fn at(&mut self, t: f64, input: Input) -> &mut Script {
            // Time passes up to `t` first, as the driver's ticks would make it.
            self.tick_to(t);
            let actions = self.g.step(input, t);
            self.actions.extend(actions.into_iter().map(|a| (t, a)));
            self
        }

        fn tick_to(&mut self, t: f64) {
            while let Some(due) = self.g.next_deadline() {
                if due > t {
                    break;
                }
                let actions = self.g.step(Input::Tick, due);
                self.actions.extend(actions.into_iter().map(|a| (due, a)));
                if self.g.next_deadline() == Some(due) {
                    break;
                }
            }
        }

        fn key(&mut self, t: f64, vk: u32, down: bool) -> &mut Script {
            self.epoch += 1;
            let epoch = self.epoch;
            self.at(
                t,
                Input::Key(KeyInput {
                    vk,
                    down,
                    altgr: false,
                    epoch,
                    time: t,
                }),
            )
        }

        fn altgr(&mut self, t: f64, down: bool) -> &mut Script {
            self.epoch += 1;
            let epoch = self.epoch;
            self.at(
                t,
                Input::Key(KeyInput {
                    vk: LCTRL,
                    down,
                    altgr: true,
                    epoch,
                    time: t,
                }),
            )
        }

        fn moved(&mut self, t: f64, x: i32, y: i32) -> &mut Script {
            self.at(
                t,
                Input::Mouse(MouseEvent::Move {
                    x,
                    y,
                    time: t,
                    injected: false,
                }),
            )
        }

        fn press(&mut self, t: f64, touch: bool) -> &mut Script {
            self.at(
                t,
                Input::Mouse(MouseEvent::Down {
                    button: Button::Left,
                    x: 5,
                    y: 5,
                    time: t,
                    click_count: 1,
                    injected: false,
                    window: 0,
                    touch,
                }),
            )
        }

        fn release(&mut self, t: f64) -> &mut Script {
            self.at(
                t,
                Input::Mouse(MouseEvent::Up {
                    button: Button::Left,
                    x: 5,
                    y: 5,
                    time: t,
                    injected: false,
                }),
            )
        }

        /// Answers the newest ask.
        fn answer(&mut self, t: f64, empty: bool) -> &mut Script {
            let ask = self.last_ask().expect("an ask to answer");
            self.at(t, Input::Answer { ask, empty })
        }

        fn last_ask(&self) -> Option<u64> {
            self.actions.iter().rev().find_map(|(_, a)| match a {
                Action::Ask { ask, .. } => Some(*ask),
                _ => None,
            })
        }

        fn count(&self, f: impl Fn(&Action) -> bool) -> usize {
            self.actions.iter().filter(|(_, a)| f(a)).count()
        }

        fn asks(&self) -> usize {
            self.count(|a| matches!(a, Action::Ask { .. }))
        }
        fn arms(&self) -> usize {
            self.count(|a| matches!(a, Action::Arm { .. }))
        }
        fn masks(&self) -> usize {
            self.count(|a| matches!(a, Action::Mask { .. }))
        }
        fn shows(&self) -> usize {
            self.count(|a| matches!(a, Action::Show { .. }))
        }
        fn summons(&self) -> usize {
            self.count(|a| matches!(a, Action::Summon { .. }))
        }
        fn hides(&self) -> usize {
            self.count(|a| matches!(a, Action::Hide { .. }))
        }
        fn disarms(&self) -> usize {
            self.count(|a| matches!(a, Action::Disarm))
        }

        /// Nothing at all touched the gate, the keyboard or the screen.
        fn untouched(&self) -> bool {
            self.arms() == 0 && self.masks() == 0 && self.shows() == 0 && self.summons() == 0
        }

        /// Alt down at 0, held past the delay over empty space: the pill shows.
        fn showing(modifier: Modifier) -> Script {
            let mut s = Script::new(modifier);
            if modifier == Modifier::CtrlAlt {
                s.key(0.0, LCTRL, true);
            }
            s.key(0.0, LALT, true).moved(0.1, 100, 100);
            s.tick_to(0.35);
            assert_eq!(s.asks(), 1, "the lookup runs once the delay passed");
            s.answer(0.36, true);
            assert_eq!(s.g.phase(), Phase::Showing);
            s
        }
    }

    #[test]
    fn hold_then_click_makes_a_note() {
        let mut s = Script::showing(Modifier::Alt);
        assert_eq!((s.arms(), s.masks(), s.shows()), (1, 1, 1));
        // The arm and the mask carry the epoch of the Alt press.
        assert!(
            s.actions
                .iter()
                .any(|(_, a)| *a == Action::Arm { epoch: 1 })
        );
        assert!(
            s.actions
                .iter()
                .any(|(_, a)| *a == Action::Mask { epoch: 1 })
        );
        // Arm comes before show: a click is never shown a pill it cannot take.
        let arm = s
            .actions
            .iter()
            .position(|(_, a)| matches!(a, Action::Arm { .. }));
        let show = s
            .actions
            .iter()
            .position(|(_, a)| matches!(a, Action::Show { .. }));
        assert!(arm < show);
        s.at(0.5, Input::ClickTaken { x: 120, y: 130 });
        let summon = s.actions.iter().find_map(|(_, a)| match a {
            Action::Summon { point, ask } => Some((*point, *ask)),
            _ => None,
        });
        assert_eq!(summon, Some(((120, 130), Some(1))));
        assert!(
            s.actions
                .iter()
                .any(|(_, a)| *a == Action::Hide { animate: false })
        );
        assert_eq!(s.g.phase(), Phase::Clicked);
        // A second click in the same hold makes nothing: the gate is not armed again.
        s.press(0.6, false).release(0.65);
        assert_eq!(s.summons(), 1);
        assert_eq!(s.arms(), 1);
        // Release: a short cooldown, then idle.
        s.key(0.8, LALT, false);
        assert_eq!(s.g.phase(), Phase::Cooldown);
        s.tick_to(0.8 + COOLDOWN + 0.01);
        assert_eq!(s.g.phase(), Phase::Idle);
    }

    #[test]
    fn hold_and_release_without_a_click_makes_nothing_and_masks_the_menu() {
        let mut s = Script::showing(Modifier::Alt);
        s.key(0.9, LALT, false);
        assert_eq!(s.summons(), 0);
        // The mask key was requested while Alt was still down (at show time), so the
        // release is not a lone Alt.
        assert_eq!(s.masks(), 1);
        let mask = s
            .actions
            .iter()
            .find(|(_, a)| matches!(a, Action::Mask { .. }))
            .unwrap()
            .0;
        assert!(mask < 0.9);
        // The release disarms and hides.
        assert_eq!((s.disarms(), s.hides()), (1, 1));
        assert_eq!(s.g.phase(), Phase::Idle);
    }

    #[test]
    fn a_quick_tap_is_left_completely_alone() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true).key(0.12, LALT, false);
        s.tick_to(2.0);
        assert!(s.untouched());
        assert_eq!(s.asks(), 0, "not even a lookup");
        assert_eq!(s.disarms(), 0);
        assert_eq!(s.g.phase(), Phase::Idle);
    }

    #[test]
    fn a_slow_hold_released_before_the_lookup_answers_is_left_alone() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true);
        s.tick_to(0.31);
        assert_eq!(s.asks(), 1);
        s.key(0.33, LALT, false);
        s.answer(0.34, true);
        assert!(s.untouched());
        assert_eq!(s.g.phase(), Phase::Idle);
    }

    #[test]
    fn alt_tab_and_alt_f4_and_access_keys_are_not_disturbed() {
        for other in [TAB, F4, KEY_F] {
            // Quick: no lookup, nothing.
            let mut s = Script::new(Modifier::Alt);
            s.key(0.0, LALT, true)
                .key(0.1, other, true)
                .key(0.15, other, false);
            s.tick_to(1.0);
            assert!(s.untouched(), "{other:#x}");
            assert_eq!(s.asks(), 0);
            assert_eq!(s.g.phase(), Phase::Cancelled);
            s.key(1.1, LALT, false);
            assert_eq!(s.g.phase(), Phase::Idle);

            // After the pill showed (Alt held over the desktop, then Tab): the pill goes,
            // the gate is disarmed, nothing else happens; Alt stays down for the system.
            let mut s = Script::showing(Modifier::Alt);
            s.key(0.7, other, true);
            assert_eq!(s.g.phase(), Phase::Cancelled);
            assert_eq!((s.disarms(), s.hides(), s.summons()), (1, 1, 0));
            // More Tabs: nothing new.
            s.key(0.75, other, false)
                .key(0.8, other, true)
                .key(0.85, other, false);
            s.key(0.9, LALT, false);
            assert_eq!((s.arms(), s.masks(), s.summons()), (1, 1, 0));
            assert_eq!(s.g.phase(), Phase::Idle);
        }
    }

    #[test]
    fn auto_repeat_of_the_modifier_does_not_end_the_hold() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true);
        // The hook drops repeats, but the machine is safe against them too.
        let epoch = s.epoch;
        s.at(
            0.2,
            Input::Key(KeyInput {
                vk: LALT,
                down: true,
                altgr: false,
                epoch,
                time: 0.2,
            }),
        );
        assert_eq!(s.g.phase(), Phase::Held);
    }

    #[test]
    fn alt_with_a_mouse_button_already_down_is_an_alt_drag() {
        let mut s = Script::new(Modifier::Alt);
        s.press(0.0, false).key(0.1, LALT, true);
        s.tick_to(1.0);
        assert!(s.untouched());
        assert_eq!(s.g.phase(), Phase::Cancelled);
        s.release(1.1).key(1.2, LALT, false);
        assert_eq!(s.g.phase(), Phase::Idle);
        // The driver's resync of the buttons does the same when the feed lost the press.
        let mut s = Script::new(Modifier::Alt);
        s.at(0.0, Input::Buttons(button_bit(Button::Left)));
        s.key(0.0, LALT, true);
        s.tick_to(1.0);
        assert!(s.untouched());
    }

    #[test]
    fn a_click_during_the_hold_before_the_pill_is_the_apps() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true).press(0.1, false);
        s.tick_to(1.0);
        assert!(s.untouched());
        assert_eq!(s.g.phase(), Phase::Cancelled);
    }

    #[test]
    fn altgr_never_starts_the_gesture() {
        for modifier in [Modifier::Alt, Modifier::CtrlAlt] {
            let mut s = Script::new(modifier);
            // AltGr: fake left Ctrl (scan 0x21D), then right Alt.
            s.altgr(0.0, true).key(0.0, RALT, true);
            s.tick_to(2.0);
            assert!(s.untouched(), "{modifier:?}");
            assert_eq!(s.asks(), 0);
            s.altgr(2.1, false).key(2.1, RALT, false);
            assert_eq!(s.g.phase(), Phase::Idle);
        }
        assert!(is_altgr_ctrl(0xA2, 0x21D));
        assert!(!is_altgr_ctrl(0xA2, 0x1D));
        assert!(!is_altgr_ctrl(0xA3, 0x21D));
    }

    #[test]
    fn right_alt_alone_works_on_layouts_without_altgr() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, RALT, true);
        s.tick_to(0.35);
        s.answer(0.36, true);
        assert_eq!(s.g.phase(), Phase::Showing);
    }

    #[test]
    fn another_key_during_the_hold_cancels() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true).key(0.2, 0x10 + 0x90, true); // left Shift
        s.tick_to(1.0);
        assert!(s.untouched());
        // Released and held again: works again.
        s.key(1.0, 0xA0, false).key(1.0, LALT, false);
        s.key(2.0, LALT, true);
        s.tick_to(2.35);
        s.answer(2.36, true);
        assert_eq!(s.g.phase(), Phase::Showing);
    }

    #[test]
    fn leaving_empty_space_hides_the_pill_and_a_click_then_is_not_taken() {
        let mut s = Script::showing(Modifier::Alt);
        // Move over a text field: a re-check after the gap says not empty.
        s.moved(0.40, 300, 300);
        s.tick_to(0.50);
        assert_eq!(s.asks(), 2);
        s.answer(0.52, false);
        assert_eq!(s.g.phase(), Phase::Looking);
        assert_eq!((s.disarms(), s.hides()), (1, 1));
        // A click now reaches us through the feed (the gate was disarmed, so the hook let
        // it through to the app): it uses up the hold and makes nothing.
        s.press(0.6, false);
        assert_eq!(s.summons(), 0);
        assert_eq!(s.g.phase(), Phase::Cancelled);
        assert_eq!(s.arms(), 1);
    }

    #[test]
    fn back_on_empty_space_the_pill_returns_without_a_second_mask() {
        let mut s = Script::showing(Modifier::Alt);
        s.moved(0.40, 300, 300);
        s.tick_to(0.50);
        s.answer(0.52, false);
        s.moved(0.60, 100, 100);
        s.tick_to(0.70);
        s.answer(0.71, true);
        assert_eq!(s.g.phase(), Phase::Showing);
        assert_eq!((s.arms(), s.shows(), s.masks()), (2, 2, 1));
    }

    #[test]
    fn rechecks_are_throttled_and_still_cursors_are_not_rechecked() {
        let mut s = Script::showing(Modifier::Alt);
        for i in 0..40 {
            s.moved(0.37 + f64::from(i) * 0.005, 100 + i * 10, 100);
            if let Some(ask) = s.g.pending {
                s.at(
                    0.37 + f64::from(i) * 0.005 + 0.001,
                    Input::Answer { ask, empty: true },
                );
            }
        }
        // 0.2 s of moves at 200 Hz: at most one check per RECHECK_GAP, plus the first.
        assert!(
            s.asks() <= 1 + (0.2 / RECHECK_GAP).ceil() as usize + 1,
            "{}",
            s.asks()
        );
        let asks = s.asks();
        s.tick_to(3.0);
        assert!(s.asks() <= asks + 1);
        let settled = s.asks();
        s.tick_to(6.0);
        assert_eq!(s.asks(), settled, "no checks while the cursor is still");
    }

    #[test]
    fn over_a_text_field_no_pill() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true);
        s.tick_to(0.35);
        s.answer(0.36, false);
        assert_eq!(s.g.phase(), Phase::Looking);
        s.tick_to(2.0);
        assert!(s.untouched());
        s.key(2.0, LALT, false);
        assert_eq!(s.g.phase(), Phase::Idle);
    }

    #[test]
    fn a_lookup_timeout_means_no_pill() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true);
        s.tick_to(0.35);
        // The worker answers TimedOut at its deadline: the driver reports "not empty".
        s.answer(0.5, false);
        s.tick_to(2.0);
        assert!(s.untouched());
        // A lost answer is given up on, and a move asks again.
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true);
        s.tick_to(0.35);
        s.tick_to(0.35 + ASK_GIVE_UP + 0.01);
        s.moved(1.5, 200, 200);
        assert_eq!(s.asks(), 2);
    }

    #[test]
    fn blocked_by_a_paste_or_a_capture() {
        // Blocked before the hold: no lookup at all.
        let mut s = Script::new(Modifier::Alt);
        s.at(0.0, Input::Blocked(true));
        s.key(0.1, LALT, true);
        s.tick_to(1.0);
        assert!(s.untouched());
        assert_eq!(s.asks(), 0);
        // Blocked during the hold, before the delay.
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true).at(0.1, Input::Blocked(true));
        s.tick_to(1.0);
        assert_eq!(s.asks(), 0);
        // Blocked while the pill shows: it goes and the gate is disarmed.
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::Blocked(true));
        assert_eq!((s.disarms(), s.hides()), (1, 1));
        assert_eq!(s.g.phase(), Phase::Cancelled);
        // Unblocked: the next hold works.
        s.at(0.6, Input::Blocked(false))
            .key(0.7, LALT, false)
            .key(1.0, LALT, true);
        s.tick_to(1.35);
        s.answer(1.36, true);
        assert_eq!(s.g.phase(), Phase::Showing);
    }

    #[test]
    fn disabled_means_no_gesture() {
        let mut s = Script::new(Modifier::Alt);
        s.at(0.0, Input::Enabled(false)).key(0.1, LALT, true);
        s.tick_to(1.0);
        assert!(s.untouched());
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::Enabled(false));
        assert_eq!((s.disarms(), s.hides()), (1, 1));
    }

    #[test]
    fn pen_and_touch_presses_never_summon() {
        // The gate does not take them (hook tests); they reach the feed and use up the hold.
        let mut s = Script::showing(Modifier::Alt);
        s.press(0.5, true);
        assert_eq!(s.summons(), 0);
        assert_eq!(s.g.phase(), Phase::Cancelled);
        assert_eq!((s.disarms(), s.hides()), (1, 1));
    }

    #[test]
    fn a_refused_arm_shows_nothing() {
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true);
        s.tick_to(0.35);
        s.answer(0.36, true);
        s.at(0.36, Input::ArmFailed);
        assert_eq!(s.g.phase(), Phase::Cancelled);
    }

    #[test]
    fn the_hook_restarting_mid_gesture_cancels_until_release() {
        let mut s = Script::showing(Modifier::Alt);
        let held = Keys {
            left_alt: true,
            ..Keys::default()
        };
        s.at(0.5, Input::KeysReset(held));
        assert_eq!(s.g.phase(), Phase::Cancelled);
        assert_eq!((s.disarms(), s.hides()), (1, 1));
        // The release the new hook sees ends it.
        s.key(0.9, LALT, false);
        assert_eq!(s.g.phase(), Phase::Idle);
        // Restarted while nothing is held: idle at once.
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::KeysReset(Keys::default()));
        assert_eq!(s.g.phase(), Phase::Idle);
    }

    #[test]
    fn quit_mid_gesture_disarms_and_hides_at_once() {
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::Shutdown);
        let last: Vec<Action> = s.actions.iter().rev().take(2).map(|(_, a)| *a).collect();
        assert!(last.contains(&Action::Disarm));
        assert!(last.contains(&Action::Hide { animate: false }));
        assert_eq!(s.summons(), 0);
        assert_eq!(s.masks(), 1, "nothing new is injected on the way out");
        // Even idle, shutdown disarms (belt and braces).
        let mut s = Script::new(Modifier::Alt);
        s.at(0.0, Input::Shutdown);
        assert_eq!(s.disarms(), 1);
    }

    #[test]
    fn ctrl_alt_mode() {
        // Either order of the two keys starts it; no mask key is needed or sent.
        let mut s = Script::showing(Modifier::CtrlAlt);
        assert_eq!((s.arms(), s.masks(), s.shows()), (1, 0, 1));
        s.at(0.5, Input::ClickTaken { x: 1, y: 2 });
        assert_eq!(s.summons(), 1);
        s.key(0.6, LALT, false);
        assert_eq!(s.g.phase(), Phase::Clicked, "Ctrl still down");
        s.key(0.7, LCTRL, false);
        assert_eq!(s.g.phase(), Phase::Cooldown);

        let mut s = Script::new(Modifier::CtrlAlt);
        s.key(0.0, LALT, true).key(0.05, LCTRL, true);
        s.tick_to(0.4);
        assert_eq!(s.asks(), 1);

        // Alt alone does nothing in Ctrl+Alt mode.
        let mut s = Script::new(Modifier::CtrlAlt);
        s.key(0.0, LALT, true);
        s.tick_to(1.0);
        assert_eq!(s.asks(), 0);

        // And Ctrl+Alt does nothing in Alt mode.
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LCTRL, true).key(0.0, LALT, true);
        s.tick_to(1.0);
        assert_eq!(s.asks(), 0);
        // Not even when Ctrl is let go and Alt stays: that release is not a fresh hold.
        s.key(1.0, LCTRL, false);
        s.tick_to(2.0);
        assert_eq!(s.asks(), 0);
    }

    #[test]
    fn changing_the_modifier_mid_hold_cancels() {
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::Modifier(Modifier::CtrlAlt));
        assert_eq!(s.g.phase(), Phase::Cancelled);
        assert_eq!((s.disarms(), s.hides()), (1, 1));
    }

    #[test]
    fn a_click_taken_after_the_release_still_makes_its_note() {
        // The hook took the click just before Alt went up, but the summoner saw the key
        // first (different threads): the click is gone, so the note is made.
        let mut s = Script::showing(Modifier::Alt);
        s.key(0.5, LALT, false);
        s.at(0.5, Input::ClickTaken { x: 3, y: 4 });
        assert_eq!(s.summons(), 1);
        assert_eq!(s.g.phase(), Phase::Cooldown);
    }

    #[test]
    fn a_new_hold_in_the_cooldown_is_ignored_until_released() {
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::ClickTaken { x: 3, y: 4 });
        s.key(0.6, LALT, false).key(0.7, LALT, true);
        s.tick_to(1.5);
        assert_eq!(s.g.phase(), Phase::Cancelled);
        assert_eq!(s.asks(), 1);
    }

    #[test]
    fn moves_matter_only_during_a_hold() {
        let mut s = Script::new(Modifier::Alt);
        assert!(!s.g.wants_moves());
        s.key(0.0, LALT, true);
        assert!(s.g.wants_moves());
        s.key(0.1, LALT, false);
        assert!(!s.g.wants_moves());
    }

    // --- The pill ---

    #[test]
    fn spring_starts_at_zero_overshoots_a_little_and_settles() {
        assert_eq!(spring(0.0), 0.0);
        assert_eq!(spring(APPEAR), 1.0);
        let peak = (1..36)
            .map(|i| spring(f64::from(i) * 0.01))
            .fold(0.0, f64::max);
        assert!(peak > 1.0 && peak < 1.08, "{peak}");
        assert!((spring(APPEAR - 0.001) - 1.0).abs() < 0.01);
    }

    #[test]
    fn motion_runs_only_while_it_changes_and_is_instant_with_reduced_motion() {
        let mut m = PillMotion::hidden();
        assert!(!m.moving(0.0));
        m.appear(1.0, true);
        assert!(m.moving(1.1));
        assert!(!m.moving(1.0 + APPEAR + 1e-9));
        assert_eq!(m.value(1.0 + APPEAR + 1e-9), 1.0);
        m.disappear(2.0, true);
        assert!(m.moving(2.05));
        assert_eq!(m.value(2.0 + DISAPPEAR + 1e-9), 0.0);
        assert!(!m.moving(2.0 + DISAPPEAR + 1e-9));
        // Reduced motion: no clock at all.
        let mut m = PillMotion::hidden();
        m.appear(1.0, false);
        assert!(!m.moving(1.0));
        assert_eq!(m.value(1.0), 1.0);
        m.disappear(1.5, false);
        assert_eq!(m.value(1.5), 0.0);
        // Hiding half way through appearing shrinks from where it was.
        let mut m = PillMotion::hidden();
        m.appear(0.0, true);
        let mid = m.value(0.05);
        m.disappear(0.05, true);
        assert!((m.value(0.05) - mid.clamp(0.0, 1.0)).abs() < 1e-9);
    }

    #[test]
    fn the_pill_is_centred_and_fits_its_window() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let (w, h) = pill_window_size(scale);
            assert_eq!((w % 2, h % 2), (0, 0));
            let mut pixmap = Pixmap::new(w, h).unwrap();
            // At the spring's peak the plate still leaves a clear edge.
            render_pill(&mut pixmap, 1.06, scale, false);
            let alpha = |x: u32, y: u32| pixmap.pixel(x, y).unwrap().alpha();
            assert_eq!(alpha(0, 0), 0);
            assert_eq!(alpha(w - 1, h - 1), 0);
            assert!(alpha(w / 2, h / 2) > 200);
            // Symmetric left to right around the hot spot.
            assert_eq!(alpha(w / 2 - 10, h / 2), alpha(w / 2 + 9, h / 2));
        }
        let mut pixmap = Pixmap::new(80, 60).unwrap();
        render_pill(&mut pixmap, 0.0, 1.0, true);
        assert!(pixmap.pixels().iter().all(|p| p.alpha() == 0));
    }

    #[test]
    fn modifier_flag_parses() {
        assert_eq!(Modifier::parse("alt"), Some(Modifier::Alt));
        assert_eq!(Modifier::parse("Ctrl-Alt"), Some(Modifier::CtrlAlt));
        assert_eq!(Modifier::parse("shift"), None);
    }
}
