//! Synthetic keystrokes for the paste, from `Keys.swift`: `Ctrl+V`, and the Right arrow and
//! Escape that `PasteTarget.swift` presses before it.
//!
//! Rule 7: [`send_guarded`] is the only way keys go out. It runs the caller's last check (the
//! clipboard still holds our staging), then reads the foreground window, its process, the
//! focused control of its thread and the modifier keys, and sends the whole batch in one
//! `SendInput` call, with nothing in between. Scan codes come from the keyboard layout of the
//! thread just checked. The user's own typing cannot land inside a batch.
//!
//! Every event carries [`INJECTED_MARKER`] in `dwExtraInfo`, so the app's own low-level hooks
//! can tell these events from the user's (`KBDLLHOOKSTRUCT::dwExtraInfo`).
//!
//! Without UI access, UIPI can block input to a higher-integrity target. `SendInput` does
//! not report UIPI as the reason.

use std::mem::size_of;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, HKL, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS,
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC_EX, MapVirtualKeyExW,
    SendInput, VIRTUAL_KEY, VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_LWIN,
    VK_MENU, VK_RIGHT, VK_RWIN, VK_SHIFT, VK_UP, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GUITHREADINFO, GetAncestor, GetForegroundWindow, GetGUIThreadInfo,
    GetWindowThreadProcessId,
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
    /// `Ctrl+V` becomes "paste as plain text" in Chrome and Slack, and a Ctrl released between
    /// the check and the injection would turn the paste into a plain "v".
    Blocked(Modifier),
    /// `SendInput` took only `sent` of `total` events. Keys left down were released;
    /// `released` says whether they read as up afterwards.
    Incomplete {
        sent: u32,
        total: u32,
        released: bool,
    },
    /// `SendInput` took none of the events (blocked input, a UIPI refusal it reports).
    Failed,
    /// The target window (or its process) is not in front; nothing was sent.
    NotInFront,
    /// The focused control of the target's thread is not the one of the first batch.
    FocusMoved,
    /// The caller's last check said no (the clipboard no longer holds our staging).
    Refused,
}

/// What one send presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batch {
    Key(Key),
    /// Ctrl+V.
    Paste,
    /// Ctrl+Shift+V (WezTerm, Alacritty).
    PasteWithShift,
}

/// What must still be true when a batch goes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard {
    /// The target's top-level window.
    pub window: HWND,
    /// Its process; 0 skips the check.
    pub pid: u32,
    /// The focused control of the window's thread at the first batch; None for the first.
    pub focus: Option<HWND>,
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

/// Sends one batch after the last checks, with nothing in between (rule 7). Returns the
/// result and the focused control of the target's thread seen at the check, which the caller
/// passes as `guard.focus` for the next batch of the same delivery.
pub fn send_guarded(
    guard: &Guard,
    batch: Batch,
    precheck: impl FnOnce() -> bool,
) -> (Sent, Option<HWND>) {
    if !precheck() {
        return (Sent::Refused, None);
    }
    // SAFETY: no arguments.
    let front = unsafe { GetForegroundWindow() };
    if front.is_invalid() {
        return (Sent::NotInFront, None);
    }
    let mut front_pid = 0u32;
    // SAFETY: `front_pid` is a local; plain value arguments otherwise.
    let (thread, front_root, target_root) = unsafe {
        let thread = GetWindowThreadProcessId(front, Some(&mut front_pid));
        (
            thread,
            GetAncestor(front, GA_ROOT),
            GetAncestor(guard.window, GA_ROOT),
        )
    };
    if !same_target(
        front_root.0 as isize,
        front_pid,
        target_root.0 as isize,
        guard.pid,
    ) {
        return (Sent::NotInFront, None);
    }
    let focus = focused_control(thread);
    if !focus_unchanged(guard.focus, focus) {
        return (Sent::FocusMoved, focus);
    }
    // SAFETY: plain value argument: the thread just checked.
    let layout = unsafe { GetKeyboardLayout(thread) };
    let inputs = match batch {
        Batch::Key(key) => {
            let vk = match key {
                Key::Right => VK_RIGHT,
                Key::Escape => VK_ESCAPE,
            };
            key_inputs(held(), vk, scan(vk, layout))
        }
        Batch::Paste => paste_inputs(held(), scan(VK_CONTROL, layout), scan(VK_V, layout)),
        Batch::PasteWithShift => shift_paste_inputs(
            held(),
            scan(VK_CONTROL, layout),
            scan(VK_SHIFT, layout),
            scan(VK_V, layout),
        ),
    };
    match inputs {
        Ok(inputs) => (send(&inputs), focus),
        Err(modifier) => (Sent::Blocked(modifier), focus),
    }
}

/// The foreground's top-level window is the target's, and so is its process (when known).
fn same_target(front_root: isize, front_pid: u32, target_root: isize, target_pid: u32) -> bool {
    target_root != 0 && front_root == target_root && (target_pid == 0 || front_pid == target_pid)
}

/// The focus seen now is the one of the first batch (nothing to compare at the first).
fn focus_unchanged(expected: Option<HWND>, now: Option<HWND>) -> bool {
    expected.is_none_or(|expected| now == Some(expected))
}

/// The control with the keyboard focus on `thread` (`GetGUIThreadInfo`; window manager
/// state only, it waits on no other process).
fn focused_control(thread: u32) -> Option<HWND> {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a local with `cbSize` set, as the call requires.
    unsafe { GetGUIThreadInfo(thread, &mut info) }.ok()?;
    (!info.hwndFocus.is_invalid()).then_some(info.hwndFocus)
}

/// Whether the target is in front: its top-level window, and its process when both are known.
pub fn foreground_matches(window: HWND, pid: u32) -> bool {
    // SAFETY: no arguments.
    let front = unsafe { GetForegroundWindow() };
    if front.is_invalid() {
        return false;
    }
    let mut front_pid = 0u32;
    // SAFETY: `front_pid` is a local; plain value arguments.
    let (front_root, target_root) = unsafe {
        GetWindowThreadProcessId(front, Some(&mut front_pid));
        (GetAncestor(front, GA_ROOT), GetAncestor(window, GA_ROOT))
    };
    same_target(
        front_root.0 as isize,
        front_pid,
        target_root.0 as isize,
        pid,
    )
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

/// Ctrl+V, the full chord: any modifier the user holds blocks it (a held Ctrl released
/// between the check and the injection would leave a plain "v").
fn paste_inputs(held: Held, ctrl: Scan, v: Scan) -> Result<Vec<INPUT>, Modifier> {
    blocker(held, true)?;
    Ok(vec![
        keyboard(VK_CONTROL, ctrl, false),
        keyboard(VK_V, v, false),
        keyboard(VK_V, v, true),
        keyboard(VK_CONTROL, ctrl, true),
    ])
}

/// Ctrl+Shift+V, the full chord; any held modifier blocks it.
fn shift_paste_inputs(
    held: Held,
    ctrl: Scan,
    shift: Scan,
    v: Scan,
) -> Result<Vec<INPUT>, Modifier> {
    blocker(held, true)?;
    Ok(vec![
        keyboard(VK_CONTROL, ctrl, false),
        keyboard(VK_SHIFT, shift, false),
        keyboard(VK_V, v, false),
        keyboard(VK_V, v, true),
        keyboard(VK_SHIFT, shift, true),
        keyboard(VK_CONTROL, ctrl, true),
    ])
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
    outcome(inputs, sent, |releases| {
        // SAFETY: as above.
        unsafe { SendInput(releases, size_of::<INPUT>() as i32) };
        releases.iter().all(|input| {
            // SAFETY: every INPUT this module builds is a keyboard input.
            let vk = unsafe { input.Anonymous.ki.wVk };
            // SAFETY: plain value argument. The high bit is set while the key is down.
            let state = unsafe { GetAsyncKeyState(vk.0 as i32) };
            state >= 0
        })
    })
}

/// The result of a `SendInput` that took `sent` of `inputs`. `release` sends the key-ups for
/// what was left down and says whether those keys then read as up.
fn outcome(inputs: &[INPUT], sent: u32, release: impl FnOnce(&[INPUT]) -> bool) -> Sent {
    if sent as usize >= inputs.len() {
        return Sent::Done;
    }
    if sent == 0 {
        return Sent::Failed;
    }
    let releases = releases(&inputs[..sent as usize]);
    let released = releases.is_empty() || release(&releases);
    Sent::Incomplete {
        sent,
        total: inputs.len() as u32,
        released,
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
    fn shift_paste_presses_ctrl_shift_v() {
        const SHIFT: Scan = Scan {
            code: 0x2A,
            extended: false,
        };
        let inputs = shift_paste_inputs(Held::default(), CTRL, SHIFT, V).unwrap();
        assert_eq!(
            events(&inputs),
            [
                (VK_CONTROL, 0x1D, false, false),
                (VK_SHIFT, 0x2A, false, false),
                (VK_V, 0x2F, false, false),
                (VK_V, 0x2F, true, false),
                (VK_SHIFT, 0x2A, true, false),
                (VK_CONTROL, 0x1D, true, false),
            ]
        );
        let held = Held {
            shift: true,
            ..Held::default()
        };
        assert_eq!(
            shift_paste_inputs(held, CTRL, SHIFT, V).err(),
            Some(Modifier::Shift)
        );
        let held = Held {
            alt: true,
            ..Held::default()
        };
        assert_eq!(
            shift_paste_inputs(held, CTRL, SHIFT, V).err(),
            Some(Modifier::Alt)
        );
    }

    #[test]
    fn paste_waits_while_the_user_holds_ctrl() {
        let held = Held {
            ctrl: true,
            ..Held::default()
        };
        assert_eq!(paste_inputs(held, CTRL, V).err(), Some(Modifier::Ctrl));
    }

    #[test]
    fn a_send_that_took_nothing_failed_and_a_partial_one_is_released() {
        let inputs = paste_inputs(Held::default(), CTRL, V).unwrap();
        assert_eq!(outcome(&inputs, 4, |_| unreachable!()), Sent::Done);
        assert_eq!(outcome(&inputs, 0, |_| unreachable!()), Sent::Failed);
        let mut released_keys = Vec::new();
        let sent = outcome(&inputs, 2, |up| {
            released_keys = events(up);
            true
        });
        assert_eq!(
            sent,
            Sent::Incomplete {
                sent: 2,
                total: 4,
                released: true
            }
        );
        assert_eq!(
            released_keys,
            [(VK_V, 0x2F, true, false), (VK_CONTROL, 0x1D, true, false)]
        );
        // Still down after the release: reported, not hidden.
        assert_eq!(
            outcome(&inputs, 1, |_| false),
            Sent::Incomplete {
                sent: 1,
                total: 4,
                released: false
            }
        );
    }

    #[test]
    fn the_target_must_be_in_front_with_its_process_and_focus() {
        assert!(same_target(0x10, 100, 0x10, 100));
        assert!(same_target(0x10, 100, 0x10, 0));
        // Same window handle but another process (a reused handle): no.
        assert!(!same_target(0x10, 7, 0x10, 100));
        assert!(!same_target(0x20, 100, 0x10, 100));
        assert!(!same_target(0, 100, 0, 100));
        let (a, b) = (HWND(0x10 as *mut _), HWND(0x20 as *mut _));
        assert!(focus_unchanged(None, Some(a)));
        assert!(focus_unchanged(Some(a), Some(a)));
        assert!(!focus_unchanged(Some(a), Some(b)));
        assert!(!focus_unchanged(Some(a), None));
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
