//! Synthetic keystrokes for the paste, from `Keys.swift`: `Ctrl+V`, and the Right arrow and
//! Escape that `PasteTarget.swift` presses before it.
//!
//! Rule 7: each call reads the modifier keys right before `SendInput`, fills scan codes from
//! the foreground window's keyboard layout, and sends all its events in one `SendInput` call,
//! so the user's own typing cannot land between them. The caller checks the foreground window
//! with [`foreground_is`] right before calling.
//!
//! Every event carries [`INJECTED_MARKER`] in `dwExtraInfo`, so the app's own low-level hooks
//! can tell these events from the user's (`KBDLLHOOKSTRUCT::dwExtraInfo`).
//!
//! `SendInput` cannot report that UIPI dropped the events (an elevated target from a normal
//! process): the call then still reports them all as sent.

use std::mem::size_of;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, HKL, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS,
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC_EX, MapVirtualKeyExW,
    SendInput, VIRTUAL_KEY, VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_LWIN,
    VK_MENU, VK_RIGHT, VK_RWIN, VK_SHIFT, VK_UP, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId,
};

/// `dwExtraInfo` of every event this module sends ("IGOR").
pub const INJECTED_MARKER: usize = 0x4947_4F52;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Shift,
    Ctrl,
    Alt,
    Win,
}

/// Result of one send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// Every event was sent.
    Done,
    /// The user is holding this modifier; nothing was sent. Wait and try again: with Shift,
    /// `Ctrl+V` becomes "paste as plain text" in Chrome and Slack.
    Blocked(Modifier),
    /// `SendInput` took only `sent` of `total` events; keys left down were released.
    Incomplete { sent: u32, total: u32 },
}

/// Keys pressed before the paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Collapses the word selection a double-click made.
    Right,
    /// Closes a whiteboard's fresh text box.
    Escape,
}

/// Modifier keys down right now (`GetAsyncKeyState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Held {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub win: bool,
}

pub fn held() -> Held {
    // SAFETY: plain value argument. The high bit is set while the key is down.
    let down = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } < 0;
    Held {
        shift: down(VK_SHIFT),
        ctrl: down(VK_CONTROL),
        alt: down(VK_MENU),
        win: down(VK_LWIN) || down(VK_RWIN),
    }
}

/// Presses `Ctrl+V`. If Ctrl is already down (the user holds it), sends only V.
pub fn paste_shortcut() -> Sent {
    let layout = foreground_layout();
    let ctrl = scan(VK_CONTROL, layout);
    let v = scan(VK_V, layout);
    match paste_inputs(held(), ctrl, v) {
        Ok(inputs) => send(&inputs),
        Err(modifier) => Sent::Blocked(modifier),
    }
}

/// Presses and releases one key, with no modifier: blocked while any modifier is down
/// (Ctrl+Right jumps a word; Ctrl+Escape opens Start).
pub fn press(key: Key) -> Sent {
    let vk = match key {
        Key::Right => VK_RIGHT,
        Key::Escape => VK_ESCAPE,
    };
    match key_inputs(held(), vk, scan(vk, foreground_layout())) {
        Ok(inputs) => send(&inputs),
        Err(modifier) => Sent::Blocked(modifier),
    }
}

/// What must still be in front for the paste to go to the right place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreground {
    /// The target window (any window inside the target top-level window matches).
    Window(HWND),
    Process(u32),
}

/// Whether the foreground window is the target. Call right before sending.
pub fn foreground_is(target: Foreground) -> bool {
    // SAFETY: no arguments.
    let front = unsafe { GetForegroundWindow() };
    if front.is_invalid() {
        return false;
    }
    match target {
        Foreground::Process(pid) => window_process(front) == Some(pid),
        // SAFETY: plain value arguments; an invalid handle returns null.
        Foreground::Window(window) => unsafe {
            let root = GetAncestor(window, GA_ROOT);
            !root.is_invalid() && root == GetAncestor(front, GA_ROOT)
        },
    }
}

/// The process of the foreground window.
pub fn foreground_process() -> Option<u32> {
    // SAFETY: no arguments.
    let front = unsafe { GetForegroundWindow() };
    (!front.is_invalid())
        .then(|| window_process(front))
        .flatten()
}

fn window_process(window: HWND) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: `pid` is a local.
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

/// The keyboard layout of the foreground window's thread, which is the one that will
/// translate the keys.
fn foreground_layout() -> HKL {
    // SAFETY: no pointers; a null window gives thread 0, whose layout is our own.
    unsafe {
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        GetKeyboardLayout(thread)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scan {
    code: u16,
    extended: bool,
}

/// Keys whose scan code needs the E0 prefix (`KEYEVENTF_EXTENDEDKEY`). `MapVirtualKeyExW`
/// returns the arrows without the prefix (measured on Windows 11: `VK_RIGHT` gives 0x4D, the
/// same code as numpad 6), so the flag cannot come from it alone.
const EXTENDED: &[VIRTUAL_KEY] = &[VK_RIGHT, VK_LEFT, VK_UP, VK_DOWN, VK_HOME, VK_END];

fn scan(vk: VIRTUAL_KEY, layout: HKL) -> Scan {
    // SAFETY: plain value arguments. `_EX` puts an E0/E1 prefix in the high byte for the
    // extended keys it knows.
    let ex = unsafe { MapVirtualKeyExW(vk.0 as u32, MAPVK_VK_TO_VSC_EX, Some(layout)) };
    Scan {
        code: (ex & 0xFF) as u16,
        extended: matches!(ex >> 8, 0xE0 | 0xE1) || EXTENDED.contains(&vk),
    }
}

fn keyboard(vk: VIRTUAL_KEY, scan: Scan, up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if scan.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan.code,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_MARKER,
            },
        },
    }
}

/// Shift, Alt or Win block the paste; a held Ctrl is used as it is.
fn paste_inputs(held: Held, ctrl: Scan, v: Scan) -> Result<Vec<INPUT>, Modifier> {
    blocker(held, false)?;
    let v_down_up = [keyboard(VK_V, v, false), keyboard(VK_V, v, true)];
    if held.ctrl {
        return Ok(v_down_up.to_vec());
    }
    let mut inputs = vec![keyboard(VK_CONTROL, ctrl, false)];
    inputs.extend(v_down_up);
    inputs.push(keyboard(VK_CONTROL, ctrl, true));
    Ok(inputs)
}

fn key_inputs(held: Held, vk: VIRTUAL_KEY, scan: Scan) -> Result<Vec<INPUT>, Modifier> {
    blocker(held, true)?;
    Ok(vec![keyboard(vk, scan, false), keyboard(vk, scan, true)])
}

fn blocker(held: Held, ctrl_blocks: bool) -> Result<(), Modifier> {
    if held.shift {
        Err(Modifier::Shift)
    } else if held.alt {
        Err(Modifier::Alt)
    } else if held.win {
        Err(Modifier::Win)
    } else if held.ctrl && ctrl_blocks {
        Err(Modifier::Ctrl)
    } else {
        Ok(())
    }
}

fn send(inputs: &[INPUT]) -> Sent {
    // SAFETY: a slice of fully initialised keyboard INPUTs with the right element size.
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize >= inputs.len() {
        return Sent::Done;
    }
    let releases = releases(&inputs[..sent as usize]);
    if !releases.is_empty() {
        // SAFETY: as above.
        unsafe { SendInput(&releases, size_of::<INPUT>() as i32) };
    }
    Sent::Incomplete {
        sent,
        total: inputs.len() as u32,
    }
}

/// Key-ups for keys the sent events left down, last pressed first.
fn releases(sent: &[INPUT]) -> Vec<INPUT> {
    let mut down: Vec<KEYBDINPUT> = Vec::new();
    for input in sent {
        // SAFETY: every INPUT this module builds is a keyboard input.
        let ki = unsafe { input.Anonymous.ki };
        if ki.dwFlags.contains(KEYEVENTF_KEYUP) {
            down.retain(|d| d.wVk != ki.wVk);
        } else {
            down.push(ki);
        }
    }
    down.iter()
        .rev()
        .map(|ki| {
            let scan = Scan {
                code: ki.wScan,
                extended: ki.dwFlags.contains(KEYEVENTF_EXTENDEDKEY),
            };
            keyboard(ki.wVk, scan, true)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: Scan = Scan {
        code: 0x1D,
        extended: false,
    };
    const V: Scan = Scan {
        code: 0x2F,
        extended: false,
    };
    const RIGHT: Scan = Scan {
        code: 0x4D,
        extended: true,
    };
    const ESCAPE: Scan = Scan {
        code: 0x01,
        extended: false,
    };

    /// (vk, scan, up, extended) of each event; checks the type and marker on the way.
    fn events(inputs: &[INPUT]) -> Vec<(VIRTUAL_KEY, u16, bool, bool)> {
        inputs
            .iter()
            .map(|input| {
                assert_eq!(input.r#type, INPUT_KEYBOARD);
                // SAFETY: built as a keyboard input.
                let ki = unsafe { input.Anonymous.ki };
                assert_eq!(ki.dwExtraInfo, INJECTED_MARKER);
                assert_eq!(ki.time, 0);
                (
                    ki.wVk,
                    ki.wScan,
                    ki.dwFlags.contains(KEYEVENTF_KEYUP),
                    ki.dwFlags.contains(KEYEVENTF_EXTENDEDKEY),
                )
            })
            .collect()
    }

    #[test]
    fn paste_with_no_modifier_held() {
        let inputs = paste_inputs(Held::default(), CTRL, V).unwrap();
        assert_eq!(
            events(&inputs),
            [
                (VK_CONTROL, 0x1D, false, false),
                (VK_V, 0x2F, false, false),
                (VK_V, 0x2F, true, false),
                (VK_CONTROL, 0x1D, true, false),
            ]
        );
    }

    #[test]
    fn paste_with_ctrl_held_sends_only_v() {
        let held = Held {
            ctrl: true,
            ..Held::default()
        };
        let inputs = paste_inputs(held, CTRL, V).unwrap();
        assert_eq!(
            events(&inputs),
            [(VK_V, 0x2F, false, false), (VK_V, 0x2F, true, false)]
        );
    }

    #[test]
    fn paste_blocked_by_shift_alt_win() {
        let cases = [
            (
                Held {
                    shift: true,
                    ..Held::default()
                },
                Modifier::Shift,
            ),
            (
                Held {
                    alt: true,
                    ..Held::default()
                },
                Modifier::Alt,
            ),
            (
                Held {
                    win: true,
                    ..Held::default()
                },
                Modifier::Win,
            ),
            (
                Held {
                    shift: true,
                    ctrl: true,
                    alt: true,
                    win: true,
                },
                Modifier::Shift,
            ),
            (
                Held {
                    ctrl: true,
                    win: true,
                    ..Held::default()
                },
                Modifier::Win,
            ),
        ];
        for (held, modifier) in cases {
            assert_eq!(
                paste_inputs(held, CTRL, V).err(),
                Some(modifier),
                "{held:?}"
            );
        }
    }

    #[test]
    fn plain_keys() {
        let inputs = key_inputs(Held::default(), VK_RIGHT, RIGHT).unwrap();
        assert_eq!(
            events(&inputs),
            [(VK_RIGHT, 0x4D, false, true), (VK_RIGHT, 0x4D, true, true)]
        );
        let inputs = key_inputs(Held::default(), VK_ESCAPE, ESCAPE).unwrap();
        assert_eq!(
            events(&inputs),
            [(VK_ESCAPE, 1, false, false), (VK_ESCAPE, 1, true, false)]
        );
        let held = Held {
            ctrl: true,
            ..Held::default()
        };
        assert_eq!(
            key_inputs(held, VK_RIGHT, RIGHT).err(),
            Some(Modifier::Ctrl)
        );
        let held = Held {
            alt: true,
            ..Held::default()
        };
        assert_eq!(
            key_inputs(held, VK_ESCAPE, ESCAPE).err(),
            Some(Modifier::Alt)
        );
    }

    #[test]
    fn partial_send_releases_what_is_down() {
        let inputs = paste_inputs(Held::default(), CTRL, V).unwrap();
        assert!(releases(&inputs[..0]).is_empty());
        assert_eq!(
            events(&releases(&inputs[..1])),
            [(VK_CONTROL, 0x1D, true, false)]
        );
        assert_eq!(
            events(&releases(&inputs[..2])),
            [(VK_V, 0x2F, true, false), (VK_CONTROL, 0x1D, true, false)]
        );
        assert_eq!(
            events(&releases(&inputs[..3])),
            [(VK_CONTROL, 0x1D, true, false)]
        );
        assert!(releases(&inputs).is_empty());
        let right = key_inputs(Held::default(), VK_RIGHT, RIGHT).unwrap();
        assert_eq!(
            events(&releases(&right[..1])),
            [(VK_RIGHT, 0x4D, true, true)]
        );
    }

    #[test]
    fn scan_codes_from_the_layout() {
        // Reads the layout table only; sends nothing.
        // SAFETY: thread 0 is the calling thread.
        let layout = unsafe { GetKeyboardLayout(0) };
        assert_eq!(scan(VK_CONTROL, layout), CTRL);
        assert_eq!(scan(VK_RIGHT, layout), RIGHT);
        assert_eq!(scan(VK_ESCAPE, layout), ESCAPE);
    }
}
