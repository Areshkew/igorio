//! For self-tests: which monitor test windows open on. With `IGORIO_TEST_MONITOR=secondary`
//! they go to the first monitor that is not the primary one, so a full-screen app on the
//! primary monitor is left alone. Unset, or with one monitor: `None`, the usual placement.
//! Include it with `#[path = "common/test_monitor.rs"] mod test_monitor;`.

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::BOOL;

/// The middle of the chosen monitor's work area, physical pixels.
pub fn test_monitor_point() -> Option<(i32, i32)> {
    if std::env::var("IGORIO_TEST_MONITOR").ok()?.trim() != "secondary" {
        return None;
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the Vec passed below, alive for the whole enumeration.
        unsafe { (*(data.0 as *mut Vec<HMONITOR>)).push(monitor) };
        true.into()
    }
    // SAFETY: the callback only pushes into the local Vec, which outlives the call.
    let _ = unsafe {
        EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut monitors as *mut _ as isize))
    };
    monitors.into_iter().find_map(|monitor| {
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: a monitor handle from the enumeration and a sized, writable MONITORINFO.
        let ok = unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool();
        let work = info.rcWork;
        (ok && info.dwFlags & MONITORINFOF_PRIMARY == 0)
            .then_some(((work.left + work.right) / 2, (work.top + work.bottom) / 2))
    })
}
