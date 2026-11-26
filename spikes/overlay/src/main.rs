mod pixels;
mod selftest;

use std::error::Error;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::Console::{CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE,
    SYNCHRONIZATION_SYNCHRONIZE, SetWaitableTimer, TIMER_MODIFY_STATE,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, Error as WinError, PCWSTR, w};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const CLASS: PCWSTR = w!("IgorioOverlaySpike");
const STILL: SET_WINDOW_POS_FLAGS =
    SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0 | SWP_NOZORDER.0 | SWP_NOOWNERZORDER.0);
static STOP: AtomicBool = AtomicBool::new(false);

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("overlay-spike: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let (duration, selftest) = match args.as_slice() {
        [] => (Duration::from_secs(20), false),
        [flag] if flag == "--selftest" => (Duration::from_secs(3), true),
        [flag, seconds] if flag == "--seconds" => (Duration::from_secs(seconds.parse()?), false),
        _ => return Err("usage: overlay-spike [--seconds N | --selftest]".into()),
    };

    // Set the process default before creating any windows.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
        SetConsoleCtrlHandler(Some(console_control), true)?;
    }
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(WinError::from_thread().into());
    }

    let overlay = Window::new(
        instance,
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        POINT::default(),
    )?;
    unsafe { SetWindowDisplayAffinity(overlay.0, WDA_EXCLUDEFROMCAPTURE)? };
    let observer = selftest
        .then(|| selftest::ForegroundObserver::new(overlay.0))
        .transpose()?;

    let mut follower = Follower::default();
    follower.tick(overlay.0, instance, 0.0)?;
    set_visible(overlay.0, true)?;
    let started = Instant::now();
    let mut last_tick = started;

    // Integer milliseconds make this 125 Hz nominally. The spring uses actual elapsed time.
    let timer = unsafe {
        let raw = CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            (TIMER_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE).0,
        )?;
        OwnedHandle::from_raw_handle(raw.0)
    };
    let handle = HANDLE(timer.as_raw_handle());
    unsafe { SetWaitableTimer(handle, &-80_000, 8, None, None, false)? };

    while started.elapsed() < duration && !STOP.load(Relaxed) {
        pump_messages();
        if let Some(observer) = &observer {
            observer.sample();
        }
        let ready = unsafe {
            MsgWaitForMultipleObjectsEx(Some(&[handle]), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
        if ready == WAIT_FAILED {
            return Err(WinError::from_thread().into());
        }
        if ready == WAIT_OBJECT_0 {
            let now = Instant::now();
            let dt = (now - last_tick).as_secs_f64().min(0.05);
            last_tick = now;
            follower.tick(overlay.0, instance, dt)?;
        }
    }

    match observer {
        Some(observer) if !STOP.load(Relaxed) => Ok(selftest::check(overlay.0, &observer)),
        Some(_) => Ok(false),
        None => Ok(true),
    }
}

struct Window(HWND);

impl Window {
    fn new(instance: HINSTANCE, style: WINDOW_EX_STYLE, at: POINT) -> Result<Self> {
        Ok(Self(unsafe {
            CreateWindowExW(
                style,
                CLASS,
                w!("Igorio companion"),
                WS_POPUP,
                at.x,
                at.y,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )?
        }))
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

#[derive(Default)]
struct Follower {
    probe: Option<(HMONITOR, Window)>,
    position: Option<(f64, f64)>,
    previous_origin: POINT,
    previous_dpi: u32,
}

impl Follower {
    fn tick(&mut self, hwnd: HWND, instance: HINSTANCE, dt: f64) -> Result<()> {
        let mut cursor = POINT::default();
        unsafe { GetCursorPos(&mut cursor)? };
        let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetMonitorInfoW(monitor, &mut info).ok()? };
        if self.probe.as_ref().map(|(monitor, _)| *monitor) != Some(monitor) {
            // The visible window can lag across a boundary. A hidden window created on the
            // cursor's monitor lets GetDpiForWindow report that monitor's effective DPI.
            let bounds = info.rcMonitor;
            let center = POINT {
                x: bounds.left + (bounds.right - bounds.left) / 2,
                y: bounds.top + (bounds.bottom - bounds.top) / 2,
            };
            self.probe = Some((monitor, Window::new(instance, WS_EX_NOACTIVATE, center)?));
        }
        let (_, Window(dpi_window)) = self.probe.as_ref().unwrap();
        let dpi = unsafe { GetDpiForWindow(*dpi_window) };
        if dpi == 0 {
            return Err("could not read the cursor monitor's DPI".into());
        }
        let scale = f64::from(dpi) / 96.0;
        let scaled = |logical: f64| (logical * scale).round() as i32;
        let size = SIZE {
            cx: scaled(240.0),
            cy: scaled(200.0),
        };
        let work = info.rcWork;
        let target_x = target_axis(cursor.x, scaled(14.0), size.cx, work.left, work.right);
        let target_y = target_axis(cursor.y, scaled(18.0), size.cy, work.top, work.bottom);
        let target = (f64::from(target_x), f64::from(target_y));
        let (x, y) = self.position.unwrap_or(target);
        let alpha = 1.0 - (-16.0 * dt).exp();
        let position = (x + (target.0 - x) * alpha, y + (target.1 - y) * alpha);
        let origin = POINT {
            x: position.0.round() as i32,
            y: position.1.round() as i32,
        };
        if dpi != self.previous_dpi {
            pixels::draw(hwnd, origin, size, scale as f32)?;
        } else if origin != self.previous_origin {
            unsafe {
                SetWindowPos(hwnd, None, origin.x, origin.y, 0, 0, STILL | SWP_NOSIZE)?;
            }
        }
        self.position = Some(position);
        self.previous_origin = origin;
        self.previous_dpi = dpi;
        Ok(())
    }
}

fn target_axis(cursor: i32, gap: i32, size: i32, start: i32, end: i32) -> i32 {
    let forward = cursor + gap;
    let origin = if forward + size > end {
        cursor - gap - size
    } else {
        forward
    };
    origin.clamp(start, (end - size).max(start))
}

fn set_visible(hwnd: HWND, visible: bool) -> Result<()> {
    let flag = if visible {
        SWP_SHOWWINDOW
    } else {
        SWP_HIDEWINDOW
    };
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            STILL | SWP_NOMOVE | SWP_NOSIZE | flag,
        )?
    };
    Ok(())
}

fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

unsafe extern "system" fn console_control(event: u32) -> BOOL {
    if event == CTRL_C_EVENT {
        STOP.store(true, Relaxed);
        true.into()
    } else {
        false.into()
    }
}
