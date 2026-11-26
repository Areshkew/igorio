use std::{cell::RefCell, fmt, mem::size_of, path::Path, time::Instant};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
            Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow},
        },
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::{BOOL, PWSTR},
};

use crate::classify::Shortcut;

#[derive(Debug, Default)]
pub struct WindowInfo {
    pub hwnd: HWND,
    pub pid: u32,
    pub class: String,
    pub exe: String,
    pub capture_process: bool,
}

impl WindowInfo {
    pub fn read(hwnd: HWND) -> Self {
        if hwnd.is_invalid() {
            return Self::default();
        }
        let mut name = [0; 256];
        let mut pid = 0;
        let (length, path) = unsafe {
            let length = GetClassNameW(hwnd, &mut name);
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let path = process_path(pid);
            (length, path)
        };
        let exe = Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("none")
            .to_owned();
        let capture_process = [
            "SnippingTool.exe",
            "ScreenClippingHost.exe",
            "ScreenSketch.exe",
        ]
        .iter()
        .any(|name| exe.eq_ignore_ascii_case(name))
            || path
                .to_ascii_lowercase()
                .contains("\\microsoft.screensketch_");
        Self {
            hwnd,
            pid,
            class: String::from_utf16_lossy(&name[..length as usize]),
            exe,
            capture_process,
        }
    }
}

impl fmt::Display for WindowInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.hwnd.is_invalid() {
            return f.write_str("none");
        }
        write!(
            f,
            "{{hwnd={:p} pid={} class={:?} exe={:?}}}",
            self.hwnd.0, self.pid, self.class, self.exe
        )
    }
}

unsafe fn process_path(pid: u32) -> String {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        if result.is_ok() {
            String::from_utf16_lossy(&path[..length as usize])
        } else {
            String::new()
        }
    }
}

pub fn is_overlay(window: &WindowInfo) -> bool {
    window.capture_process && covers_monitor(window.hwnd)
}

pub fn visible_overlay() -> Option<WindowInfo> {
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
        unsafe {
            if covers_monitor(hwnd) {
                let info = WindowInfo::read(hwnd);
                if info.capture_process {
                    *(data.0 as *mut Option<WindowInfo>) = Some(info);
                    return false.into();
                }
            }
        }
        true.into()
    }

    let mut found = None;
    unsafe {
        let _ = EnumWindows(
            Some(visit),
            LPARAM((&mut found as *mut Option<WindowInfo>) as isize),
        );
    }
    found
}

fn covers_monitor(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool()
        {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return false;
        }
        let screen = info.rcMonitor;
        let width = rect.right.min(screen.right) - rect.left.max(screen.left);
        let height = rect.bottom.min(screen.bottom) - rect.top.max(screen.top);
        if width * 10 < (screen.right - screen.left) * 9
            || height * 10 < (screen.bottom - screen.top) * 9
        {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        );
        cloaked == 0
    }
}

const MODIFIERS: [VIRTUAL_KEY; 8] = [
    VK_LWIN,
    VK_RWIN,
    VK_LSHIFT,
    VK_RSHIFT,
    VK_LMENU,
    VK_RMENU,
    VK_LCONTROL,
    VK_RCONTROL,
];

#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub at: Instant,
    pub shortcut: Shortcut,
    pub injected: bool,
}

#[derive(Default)]
struct Keyboard {
    down: u8,
    print_down: bool,
    s_down: bool,
    events: Vec<KeyEvent>,
}

thread_local! {
    static KEYBOARD: RefCell<Keyboard> = RefCell::default();
}

pub fn prime_modifiers() {
    KEYBOARD.with_borrow_mut(|keyboard| {
        for (index, key) in MODIFIERS.iter().enumerate() {
            if unsafe { GetAsyncKeyState(i32::from(key.0)) } < 0 {
                keyboard.down |= 1 << index;
            }
        }
    });
}

pub fn take_key_events() -> Vec<KeyEvent> {
    KEYBOARD.with_borrow_mut(|keyboard| std::mem::take(&mut keyboard.events))
}

pub unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let key = &*(data.0 as *const KBDLLHOOKSTRUCT);
            let pressed = matches!(message.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            KEYBOARD.with_borrow_mut(|keyboard| {
                if let Some(index) = MODIFIERS
                    .iter()
                    .position(|modifier| u32::from(modifier.0) == key.vkCode)
                {
                    if pressed {
                        keyboard.down |= 1 << index;
                    } else {
                        keyboard.down &= !(1 << index);
                    }
                    return;
                }
                let win = keyboard.down & 0b0000_0011 != 0;
                let shift = keyboard.down & 0b0000_1100 != 0;
                let alt = keyboard.down & 0b0011_0000 != 0 || key.flags.contains(LLKHF_ALTDOWN);
                let ctrl = keyboard.down & 0b1100_0000 != 0;
                let shortcut = if key.vkCode == u32::from(VK_SNAPSHOT.0) {
                    // Some keyboards send only a Print Screen release.
                    let first = !std::mem::replace(&mut keyboard.print_down, pressed);
                    (first && !ctrl && !shift && !(win && alt)).then_some(if win {
                        Shortcut::WinPrintScreen
                    } else if alt {
                        Shortcut::AltPrintScreen
                    } else {
                        Shortcut::PrintScreen
                    })
                } else if key.vkCode == u32::from(VK_S.0) {
                    let was_down = std::mem::replace(&mut keyboard.s_down, pressed);
                    (pressed && !was_down && win && shift && !alt && !ctrl)
                        .then_some(Shortcut::WinShiftS)
                } else {
                    None
                };
                if let Some(shortcut) = shortcut {
                    keyboard.events.push(KeyEvent {
                        at: Instant::now(),
                        shortcut,
                        injected: key.flags.contains(LLKHF_INJECTED),
                    });
                }
            });
        }
        CallNextHookEx(None, code, message, data)
    }
}
