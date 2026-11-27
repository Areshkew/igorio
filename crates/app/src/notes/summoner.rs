//! The tool menu at the cursor: hold Alt on its own over empty space (the desktop, a page
//! margin, a window background) and a small menu of tools opens under the cursor (New note,
//! Record screen; [`TOOLS`]). Its first row sits under the cursor, so "hold Alt, click" still
//! makes a note in one click; moving to another row and clicking runs that tool. The click is
//! swallowed, so the app underneath never sees an Alt+click. Releasing Alt closes the menu.
//! It replaces the summon pill of `NoteSummoner.swift` (the gesture is the same; the direct
//! action is now a menu, `ROADMAP.md` "Tool menu at the cursor").
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
//! 3. Inject the mask key while Alt is still held, at the moment the menu opens, and
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
//!   left down by this module in any path: not when the menu shows and Alt is released, not
//!   when Alt is released during a click, not on a focus change, not when Windows removes
//!   the hook, not when the app quits or is killed. There is no key navigation in the menu:
//!   Alt is held, so letters and arrows belong to the foreground app (access keys).
//! - **Injected: one `0xE8` down and up, in one `SendInput` call** (the system inserts the
//!   pair as one unit, nothing in between), with [`INJECTED_MARKER`], once per hold, only in
//!   Alt mode, only when the menu first opens in that hold, and only if right before the
//!   call Alt is still down, Ctrl, Shift and Win are up, and no key changed since the hold
//!   began (rule 7). The app's own hooks ignore it by the marker. A quick tap, a hold that
//!   never opens the menu (or offers no tool), Alt+Tab, Alt+F4, Alt+letter and Alt+drag
//!   never inject anything. If the menu shows over the desktop and the user then presses
//!   Tab, Alt+Tab still works: Alt is still down.
//! - **Mouse: one press and its release**, of the primary button, and only while the menu
//!   shows: the summoner arms `hook::CLICK_GATE` when the menu opens and disarms it when it
//!   closes. The app's single mouse hook decides in its callback from that one atomic
//!   ([`crate::hook::ClickGate::on_press`]). The gate knows no rectangle, so while it is
//!   armed it takes the next primary press wherever it lands: on a row it runs that tool,
//!   anywhere else it only closes the menu (see "Clicks outside the menu" below). The
//!   keyboard hook disarms the gate on every key change, on its own thread, before Windows
//!   passes on the next input, so a click right after Alt is released is never swallowed
//!   even if the summoner thread is late. Pen and touch presses, injected presses and other
//!   buttons are never taken (they disarm). An arm based on an older key state is refused
//!   (the gate's epoch).
//!
//! The mask is sent at open time, not at release, so when the user lets go without
//! clicking nothing more happens; the release is an ordinary Alt release. When the menu
//! never opened (the hold was over a text field, or shorter than [`HOLD_DELAY`]), the
//! release is left alone and a lone Alt activates the menu bar as usual: the user may have
//! meant that. Holding Alt over a foreground window of an elevated app: Windows does not call
//! our keyboard hook then, so the gesture simply does not start.
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
//! # When the menu opens (the rules of `NoteSummoner.swift`)
//!
//! - The modifier went down on its own: no other key, no mouse button down (an Alt+drag
//!   already under way), not AltGr. In Ctrl+Alt mode, the hold starts when the second of the
//!   two goes down.
//! - It was held for [`HOLD_DELAY`] (0.3 s), so Alt shortcuts and quick taps never flash
//!   the menu. Any other key or button during the hold uses the hold up
//!   ([`Phase::Cancelled`]): no menu until the modifier is released and pressed again.
//! - The feature is enabled and nothing blocks it ([`Summoner::set_blocked`]: a paste
//!   delivery, a screenshot capture, a recording, the app paused).
//! - A UI Automation lookup at the cursor says the spot is empty
//!   ([`super::context::is_empty_space`]). The lookup runs once the hold delay has passed,
//!   not on every move.
//! - At least one tool is offered ([`offered`], from the [`ToolContext`] the app passes in
//!   and the kind of spot the lookup found). No tool: the menu does not open, nothing is
//!   armed or injected, and the hold is used up.
//!
//! # While the menu shows
//!
//! - It stays where it opened. The pointer over the menu keeps it open: the menu is our own
//!   window, which the empty-space rule would call "not empty", so no lookup runs while the
//!   pointer is inside the menu's card. Outside the card the spot is checked again at most
//!   every [`RECHECK_GAP`]: over empty space the menu stays; over anything else (text, a
//!   control) the menu closes and the gate is disarmed, so a click there is the app's. Back
//!   on empty space the menu opens again at the new spot (no second mask key). A lookup
//!   that times out means "not empty".
//! - The row under the pointer is highlighted.
//! - A click on a row runs that tool ([`SummonerEvent::ToolChosen`]); the point given is
//!   where the menu opened. A click outside every row closes the menu with no tool.
//! - Releasing the modifier, any other key, a button the gate did not take, being blocked or
//!   disabled, a hook restart and quitting close it.
//!
//! # Clicks outside the menu
//!
//! The gate is decided in the mouse hook from one atomic and knows nothing of the menu's
//! position, so today a primary click anywhere while the menu shows is swallowed; outside the
//! rows it only closes the menu. To let such a click reach the app, `hook.rs` would need the
//! armed rectangle: `ClickGate` gets two more `AtomicU64` fields with the menu card's screen
//! rectangle (left and top packed as two `i32` in one, right and bottom in the other), stored
//! by a new `arm_in(epoch, button, rect)` before its `state` compare-and-swap; `on_press`
//! gains the point and, after the compare-and-swap that takes the gate (unchanged, so an
//! outside press still disarms it), returns false when the point is outside the rectangle;
//! the hook passes `x, y`. The summoner then sees that press in its feed, where it already
//! closes the menu (the [`Gesture`]'s mouse rule), and the app gets the click.
//!
//! # Opening the menu without the gesture
//!
//! [`Summoner::open_menu_at_cursor`] (for a future shortcut) opens the same menu at the
//! cursor with no modifier held and no mask key. The gate is armed as for the gesture and is
//! armed again after each key release (a release disarms it in the keyboard hook; the
//! shortcut's own keys are released after the menu opened). Any key press, any click and
//! [`MENU_IDLE`] without the pointer over the menu close it. Esc cannot be seen without
//! focus, which the menu never takes.
//!
//! # Threads
//!
//! - The UI thread owns the [`Summoner`] and drives it with calls that never block:
//!   [`feed`](Summoner::feed) with the mouse events its own mouse hook delivers (there is
//!   still exactly one mouse hook in the process), [`set_blocked`](Summoner::set_blocked),
//!   [`set_enabled`](Summoner::set_enabled), [`apply_config`](Summoner::apply_config),
//!   [`set_tool_context`](Summoner::set_tool_context),
//!   [`set_shortcuts`](Summoner::set_shortcuts),
//!   [`summon_at_cursor`](Summoner::summon_at_cursor) for the NewNote hotkey, and
//!   [`drain`](Summoner::drain) after [`wake_handle`](Summoner::wake_handle) is signalled.
//! - The summoner thread owns the menu window and the state machine ([`Gesture`]), and asks
//!   UI Automation through its own lane of the app's one worker (`uia::Requester`). It
//!   never calls UI Automation itself and never waits on another process.
//! - The keyboard hook thread (rule 1) only pumps messages. Its callback copies the event,
//!   skips auto-repeats and our own injected keys, disarms the click gate, pushes a small
//!   event into a preallocated queue, sets an event, and always calls `CallNextHookEx`. A
//!   once-a-second check restarts it when it is gone or when the modifier keys Windows
//!   reports disagree with what it saw, twice in a row.
//!
//! # The menu window
//!
//! A layered, click-through, topmost, no-activate tool window excluded from capture
//! (`overlay::Overlay`, the window the overlay uses): it never takes focus or input, so the
//! foreground app keeps its focus and caret; the click gate, not the window, takes the click.
//! A rounded card in the style of the settings window and the notes' slash menu (drawn with
//! the editor's `Surface`, `Canvas`, fonts and palettes, light or dark with the system's app
//! mode): one row per tool with its icon, its label and its current shortcut as key chips.
//! Placed by [`menu_layout`]: the first row under the cursor, the card inside the monitor's
//! work area, opening upward near the bottom edge (the first row is then the bottom one).
//!
//! Motion runs on the animation clock only while something changes: it fades in and rises
//! [`RISE`] DIPs in [`OPEN_MS`], the highlight slides between rows in [`SLIDE_MS`], a chosen
//! row flashes for [`FLASH_MS`] and the menu then fades out in [`CLOSE_MS`]. With reduced
//! motion ("Animation effects" off) all of it is instant.
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
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Foundation::{
    COLORREF, CloseHandle, HANDLE, LPARAM, LRESULT, RECT, SIZE, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, GetTextExtentPoint32W,
    SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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
    PeekMessageW, PostThreadMessageW, QS_ALLINPUT, SM_SWAPBUTTON, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use super::context::{self, AppNames, facts_at, is_empty_space};
use super::editor::{
    GdiFonts, Palette, Surface, Transition, animation_effects, mix, px, rounded_path, system_dark,
};
use super::store::{NoteKind, NoteOrigin};
use crate::hook::{self, Button, CLICK_GATE, MouseEvent};
use crate::input::INJECTED_MARKER;
use crate::overlay::{Overlay, monitor_at};
use crate::shortcuts::{self, Shortcuts};
use crate::uia::{Lookup, Outcome, RequestId, Requester};

// ---------------------------------------------------------------------------------------
// Constants (the Swift values where there is one)
// ---------------------------------------------------------------------------------------

/// The modifier must be held this long first, so shortcuts and quick taps never flash the
/// menu (`holdDelay`).
pub const HOLD_DELAY: f64 = 0.3;
/// While the hold tracks the cursor, the spot is checked again at most this often (`0.12`
/// in `track()`).
pub const RECHECK_GAP: f64 = 0.12;
/// A move shorter than this (physical px) since the last check needs no new check.
pub const MOVE_SLOP: i32 = 3;
/// Deadline of one lookup. Longer than a frame, shorter than `uia::DEFAULT_DEADLINE`: a menu
/// that comes late is worse than none.
pub const LOOKUP_DEADLINE: Duration = Duration::from_millis(200);
/// A lookup with no answer after this long is forgotten (the worker always answers by its
/// deadline; this only guards against a lost answer).
pub const ASK_GIVE_UP: f64 = 1.0;
/// After a tool is chosen and the modifier released, a new hold is ignored this long, so a
/// fumbled double press does not run a tool twice.
pub const COOLDOWN: f64 = 0.25;
/// The NewNote hotkey's lookup: the note is made from the window facts alone if no answer
/// came after this long.
pub const HOTKEY_GIVE_UP: f64 = 0.5;
/// A menu opened by [`Summoner::open_menu_at_cursor`] closes after this long without the
/// pointer over it, seconds.
pub const MENU_IDLE: f64 = 4.0;

/// The menu, in DIPs: card width at least, corner radius, padding above and below the rows,
/// row height, and the room around the card for its shadow.
pub const MENU_MIN_WIDTH: f64 = 196.0;
pub const MENU_RADIUS: f64 = 10.0;
pub const MENU_PAD: f64 = 5.0;
pub const ROW_HEIGHT: f64 = 32.0;
pub const MENU_SHADOW: f64 = 12.0;
/// The cursor's hot spot on the first row, from the card's left edge and the row's top: in the
/// row's left gutter, so the arrow covers neither the icon nor the label.
pub const ANCHOR: (f64, f64) = (9.0, 9.0);
/// Row contents, from the card's left edge: the icon box and the label; the chips end this far
/// from the right edge.
const ROW_INSET: f64 = 5.0;
const ROW_RADIUS: f64 = 6.0;
const ICON_X: f64 = 26.0;
const ICON: f64 = 18.0;
const LABEL_X: f64 = 54.0;
const LABEL_GAP: f64 = 24.0;
const CHIPS_RIGHT: f64 = 12.0;
const CHIP_HEIGHT: f64 = 20.0;
const CHIP_PAD: f64 = 6.0;
const CHIP_GAP: f64 = 4.0;
const CHIP_RADIUS: f64 = 5.0;

/// Motion, milliseconds: open (fade in and rise), the highlight's slide, a chosen row's
/// flash, and the closing fade. The menu rises [`RISE`] DIPs as it opens.
pub const OPEN_MS: f64 = 120.0;
pub const SLIDE_MS: f64 = 120.0;
pub const FLASH_MS: f64 = 80.0;
pub const CLOSE_MS: f64 = 80.0;
pub const RISE: f64 = 4.0;

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
    /// Let screenshots see the menu (`FAIRY_CAPTURABLE=1`, for demos).
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

// ---------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------

/// A tool of the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolId {
    NewNote,
    RecordScreen,
}

impl ToolId {
    /// Short id for logs and the demo (`note`, `record`).
    pub fn id(self) -> &'static str {
        tool(self).id_text
    }

    pub fn parse(text: &str) -> Option<ToolId> {
        TOOLS
            .iter()
            .find(|t| t.id_text == text.trim())
            .map(|t| t.id)
    }
}

/// The drawn icon of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// A small note sheet with a folded corner.
    NoteSheet,
    /// A record dot.
    RecordDot,
}

/// What kind of spot the menu opens over (from the lookup that found it empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Spot {
    /// Not known (the menu opened by a shortcut, or no lookup).
    #[default]
    Unknown,
    Desktop,
    /// An app window's background.
    App,
    /// A web page's margin.
    Web,
}

impl Spot {
    pub fn from_kind(kind: NoteKind) -> Spot {
        match kind {
            NoteKind::Desktop => Spot::Desktop,
            NoteKind::App => Spot::App,
            NoteKind::Web => Spot::Web,
        }
    }
}

/// What the tools' conditions look at. The app passes it in
/// ([`Summoner::set_tool_context`]); `spot` is filled in by the summoner from its lookup.
/// Later context-aware tools add the facts they need here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolContext {
    /// The notes capability is on.
    pub notes: bool,
    /// Screen recording is built ([`shortcuts::Action::available`]).
    pub recording_available: bool,
    /// Screen recording is switched on in the settings.
    pub recording_enabled: bool,
    /// A recording is running now.
    pub recording: bool,
    /// The user carries cards.
    pub cards_held: bool,
    pub spot: Spot,
}

/// One entry of the menu.
#[derive(Debug, Clone, Copy)]
pub struct Tool {
    pub id: ToolId,
    id_text: &'static str,
    pub label: &'static str,
    pub icon: Icon,
    /// The shortcut action of the same thing, for the key chips.
    pub action: shortcuts::Action,
    /// Whether the tool is offered now.
    pub offered: fn(&ToolContext) -> bool,
}

/// Every tool, in menu order; the first offered one sits under the cursor. Only tools go
/// here (restore, discard, settings and recent notes stay in the tray menu). This list is
/// where later, context-aware tools go (an idea for later, not built: a grammar check offered
/// while the user types in a text field): an entry whose `offered` reads the facts it needs
/// from [`ToolContext`], with those facts added there.
pub const TOOLS: &[Tool] = &[
    Tool {
        id: ToolId::NewNote,
        id_text: "note",
        label: "New note",
        icon: Icon::NoteSheet,
        action: shortcuts::Action::NewNote,
        offered: |c| c.notes,
    },
    Tool {
        id: ToolId::RecordScreen,
        id_text: "record",
        label: "Record screen",
        icon: Icon::RecordDot,
        action: shortcuts::Action::ToggleRecording,
        offered: |c| c.recording_available && c.recording_enabled && !c.recording,
    },
];

pub fn tool(id: ToolId) -> &'static Tool {
    TOOLS
        .iter()
        .find(|t| t.id == id)
        .expect("every ToolId is in TOOLS")
}

/// The tools offered in this context, in menu order.
pub fn offered(context: &ToolContext) -> Vec<ToolId> {
    TOOLS
        .iter()
        .filter(|t| (t.offered)(context))
        .map(|t| t.id)
        .collect()
}

/// The key chips of a tool's current shortcut (none when it has none).
pub fn tool_chips(id: ToolId, shortcuts: &Shortcuts) -> Vec<String> {
    shortcuts
        .get(tool(id).action)
        .map(|chord| chord.chips())
        .unwrap_or_default()
}

/// A tool was chosen in the menu (or the NewNote hotkey asked for a note).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolChosen {
    pub tool: ToolId,
    /// Physical pixels, virtual-screen coordinates: where the menu opened (or the cursor for
    /// the hotkey). For a note, its place.
    pub point: (i32, i32),
    /// What the spot belongs to, for a note.
    pub origin: NoteOrigin,
    /// The clock of `hook::now`.
    pub time: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SummonerEvent {
    ToolChosen(ToolChosen),
    /// A debug line (only with `SummonConfig::debug`).
    Log(String),
}

// ---------------------------------------------------------------------------------------
// Where the menu goes (pure)
// ---------------------------------------------------------------------------------------

/// A rectangle in physical pixels, right and bottom exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Bounds {
    pub fn contains(&self, (x, y): (i32, i32)) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn inside(&self, outer: &Bounds) -> bool {
        self.left >= outer.left
            && self.top >= outer.top
            && self.right <= outer.right
            && self.bottom <= outer.bottom
    }
}

/// Where the menu is, in physical pixels on the virtual screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuLayout {
    /// The card (the rows and their padding).
    pub card: Bounds,
    /// The window: the card and room for its shadow.
    pub window: Bounds,
    pub rows: usize,
    pub row_height: i32,
    pub pad: i32,
    /// Opened upward (near the bottom edge): the first tool is the bottom row.
    pub upward: bool,
    pub scale: f64,
}

impl MenuLayout {
    /// Top to bottom position of tool `index`.
    fn visual(&self, index: usize) -> usize {
        if self.upward {
            self.rows - 1 - index
        } else {
            index
        }
    }

    /// The rectangle of tool `index`'s row (full card width).
    pub fn row(&self, index: usize) -> Bounds {
        let top = self.card.top + self.pad + self.visual(index) as i32 * self.row_height;
        Bounds {
            left: self.card.left,
            top,
            right: self.card.right,
            bottom: top + self.row_height,
        }
    }

    /// The tool under a point: anywhere on the card counts, the padding above and below
    /// belongs to the nearest row.
    pub fn row_at(&self, point: (i32, i32)) -> Option<usize> {
        if self.rows == 0 || !self.card.contains(point) {
            return None;
        }
        let visual = ((point.1 - self.card.top - self.pad).div_euclid(self.row_height.max(1)))
            .clamp(0, self.rows as i32 - 1) as usize;
        Some(self.visual(visual))
    }

    pub fn contains(&self, point: (i32, i32)) -> bool {
        self.card.contains(point)
    }
}

/// Places a menu of `rows` rows and `width` DIPs at `cursor` on a monitor with this work area
/// and DPI scale: the first row under the cursor (the hot spot at [`ANCHOR`] on it), the card
/// inside the work area. When it does not fit below, it opens upward: the first tool is then
/// the bottom row, still under the cursor. Pushed back from an edge, the card still covers
/// the cursor (the padding counts as the first row).
pub fn menu_layout(
    cursor: (i32, i32),
    rows: usize,
    width: f64,
    work: Bounds,
    scale: f64,
) -> MenuLayout {
    let dpi = (scale * 96.0).round() as u32;
    let row_height = px(ROW_HEIGHT, dpi).max(1);
    let pad = px(MENU_PAD, dpi);
    let shadow = px(MENU_SHADOW, dpi);
    let card_w = px(width.max(MENU_MIN_WIDTH), dpi);
    let card_h = 2 * pad + row_height * rows.max(1) as i32;
    let (ax, ay) = (px(ANCHOR.0, dpi), px(ANCHOR.1, dpi));
    let mut top = cursor.1 - ay - pad;
    let upward = top + card_h > work.bottom;
    if upward {
        top = cursor.1 - ay - pad - row_height * (rows.max(1) as i32 - 1);
    }
    let top = top.min(work.bottom - card_h).max(work.top);
    let left = (cursor.0 - ax).min(work.right - card_w).max(work.left);
    let card = Bounds {
        left,
        top,
        right: left + card_w,
        bottom: top + card_h,
    };
    MenuLayout {
        card,
        window: Bounds {
            left: card.left - shadow,
            top: card.top - shadow,
            right: card.right + shadow,
            bottom: card.bottom + shadow,
        },
        rows,
        row_height,
        pad,
        upward,
        scale,
    }
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
    /// The delay passed; waiting for a lookup to say the spot is empty (no menu).
    Looking,
    /// The menu shows; the click gate is armed.
    Showing,
    /// A tool was chosen; this hold is used up until the modifier is released.
    Clicked,
    /// This hold is used up (another key, a button, blocked, no tool, a click outside the
    /// rows); waiting for the release.
    Cancelled,
    /// A tool was chosen and the modifier released; a new hold waits [`COOLDOWN`].
    Cooldown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    Key(KeyInput),
    Mouse(MouseEvent),
    /// The mouse buttons down now (`GetAsyncKeyState`), read before a hold may start, so a
    /// lost release in the feed cannot block the gesture for good.
    Buttons(u8),
    /// The answer to [`Action::Ask`]: is the spot empty, and what kind of spot it is.
    Answer {
        ask: u64,
        empty: bool,
        spot: Spot,
    },
    /// The click gate swallowed a press here.
    ClickTaken {
        x: i32,
        y: i32,
    },
    Blocked(bool),
    Enabled(bool),
    Modifier(Modifier),
    /// What the tools' conditions look at (`spot` is ignored: the lookup fills it).
    Context(ToolContext),
    /// The driver placed the menu that [`Action::Open`] asked for.
    Placed(MenuLayout),
    /// Open the menu at `point` without the gesture ([`Summoner::open_menu_at_cursor`]);
    /// `epoch` is the gate's epoch now.
    OpenMenu {
        point: (i32, i32),
        epoch: u64,
    },
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
    /// Arm the click gate for the primary button, with this epoch.
    Arm {
        epoch: u64,
    },
    /// Inject the mask key (Alt mode), if the hold is still exactly as at `epoch`.
    Mask {
        epoch: u64,
    },
    /// Open the menu at `point` with [`Gesture::tools`]; answer with [`Input::Placed`].
    Open {
        point: (i32, i32),
    },
    /// Disarm the click gate (keeping its epoch, so the same hold may arm it again).
    Disarm,
    Close {
        animate: bool,
    },
    /// Run `tool` (the menu's row `row`) for the spot at `point`; `ask` is the lookup that
    /// found the spot.
    Choose {
        tool: ToolId,
        row: usize,
        point: (i32, i32),
        ask: Option<u64>,
    },
}

/// The menu as the gesture knows it.
#[derive(Debug, Clone, PartialEq)]
struct Menu {
    tools: Vec<ToolId>,
    /// Where it opened (the cursor then).
    anchor: (i32, i32),
    layout: Option<MenuLayout>,
    /// Opened by [`Input::OpenMenu`], not by a hold.
    free: bool,
    /// A free menu: when the pointer was last over it (or it opened).
    seen: f64,
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
    context: ToolContext,
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
    /// The latest lookup that found the spot empty (any answer for a free menu).
    empty_ask: Option<u64>,
    /// The mask key was requested in this hold.
    masked: bool,
    /// The menu that shows, or showed last (a click the gate took just before the menu
    /// closed still lands on it).
    menu: Option<Menu>,
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
            context: ToolContext::default(),
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
            menu: None,
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

    /// The tools of the menu that shows (or showed last).
    pub fn tools(&self) -> &[ToolId] {
        self.menu.as_ref().map_or(&[], |menu| &menu.tools)
    }

    /// The menu shows and was opened without the gesture.
    pub fn is_free(&self) -> bool {
        self.phase == Phase::Showing && self.menu.as_ref().is_some_and(|menu| menu.free)
    }

    /// Whether mouse moves matter now (the hold tracks the cursor).
    pub fn wants_moves(&self) -> bool {
        matches!(self.phase, Phase::Held | Phase::Looking | Phase::Showing)
    }

    /// When to send the next [`Input::Tick`], if anything waits on time.
    pub fn next_deadline(&self) -> Option<f64> {
        match self.phase {
            Phase::Held => Some(self.since + HOLD_DELAY),
            Phase::Showing if self.is_free() => self.menu.as_ref().map(|m| m.seen + MENU_IDLE),
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
            Input::Answer { ask, empty, spot } => self.answer(ask, empty, spot, now, &mut out),
            Input::ClickTaken { x, y } => self.click_taken((x, y), now, &mut out),
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
            Input::Context(context) => self.context = context,
            Input::Placed(layout) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.layout = Some(layout);
                }
            }
            Input::OpenMenu { point, epoch } => self.open_free(point, epoch, now, &mut out),
            Input::ArmFailed => {
                if self.phase == Phase::Showing {
                    self.phase = Phase::Cancelled;
                    out.push(Action::Close { animate: false });
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
                    out.push(Action::Close { animate: false });
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

    /// The pointer is over the menu's card.
    fn inside(&self) -> bool {
        self.phase == Phase::Showing
            && self
                .menu
                .as_ref()
                .and_then(|menu| menu.layout)
                .is_some_and(|layout| layout.contains(self.cursor))
    }

    /// Ends a hold that is used up: closes the menu and disarms; then waits for the release.
    fn spoil(&mut self, out: &mut Vec<Action>, animate: bool) {
        if !self.in_hold() {
            return;
        }
        if self.phase == Phase::Showing {
            out.push(Action::Disarm);
            out.push(Action::Close { animate });
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
            Phase::Showing if self.is_free() && !key.down => {
                // A free menu stays through releases (the shortcut's own keys); the release
                // disarmed the gate in the hook, so arm it again with the new epoch.
                self.epoch = key.epoch;
                out.push(Action::Arm { epoch: key.epoch });
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
                if self.is_free() {
                    if self.inside()
                        && let Some(menu) = self.menu.as_mut()
                    {
                        menu.seen = now;
                    }
                } else if self.inside() {
                    // Over the menu: our own window, never "empty"; no lookup.
                    self.dirty = false;
                } else if matches!(self.phase, Phase::Looking | Phase::Showing) {
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
                // (an Alt+click, a pen or touch press, a click after the menu closed).
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

    fn click_taken(&mut self, point: (i32, i32), now: f64, out: &mut Vec<Action>) {
        // The gate took a press while the menu showed: the click is gone either way. On a
        // row it runs that tool; anywhere else it only closes the menu.
        let showing = self.phase == Phase::Showing;
        let chosen = self.menu.as_ref().and_then(|menu| {
            let row = menu.layout?.row_at(point)?;
            Some((*menu.tools.get(row)?, row, menu.anchor))
        });
        if let Some((tool, row, anchor)) = chosen {
            out.push(Action::Choose {
                tool,
                row,
                point: anchor,
                ask: self.empty_ask,
            });
        }
        if showing {
            out.push(Action::Close { animate: true });
        }
        self.pending = None;
        self.phase = if chosen.is_some() {
            Phase::Clicked
        } else {
            Phase::Cancelled
        };
        self.after_hold(now);
    }

    fn answer(&mut self, ask: u64, empty: bool, spot: Spot, now: f64, out: &mut Vec<Action>) {
        if self.pending != Some(ask) {
            return;
        }
        self.pending = None;
        if self.is_free() {
            // A free menu asked only for the origin of a note.
            self.empty_ask = Some(ask);
            return;
        }
        match self.phase {
            Phase::Looking if empty => self.open(ask, spot, now, out),
            Phase::Showing if empty => self.empty_ask = Some(ask),
            Phase::Showing if self.inside() => {}
            Phase::Showing => {
                // The cursor left empty space: the menu closes, and a click now is the app's.
                out.push(Action::Disarm);
                out.push(Action::Close { animate: true });
                self.phase = Phase::Looking;
            }
            _ => {}
        }
        if matches!(self.phase, Phase::Looking | Phase::Showing) {
            self.maybe_ask(now, out);
        }
    }

    /// The spot is empty: open the menu with the tools offered here, or use the hold up when
    /// there is none.
    fn open(&mut self, ask: u64, spot: Spot, now: f64, out: &mut Vec<Action>) {
        let tools = offered(&ToolContext {
            spot,
            ..self.context
        });
        if tools.is_empty() {
            self.phase = Phase::Cancelled;
            self.after_hold(now);
            return;
        }
        self.empty_ask = Some(ask);
        out.push(Action::Arm { epoch: self.epoch });
        if self.modifier == Modifier::Alt && !self.masked {
            self.masked = true;
            out.push(Action::Mask { epoch: self.epoch });
        }
        out.push(Action::Open { point: self.cursor });
        self.menu = Some(Menu {
            tools,
            anchor: self.cursor,
            layout: None,
            free: false,
            seen: now,
        });
        self.phase = Phase::Showing;
    }

    fn open_free(&mut self, point: (i32, i32), epoch: u64, now: f64, out: &mut Vec<Action>) {
        if self.in_hold() || self.blocked || !self.enabled {
            return;
        }
        let tools = offered(&self.context);
        if tools.is_empty() {
            return;
        }
        self.cursor = point;
        self.epoch = epoch;
        self.masked = true; // no modifier is held: never a mask key
        self.empty_ask = None;
        out.push(Action::Arm { epoch });
        out.push(Action::Open { point });
        self.menu = Some(Menu {
            tools,
            anchor: point,
            layout: None,
            free: true,
            seen: now,
        });
        self.phase = Phase::Showing;
        // For the origin of a note only.
        self.ask(now, out);
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
            Phase::Showing if self.is_free() => {
                if self
                    .menu
                    .as_ref()
                    .is_some_and(|m| now >= m.seen + MENU_IDLE)
                {
                    self.spoil(out, true);
                    self.after_hold(now);
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
// The menu: motion (pure) and drawing
// ---------------------------------------------------------------------------------------

/// The menu's animated values at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionFrame {
    /// Opacity of the whole menu, 0 to 1.
    pub alpha: f64,
    /// How far below its place the menu still is, DIPs (only while opening).
    pub rise: f64,
    /// The highlight's position as a tool index (fractional while it slides).
    pub highlight: f64,
    /// The highlight's opacity (0 when the pointer is over no row).
    pub lit: f64,
    /// The chosen row's flash, 1 at the click down to 0.
    pub flash: f64,
    /// Something still changes: keep the clock running.
    pub moving: bool,
    /// Closed and faded out: hide the window.
    pub gone: bool,
}

/// The menu's motion on a millisecond clock ([`OPEN_MS`], [`SLIDE_MS`], [`FLASH_MS`],
/// [`CLOSE_MS`]). With `animate` false (reduced motion) every change is instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuMotion {
    animate: bool,
    intro: Transition,
    opening: bool,
    closing: bool,
    highlight: Transition,
    lit: Transition,
    hovered: Option<usize>,
    /// When the chosen row's flash started.
    flash: Option<f64>,
}

impl MenuMotion {
    pub fn hidden() -> MenuMotion {
        MenuMotion {
            animate: false,
            intro: Transition::at(0.0),
            opening: false,
            closing: true,
            highlight: Transition::at(0.0),
            lit: Transition::at(0.0),
            hovered: None,
            flash: None,
        }
    }

    fn ms(&self, ms: f64) -> f64 {
        if self.animate { ms } else { 0.0 }
    }

    /// Opens with `hovered` lit at once (no slide in from nowhere).
    pub fn open(&mut self, hovered: Option<usize>, now: f64, animate: bool) {
        self.animate = animate;
        self.intro = Transition::at(0.0);
        self.intro.retarget(1.0, now, self.ms(OPEN_MS));
        self.opening = true;
        self.closing = false;
        self.highlight = Transition::at(hovered.unwrap_or(0) as f64);
        self.lit = Transition::at(if hovered.is_some() { 1.0 } else { 0.0 });
        self.hovered = hovered;
        self.flash = None;
    }

    /// The pointer is over this row (or none): the highlight slides there.
    pub fn hover(&mut self, row: Option<usize>, now: f64) {
        if self.closing || row == self.hovered {
            return;
        }
        self.hovered = row;
        if let Some(row) = row {
            if self.lit.value(now) <= 0.0 {
                // Lighting up from nothing: appear on the row, do not slide from elsewhere.
                self.highlight = Transition::at(row as f64);
            } else {
                self.highlight.retarget(row as f64, now, self.ms(SLIDE_MS));
            }
        }
        let lit = if row.is_some() { 1.0 } else { 0.0 };
        self.lit.retarget(lit, now, self.ms(SLIDE_MS));
    }

    /// A row was chosen: it is lit at once and flashes.
    pub fn choose(&mut self, row: usize, now: f64) {
        self.hovered = Some(row);
        self.highlight = Transition::at(row as f64);
        self.lit = Transition::at(1.0);
        self.flash = self.animate.then_some(now);
    }

    /// Fades out, after the flash of a chosen row.
    pub fn close(&mut self, now: f64, animate: bool) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.opening = false;
        let from = self.intro.value(now);
        if animate && self.animate && from > 0.0 {
            let start = self.flash.map_or(now, |at| (at + FLASH_MS).max(now));
            self.intro = Transition {
                from,
                to: 0.0,
                start,
                duration: CLOSE_MS,
            };
        } else {
            self.intro = Transition::at(0.0);
            self.flash = None;
        }
    }

    pub fn closing(&self) -> bool {
        self.closing
    }

    pub fn frame(&self, now: f64) -> MotionFrame {
        let alpha = self.intro.value(now).clamp(0.0, 1.0);
        let flash = self
            .flash
            .map_or(0.0, |at| (1.0 - (now - at) / FLASH_MS).clamp(0.0, 1.0));
        let flashing = self.flash.is_some_and(|at| now < at + FLASH_MS);
        MotionFrame {
            alpha,
            rise: if self.opening {
                (1.0 - alpha) * RISE
            } else {
                0.0
            },
            highlight: self.highlight.value(now),
            lit: self.lit.value(now).clamp(0.0, 1.0),
            flash,
            moving: self.intro.active(now)
                || self.highlight.active(now)
                || self.lit.active(now)
                || flashing,
            gone: self.closing && !self.intro.active(now) && alpha <= 0.0,
        }
    }
}

/// What one picture of the menu shows.
#[derive(Debug, Clone)]
pub struct MenuPicture<'a> {
    /// The tools in menu order, each with its key chips.
    pub rows: &'a [(ToolId, Vec<String>)],
    pub layout: &'a MenuLayout,
    pub dark: bool,
    pub highlight: f64,
    pub lit: f64,
    pub flash: f64,
}

thread_local! {
    /// GDI fonts per DPI, kept for the thread's life (as the editor keeps its own).
    static FONTS: RefCell<Vec<(u32, GdiFonts)>> = const { RefCell::new(Vec::new()) };
}

fn fonts(dpi: u32) -> GdiFonts {
    FONTS.with(|fonts| {
        let mut fonts = fonts.borrow_mut();
        if let Some((_, f)) = fonts.iter().find(|(d, _)| *d == dpi) {
            return *f;
        }
        let made = GdiFonts::new(dpi);
        fonts.push((dpi, made));
        made
    })
}

fn dpi_of(scale: f64) -> u32 {
    (scale * 96.0).round() as u32
}

/// Width of `text` in `font`, physical px.
fn text_width(surface: &Surface, font: windows::Win32::Graphics::Gdi::HFONT, text: &str) -> i32 {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    // SAFETY: the surface's own memory DC and a live font; `units` and `size` are locals.
    // The previous font is selected back before returning.
    unsafe {
        let old = SelectObject(surface.dc, font.into());
        let _ = GetTextExtentPoint32W(surface.dc, &units, &mut size);
        SelectObject(surface.dc, old);
    }
    size.cx
}

fn chips_width(surface: &Surface, fonts: &GdiFonts, chips: &[String], dpi: u32) -> i32 {
    let widths: i32 = chips
        .iter()
        .map(|chip| text_width(surface, fonts.small, chip) + 2 * px(CHIP_PAD, dpi))
        .sum();
    widths + px(CHIP_GAP, dpi) * (chips.len() as i32 - 1).max(0)
}

/// The card width these rows need at `scale`, DIPs (at least [`MENU_MIN_WIDTH`]).
pub fn menu_width(rows: &[(ToolId, Vec<String>)], scale: f64) -> f64 {
    let dpi = dpi_of(scale);
    let fonts = fonts(dpi);
    let Ok(surface) = Surface::new(1, 1) else {
        return MENU_MIN_WIDTH;
    };
    let widest = rows
        .iter()
        .map(|(id, chips)| {
            let label = text_width(&surface, fonts.body, tool(*id).label);
            let chips = if chips.is_empty() {
                0
            } else {
                px(LABEL_GAP, dpi) + chips_width(&surface, &fonts, chips, dpi)
            };
            px(LABEL_X, dpi) + label + chips + px(CHIPS_RIGHT, dpi)
        })
        .max()
        .unwrap_or(0);
    (f64::from(widest) / scale).ceil().max(MENU_MIN_WIDTH)
}

fn local(b: Bounds, origin: Bounds) -> RECT {
    RECT {
        left: b.left - origin.left,
        top: b.top - origin.top,
        right: b.right - origin.left,
        bottom: b.bottom - origin.top,
    }
}

/// Draws the menu at full opacity into a pixmap of the layout's window size (premultiplied
/// RGBA). Shapes with tiny-skia and labels with GDI, as the slash menu draws.
pub fn render_menu(picture: &MenuPicture) -> Option<Pixmap> {
    let layout = picture.layout;
    let dpi = dpi_of(layout.scale);
    let palette = Palette::for_theme(picture.dark);
    let fonts = fonts(dpi);
    let (w, h) = (
        layout.window.width().max(1) as u32,
        layout.window.height().max(1) as u32,
    );
    let mut surface = Surface::new(w, h).ok()?;
    let card = local(layout.card, layout.window);
    let radius = px(MENU_RADIUS, dpi) as f32;
    // Measured before the canvas borrows the surface.
    let chip_widths: Vec<Vec<i32>> = picture
        .rows
        .iter()
        .map(|(_, chips)| {
            chips
                .iter()
                .map(|c| text_width(&surface, fonts.small, c) + 2 * px(CHIP_PAD, dpi))
                .collect()
        })
        .collect();
    let icons: Vec<Pixmap> = picture
        .rows
        .iter()
        .filter_map(|(id, _)| icon_pixmap(tool(*id).icon, layout.scale, &palette))
        .collect();
    {
        let mut canvas = surface.canvas();
        canvas.clear();
        canvas.shadow(card, radius, px(9.0, dpi), picture.dark);
        canvas.fill(card, radius, palette.paper, 1.0);
        canvas.stroke(card, radius, palette.hairline);
        // The highlight, at its (sliding) position.
        if picture.lit > 0.0 && layout.rows > 0 {
            let position = if layout.upward {
                (layout.rows - 1) as f64 - picture.highlight
            } else {
                picture.highlight
            };
            let top =
                card.top + layout.pad + (position * f64::from(layout.row_height)).round() as i32;
            let inset = px(ROW_INSET, dpi);
            let lit = RECT {
                left: card.left + inset,
                top,
                right: card.right - inset,
                bottom: top + layout.row_height,
            };
            let color = mix(palette.selected, palette.accent, 0.3 * picture.flash);
            canvas.fill(lit, px(ROW_RADIUS, dpi) as f32, color, picture.lit);
        }
        for (index, (id, chips)) in picture.rows.iter().enumerate() {
            let row = local(layout.row(index), layout.window);
            let cy = (row.top + row.bottom) / 2;
            let icon = px(ICON, dpi);
            let icon_rect = RECT {
                left: card.left + px(ICON_X, dpi),
                top: cy - icon / 2,
                right: card.left + px(ICON_X, dpi) + icon,
                bottom: cy - icon / 2 + icon,
            };
            if let Some(pixmap) = icons.get(index) {
                canvas.image(pixmap, icon_rect, 0.0, 1.0);
            }
            let mut right = card.right - px(CHIPS_RIGHT, dpi);
            let chip_h = px(CHIP_HEIGHT, dpi);
            for (chip, width) in chips.iter().zip(&chip_widths[index]).rev() {
                let rect = RECT {
                    left: right - width,
                    top: cy - chip_h / 2,
                    right,
                    bottom: cy - chip_h / 2 + chip_h,
                };
                canvas.fill(rect, px(CHIP_RADIUS, dpi) as f32, palette.chip, 1.0);
                canvas.label(
                    rect,
                    chip,
                    palette.muted,
                    fonts.small,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                right = rect.left - px(CHIP_GAP, dpi);
            }
            let label_right = if chips.is_empty() {
                card.right - px(CHIPS_RIGHT, dpi)
            } else {
                right + px(CHIP_GAP, dpi) - px(LABEL_GAP, dpi)
            };
            canvas.label(
                RECT {
                    left: card.left + px(LABEL_X, dpi),
                    top: row.top,
                    right: label_right,
                    bottom: row.bottom,
                },
                tool(*id).label,
                palette.text,
                fonts.body,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
        canvas.finish_labels();
    }
    // The DIB is BGRA (the canvas fed colours with red and blue swapped): back to RGBA.
    let mut pixels = surface.pixels();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let size = tiny_skia::IntSize::from_wh(w, h)?;
    let mut pixmap = Pixmap::from_vec(pixels, size)?;
    opaque_inside(&mut pixmap, card, radius);
    Some(pixmap)
}

/// GDI leaves alpha at 0 where it drew text; everything fully inside the card is opaque, so
/// those pixels get alpha 255 back. The card's antialiased edge is left as tiny-skia drew
/// it (the editor's `Canvas::opaque_inside` scales edge pixels by their coverage a second
/// time, which darkens the rim of a white card).
fn opaque_inside(pixmap: &mut Pixmap, card: RECT, radius: f32) {
    let Some(path) = rounded_path(card, radius) else {
        return;
    };
    let Some(mut mask) = tiny_skia::Mask::new(pixmap.width(), pixmap.height()) else {
        return;
    };
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    for (pixel, cover) in pixmap
        .data_mut()
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(mask.data())
    {
        if *cover == 255 {
            pixel[3] = 255;
        }
    }
}

/// Multiplies a premultiplied pixmap by `alpha`.
pub fn apply_alpha(pixmap: &mut Pixmap, alpha: f64) {
    if alpha >= 1.0 {
        return;
    }
    let a = (alpha.clamp(0.0, 1.0) * 256.0).round() as u32;
    for byte in pixmap.data_mut() {
        *byte = ((u32::from(*byte) * a) >> 8) as u8;
    }
}

/// A paint in the canvas's channel order (blue in tiny-skia's red slot, as the DIB is BGRA).
fn bgr_paint(color: COLORREF) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(
        (color.0 >> 16) as u8,
        (color.0 >> 8) as u8,
        color.0 as u8,
        255,
    );
    paint.anti_alias = true;
    paint
}

const fn rgb(hex: u32) -> COLORREF {
    COLORREF(((hex & 0xff) << 16) | (hex & 0xff00) | ((hex >> 16) & 0xff))
}

/// A tool's icon in an [`ICON`]-DIP square, in the canvas's channel order.
fn icon_pixmap(icon: Icon, scale: f64, palette: &Palette) -> Option<Pixmap> {
    let size = px(ICON, dpi_of(scale)).max(1) as u32;
    let mut pixmap = Pixmap::new(size, size)?;
    let s = scale as f32;
    let at = |x: f32, y: f32| (x * s, y * s);
    let stroke = |width: f32| Stroke {
        width: width * s,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    let ink = bgr_paint(palette.muted);
    match icon {
        Icon::NoteSheet => {
            // A sheet with its top-right corner folded, and two lines of text.
            let mut sheet = PathBuilder::new();
            let points = [
                (4.0, 2.0),
                (11.0, 2.0),
                (15.0, 6.0),
                (15.0, 16.0),
                (4.0, 16.0),
            ];
            let (x, y) = at(points[0].0, points[0].1);
            sheet.move_to(x, y);
            for (px_, py) in &points[1..] {
                let (x, y) = at(*px_, *py);
                sheet.line_to(x, y);
            }
            sheet.close();
            let mut fold = PathBuilder::new();
            let (x, y) = at(11.0, 2.0);
            fold.move_to(x, y);
            let (x, y) = at(11.0, 6.0);
            fold.line_to(x, y);
            let (x, y) = at(15.0, 6.0);
            fold.line_to(x, y);
            let mut lines = PathBuilder::new();
            for (y, end) in [(9.5, 12.5), (12.5, 10.5)] {
                let (x0, y0) = at(6.75, y);
                let (x1, _) = at(end, y);
                lines.move_to(x0, y0);
                lines.line_to(x1, y0);
            }
            if let Some(path) = sheet.finish() {
                pixmap.fill_path(
                    &path,
                    &bgr_paint(palette.paper),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
                pixmap.stroke_path(&path, &ink, &stroke(1.4), Transform::identity(), None);
            }
            for path in [fold.finish(), lines.finish()].into_iter().flatten() {
                pixmap.stroke_path(&path, &ink, &stroke(1.3), Transform::identity(), None);
            }
        }
        Icon::RecordDot => {
            let (cx, cy) = at(9.0, 9.0);
            if let Some(ring) = PathBuilder::from_circle(cx, cy, 6.6 * s) {
                pixmap.stroke_path(&ring, &ink, &stroke(1.4), Transform::identity(), None);
            }
            let red = if palette.dark {
                rgb(0xff6b6b)
            } else {
                rgb(0xe5484d)
            };
            if let Some(dot) = PathBuilder::from_circle(cx, cy, 3.7 * s) {
                pixmap.fill_path(
                    &dot,
                    &bgr_paint(red),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
    }
    Some(pixmap)
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
    Context(ToolContext),
    Shortcuts(Shortcuts),
    SummonAtCursor,
    OpenMenu,
    RestartHook,
    /// For the self-test: show (or hide) the menu with these tools at the cursor, without
    /// the gesture and without arming the gate.
    TestMenu(Option<Vec<ToolId>>),
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
    menu_hwnd: AtomicIsize,
    /// Where the menu is while it shows.
    menu_layout: Mutex<Option<MenuLayout>>,
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
            menu_hwnd: AtomicIsize::new(0),
            menu_layout: Mutex::new(None),
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
#[derive(Debug, Clone)]
struct Start {
    config: SummonConfig,
    blocked: bool,
    context: ToolContext,
    shortcuts: Shortcuts,
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

/// The menu window and its motion.
struct MenuView {
    window: Overlay,
    motion: MenuMotion,
    rows: Vec<(ToolId, Vec<String>)>,
    layout: Option<MenuLayout>,
    dark: bool,
    visible: bool,
    /// The picture at full opacity, and the values it was drawn for.
    picture: Option<Pixmap>,
    pictured: Option<(f64, f64, f64)>,
    /// Opacity and rise last presented.
    presented: Option<(f64, i32)>,
}

impl MenuView {
    fn new(capturable: bool) -> windows::core::Result<MenuView> {
        let mut window = Overlay::new()?;
        window.set_capturable(capturable)?;
        Ok(MenuView {
            window,
            motion: MenuMotion::hidden(),
            rows: Vec::new(),
            layout: None,
            dark: false,
            visible: false,
            picture: None,
            pictured: None,
            presented: None,
        })
    }

    fn open(&mut self, rows: Vec<(ToolId, Vec<String>)>, layout: MenuLayout, now: f64) {
        self.dark = system_dark();
        self.rows = rows;
        self.layout = Some(layout);
        self.motion
            .open(layout.row_at(cursor()), now * 1000.0, animation_effects());
        self.picture = None;
        self.pictured = None;
        self.presented = None;
    }

    fn choose(&mut self, row: usize, now: f64) {
        self.motion.choose(row, now * 1000.0);
    }

    fn close(&mut self, now: f64, animate: bool) {
        self.motion.close(now * 1000.0, animate);
    }

    /// Presents the current frame. Returns whether it still moves.
    fn frame(&mut self, now: f64) -> bool {
        let Some(layout) = self.layout else {
            return false;
        };
        let ms = now * 1000.0;
        if !self.motion.closing() {
            self.motion.hover(layout.row_at(cursor()), ms);
        }
        let frame = self.motion.frame(ms);
        if frame.gone {
            if self.visible {
                self.window.hide();
                self.visible = false;
            }
            self.layout = None;
            self.picture = None;
            return false;
        }
        let look = (frame.highlight, frame.lit, frame.flash);
        if self.pictured != Some(look) {
            self.picture = render_menu(&MenuPicture {
                rows: &self.rows,
                layout: &layout,
                dark: self.dark,
                highlight: frame.highlight,
                lit: frame.lit,
                flash: frame.flash,
            });
            self.pictured = Some(look);
            self.presented = None;
        }
        let rise = px(frame.rise, dpi_of(layout.scale));
        if self.presented != Some((frame.alpha, rise))
            && let Some(picture) = &self.picture
        {
            let origin = (layout.window.left, layout.window.top + rise);
            if frame.alpha >= 1.0 {
                let _ = self.window.present(picture, origin);
            } else {
                let mut faded = picture.clone();
                apply_alpha(&mut faded, frame.alpha);
                let _ = self.window.present(&faded, origin);
            }
            self.presented = Some((frame.alpha, rise));
        }
        if !self.visible {
            self.window.show();
            self.visible = true;
        }
        if self.window.take_display_changed() {
            self.window.reassert();
        }
        frame.moving
    }
}

struct Driver {
    shared: Arc<Shared>,
    requester: Arc<Mutex<Requester>>,
    gesture: Gesture,
    config: SummonConfig,
    shortcuts: Shortcuts,
    view: MenuView,
    hook: Option<KeyHook>,
    asks: HashMap<RequestId, Purpose>,
    /// The latest lookup that found the spot empty (or any, for a free menu), for the origin
    /// of a chosen tool.
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
    // SAFETY: plain calls on this thread. Physical pixels for the menu and the cursor; the
    // message queue exists before anyone waits on us.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    }
    let gate_wake = CLICK_GATE.wake_handle();
    let view = match MenuView::new(start.config.capturable) {
        Ok(view) => view,
        Err(e) => {
            let _ = ready.send(Err(format!("menu window: {e}")));
            return;
        }
    };
    shared
        .menu_hwnd
        .store(view.window.hwnd().0 as isize, Relaxed);
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
    gesture.step(Input::Context(start.context), now);
    // Keys already down when we start count as a used-up hold.
    gesture.step(Input::KeysReset(Keys::now()), now);
    let mut driver = Driver {
        shared,
        requester,
        gesture,
        config: start.config,
        shortcuts: start.shortcuts,
        view,
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
            animating = self.view.frame(hook::now());
        }
        // Shutdown: nothing armed, nothing shown, the hook removed.
        let actions = self.gesture.step(Input::Shutdown, hook::now());
        self.execute(actions);
        self.view.close(hook::now(), false);
        self.view.frame(hook::now());
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
                        let _ = self.view.window.set_capturable(config.capturable);
                    }
                    self.step(Input::Modifier(config.modifier));
                    self.step(Input::Enabled(config.enabled));
                    self.log(&format!("config: modifier {}", config.modifier.label()));
                }
                Command::Context(context) => {
                    self.log(&format!("tool context {context:?}"));
                    self.step(Input::Context(context));
                }
                Command::Shortcuts(shortcuts) => self.shortcuts = shortcuts,
                Command::SummonAtCursor => self.summon_at_cursor(),
                Command::OpenMenu => {
                    let (point, epoch) = (cursor(), CLICK_GATE.epoch());
                    self.log(&format!(
                        "open the menu at {},{} (no gesture)",
                        point.0, point.1
                    ));
                    self.step(Input::OpenMenu { point, epoch });
                }
                Command::RestartHook => self.restart_hook("asked"),
                Command::TestMenu(tools) => {
                    let now = hook::now();
                    match tools {
                        Some(tools) => {
                            self.open_view(&tools, cursor(), now);
                        }
                        None => {
                            *lock(&self.shared.menu_layout) = None;
                            self.view.close(now, false);
                        }
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

    /// Places the menu for these tools at `point` and shows it. Returns its layout.
    fn open_view(&mut self, tools: &[ToolId], point: (i32, i32), now: f64) -> MenuLayout {
        let rows: Vec<(ToolId, Vec<String>)> = tools
            .iter()
            .map(|&id| (id, tool_chips(id, &self.shortcuts)))
            .collect();
        let monitor = monitor_at(Point::new(f64::from(point.0), f64::from(point.1)));
        let area = monitor.work_area;
        let work = Bounds {
            left: area.x.round() as i32,
            top: area.y.round() as i32,
            right: (area.x + area.width).round() as i32,
            bottom: (area.y + area.height).round() as i32,
        };
        let width = menu_width(&rows, monitor.scale);
        let layout = menu_layout(point, rows.len(), width, work, monitor.scale);
        *lock(&self.shared.menu_layout) = Some(layout);
        self.view.open(rows, layout, now);
        layout
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
                        self.log("gate refused (a key changed): no menu");
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
                Action::Open { point } => {
                    let tools = self.gesture.tools().to_vec();
                    let layout = self.open_view(&tools, point, now);
                    let names: Vec<&str> = tools.iter().map(|t| t.id()).collect();
                    self.log(&format!(
                        "menu open at {},{} with [{}]{}",
                        point.0,
                        point.1,
                        names.join(", "),
                        if layout.upward { ", upward" } else { "" }
                    ));
                    let more = self.gesture.step(Input::Placed(layout), now);
                    self.execute(more);
                }
                Action::Disarm => {
                    CLICK_GATE.withdraw();
                    self.log("gate disarmed");
                }
                Action::Close { animate } => {
                    self.log("menu closed");
                    *lock(&self.shared.menu_layout) = None;
                    self.view.close(now, animate);
                }
                Action::Choose {
                    tool,
                    row,
                    point,
                    ask,
                } => {
                    self.view.choose(row, now);
                    let empty = self.empty.take();
                    let none = Outcome::Failed("no lookup".into());
                    let outcome = match (&empty, ask) {
                        (Some((id, outcome)), Some(ask)) if *id == ask => outcome,
                        _ => &none,
                    };
                    let origin = self.origin_at(point, outcome);
                    self.empty = empty;
                    self.emit_chosen(tool, point, origin, now);
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

    fn emit_chosen(&self, tool: ToolId, point: (i32, i32), origin: NoteOrigin, time: f64) {
        self.log(&format!(
            "tool {} at {},{}: {} | {}",
            tool.id(),
            point.0,
            point.1,
            origin.label(),
            origin.detail().replace('\n', " | ")
        ));
        self.shared.emit(SummonerEvent::ToolChosen(ToolChosen {
            tool,
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
                    let mut spot = Spot::Unknown;
                    if empty || self.gesture.is_free() {
                        // Read the window facts now (the exe's name is cached), so the
                        // click finds them ready; they also name the kind of spot.
                        if let Outcome::Found(s) = &outcome {
                            let point = s.point;
                            spot = Spot::from_kind(self.origin_at(point, &outcome).kind);
                        }
                        self.empty = Some((ask, outcome));
                    }
                    self.step(Input::Answer { ask, empty, spot });
                }
                Some(Purpose::Hotkey { point, time }) => {
                    let origin = self.origin_at(point, &outcome);
                    self.emit_chosen(ToolId::NewNote, point, origin, time);
                }
                None => {}
            }
        }
    }

    /// The NewNote hotkey: a note at the cursor, no menu, nothing swallowed.
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
                self.emit_chosen(ToolId::NewNote, point, origin, time);
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
    // SAFETY: `msg` is a valid MSG for the calls to fill and read; the menu's window
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

// ---------------------------------------------------------------------------------------
// Public API (the UI thread)
// ---------------------------------------------------------------------------------------

/// The tool menu gesture. Owned by the UI thread; every call returns at once.
pub struct Summoner {
    shared: Arc<Shared>,
    requester: Arc<Mutex<Requester>>,
    thread: Option<JoinHandle<()>>,
    start: Start,
    blocks: u8,
}

impl Summoner {
    /// Starts the summoner thread (menu window, keyboard hook). `requester` is a lane of the
    /// app's UI Automation worker (`Uia::requester`). No tool is offered until
    /// [`set_tool_context`](Self::set_tool_context) says what can run.
    pub fn start(config: SummonConfig, requester: Requester) -> Result<Summoner, String> {
        hook::now();
        let shared = Arc::new(Shared::new().map_err(|e| format!("events: {e}"))?);
        let requester = Arc::new(Mutex::new(requester));
        let start = Start {
            config,
            blocked: false,
            context: ToolContext::default(),
            shortcuts: Shortcuts::none(),
        };
        let thread = spawn(&shared, &requester, start.clone())?;
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
    /// reason is set; a menu that shows closes at once.
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

    /// What the tools' conditions look at; sent on only when it changed. Read when the menu
    /// opens.
    pub fn set_tool_context(&mut self, context: ToolContext) {
        if context != self.start.context {
            self.start.context = context;
            self.shared.send(Command::Context(context));
        }
    }

    /// The shortcuts the rows show as key chips; sent on only when they changed. Read when
    /// the menu opens.
    pub fn set_shortcuts(&mut self, shortcuts: &Shortcuts) {
        if *shortcuts != self.start.shortcuts {
            self.start.shortcuts = shortcuts.clone();
            self.shared.send(Command::Shortcuts(shortcuts.clone()));
        }
    }

    /// The NewNote hotkey: looks up the origin at the cursor and emits
    /// [`SummonerEvent::ToolChosen`] for [`ToolId::NewNote`], with no menu and nothing
    /// swallowed.
    pub fn summon_at_cursor(&self) {
        self.shared.send(Command::SummonAtCursor);
    }

    /// Opens the menu at the cursor without the gesture (for a shortcut; none is bound yet).
    /// The click gate is armed while it shows: a click on a row runs that tool, any other
    /// click closes it, as does any key press or [`MENU_IDLE`] seconds without the pointer
    /// over it. Does nothing while a hold is under way, when blocked or disabled, or when no
    /// tool is offered.
    pub fn open_menu_at_cursor(&self) {
        self.shared.send(Command::OpenMenu);
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
        self.thread = Some(spawn(&self.shared, &self.requester, self.start.clone())?);
        Ok(())
    }

    /// Calls of the keyboard hook so far and the longest one.
    pub fn key_hook_stats(&self) -> (u64, Duration) {
        (
            self.shared.key_callbacks.load(Relaxed),
            Duration::from_nanos(self.shared.key_max_ns.load(Relaxed)),
        )
    }

    /// The menu window, for tests.
    pub fn menu_window(&self) -> isize {
        self.shared.menu_hwnd.load(Relaxed)
    }

    /// Where the menu is while it shows, for tests.
    pub fn menu_layout(&self) -> Option<MenuLayout> {
        *lock(&self.shared.menu_layout)
    }

    /// For the self-test only: shows the menu with these tools at the cursor without the
    /// gesture and without arming the gate (`None` hides it).
    pub fn test_menu(&self, tools: Option<Vec<ToolId>>) {
        self.shared.send(Command::TestMenu(tools));
    }

    /// Stops the thread: disarms the gate, hides the menu, removes the keyboard hook. True
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
    use crate::shortcuts::{Action as Shortcut, Chord};

    const LALT: u32 = 0xA4;
    const RALT: u32 = 0xA5;
    const LCTRL: u32 = 0xA2;
    const TAB: u32 = 0x09;
    const F4: u32 = 0x73;
    const KEY_F: u32 = 0x46;
    const WORK: Bounds = Bounds {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    /// Where the scripted hold opens the menu.
    const AT: (i32, i32) = (100, 100);

    fn notes_only() -> ToolContext {
        ToolContext {
            notes: true,
            ..ToolContext::default()
        }
    }

    fn both() -> ToolContext {
        ToolContext {
            notes: true,
            recording_available: true,
            recording_enabled: true,
            ..ToolContext::default()
        }
    }

    /// A scripted run: inputs at times, every action recorded. An `Open` is placed at once,
    /// as the driver does.
    struct Script {
        g: Gesture,
        actions: Vec<(f64, Action)>,
        epoch: u64,
    }

    impl Script {
        fn new(modifier: Modifier) -> Script {
            Script::with(modifier, notes_only())
        }

        fn with(modifier: Modifier, context: ToolContext) -> Script {
            let mut g = Gesture::new(modifier);
            g.step(Input::Context(context), 0.0);
            Script {
                g,
                actions: Vec::new(),
                epoch: 0,
            }
        }

        fn record(&mut self, t: f64, actions: Vec<Action>) {
            for action in actions {
                self.actions.push((t, action));
                if let Action::Open { point } = action {
                    let layout = menu_layout(point, self.g.tools().len(), 200.0, WORK, 1.0);
                    let more = self.g.step(Input::Placed(layout), t);
                    self.record(t, more);
                }
            }
        }

        fn at(&mut self, t: f64, input: Input) -> &mut Script {
            // Time passes up to `t` first, as the driver's ticks would make it.
            self.tick_to(t);
            let actions = self.g.step(input, t);
            self.record(t, actions);
            self
        }

        fn tick_to(&mut self, t: f64) {
            while let Some(due) = self.g.next_deadline() {
                if due > t {
                    break;
                }
                let actions = self.g.step(Input::Tick, due);
                self.record(due, actions);
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
            self.at(
                t,
                Input::Answer {
                    ask,
                    empty,
                    spot: Spot::Desktop,
                },
            )
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
        fn opens(&self) -> usize {
            self.count(|a| matches!(a, Action::Open { .. }))
        }
        fn chosen(&self) -> Vec<ToolId> {
            self.actions
                .iter()
                .filter_map(|(_, a)| match a {
                    Action::Choose { tool, .. } => Some(*tool),
                    _ => None,
                })
                .collect()
        }
        fn chooses(&self) -> usize {
            self.chosen().len()
        }
        fn closes(&self) -> usize {
            self.count(|a| matches!(a, Action::Close { .. }))
        }
        fn disarms(&self) -> usize {
            self.count(|a| matches!(a, Action::Disarm))
        }

        /// Nothing at all touched the gate, the keyboard or the screen.
        fn untouched(&self) -> bool {
            self.arms() == 0 && self.masks() == 0 && self.opens() == 0 && self.chooses() == 0
        }

        /// The modifier down at 0, held past the delay over empty space at [`AT`]: the menu
        /// shows.
        fn showing_with(modifier: Modifier, context: ToolContext) -> Script {
            let mut s = Script::with(modifier, context);
            if modifier == Modifier::CtrlAlt {
                s.key(0.0, LCTRL, true);
            }
            s.key(0.0, LALT, true).moved(0.1, AT.0, AT.1);
            s.tick_to(0.35);
            assert_eq!(s.asks(), 1, "the lookup runs once the delay passed");
            s.answer(0.36, true);
            assert_eq!(s.g.phase(), Phase::Showing);
            s
        }

        fn showing(modifier: Modifier) -> Script {
            Script::showing_with(modifier, notes_only())
        }

        fn layout(&self) -> MenuLayout {
            self.g.menu.as_ref().and_then(|m| m.layout).expect("placed")
        }
    }

    #[test]
    fn hold_then_click_without_moving_runs_the_first_tool() {
        let mut s = Script::showing(Modifier::Alt);
        assert_eq!((s.arms(), s.masks(), s.opens()), (1, 1, 1));
        assert_eq!(s.g.tools(), &[ToolId::NewNote]);
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
        // Arm comes before open: a click is never shown a menu that cannot take it.
        let arm = s
            .actions
            .iter()
            .position(|(_, a)| matches!(a, Action::Arm { .. }));
        let open = s
            .actions
            .iter()
            .position(|(_, a)| matches!(a, Action::Open { .. }));
        assert!(arm < open);
        // The click where the cursor already is: the first row.
        s.at(0.5, Input::ClickTaken { x: AT.0, y: AT.1 });
        let chosen = s.actions.iter().find_map(|(_, a)| match a {
            Action::Choose {
                tool,
                row,
                point,
                ask,
            } => Some((*tool, *row, *point, *ask)),
            _ => None,
        });
        assert_eq!(chosen, Some((ToolId::NewNote, 0, AT, Some(1))));
        assert!(
            s.actions
                .iter()
                .any(|(_, a)| *a == Action::Close { animate: true })
        );
        assert_eq!(s.g.phase(), Phase::Clicked);
        // A second click in the same hold runs nothing: the gate is not armed again.
        s.press(0.6, false).release(0.65);
        assert_eq!(s.chooses(), 1);
        assert_eq!(s.arms(), 1);
        // Release: a short cooldown, then idle.
        s.key(0.8, LALT, false);
        assert_eq!(s.g.phase(), Phase::Cooldown);
        s.tick_to(0.8 + COOLDOWN + 0.01);
        assert_eq!(s.g.phase(), Phase::Idle);
    }

    #[test]
    fn the_menu_opens_with_the_offered_tools() {
        let s = Script::showing_with(Modifier::Alt, both());
        assert_eq!(s.g.tools(), &[ToolId::NewNote, ToolId::RecordScreen]);
        assert_eq!(s.layout().rows, 2);
        // Recording on but running: only the note.
        let running = ToolContext {
            recording: true,
            ..both()
        };
        let s = Script::showing_with(Modifier::Alt, running);
        assert_eq!(s.g.tools(), &[ToolId::NewNote]);
        // Recording without notes: only the recording.
        let s = Script::showing_with(
            Modifier::Alt,
            ToolContext {
                notes: false,
                ..both()
            },
        );
        assert_eq!(s.g.tools(), &[ToolId::RecordScreen]);
    }

    #[test]
    fn with_no_tool_offered_nothing_opens_and_nothing_is_injected() {
        let mut s = Script::with(Modifier::Alt, ToolContext::default());
        s.key(0.0, LALT, true);
        s.tick_to(0.35);
        s.answer(0.36, true);
        assert!(s.untouched());
        assert_eq!(s.g.phase(), Phase::Cancelled);
        s.tick_to(2.0);
        assert_eq!(s.asks(), 1, "no more lookups in this hold");
        s.key(2.0, LALT, false);
        assert_eq!(s.g.phase(), Phase::Idle);
        assert!(s.untouched());
        // Recording available but switched off is not offered either.
        assert!(
            offered(&ToolContext {
                recording_available: true,
                ..ToolContext::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn moving_to_the_second_row_and_clicking_runs_it() {
        let mut s = Script::showing_with(Modifier::Alt, both());
        let second = s.layout().row(1);
        let point = (AT.0 + 40, (second.top + second.bottom) / 2);
        s.moved(0.45, point.0, point.1);
        s.tick_to(1.0);
        assert_eq!(s.asks(), 1, "no lookup while the pointer is over the menu");
        s.at(
            1.0,
            Input::ClickTaken {
                x: point.0,
                y: point.1,
            },
        );
        assert_eq!(s.chosen(), vec![ToolId::RecordScreen]);
        // The tool's point is where the menu opened, not the row.
        assert!(s.actions.iter().any(|(_, a)| matches!(
            a,
            Action::Choose {
                row: 1,
                point: AT,
                ..
            }
        )));
        assert_eq!(s.g.phase(), Phase::Clicked);
    }

    #[test]
    fn a_click_outside_every_row_closes_with_no_tool() {
        let mut s = Script::showing_with(Modifier::Alt, both());
        // Outside the card but still over empty space (the re-check says so).
        s.moved(0.4, 600, 600);
        s.tick_to(0.55);
        s.answer(0.56, true);
        assert_eq!(s.g.phase(), Phase::Showing);
        // The gate takes any primary press while armed (it knows no rectangle): swallowed,
        // and it only closes the menu.
        s.at(0.7, Input::ClickTaken { x: 600, y: 600 });
        assert_eq!(s.chooses(), 0);
        assert_eq!(s.closes(), 1);
        assert_eq!(s.g.phase(), Phase::Cancelled);
        s.key(0.9, LALT, false);
        assert_eq!(s.g.phase(), Phase::Idle, "no cooldown: nothing ran");
    }

    #[test]
    fn the_pointer_inside_the_menu_keeps_it_open() {
        let mut s = Script::showing_with(Modifier::Alt, both());
        let card = s.layout().card;
        // All over the card, slowly: no lookup, nothing closes.
        for (i, (x, y)) in [
            (card.left + 1, card.top + 1),
            (card.right - 2, card.bottom - 2),
            (card.left + 50, card.top + 40),
        ]
        .into_iter()
        .enumerate()
        {
            s.moved(0.4 + i as f64 * 0.3, x, y);
        }
        s.tick_to(2.0);
        assert_eq!(s.asks(), 1);
        assert_eq!(s.g.phase(), Phase::Showing);
        // A late "not empty" answer (the menu is our own window) while the pointer is inside
        // does not close it either.
        let mut s = Script::showing(Modifier::Alt);
        s.moved(0.4, 600, 600);
        s.tick_to(0.55);
        let ask = s.last_ask().unwrap();
        s.moved(0.56, AT.0 + 20, AT.1);
        s.at(
            0.57,
            Input::Answer {
                ask,
                empty: false,
                spot: Spot::Unknown,
            },
        );
        assert_eq!(s.g.phase(), Phase::Showing);
        assert_eq!(s.closes(), 0);
    }

    #[test]
    fn releasing_alt_closes_the_menu_and_runs_nothing() {
        let mut s = Script::showing(Modifier::Alt);
        s.key(0.9, LALT, false);
        assert_eq!(s.chooses(), 0);
        // The mask key was requested while Alt was still down (at open time), so the
        // release is not a lone Alt.
        assert_eq!(s.masks(), 1);
        let mask = s
            .actions
            .iter()
            .find(|(_, a)| matches!(a, Action::Mask { .. }))
            .unwrap()
            .0;
        assert!(mask < 0.9);
        // The release disarms and closes.
        assert_eq!((s.disarms(), s.closes()), (1, 1));
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

            // After the menu opened (Alt held over the desktop, then Tab): the menu closes,
            // the gate is disarmed, nothing else happens; Alt stays down for the system.
            let mut s = Script::showing(Modifier::Alt);
            s.key(0.7, other, true);
            assert_eq!(s.g.phase(), Phase::Cancelled);
            assert_eq!((s.disarms(), s.closes(), s.chooses()), (1, 1, 0));
            // More Tabs: nothing new.
            s.key(0.75, other, false)
                .key(0.8, other, true)
                .key(0.85, other, false);
            s.key(0.9, LALT, false);
            assert_eq!((s.arms(), s.masks(), s.chooses()), (1, 1, 0));
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
    fn a_click_during_the_hold_before_the_menu_is_the_apps() {
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
    fn leaving_empty_space_closes_the_menu_and_a_click_then_is_not_taken() {
        let mut s = Script::showing(Modifier::Alt);
        // Move over a text field outside the menu: a re-check after the gap says not empty.
        s.moved(0.40, 600, 600);
        s.tick_to(0.55);
        assert_eq!(s.asks(), 2);
        s.answer(0.56, false);
        assert_eq!(s.g.phase(), Phase::Looking);
        assert_eq!((s.disarms(), s.closes()), (1, 1));
        // A click now reaches us through the feed (the gate was disarmed, so the hook let
        // it through to the app): it uses up the hold and runs nothing.
        s.press(0.6, false);
        assert_eq!(s.chooses(), 0);
        assert_eq!(s.g.phase(), Phase::Cancelled);
        assert_eq!(s.arms(), 1);
    }

    #[test]
    fn back_on_empty_space_the_menu_opens_again_without_a_second_mask() {
        let mut s = Script::showing(Modifier::Alt);
        s.moved(0.40, 600, 600);
        s.tick_to(0.55);
        s.answer(0.56, false);
        s.moved(0.60, 800, 300);
        s.tick_to(0.70);
        s.answer(0.71, true);
        assert_eq!(s.g.phase(), Phase::Showing);
        assert_eq!((s.arms(), s.opens(), s.masks()), (2, 2, 1));
        // At the new spot.
        assert_eq!(s.layout().row_at((800, 300)), Some(0));
    }

    #[test]
    fn rechecks_outside_the_menu_are_throttled_and_still_cursors_are_not_rechecked() {
        let mut s = Script::showing(Modifier::Alt);
        for i in 0..40 {
            s.moved(0.37 + f64::from(i) * 0.005, 400 + i * 10, 400);
            if let Some(ask) = s.g.pending {
                s.at(
                    0.37 + f64::from(i) * 0.005 + 0.001,
                    Input::Answer {
                        ask,
                        empty: true,
                        spot: Spot::Desktop,
                    },
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
    fn over_a_text_field_no_menu() {
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
    fn a_lookup_timeout_means_no_menu() {
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
        // Blocked while the menu shows: it closes and the gate is disarmed.
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::Blocked(true));
        assert_eq!((s.disarms(), s.closes()), (1, 1));
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
        assert_eq!((s.disarms(), s.closes()), (1, 1));
    }

    #[test]
    fn pen_and_touch_presses_never_run_a_tool() {
        // The gate does not take them (hook tests); they reach the feed and use up the hold.
        let mut s = Script::showing(Modifier::Alt);
        s.press(0.5, true);
        assert_eq!(s.chooses(), 0);
        assert_eq!(s.g.phase(), Phase::Cancelled);
        assert_eq!((s.disarms(), s.closes()), (1, 1));
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
        assert_eq!((s.disarms(), s.closes()), (1, 1));
        // The release the new hook sees ends it.
        s.key(0.9, LALT, false);
        assert_eq!(s.g.phase(), Phase::Idle);
        // Restarted while nothing is held: idle at once.
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::KeysReset(Keys::default()));
        assert_eq!(s.g.phase(), Phase::Idle);
    }

    #[test]
    fn quit_mid_gesture_disarms_and_closes_at_once() {
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::Shutdown);
        let last: Vec<Action> = s.actions.iter().rev().take(2).map(|(_, a)| *a).collect();
        assert!(last.contains(&Action::Disarm));
        assert!(last.contains(&Action::Close { animate: false }));
        assert_eq!(s.chooses(), 0);
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
        assert_eq!((s.arms(), s.masks(), s.opens()), (1, 0, 1));
        s.at(0.5, Input::ClickTaken { x: AT.0, y: AT.1 });
        assert_eq!(s.chooses(), 1);
        s.key(0.6, LALT, false);
        assert_eq!(s.g.phase(), Phase::Clicked, "Ctrl still down");
        s.key(0.7, LCTRL, false);
        assert_eq!(s.g.phase(), Phase::Cooldown);

        let mut s = Script::new(Modifier::CtrlAlt);
        s.key(0.0, LALT, true).key(0.05, LCTRL, true);
        s.tick_to(0.4);
        assert_eq!(s.asks(), 1);

        // Releasing either key closes the menu.
        let mut s = Script::showing(Modifier::CtrlAlt);
        s.key(0.6, LCTRL, false);
        assert_eq!((s.disarms(), s.closes(), s.chooses()), (1, 1, 0));

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
        assert_eq!((s.disarms(), s.closes()), (1, 1));
    }

    #[test]
    fn a_click_taken_just_before_the_release_still_runs_its_tool() {
        // The hook took the click just before Alt went up, but the summoner saw the key
        // first (different threads): the click is gone, so the tool under it runs.
        let mut s = Script::showing(Modifier::Alt);
        s.key(0.5, LALT, false);
        s.at(0.5, Input::ClickTaken { x: AT.0, y: AT.1 });
        assert_eq!(s.chosen(), vec![ToolId::NewNote]);
        assert_eq!(s.g.phase(), Phase::Cooldown);
    }

    #[test]
    fn a_new_hold_in_the_cooldown_is_ignored_until_released() {
        let mut s = Script::showing(Modifier::Alt);
        s.at(0.5, Input::ClickTaken { x: AT.0, y: AT.1 });
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

    // --- The menu opened without the gesture ---

    fn free_menu(context: ToolContext) -> Script {
        let mut s = Script::with(Modifier::Alt, context);
        // The shortcut's keys are down when it fires (Ctrl+Shift+M, say).
        s.key(0.0, LCTRL, true)
            .key(0.0, 0xA0, true)
            .key(0.0, 0x4D, true);
        let epoch = s.epoch;
        s.at(0.01, Input::OpenMenu { point: AT, epoch });
        s
    }

    #[test]
    fn open_menu_at_cursor_shows_it_without_a_mask() {
        let mut s = free_menu(both());
        assert_eq!(s.g.phase(), Phase::Showing);
        assert!(s.g.is_free());
        assert_eq!((s.arms(), s.masks(), s.opens()), (1, 0, 1));
        assert_eq!(s.asks(), 1, "a lookup only for the origin");
        // The shortcut's keys come up: each release disarmed the gate in the hook, so it is
        // armed again with that key's epoch; the menu stays.
        s.key(0.05, 0x4D, false)
            .key(0.06, 0xA0, false)
            .key(0.07, LCTRL, false);
        assert_eq!(s.arms(), 4);
        assert!(
            s.actions
                .iter()
                .any(|(_, a)| *a == Action::Arm { epoch: s.epoch })
        );
        assert_eq!(s.g.phase(), Phase::Showing);
        // Any answer names the origin, even "not empty" (the cursor may be over a window).
        s.answer(0.1, false);
        assert_eq!(s.g.phase(), Phase::Showing);
        // A click on the second row runs it, with the origin's lookup.
        let second = s.layout().row(1);
        s.at(
            0.5,
            Input::ClickTaken {
                x: AT.0,
                y: second.top + 2,
            },
        );
        assert!(s.actions.iter().any(|(_, a)| matches!(
            a,
            Action::Choose {
                tool: ToolId::RecordScreen,
                ask: Some(1),
                ..
            }
        )));
    }

    #[test]
    fn open_menu_at_cursor_closes_on_a_key_a_click_or_after_a_while() {
        // A key press.
        let mut s = free_menu(notes_only());
        s.key(0.2, KEY_F, true);
        assert_eq!((s.disarms(), s.closes(), s.chooses()), (1, 1, 0));
        // A click outside (the gate takes it: swallowed, closes, runs nothing).
        let mut s = free_menu(notes_only());
        s.at(0.2, Input::ClickTaken { x: 900, y: 900 });
        assert_eq!((s.closes(), s.chooses()), (1, 0));
        // A click the gate did not take (another button).
        let mut s = free_menu(notes_only());
        s.press(0.2, false);
        assert_eq!(s.closes(), 1);
        // No pointer over it for MENU_IDLE.
        let mut s = free_menu(notes_only());
        s.key(0.05, 0x4D, false)
            .key(0.06, 0xA0, false)
            .key(0.07, LCTRL, false);
        s.moved(1.0, AT.0 + 10, AT.1);
        s.moved(2.0, 900, 900);
        s.tick_to(1.0 + MENU_IDLE - 0.01);
        assert_eq!(s.closes(), 0, "the pointer was over it at 1.0");
        s.tick_to(1.0 + MENU_IDLE + 0.01);
        assert_eq!((s.disarms(), s.closes()), (1, 1));
        assert_eq!(s.g.phase(), Phase::Idle);
        // Not while a hold is under way, blocked, or with no tool.
        let mut s = Script::new(Modifier::Alt);
        s.key(0.0, LALT, true);
        s.at(
            0.1,
            Input::OpenMenu {
                point: AT,
                epoch: 1,
            },
        );
        assert_eq!(s.opens(), 0);
        let mut s = Script::with(Modifier::Alt, ToolContext::default());
        s.at(
            0.1,
            Input::OpenMenu {
                point: AT,
                epoch: 0,
            },
        );
        assert_eq!(s.opens(), 0);
        let mut s = Script::new(Modifier::Alt);
        s.at(0.0, Input::Blocked(true));
        s.at(
            0.1,
            Input::OpenMenu {
                point: AT,
                epoch: 0,
            },
        );
        assert_eq!(s.opens(), 0);
    }

    // --- Placement ---

    #[test]
    fn the_first_row_is_under_the_cursor_and_the_card_on_screen_near_every_edge() {
        let monitors = [
            Bounds {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
            // Left of and above the primary, with a top taskbar.
            Bounds {
                left: -2560,
                top: -1400,
                right: 0,
                bottom: 0,
            },
            Bounds {
                left: 1920,
                top: 48,
                right: 3840,
                bottom: 1080,
            },
        ];
        for work in monitors {
            for scale in [1.0, 1.25, 1.5, 2.0] {
                for rows in [1, 2, 4] {
                    let xs = [
                        work.left,
                        work.left + 3,
                        work.left + 500,
                        work.right - 200,
                        work.right - 1,
                    ];
                    let ys = [
                        work.top,
                        work.top + 4,
                        work.top + 400,
                        work.bottom - 60,
                        work.bottom - 10,
                        work.bottom - 1,
                    ];
                    for &x in &xs {
                        for &y in &ys {
                            let l = menu_layout((x, y), rows, 220.0, work, scale);
                            let what = format!("{work:?} at {x},{y}, {scale}x, {rows} rows");
                            assert_eq!(l.row_at((x, y)), Some(0), "{what}");
                            assert!(l.card.inside(&work), "{what}: {:?}", l.card);
                            assert_eq!(l.card.height(), 2 * l.pad + rows as i32 * l.row_height);
                            assert!(l.window.left < l.card.left && l.window.bottom > l.card.bottom);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_cursor_sits_at_the_anchor_away_from_edges_and_near_the_bottom_it_opens_upward() {
        let l = menu_layout((500, 400), 2, 220.0, WORK, 1.0);
        assert!(!l.upward);
        assert_eq!(l.card.left, 500 - ANCHOR.0 as i32);
        assert_eq!(l.row(0).top, 400 - ANCHOR.1 as i32);
        assert!(l.row(1).top > l.row(0).top);
        assert_eq!(l.row_at((520, 400 + 32)), Some(1));
        // 125%: everything scales.
        let l = menu_layout((500, 400), 2, 220.0, WORK, 1.25);
        assert_eq!(l.row_height, 40);
        assert_eq!(l.card.width(), 275);
        // Near the bottom: upward, the first tool at the bottom, exactly where it would be.
        let l = menu_layout((500, 1000), 2, 220.0, WORK, 1.0);
        assert!(l.upward);
        assert_eq!(l.row(0).top, 1000 - ANCHOR.1 as i32);
        assert!(l.row(1).top < l.row(0).top);
        assert_eq!(l.row_at((520, 1000 - 32)), Some(1));
        // The padding counts as the nearest row; outside the card is no row.
        let l = menu_layout((500, 400), 2, 220.0, WORK, 1.0);
        assert_eq!(l.row_at((520, l.card.top)), Some(0));
        assert_eq!(l.row_at((520, l.card.bottom - 1)), Some(1));
        assert_eq!(l.row_at((520, l.card.bottom)), None);
        assert_eq!(l.row_at((l.card.left - 1, 400)), None);
    }

    // --- Chips ---

    #[test]
    fn the_chips_come_from_the_current_shortcuts() {
        let mut shortcuts = Shortcuts::defaults();
        assert!(tool_chips(ToolId::NewNote, &shortcuts).is_empty());
        assert_eq!(
            tool_chips(ToolId::RecordScreen, &shortcuts),
            vec!["Ctrl", "Shift", "6"]
        );
        shortcuts
            .set(Shortcut::NewNote, Some(Chord::parse("Ctrl+Alt+N").unwrap()))
            .unwrap();
        shortcuts.set(Shortcut::ToggleRecording, None).unwrap();
        assert_eq!(
            tool_chips(ToolId::NewNote, &shortcuts),
            Chord::parse("Ctrl+Alt+N").unwrap().chips()
        );
        assert!(tool_chips(ToolId::RecordScreen, &shortcuts).is_empty());
    }

    #[test]
    fn tools_are_one_list_with_their_actions() {
        assert_eq!(
            TOOLS[0].id,
            ToolId::NewNote,
            "the note stays under the cursor"
        );
        assert_eq!(tool(ToolId::NewNote).action, Shortcut::NewNote);
        assert_eq!(tool(ToolId::RecordScreen).action, Shortcut::ToggleRecording);
        assert_eq!(ToolId::parse("record"), Some(ToolId::RecordScreen));
        assert_eq!(ToolId::NewNote.id(), "note");
        // Recording is built (M8): switched on and idle, the menu offers both tools.
        let app = ToolContext {
            notes: true,
            recording_available: Shortcut::ToggleRecording.available(),
            recording_enabled: true,
            ..ToolContext::default()
        };
        assert_eq!(offered(&app), vec![ToolId::NewNote, ToolId::RecordScreen]);
        let running = ToolContext { recording: true, ..app };
        assert_eq!(offered(&running), vec![ToolId::NewNote]);
    }

    // --- Motion and drawing ---

    #[test]
    fn motion_runs_only_while_something_changes() {
        let mut m = MenuMotion::hidden();
        assert!(!m.frame(0.0).moving);
        m.open(Some(0), 1000.0, true);
        let f = m.frame(1000.0 + OPEN_MS / 2.0);
        assert!(f.moving && f.alpha > 0.0 && f.alpha < 1.0 && f.rise > 0.0);
        let f = m.frame(1000.0 + OPEN_MS + 1.0);
        assert!(!f.moving);
        assert_eq!((f.alpha, f.rise, f.highlight, f.lit), (1.0, 0.0, 0.0, 1.0));
        // The highlight slides to the second row.
        m.hover(Some(1), 2000.0);
        let mid = m.frame(2000.0 + SLIDE_MS / 4.0);
        assert!(mid.moving && mid.highlight > 0.0 && mid.highlight < 1.0);
        assert_eq!(m.frame(2000.0 + SLIDE_MS + 1.0).highlight, 1.0);
        assert!(!m.frame(2000.0 + SLIDE_MS + 1.0).moving);
        // Chosen: a flash, then the fade, then gone.
        m.choose(1, 3000.0);
        m.close(3000.0, true);
        let f = m.frame(3000.0 + FLASH_MS / 2.0);
        assert!(f.flash > 0.0 && f.alpha == 1.0 && f.moving && !f.gone);
        let f = m.frame(3000.0 + FLASH_MS + CLOSE_MS / 2.0);
        assert!(f.alpha > 0.0 && f.alpha < 1.0);
        let f = m.frame(3000.0 + FLASH_MS + CLOSE_MS + 1.0);
        assert!(f.gone && !f.moving);
        // Closing without a choice fades at once.
        let mut m = MenuMotion::hidden();
        m.open(Some(0), 0.0, true);
        m.close(500.0, true);
        assert!(m.frame(500.0 + CLOSE_MS / 2.0).alpha < 1.0);
        assert!(m.frame(500.0 + CLOSE_MS + 1.0).gone);
    }

    #[test]
    fn reduced_motion_is_instant() {
        let mut m = MenuMotion::hidden();
        m.open(Some(0), 0.0, false);
        let f = m.frame(0.0);
        assert_eq!((f.alpha, f.rise, f.moving), (1.0, 0.0, false));
        m.hover(Some(1), 10.0);
        assert_eq!(m.frame(10.0).highlight, 1.0);
        assert!(!m.frame(10.0).moving);
        m.choose(1, 20.0);
        m.close(20.0, true);
        let f = m.frame(20.0);
        assert!(f.gone && !f.moving && f.flash == 0.0);
    }

    #[test]
    fn the_menu_draws_inside_its_window() {
        let rows = vec![
            (ToolId::NewNote, Vec::new()),
            (
                ToolId::RecordScreen,
                vec!["Ctrl".into(), "Shift".into(), "6".into()],
            ),
        ];
        for scale in [1.0, 1.25] {
            for dark in [false, true] {
                let width = menu_width(&rows, scale);
                assert!(width >= MENU_MIN_WIDTH);
                let layout = menu_layout((300, 300), 2, width, WORK, scale);
                let pixmap = render_menu(&MenuPicture {
                    rows: &rows,
                    layout: &layout,
                    dark,
                    highlight: 0.0,
                    lit: 1.0,
                    flash: 0.0,
                })
                .expect("drawn");
                assert_eq!(pixmap.width() as i32, layout.window.width());
                let alpha = |x: i32, y: i32| {
                    pixmap
                        .pixel(
                            (x - layout.window.left) as u32,
                            (y - layout.window.top) as u32,
                        )
                        .unwrap()
                        .alpha()
                };
                // Corners of the window are clear; the card is opaque.
                assert_eq!(alpha(layout.window.left, layout.window.top), 0);
                assert_eq!(alpha(layout.window.right - 1, layout.window.top), 0);
                let row = layout.row(1);
                assert_eq!(alpha(row.left + 3, (row.top + row.bottom) / 2), 255);
                // Fading multiplies every channel.
                let mut faded = pixmap.clone();
                apply_alpha(&mut faded, 0.5);
                let x = (row.left + 3 - layout.window.left) as u32;
                let y = ((row.top + row.bottom) / 2 - layout.window.top) as u32;
                assert!((i32::from(faded.pixel(x, y).unwrap().alpha()) - 128).abs() <= 1);
            }
        }
    }

    #[test]
    fn modifier_flag_parses() {
        assert_eq!(Modifier::parse("alt"), Some(Modifier::Alt));
        assert_eq!(Modifier::parse("Ctrl-Alt"), Some(Modifier::CtrlAlt));
        assert_eq!(Modifier::parse("shift"), None);
    }
}
