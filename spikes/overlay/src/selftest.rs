use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering::Relaxed};

use windows::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::Error as WinError;

use crate::{Result, pixels::Dib, pump_messages, set_visible};

static TARGET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static WAS_FOREGROUND: AtomicBool = AtomicBool::new(false);

pub struct ForegroundObserver {
    hwnd: HWND,
    hook: HWINEVENTHOOK,
}

impl ForegroundObserver {
    pub fn new(hwnd: HWND) -> Result<Self> {
        TARGET.store(hwnd.0, Relaxed);
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(foreground_event),
                0,
                GetCurrentThreadId(),
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.is_invalid() {
            return Err(WinError::from_thread().into());
        }
        Ok(Self { hwnd, hook })
    }

    pub fn sample(&self) {
        if unsafe { GetForegroundWindow() } == self.hwnd {
            WAS_FOREGROUND.store(true, Relaxed);
        }
    }
}

impl Drop for ForegroundObserver {
    fn drop(&mut self) {
        let _ = unsafe { UnhookWinEvent(self.hook) };
    }
}

unsafe extern "system" fn foreground_event(
    _: HWINEVENTHOOK,
    _: u32,
    hwnd: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if hwnd.0 == TARGET.load(Relaxed) {
        WAS_FOREGROUND.store(true, Relaxed);
    }
}

pub fn check(hwnd: HWND, observer: &ForegroundObserver) -> bool {
    let mut rect = RECT::default();
    let geometry = unsafe { GetWindowRect(hwnd, &mut rect) };
    let click_through = match &geometry {
        Ok(()) => {
            let center = POINT {
                x: rect.left + (rect.right - rect.left) / 2,
                y: rect.top + (rect.bottom - rect.top) / 2,
            };
            let hit = unsafe { WindowFromPoint(center) };
            report(
                !hit.is_invalid() && hit != hwnd,
                format_args!("click-through (WindowFromPoint {hit:?}, overlay {hwnd:?})"),
            )
        }
        Err(err) => report(false, format_args!("click-through ({err})")),
    };

    let mut affinity = 0;
    let affinity_result = unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) };
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    // Allow 0.1% of the region to change between frames on a live desktop.
    let tolerance = width as usize * height as usize / 1_000;
    let capture = geometry
        .map_err(Into::into)
        .and_then(|()| compare(hwnd, rect));
    pump_messages();
    observer.sample();
    let focus = report(
        !WAS_FOREGROUND.load(Relaxed),
        "overlay never became foreground",
    );
    let capture = match capture {
        Ok((excluded, included)) => report(
            excluded <= tolerance && included > tolerance,
            format_args!(
                "BitBlt capture ({width}x{height}, {excluded} differing pixels with exclusion, {included} with affinity off, tolerance {tolerance})"
            ),
        ),
        Err(err) => report(false, format_args!("BitBlt capture ({err})")),
    };
    let affinity = match affinity_result {
        Ok(()) => report(
            affinity == WDA_EXCLUDEFROMCAPTURE.0,
            format_args!("GetWindowDisplayAffinity returned 0x{affinity:02X}"),
        ),
        Err(err) => report(false, format_args!("GetWindowDisplayAffinity ({err})")),
    };
    click_through && focus && capture && affinity
}

fn compare(hwnd: HWND, rect: RECT) -> Result<(usize, usize)> {
    let mut dib = Dib::new(SIZE {
        cx: rect.right - rect.left,
        cy: rect.bottom - rect.top,
    })?;
    let mut compare_pair = || -> Result<usize> {
        unsafe { DwmFlush()? };
        let shown = dib.capture(rect)?.to_vec();
        set_visible(hwnd, false)?;
        unsafe { DwmFlush()? };
        let hidden = dib.capture(rect)?;
        // The fourth byte of a screen BitBlt is unspecified. Compare RGB only.
        Ok(shown
            .chunks_exact(4)
            .zip(hidden.chunks_exact(4))
            .filter(|(a, b)| a[..3] != b[..3])
            .count())
    };

    // The desktop under the overlay may be changing (streaming text, video). Keep the
    // quietest of three tries: a broken exclusion differs every time.
    let mut excluded = compare_pair()?;
    for _ in 0..2 {
        if excluded == 0 {
            break;
        }
        set_visible(hwnd, true)?;
        excluded = excluded.min(compare_pair()?);
    }
    // Change affinity while hidden and freeze the placement throughout both pairs.
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_NONE)? };
    let included = set_visible(hwnd, true).and_then(|()| compare_pair());
    set_visible(hwnd, false)?;
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)? };
    Ok((excluded, included?))
}

fn report(pass: bool, description: impl std::fmt::Display) -> bool {
    let status = if pass { "PASS" } else { "FAIL" };
    println!("{status} {description}");
    pass
}
