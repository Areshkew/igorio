//! Keyboard shortcuts the user chooses (`ROADMAP.md`, "Shortcuts the user chooses"): the
//! actions, the chords, the rules a chord must pass, their place in `settings.json`, and the
//! global hotkey service ([`Hotkeys`]).
//!
//! ## Chords and layouts
//!
//! A [`Chord`] stores the virtual-key code, not a character, so a shortcut keeps working
//! when the user switches keyboard layout. Letters and digits are safe: Windows gives the key
//! labelled "Z" the code `VK_Z` on every Latin layout (on German QWERTZ that is the key where
//! US has "Y"), and the digit row is `VK_0` to `VK_9` everywhere, AZERTY included, where the
//! key cap shows the digit as its shifted character. The punctuation keys (`VK_OEM_*`) are
//! named by their US label in [`Chord::display`], which is pure and the same on every
//! machine: the key right of "P" is shown as "[" even on a German layout where it types "ü".
//! [`display_for_layout`] shows the character the active layout gives that key instead
//! (`MapVirtualKeyW` with `MAPVK_VK_TO_CHAR`); the settings window should use it for what the
//! user reads, and keep `display` for `settings.json`, which must not change with the layout.
//!
//! ## The app's own input
//!
//! `input.rs` sends `Ctrl+V`, `Ctrl+Shift+V`, `Right` and `Escape` through `SendInput` to
//! paste. A registered hotkey fires on injected input too, so those chords must never be
//! registered: [`Chord::validate`] refuses them, and every chord in a [`Shortcuts`] passed
//! validation. That is the guard; the hotkey service does not filter injected input.
//!
//! ## The screenshot keys
//!
//! `snip.rs` watches `PrtScn` and `Win+Shift+S` with a low-level keyboard hook. Low-level
//! hooks see a key before Windows matches it against registered hotkeys, so a hotkey never
//! hides a key from that hook. The screenshot keys themselves are refused, so Windows keeps
//! handling them as the user set them up.
//!
//! ## Hotkeys and the foreground window (what is known)
//!
//! - `RegisterHotKey` documentation: "Keyboard shortcuts that involve the WINDOWS key are
//!   reserved for use by the operating system", and F12 is reserved for the debugger. Hence
//!   the Win and F12 rules in [`Chord::validate`]. `MOD_WIN` is accepted by the API, and a
//!   Win chord that Windows does not use (for example `Win+Alt+F9`) is expected to register
//!   and fire; that is not verified on this machine.
//! - `WM_HOTKEY` is posted by the system to the registering thread; it is not input sent by
//!   the foreground program, so UIPI does not apply and hotkeys are expected to fire while an
//!   elevated window is in front. Not verified here (it needs a person at an elevated window).
//! - Full-screen games: a hotkey is matched before the game reads the key, so it fires and
//!   the game does not see the key. A game (or its anti-cheat) that installs its own
//!   low-level keyboard hook and swallows keys can stop it; exclusive full-screen DirectX is
//!   not known to block it. Not verified.
//! - The secure desktop (UAC prompt, lock screen, Ctrl+Alt+Del screen) gets no hotkeys.

use std::collections::VecDeque;
use std::fmt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, ERROR_HOTKEY_ALREADY_REGISTERED, HANDLE, HWND, LPARAM, WAIT_OBJECT_0,
    WPARAM,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, HOT_KEY_MODIFIERS, MAPVK_VK_TO_CHAR, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, MapVirtualKeyW, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PostMessageW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_HOTKEY,
};
use windows::core::{Error, HRESULT, w};

use crate::hook;
use crate::input::Held;

// MARK: Actions

/// Something a shortcut can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    RestoreLast,
    DiscardTop,
    DiscardAll,
    OpenSettings,
    NewNote,
    ToggleRecording,
    Pen,
    Highlighter,
}

impl Action {
    /// Every action, in the order of the settings rows. This order also decides which action
    /// keeps a chord that two actions share in `settings.json`.
    pub const ALL: &[Action] = &[
        Action::RestoreLast,
        Action::DiscardTop,
        Action::DiscardAll,
        Action::OpenSettings,
        Action::NewNote,
        Action::ToggleRecording,
        Action::Pen,
        Action::Highlighter,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// Stable id used in settings.json ("restore_last", "discard_top", ...).
    pub fn id(self) -> &'static str {
        match self {
            Action::RestoreLast => "restore_last",
            Action::DiscardTop => "discard_top",
            Action::DiscardAll => "discard_all",
            Action::OpenSettings => "open_settings",
            Action::NewNote => "new_note",
            Action::ToggleRecording => "toggle_recording",
            Action::Pen => "pen",
            Action::Highlighter => "highlighter",
        }
    }

    pub fn from_id(id: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|action| action.id() == id)
    }

    /// Text for the settings row, from the ROADMAP table.
    pub fn label(self) -> &'static str {
        match self {
            Action::RestoreLast => "Restore last screenshot",
            Action::DiscardTop => "Discard top card",
            Action::DiscardAll => "Discard all cards",
            Action::OpenSettings => "Open settings",
            Action::NewNote => "New note at the cursor",
            Action::ToggleRecording => "Start or stop recording",
            Action::Pen => "Pen",
            Action::Highlighter => "Highlighter",
        }
    }

    /// The shortcut a fresh install has, from the ROADMAP table: Ctrl+Shift+6, 7 and 8 for
    /// recording, pen and highlighter.
    pub fn default_chord(self) -> Option<Chord> {
        let ctrl_shift = |vk: u16| Chord {
            ctrl: true,
            alt: false,
            shift: true,
            win: false,
            key: Key(vk),
        };
        match self {
            Action::ToggleRecording => Some(ctrl_shift(0x36)),
            Action::Pen => Some(ctrl_shift(0x37)),
            Action::Highlighter => Some(ctrl_shift(0x38)),
            _ => None,
        }
    }

    /// False for an action whose feature is not built yet: shown disabled, and never
    /// registered. Every feature is built now (recording since M8); the rule stays for the
    /// next action that arrives before its feature.
    pub fn available(self) -> bool {
        true
    }

    /// Registered only while a recording runs ([`Hotkeys::set_recording`]). Pen and
    /// Highlighter default to Ctrl+Shift+7 and Ctrl+Shift+8, which the note editor uses for
    /// its numbered and bulleted lists: while no recording runs those keys stay free for it
    /// (and for every other program), and a pen has nothing to draw on anyway.
    pub fn recording_only(self) -> bool {
        matches!(self, Action::Pen | Action::Highlighter)
    }
}

// MARK: Keys

/// A non-modifier key, by virtual-key code, restricted to keys that make sense in a
/// shortcut: letters, digits, F1 to F24, the named keys, the numpad and the OEM punctuation
/// keys. Escape and PrtScn are included so that they can be named, and refused, by
/// [`Chord::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(u16);

const LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &str = "0123456789";
const FUNCTION_KEYS: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];
const NUMPAD_DIGITS: [&str; 10] = [
    "Num 0", "Num 1", "Num 2", "Num 3", "Num 4", "Num 5", "Num 6", "Num 7", "Num 8", "Num 9",
];

const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_SPACE: u16 = 0x20;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_SNAPSHOT: u16 = 0x2C;
const VK_DELETE: u16 = 0x2E;
const VK_PAUSE: u16 = 0x13;
const VK_F1: u16 = 0x70;
const VK_F4: u16 = 0x73;
const VK_F12: u16 = 0x7B;
const VK_F24: u16 = 0x87;
const VK_NUMPAD0: u16 = 0x60;
const VK_NUMPAD9: u16 = 0x69;
const VK_ADD: u16 = 0x6B;
const VK_SUBTRACT: u16 = 0x6D;
const VK_S: u16 = 0x53;
const VK_V: u16 = 0x56;

/// Named keys that are not a letter, digit, function key or numpad digit. No name contains
/// "+", the separator, so "Num Plus" spells out the numpad add key.
const NAMED_KEYS: &[(u16, &str)] = &[
    (VK_BACK, "Backspace"),
    (VK_TAB, "Tab"),
    (VK_RETURN, "Enter"),
    (VK_PAUSE, "Pause"),
    (VK_ESCAPE, "Esc"),
    (VK_SPACE, "Space"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x23, "End"),
    (VK_HOME, "Home"),
    (VK_LEFT, "Left"),
    (0x26, "Up"),
    (VK_RIGHT, "Right"),
    (VK_DOWN, "Down"),
    (VK_SNAPSHOT, "PrtScn"),
    (0x2D, "Insert"),
    (VK_DELETE, "Delete"),
    (0x6A, "Num *"),
    (VK_ADD, "Num Plus"),
    (VK_SUBTRACT, "Num -"),
    (0x6E, "Num ."),
    (0x6F, "Num /"),
    // The OEM keys, by their label on the US layout (see the module comment).
    (0xBA, ";"),
    (0xBB, "="),
    (0xBC, ","),
    (0xBD, "-"),
    (0xBE, "."),
    (0xBF, "/"),
    (0xC0, "`"),
    (0xDB, "["),
    (0xDC, "\\"),
    (0xDD, "]"),
    (0xDE, "'"),
];

/// Other spellings `parse` accepts, already lower case without spaces.
const KEY_ALIASES: &[(&str, u16)] = &[
    ("escape", VK_ESCAPE),
    ("return", VK_RETURN),
    ("del", VK_DELETE),
    ("ins", 0x2D),
    ("pgup", 0x21),
    ("pgdn", 0x22),
    ("printscreen", VK_SNAPSHOT),
    ("prtsc", VK_SNAPSHOT),
    ("break", VK_PAUSE),
];

impl Key {
    /// The key for a virtual-key code, if it is one a shortcut may use.
    pub fn from_vk(vk: u16) -> Option<Key> {
        name_of(vk).map(|_| Key(vk))
    }

    pub fn vk(self) -> u16 {
        self.0
    }

    /// The name used by [`Chord::display`] (US labels for punctuation).
    pub fn name(self) -> &'static str {
        name_of(self.0).unwrap_or("?")
    }

    /// Every key a shortcut may use.
    pub fn all() -> impl Iterator<Item = Key> {
        (0u16..=0xFF).filter_map(Key::from_vk)
    }

    fn is_function_key(self) -> bool {
        (VK_F1..=VK_F24).contains(&self.0)
    }

    fn is_letter(self) -> bool {
        (0x41..=0x5A).contains(&self.0)
    }

    fn is_digit(self) -> bool {
        (0x30..=0x39).contains(&self.0)
    }

    fn is_oem(self) -> bool {
        self.0 >= 0xBA
    }
}

fn name_of(vk: u16) -> Option<&'static str> {
    let at = |text: &'static str, i: u16| &text[i as usize..i as usize + 1];
    match vk {
        0x41..=0x5A => Some(at(LETTERS, vk - 0x41)),
        0x30..=0x39 => Some(at(DIGITS, vk - 0x30)),
        VK_F1..=VK_F24 => Some(FUNCTION_KEYS[(vk - VK_F1) as usize]),
        VK_NUMPAD0..=VK_NUMPAD9 => Some(NUMPAD_DIGITS[(vk - VK_NUMPAD0) as usize]),
        _ => NAMED_KEYS
            .iter()
            .find(|(code, _)| *code == vk)
            .map(|(_, name)| *name),
    }
}

/// Lower case without spaces, for comparing what a person typed.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn key_from_name(token: &str) -> Option<Key> {
    let wanted = squash(token);
    if let Some((_, vk)) = KEY_ALIASES.iter().find(|(alias, _)| *alias == wanted) {
        return Some(Key(*vk));
    }
    Key::all().find(|key| squash(key.name()) == wanted)
}

// MARK: Chords

/// One key with the modifiers held with it. The left and right modifier keys are the same
/// (that is how `RegisterHotKey` matches them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: Key,
}

/// Why a chord cannot be used. [`fmt::Display`] gives the text for the settings row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordError {
    /// Needs Ctrl, Alt or Win (only F1 to F24 may stand alone or with Shift alone).
    NoModifier,
    /// Windows uses this chord itself. The text says what it does.
    ReservedByWindows(&'static str),
    /// PrtScn or Win+Shift+S: the screenshot keys stay with Windows.
    ScreenshotKey,
    /// The app sends this chord itself to paste. The text says why.
    UsedByApp(&'static str),
    /// `parse` met a word that is not a modifier or a known key, or two keys.
    UnknownKey(String),
    /// Nothing, or only modifiers.
    Empty,
}

impl fmt::Display for ChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChordError::NoModifier => {
                f.write_str("Add Ctrl, Alt or Win. Only F1 to F24 work alone or with Shift alone.")
            }
            ChordError::ReservedByWindows(why) => write!(f, "Windows uses this shortcut: {why}."),
            ChordError::ScreenshotKey => {
                f.write_str("This is a Windows screenshot shortcut. Change it in Windows settings.")
            }
            ChordError::UsedByApp(why) => write!(f, "Igorio uses this keystroke: {why}."),
            ChordError::UnknownKey(key) => write!(f, "Unknown key \"{key}\"."),
            ChordError::Empty => f.write_str("Press a key together with the modifiers."),
        }
    }
}

impl std::error::Error for ChordError {}

impl Chord {
    /// A chord with no modifiers.
    pub fn key(key: Key) -> Chord {
        Chord {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            key,
        }
    }

    fn modifier_names(&self) -> impl Iterator<Item = &'static str> {
        [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.win, "Win"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
    }

    /// "Ctrl+Shift+6", "Win+Alt+F9": modifiers in the order Ctrl, Alt, Shift, Win, as Windows
    /// shows them, then the key by its US name. The same on every machine; this is the text
    /// stored in settings.json.
    pub fn display(&self) -> String {
        self.chips().join("+")
    }

    /// Reads what [`display`](Self::display) writes. Case does not matter and spaces are
    /// allowed ("ctrl + shift + num 6"). A few other spellings are accepted (Control,
    /// Windows, Escape, Del, PgUp, PrintScreen). Only the syntax is checked here; call
    /// [`validate`](Self::validate) before using the chord.
    pub fn parse(text: &str) -> Result<Chord, ChordError> {
        if text.trim().is_empty() {
            return Err(ChordError::Empty);
        }
        let mut chord = Chord::key(Key(0));
        let mut key = None;
        for token in text.split('+') {
            match squash(token).as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "alt" => chord.alt = true,
                "shift" => chord.shift = true,
                "win" | "windows" => chord.win = true,
                _ => match key_from_name(token) {
                    Some(found) if key.is_none() => key = Some(found),
                    _ => return Err(ChordError::UnknownKey(token.trim().to_owned())),
                },
            }
        }
        chord.key = key.ok_or(ChordError::Empty)?;
        Ok(chord)
    }

    /// The pieces for the key-chip drawing in the settings window: ["Ctrl", "Shift", "6"].
    pub fn chips(&self) -> Vec<String> {
        self.modifier_names()
            .chain([self.key.name()])
            .map(str::to_owned)
            .collect()
    }

    /// Checks the rules of `ROADMAP.md`. In order: the screenshot keys, the chords Windows
    /// uses, the chords this app sends, then the modifier rule.
    ///
    /// All Win chords on letters, digits, arrows, punctuation and the other keys Windows
    /// binds are refused, not only the ones in use today: the `RegisterHotKey` documentation
    /// reserves every Win shortcut for Windows, Windows 11 already uses nearly every
    /// Win+letter (A, B, C, D, E, G, H, I, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Z, and
    /// more with Shift, Ctrl or Alt), and updates add new ones (Win+C became Copilot), so a
    /// letter free today can be taken by the next update. Win with F13 to F24, Insert,
    /// Delete, End, PageUp, PageDown and the numpad digits stays allowed: Windows binds none
    /// of them.
    pub fn validate(&self) -> Result<(), ChordError> {
        let Chord {
            ctrl,
            alt,
            shift,
            win,
            key,
        } = *self;
        let vk = key.vk();
        let only = |c: bool, a: bool, s: bool, w: bool| (ctrl, alt, shift, win) == (c, a, s, w);

        // The screenshot keys: PrtScn with any modifiers (PrtScn, Alt+PrtScn, Win+PrtScn and
        // the rest go to Windows' own capture), and Win+Shift+S (Snipping Tool). snip.rs
        // watches these; they are set up in Windows, not here.
        if vk == VK_SNAPSHOT || (win && shift && vk == VK_S) {
            return Err(ChordError::ScreenshotKey);
        }

        // Chords Windows keeps for itself.
        let reserved = if ctrl && alt && vk == VK_DELETE {
            // The secure attention sequence: handled by Winlogon, never given to programs.
            Some("Ctrl+Alt+Del opens the security screen")
        } else if vk == VK_F12 {
            // RegisterHotKey documentation: F12 is reserved for the debugger at all times.
            Some("F12 is reserved for the debugger")
        } else if alt && vk == VK_TAB {
            // Alt+Tab, Alt+Shift+Tab, Ctrl+Alt+Tab: the window switcher.
            Some("Alt+Tab switches windows")
        } else if alt && !ctrl && !win && vk == VK_F4 {
            // Alt+F4 closes the active window in every program.
            Some("Alt+F4 closes the active window")
        } else if ctrl && shift && !alt && !win && vk == VK_ESCAPE {
            // Ctrl+Shift+Esc opens Task Manager.
            Some("Ctrl+Shift+Esc opens Task Manager")
        } else if ctrl && !alt && !win && vk == VK_ESCAPE {
            // Ctrl+Esc opens Start.
            Some("Ctrl+Esc opens Start")
        } else if alt && !win && vk == VK_ESCAPE {
            // Alt+Esc and Alt+Shift+Esc cycle through the windows.
            Some("Alt+Esc cycles through windows")
        } else if alt && !ctrl && !win && vk == VK_SPACE {
            // Alt+Space opens the window menu of the active window.
            Some("Alt+Space opens the window menu")
        } else if win && vk == 0x4C {
            // Win+L locks the computer; it cannot be registered or blocked at all.
            Some("Win+L locks the computer")
        } else if win && key.is_letter() {
            Some("Windows keeps Win+letter shortcuts for itself (Win+D, Win+E, Win+V, ...)")
        } else if win && key.is_digit() {
            // Win+1..0 start or switch to taskbar apps; with Shift, Ctrl or Alt too.
            Some("Win+1 to Win+0 open the taskbar apps")
        } else if win && matches!(vk, VK_LEFT..=VK_DOWN | VK_HOME) {
            // Win+arrows snap and move windows, Win+Home minimizes the others.
            Some("Win+arrows and Win+Home arrange windows")
        } else if win && matches!(vk, VK_TAB | VK_SPACE | VK_RETURN | VK_ESCAPE | VK_PAUSE) {
            // Win+Tab task view, Win+Space input language, Ctrl+Win+Enter Narrator,
            // Win+Esc closes Magnifier, Win+Pause the About page.
            Some("Windows uses this Win shortcut")
        } else if win && (key.is_oem() || matches!(vk, VK_ADD | VK_SUBTRACT)) {
            // Win+. and Win+; emoji, Win+, peek, Win+= and Win+- Magnifier, Win+/ IME.
            Some("Windows uses Win with punctuation keys")
        } else if win && vk == VK_F1 {
            // Win+F1 opens Windows help.
            Some("Win+F1 opens Windows help")
        } else if win && ctrl && vk == VK_F4 {
            // Win+Ctrl+F4 closes the current virtual desktop.
            Some("Win+Ctrl+F4 closes the virtual desktop")
        } else {
            None
        };
        if let Some(why) = reserved {
            return Err(ChordError::ReservedByWindows(why));
        }

        // Chords input.rs sends to paste. A registered hotkey would catch our own keystrokes.
        if only(true, false, false, false) && vk == VK_V {
            return Err(ChordError::UsedByApp("Ctrl+V pastes the screenshot"));
        }
        if only(true, false, true, false) && vk == VK_V {
            return Err(ChordError::UsedByApp("Ctrl+Shift+V pastes into terminals"));
        }
        if only(false, false, false, false) && vk == VK_ESCAPE {
            return Err(ChordError::UsedByApp(
                "Esc closes a whiteboard text box before a paste",
            ));
        }
        if only(false, false, false, false) && vk == VK_RIGHT {
            return Err(ChordError::UsedByApp(
                "Right clears the selection before a paste",
            ));
        }

        // At least Ctrl, Alt or Win. Shift alone counts as no modifier, because Shift with a
        // letter, digit or punctuation key types a character and with the other keys it
        // selects text. Function keys may stand alone.
        if !ctrl && !alt && !win && !key.is_function_key() {
            return Err(ChordError::NoModifier);
        }
        Ok(())
    }

    fn modifiers(&self) -> HOT_KEY_MODIFIERS {
        let mut mods = MOD_NOREPEAT;
        for (on, flag) in [
            (self.ctrl, MOD_CONTROL),
            (self.alt, MOD_ALT),
            (self.shift, MOD_SHIFT),
            (self.win, MOD_WIN),
        ] {
            if on {
                mods |= flag;
            }
        }
        mods
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Like [`Chord::display`], but a punctuation key shows the character the calling thread's
/// keyboard layout gives it ("Ctrl+Alt+ü" on German instead of "Ctrl+Alt+["). Letters,
/// digits and named keys are the same as in `display`. Use it for what the user reads; store
/// `display` in settings.json.
pub fn display_for_layout(chord: &Chord) -> String {
    let mut chips = chord.chips();
    if chord.key.is_oem() {
        // SAFETY: plain value arguments. Uses the input language of the calling thread.
        let mapped = unsafe { MapVirtualKeyW(chord.key.vk() as u32, MAPVK_VK_TO_CHAR) };
        // The top bit marks a dead key; the rest is still its character.
        let shown = char::from_u32(mapped & 0x7FFF_FFFF).filter(|c| *c != '\0' && !c.is_control());
        if let (Some(shown), Some(last)) = (shown, chips.last_mut()) {
            *last = shown.to_uppercase().collect();
        }
    }
    chips.join("+")
}

// MARK: The recorder

/// What one key press means to the shortcut recorder in the settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderInput {
    /// A full chord was pressed. It may still fail [`Shortcuts::set`]; show that error.
    Chord(Chord),
    /// Esc alone: stop recording, keep the old shortcut.
    Cancel,
    /// Backspace or Delete alone: the action gets no shortcut.
    Clear,
    /// A modifier alone, or a key a shortcut cannot use: keep waiting.
    Pending,
}

/// Virtual keys that only modify others (Shift, Ctrl, Alt, Win, left and right).
const MODIFIER_KEYS: &[u16] = &[
    0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5,
];

/// Turns a `WM_KEYDOWN` or `WM_SYSKEYDOWN` (its `wParam`) and the modifiers held at that
/// message ([`held_for_message`]) into what the recorder should do.
pub fn chord_from_key_event(vk: u16, held: Held) -> RecorderInput {
    if MODIFIER_KEYS.contains(&vk) {
        return RecorderInput::Pending;
    }
    let bare = !held.ctrl && !held.alt && !held.shift && !held.win;
    if bare && vk == VK_ESCAPE {
        return RecorderInput::Cancel;
    }
    if bare && (vk == VK_BACK || vk == VK_DELETE) {
        return RecorderInput::Clear;
    }
    match Key::from_vk(vk) {
        Some(key) => RecorderInput::Chord(Chord {
            ctrl: held.ctrl,
            alt: held.alt,
            shift: held.shift,
            win: held.win,
            key,
        }),
        None => RecorderInput::Pending,
    }
}

/// The modifiers as they were when the key message being handled was posted
/// (`GetKeyState`, which follows the thread's message queue). Call it from the window
/// procedure while handling that message; `input::held` reads the live state instead.
pub fn held_for_message() -> Held {
    // SAFETY: plain value argument. The high bit is set while the key is down.
    let down = |vk: i32| unsafe { GetKeyState(vk) } < 0;
    Held {
        shift: down(0x10),
        ctrl: down(0x11),
        alt: down(0x12),
        win: down(0x5B) || down(0x5C),
    }
}

// MARK: The user's choice

/// What the user chose: every action maps to a chord or to none. Every chord in it passed
/// [`Chord::validate`] and no two actions share one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcuts {
    chords: [Option<Chord>; 8],
}

/// Why [`Shortcuts::set`] refused a chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetError {
    Invalid(ChordError),
    /// Another action already has this chord.
    InUse(Action),
}

impl fmt::Display for SetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetError::Invalid(error) => error.fmt(f),
            SetError::InUse(action) => write!(f, "Already used by \"{}\".", action.label()),
        }
    }
}

impl std::error::Error for SetError {}

impl Default for Shortcuts {
    fn default() -> Self {
        Shortcuts::defaults()
    }
}

impl Shortcuts {
    pub fn defaults() -> Shortcuts {
        let mut chords = [None; 8];
        for action in Action::ALL {
            chords[action.index()] = action.default_chord();
        }
        Shortcuts { chords }
    }

    /// No shortcut for any action.
    pub fn none() -> Shortcuts {
        Shortcuts { chords: [None; 8] }
    }

    pub fn get(&self, action: Action) -> Option<Chord> {
        self.chords[action.index()]
    }

    /// Sets or clears one action. Refuses an invalid chord and a chord another action uses
    /// (the error names that action).
    pub fn set(&mut self, action: Action, chord: Option<Chord>) -> Result<(), SetError> {
        if let Some(chord) = chord {
            chord.validate().map_err(SetError::Invalid)?;
            if let Some(other) = self.action_for(chord).filter(|other| *other != action) {
                return Err(SetError::InUse(other));
            }
        }
        self.chords[action.index()] = chord;
        Ok(())
    }

    /// Gives the action its default chord. If another action holds that chord, the other
    /// action loses it (the user asked for the default explicitly; the settings window
    /// redraws every row, so the cleared row shows).
    pub fn reset(&mut self, action: Action) {
        let default = action.default_chord();
        if let Some(chord) = default
            && let Some(other) = self.action_for(chord)
        {
            self.chords[other.index()] = None;
        }
        self.chords[action.index()] = default;
    }

    pub fn action_for(&self, chord: Chord) -> Option<Action> {
        Action::ALL
            .iter()
            .copied()
            .find(|action| self.get(*action) == Some(chord))
    }

    /// Reads the `shortcuts` value of settings.json leniently: an action that is missing
    /// takes its default, an unknown id is ignored, a chord that does not parse or validate
    /// gives that action its default, a value that is not a string or null too. When two
    /// actions end up with one chord, the first in `Action::ALL` keeps it and the other gets
    /// none. A value that is not an object gives the defaults.
    pub fn from_json(value: &serde_json::Value) -> Shortcuts {
        let Some(map) = value.as_object() else {
            return Shortcuts::defaults();
        };
        let mut result = Shortcuts::none();
        for action in Action::ALL.iter().copied() {
            let wanted = match map.get(action.id()) {
                Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(text)) => Chord::parse(text)
                    .ok()
                    .filter(|chord| chord.validate().is_ok())
                    .or_else(|| action.default_chord()),
                _ => action.default_chord(),
            };
            // Only a clash is possible here: `wanted` is valid.
            let _ = result.set(action, wanted);
        }
        result
    }
}

impl Serialize for Shortcuts {
    /// An object from action id to chord text or null, in `Action::ALL` order.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(Action::ALL.len()))?;
        for action in Action::ALL {
            map.serialize_entry(action.id(), &self.get(*action).map(|chord| chord.display()))?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Shortcuts {
    /// Never fails on a well-formed JSON value; see [`Shortcuts::from_json`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Shortcuts::from_json(&value))
    }
}

// MARK: The hotkey service

/// One press of a registered shortcut.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressed {
    pub action: Action,
    /// `hook::now()` when the hotkey thread received it.
    pub time: f64,
}

/// How an action's shortcut stands with Windows.
#[derive(Debug, Clone, PartialEq)]
pub enum Registration {
    /// Registered: pressing it anywhere runs the action.
    Active,
    /// The action has no shortcut.
    NotSet,
    /// The action's feature is not built yet, so its shortcut is not registered.
    Unavailable,
    /// A recording-only action ([`Action::recording_only`]) while no recording runs: its
    /// shortcut is registered when one starts.
    WhileRecording,
    /// Another program holds the chord (`ERROR_HOTKEY_ALREADY_REGISTERED`).
    Taken,
    /// `RegisterHotKey` failed for another reason.
    Failed(String),
}

/// The rule the hotkey thread follows for one action: `Ok` registers the chord, `Err` is the
/// status of an action it leaves unregistered. `recording`: a recording runs now.
pub fn registration_plan(
    action: Action,
    chord: Option<Chord>,
    recording: bool,
) -> Result<Chord, Registration> {
    match chord {
        _ if !action.available() => Err(Registration::Unavailable),
        None => Err(Registration::NotSet),
        Some(_) if action.recording_only() && !recording => Err(Registration::WhileRecording),
        Some(chord) => Ok(chord),
    }
}

/// Most presses kept between two drains. More are dropped (a person cannot press faster).
const QUEUE_CAPACITY: usize = 64;
/// How long `shutdown` and `Drop` wait for the hotkey thread.
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
/// Window messages to the hotkey thread's window.
const WM_APPLY: u32 = WM_APP + 1;
const WM_PAUSE: u32 = WM_APP + 2;
const WM_STOP: u32 = WM_APP + 3;
const WM_REPORT: u32 = WM_APP + 4;
const WM_RECORDING: u32 = WM_APP + 5;

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

/// State shared by the hotkey thread and the owner.
struct Shared {
    queue: Mutex<VecDeque<Pressed>>,
    wake: Event,
    status: Mutex<Vec<(Action, Registration)>>,
    reports: Mutex<Vec<RegistrationReport>>,
    /// The latest `apply`, not yet taken by the thread.
    pending: Mutex<Option<Shortcuts>>,
    paused: AtomicBool,
    /// A recording runs: the recording-only actions are registered.
    recording: AtomicBool,
}

/// A status snapshot taken after all earlier commands on the hotkey thread.
pub struct RegistrationReport {
    pub tag: usize,
    pub entries: Vec<(Action, Registration)>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The global shortcuts: one thread with a hidden message-only window and its own message
/// loop registers the chords (`RegisterHotKey` with `MOD_NOREPEAT`; registrations belong to
/// the thread whose window they name) and turns `WM_HOTKEY` into [`Pressed`] events. The
/// owner waits on [`wake_handle`](Self::wake_handle) and calls [`drain`](Self::drain).
///
/// ## Pause
///
/// While paused, every hotkey is unregistered, and `WM_HOTKEY` messages still in the queue
/// are dropped. Unregistering is the only way: Windows consumes a registered hotkey's key
/// press before any window sees it, so with the registrations kept the recorder could never
/// receive an existing shortcut to record it (it would simply fire, or vanish if dropped).
/// On resume the thread registers the latest shortcuts again. `status` keeps its last
/// values while paused, and is refreshed on resume.
///
/// ## While recording
///
/// The recording-only actions ([`Action::recording_only`]: Pen and Highlighter) are
/// registered only between `set_recording(true)` and `set_recording(false)`. Otherwise their
/// status is [`Registration::WhileRecording`] and their keys reach the foreground program
/// (the note editor uses them for lists). Like pause, the change is made on the hotkey
/// thread, in order with apply and pause.
pub struct Hotkeys {
    thread: Option<JoinHandle<()>>,
    /// The thread's message-only window, as an integer (`HWND` is not `Send`).
    window: isize,
    shared: Arc<Shared>,
}

impl Hotkeys {
    /// Starts the hotkey thread (hidden message-only window, its own message loop) and
    /// registers the chords of all available actions. Returns once they are registered, so
    /// `status` is already filled.
    pub fn start(shortcuts: &Shortcuts) -> windows::core::Result<Hotkeys> {
        hook::now();
        // SAFETY: no pointers are passed; the returned handle is owned by `Event`.
        let wake = Event(unsafe { CreateEventW(None, false, false, None)? });
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::with_capacity(QUEUE_CAPACITY)),
            wake,
            status: Mutex::new(Vec::new()),
            reports: Mutex::new(Vec::new()),
            pending: Mutex::new(None),
            paused: AtomicBool::new(false),
            recording: AtomicBool::new(false),
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let initial = shortcuts.clone();
        let thread = thread::Builder::new()
            .name("hotkeys".into())
            .spawn(move || run(thread_shared, initial, ready_tx))
            .map_err(|_| Error::from(E_FAIL))?;
        match ready_rx.recv() {
            Ok(Ok(window)) => Ok(Hotkeys {
                thread: Some(thread),
                window,
                shared,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(Error::from(E_FAIL))
            }
        }
    }

    fn post(&self, message: u32, wparam: usize) -> bool {
        // SAFETY: posts a message with no pointers to our own window. If the window is gone
        // the call fails and nothing else happens.
        unsafe {
            PostMessageW(
                Some(HWND(self.window as *mut _)),
                message,
                WPARAM(wparam),
                LPARAM(0),
            )
        }
        .is_ok()
    }

    /// Replaces the registrations with a new set: unregisters what changed, registers the
    /// new ones. Never blocks: the work happens on the hotkey thread; the result arrives in
    /// [`status`](Self::status).
    pub fn apply(&self, shortcuts: &Shortcuts) {
        *lock(&self.shared.pending) = Some(shortcuts.clone());
        self.post(WM_APPLY, 0);
    }

    /// The latest known registration of every action, in `Action::ALL` order.
    pub fn status(&self) -> Vec<(Action, Registration)> {
        lock(&self.shared.status).clone()
    }

    /// Requests a tagged status on the same queue as apply and pause.
    pub fn report_after_commands(&self, tag: usize) -> bool {
        self.post(WM_REPORT, tag)
    }

    /// Takes the reports made so far, oldest first. The wake handle is set when one arrives.
    pub fn reports(&self) -> Vec<RegistrationReport> {
        std::mem::take(&mut *lock(&self.shared.reports))
    }

    /// Appends the presses received so far to `out`, oldest first.
    pub fn drain(&self, out: &mut Vec<Pressed>) {
        out.extend(lock(&self.shared.queue).drain(..));
    }

    /// An auto-reset event, set when a press arrives in an empty queue. Do not close it.
    pub fn wake_handle(&self) -> HANDLE {
        self.shared.wake.0
    }

    /// While paused no hotkey fires and none is registered (see the type's comment). Takes
    /// effect at once for presses; the registrations follow on the hotkey thread.
    pub fn set_paused(&self, paused: bool) {
        if self.shared.paused.swap(paused, Relaxed) != paused {
            self.post(WM_PAUSE, 0);
        }
    }

    /// A recording started (`true`) or stopped: registers or unregisters the recording-only
    /// actions (see the type's comment). Never blocks; the result arrives in `status`.
    pub fn set_recording(&self, recording: bool) {
        if self.shared.recording.swap(recording, Relaxed) != recording {
            self.post(WM_RECORDING, 0);
        }
    }

    /// Posts `WM_HOTKEY` for `action` to the service's own window, as Windows would on a
    /// press. For the self-test: it exercises the message path without sending input.
    #[doc(hidden)]
    pub fn post_hotkey_for_test(&self, action: Action) -> bool {
        self.post(WM_HOTKEY, hotkey_id(action) as usize)
    }

    /// Unregisters everything and ends the thread. True if it ended within one second;
    /// otherwise it is left running detached.
    pub fn shutdown(mut self) -> bool {
        self.stop()
    }

    fn stop(&mut self) -> bool {
        let Some(thread) = self.thread.take() else {
            return true;
        };
        if !self.post(WM_STOP, 0) && !thread.is_finished() {
            return false;
        }
        let handle = HANDLE(thread.as_raw_handle());
        // SAFETY: `handle` is the live thread handle owned by `thread`, which outlives the wait.
        let ended = unsafe { WaitForSingleObject(handle, STOP_TIMEOUT.as_millis() as u32) }
            == WAIT_OBJECT_0;
        if ended {
            let _ = thread.join();
        }
        ended
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Hotkey ids: one per action, 1 to 8 (ids up to 0xBFFF are for applications).
fn hotkey_id(action: Action) -> i32 {
    action.index() as i32 + 1
}

fn action_for_id(id: usize) -> Option<Action> {
    id.checked_sub(1).and_then(|i| Action::ALL.get(i)).copied()
}

/// The hotkey thread's registrations.
struct Registrar {
    window: HWND,
    /// What is registered now, per action.
    registered: [Option<Chord>; 8],
}

impl Registrar {
    /// Makes the registrations match `target` (None: nothing registered) and returns the
    /// status. Everything that changes is unregistered first, so two actions can swap chords.
    fn sync(&mut self, target: Option<&Shortcuts>, recording: bool) -> Vec<(Action, Registration)> {
        let plan = |action: Action| {
            registration_plan(action, target.and_then(|shortcuts| shortcuts.get(action)), recording)
        };
        for action in Action::ALL.iter().copied() {
            if self.registered[action.index()].is_some()
                && self.registered[action.index()] != plan(action).ok()
            {
                // SAFETY: unregisters a hotkey this thread registered on its own window.
                let _ = unsafe { UnregisterHotKey(Some(self.window), hotkey_id(action)) };
                self.registered[action.index()] = None;
            }
        }
        let mut status = Vec::with_capacity(Action::ALL.len());
        for action in Action::ALL.iter().copied() {
            let state = match plan(action) {
                Err(state) => state,
                Ok(chord) if self.registered[action.index()] == Some(chord) => {
                    Registration::Active
                }
                Ok(chord) => {
                    // SAFETY: registers on this thread's own window; no pointers.
                    let result = unsafe {
                        RegisterHotKey(
                            Some(self.window),
                            hotkey_id(action),
                            chord.modifiers(),
                            chord.key.vk() as u32,
                        )
                    };
                    match result {
                        Ok(()) => {
                            self.registered[action.index()] = Some(chord);
                            Registration::Active
                        }
                        Err(error)
                            if error.code()
                                == HRESULT::from_win32(ERROR_HOTKEY_ALREADY_REGISTERED.0) =>
                        {
                            Registration::Taken
                        }
                        Err(error) => Registration::Failed(error.message()),
                    }
                }
            };
            status.push((action, state));
        }
        status
    }
}

/// The hotkey thread: creates the window, registers, then pumps messages until `WM_STOP`.
fn run(shared: Arc<Shared>, initial: Shortcuts, ready: mpsc::Sender<windows::core::Result<isize>>) {
    // SAFETY: creates a message-only window of the system "STATIC" class on this thread; no
    // pointers besides the static class name.
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            None,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    };
    let window = match window {
        Ok(window) => window,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut registrar = Registrar {
        window,
        registered: [None; 8],
    };
    let mut desired = initial;
    *lock(&shared.status) = registrar.sync(Some(&desired), shared.recording.load(Relaxed));
    let _ = ready.send(Ok(window.0 as isize));
    drop(ready);

    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid MSG for the calls to fill and read; this thread owns `window`.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        let ours = msg.hwnd == window;
        match msg.message {
            WM_HOTKEY if ours => {
                let action = action_for_id(msg.wParam.0)
                    .filter(|action| registrar.registered[action.index()].is_some());
                if let Some(action) = action
                    && !shared.paused.load(Relaxed)
                {
                    let mut queue = lock(&shared.queue);
                    if queue.len() < QUEUE_CAPACITY {
                        let was_empty = queue.is_empty();
                        queue.push_back(Pressed {
                            action,
                            time: hook::now(),
                        });
                        if was_empty {
                            // SAFETY: the event lives in `shared`, which this thread holds.
                            let _ = unsafe { SetEvent(shared.wake.0) };
                        }
                    }
                }
            }
            WM_APPLY if ours => {
                if let Some(next) = lock(&shared.pending).take() {
                    desired = next;
                }
                if !shared.paused.load(Relaxed) {
                    let status = registrar.sync(Some(&desired), shared.recording.load(Relaxed));
                    *lock(&shared.status) = status;
                }
            }
            WM_PAUSE if ours => {
                if shared.paused.load(Relaxed) {
                    registrar.sync(None, false);
                } else {
                    let status = registrar.sync(Some(&desired), shared.recording.load(Relaxed));
                    *lock(&shared.status) = status;
                }
            }
            // While paused nothing is registered; the resume uses the latest flag.
            WM_RECORDING if ours => {
                if !shared.paused.load(Relaxed) {
                    let status = registrar.sync(Some(&desired), shared.recording.load(Relaxed));
                    *lock(&shared.status) = status;
                }
            }
            WM_STOP if ours => break,
            WM_REPORT if ours => {
                let entries = lock(&shared.status).clone();
                lock(&shared.reports).push(RegistrationReport { tag: msg.wParam.0, entries });
                // SAFETY: owner and worker both hold the event through Shared.
                let _ = unsafe { SetEvent(shared.wake.0) };
            }
            _ => {
                // SAFETY: as above.
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }
    registrar.sync(None, false);
    // SAFETY: the window was created by this thread and is destroyed once.
    let _ = unsafe { DestroyWindow(window) };
}
