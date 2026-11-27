//! The overlay window: a small layered, click-through, topmost popup that shows the pile.
//! From `FairyPanel.swift` and the visibility parts of `Follower.swift`.
//!
//! This module only owns the window. `draw` paints the pixmap, `follower` picks the origin.
//! All coordinates are physical pixels with a top-left origin, so the process must be
//! Per-Monitor V2 aware (the manifest sets it; dev builds call [`ensure_dpi_awareness`]).
//!
//! The window never takes input or focus:
//! - `WS_EX_LAYERED | WS_EX_TRANSPARENT` makes the window manager skip it in hit tests, so
//!   clicks from any process go to the window below. `WM_NCHITTEST` returns
//!   `HTTRANSPARENT` as well, for hit tests on our own thread.
//! - `WS_EX_NOACTIVATE`, `WM_MOUSEACTIVATE` -> `MA_NOACTIVATE` and `SWP_NOACTIVATE` on
//!   every show keep it from becoming the foreground window.
//! - `WS_EX_TOOLWINDOW` without an owner keeps it out of Alt+Tab and the taskbar.
//! - `WDA_EXCLUDEFROMCAPTURE` keeps it out of screenshots and recordings.
//!
//! UI Automation: because of `WS_EX_TRANSPARENT`, `WindowFromPoint` and
//! `IUIAutomation::ElementFromPoint` skip this window (the self-test checks both from
//! another process). Callers should still drop any element whose process id is ours
//! (rule 3), and can compare a window handle against [`Overlay::hwnd`].
//!
//! The window procedure keeps no pointer in the window's user data and never touches an
//! `Overlay`, so it is safe to re-enter from inside any call below.

use std::cell::Cell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem::size_of;
use std::ptr;

use igorio_core::geom::{Point, Rect};
use tiny_skia::Pixmap;
use windows::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush,
    GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetSystemMetrics,
    HTTRANSPARENT, HWND_TOPMOST, IsWindowVisible, MA_NOACTIVATE, RegisterClassW, SM_CXSCREEN,
    SM_CYSCREEN, SPI_SETWORKAREA, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowDisplayAffinity, SetWindowPos,
    ULW_ALPHA, UPDATE_LAYERED_WINDOW_FLAGS, UPDATELAYEREDWINDOWINFO, UpdateLayeredWindow,
    UpdateLayeredWindowIndirect, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_SETTINGCHANGE, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{Error, PCWSTR, Result, w};

const CLASS: PCWSTR = w!("IgorioOverlay");

thread_local! {
    /// Set by the window procedure when the display layout, DPI or work area changed.
    static DISPLAY_CHANGED: Cell<bool> = const { Cell::new(false) };
}

/// The monitor under a point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorInfo {
    /// Physical pixels, top-left origin. Can be negative on monitors left of or above
    /// the primary one.
    pub work_area: Rect,
    /// Effective DPI / 96 (1.0 at 100%, 1.25 at 125%).
    pub scale: f64,
}

/// The overlay window. It belongs to the thread that created it (not `Send`): that thread
/// must pump messages, and must be the one that drops it.
pub struct Overlay {
    hwnd: HWND,
    surface: Surface,
    /// Last origin given to `present` or `move_to`.
    origin: POINT,
    /// What the caller asked for; `reassert` restores it.
    wanted_visible: bool,
    _not_send: PhantomData<*mut ()>,
}

impl Overlay {
    /// Creates the hidden window on the calling thread, which must pump messages.
    /// The window starts excluded from capture.
    pub fn new() -> Result<Overlay> {
        // SAFETY: a null module name returns the exe's own module; nothing is borrowed.
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        register_class(instance)?;
        // SAFETY: the class is registered above with `window_proc`; all strings are
        // static; no creation parameter is passed, so nothing outlives this call.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                CLASS,
                w!("Igorio overlay"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )?
        };
        // From here `Drop` destroys the window, also on the error paths below.
        let mut overlay = Overlay {
            hwnd,
            surface: Surface::new()?,
            origin: POINT::default(),
            wanted_visible: false,
            _not_send: PhantomData,
        };
        overlay.set_capturable(false)?;
        Ok(overlay)
    }

    /// Copies `pixmap` (premultiplied RGBA) into the cached DIB as premultiplied BGRA and
    /// shows it at `origin` (physical px) with one `UpdateLayeredWindow` call that sets
    /// position, size and content. The DIB is recreated only when the size changed.
    /// Does not change visibility: a hidden overlay stays hidden.
    pub fn present(&mut self, pixmap: &Pixmap, origin: (i32, i32)) -> Result<()> {
        let size = SIZE {
            cx: pixmap.width() as i32,
            cy: pixmap.height() as i32,
        };
        self.surface.resize(size)?;
        copy_rgba_to_bgra(pixmap.data(), self.surface.pixels());
        let origin = POINT {
            x: origin.0,
            y: origin.1,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: every pointer is to a local that lives for the call; the memory DC holds
        // the DIB of exactly `size`, selected in `Surface::resize`.
        unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&origin),
                Some(&size),
                Some(self.surface.dc),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
        }
        self.origin = origin;
        Ok(())
    }

    /// Moves without redrawing (the content did not change). Does nothing if the origin
    /// is the same as last time.
    pub fn move_to(&mut self, origin: (i32, i32)) -> Result<()> {
        let origin = POINT {
            x: origin.0,
            y: origin.1,
        };
        if origin == self.origin {
            return Ok(());
        }
        // SAFETY: `origin` lives for the call. With no source DC, no size and no flags
        // (`ULW_ALPHA` without a source DC fails), the window keeps its content and size
        // and only moves.
        unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&origin),
                None,
                None,
                None,
                COLORREF(0),
                None,
                UPDATE_LAYERED_WINDOW_FLAGS(0),
            )?;
        }
        self.origin = origin;
        Ok(())
    }

    /// Shows the window without activating it and puts it on top of the topmost band.
    /// Call `present` at least once first: a layered window has no content before that.
    pub fn show(&mut self) {
        self.wanted_visible = true;
        self.place(true);
    }

    pub fn hide(&mut self) {
        self.wanted_visible = false;
        self.place(false);
    }

    /// True when the window is on screen (shown and not hidden by anyone else).
    pub fn is_visible(&self) -> bool {
        // SAFETY: reads a flag of our own window handle.
        unsafe { IsWindowVisible(self.hwnd) }.as_bool()
    }

    /// Call when the foreground window or the display configuration changed: if the
    /// overlay should be visible, shows it again and puts it back on top of the topmost
    /// band (another topmost window may have come above it). Like `reassertVisibility`
    /// in `Follower.swift`.
    pub fn reassert(&mut self) {
        if self.wanted_visible {
            self.place(true);
        }
    }

    /// Returns true once after the window procedure saw a display change
    /// (`WM_DISPLAYCHANGE`, a work area change, or a DPI change of the overlay's monitor).
    /// The caller should then call `reassert`, re-read `monitor_at` and redraw.
    /// The flag is per thread; poll it after pumping messages.
    pub fn take_display_changed(&self) -> bool {
        DISPLAY_CHANGED.with(|flag| flag.replace(false))
    }

    /// Default false: `WDA_EXCLUDEFROMCAPTURE`. True: `WDA_NONE`, for debugging
    /// (`FAIRY_CAPTURABLE=1` in the Swift app).
    pub fn set_capturable(&mut self, capturable: bool) -> Result<()> {
        let affinity = if capturable {
            WDA_NONE
        } else {
            WDA_EXCLUDEFROMCAPTURE
        };
        // SAFETY: our own window, created on this thread.
        unsafe { SetWindowDisplayAffinity(self.hwnd, affinity) }
    }

    /// The window handle, for excluding it from hit tests and UI Automation results.
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    fn place(&self, visible: bool) {
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        // SAFETY: our own window. `SWP_NOACTIVATE` keeps it from taking focus. The only
        // failure is an invalid handle, which `Drop` rules out, so the result is ignored.
        unsafe {
            let _ = if visible {
                SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags | SWP_SHOWWINDOW)
            } else {
                SetWindowPos(self.hwnd, None, 0, 0, 0, 0, flags | SWP_NOZORDER | SWP_HIDEWINDOW)
            };
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: the window was created on this thread (`Overlay` is not `Send`) and is
        // destroyed once. The surface frees its GDI objects after this, in its own `Drop`.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

/// The cursor position in physical pixels.
pub fn cursor_pos() -> Point {
    let mut point = POINT::default();
    // SAFETY: writes into a local. On failure (secure desktop) it reads as the origin.
    let _ = unsafe { GetCursorPos(&mut point) };
    Point::new(f64::from(point.x), f64::from(point.y))
}

/// The monitor nearest to `point` (physical px): its work area and effective DPI scale.
pub fn monitor_at(point: Point) -> MonitorInfo {
    let point = POINT {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
    };
    // SAFETY: plain value arguments; `MONITOR_DEFAULTTONEAREST` always returns a monitor.
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a local with `cbSize` set.
    let work = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        info.rcWork
    } else {
        // The monitor went away between the two calls. Use the primary screen.
        // SAFETY: plain value arguments.
        let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        }
    };
    let (mut dpi_x, mut dpi_y) = (0, 0);
    // SAFETY: writes into locals.
    let dpi = match unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) } {
        Ok(()) if dpi_x > 0 => dpi_x,
        _ => 96,
    };
    MonitorInfo {
        work_area: Rect::new(
            f64::from(work.left),
            f64::from(work.top),
            f64::from(work.right - work.left),
            f64::from(work.bottom - work.top),
        ),
        scale: f64::from(dpi) / 96.0,
    }
}

/// A monitor's full rectangle, its work area and its DPI scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorBounds {
    /// The whole monitor, physical pixels, top-left origin (`rcMonitor`).
    pub bounds: Rect,
    /// The part not covered by the taskbar and docked bars (`rcWork`).
    pub work_area: Rect,
    /// Effective DPI / 96.
    pub scale: f64,
}

/// The monitor nearest to `point` (physical px), with its full rectangle.
pub fn monitor_bounds_at(point: Point) -> MonitorBounds {
    let at = POINT {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
    };
    // SAFETY: plain value arguments; `MONITOR_DEFAULTTONEAREST` always returns a monitor.
    let monitor = unsafe { MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let rect = |r: RECT| {
        Rect::new(
            f64::from(r.left),
            f64::from(r.top),
            f64::from(r.right - r.left),
            f64::from(r.bottom - r.top),
        )
    };
    // SAFETY: `info` is a local with `cbSize` set.
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let scale = monitor_at(point).scale;
        MonitorBounds {
            bounds: rect(info.rcMonitor),
            work_area: rect(info.rcWork),
            scale,
        }
    } else {
        let fallback = monitor_at(point);
        MonitorBounds {
            bounds: fallback.work_area,
            work_area: fallback.work_area,
            scale: fallback.scale,
        }
    }
}

/// The cached DIB behind a layered window that has its own window procedure (the overlay's
/// `present` is for the overlay's own window). Presents a whole pixmap, or only a dirty
/// rectangle of it with `UpdateLayeredWindowIndirect` and `prcDirty`: only that rectangle is
/// converted into the DIB and handed to the compositor, which is what keeps a monitor-sized
/// window cheap to update.
pub struct LayeredSurface {
    surface: Surface,
    /// Whether the DIB holds the alpha floor (see [`present`](Self::present)).
    floor: bool,
}

impl LayeredSurface {
    pub fn new() -> Result<LayeredSurface> {
        Ok(LayeredSurface {
            surface: Surface::new()?,
            floor: false,
        })
    }

    /// Copies `pixmap` (premultiplied RGBA) into the DIB and updates `hwnd`'s content at
    /// `origin` with the constant `alpha` (255: as drawn). With `dirty` (`x, y, w, h`), only
    /// that rectangle is copied and presented; the rest of the window keeps what it showed.
    /// A size change, or a change of `floor`, presents the whole pixmap.
    ///
    /// `floor`: pixels with alpha 0 are written as black at alpha 1 (one 255th: no visible
    /// change). A layered window lets the mouse through wherever its alpha is 0, so a window
    /// that must take the mouse everywhere needs the floor.
    pub fn present(
        &mut self,
        hwnd: HWND,
        pixmap: &Pixmap,
        origin: (i32, i32),
        dirty: Option<(u32, u32, u32, u32)>,
        floor: bool,
        alpha: u8,
    ) -> Result<()> {
        let size = SIZE {
            cx: pixmap.width() as i32,
            cy: pixmap.height() as i32,
        };
        let whole =
            size != self.surface.size || self.surface.bitmap.is_invalid() || floor != self.floor;
        self.surface.resize(size)?;
        self.floor = floor;
        let (x, y, w, h) = match dirty {
            Some(rect) if !whole => rect,
            _ => (0, 0, pixmap.width(), pixmap.height()),
        };
        if w == 0 || h == 0 {
            return Ok(());
        }
        let stride = pixmap.width() as usize * 4;
        let src = pixmap.data();
        let dst = self.surface.pixels();
        for row in y as usize..(y + h) as usize {
            let start = row * stride + x as usize * 4;
            let end = start + w as usize * 4;
            copy_rgba_to_bgra_floor(&src[start..end], &mut dst[start..end], floor);
        }
        let dirty = RECT {
            left: x as i32,
            top: y as i32,
            right: (x + w) as i32,
            bottom: (y + h) as i32,
        };
        self.update(hwnd, origin, size, alpha, &dirty)
    }

    /// Changes only the window's constant alpha (a fade), re-sending one pixel.
    pub fn set_alpha(&mut self, hwnd: HWND, origin: (i32, i32), alpha: u8) -> Result<()> {
        let size = self.surface.size;
        if size.cx <= 0 || size.cy <= 0 {
            return Ok(());
        }
        let one = RECT {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        };
        self.update(hwnd, origin, size, alpha, &one)
    }

    fn update(
        &self,
        hwnd: HWND,
        origin: (i32, i32),
        size: SIZE,
        alpha: u8,
        dirty: &RECT,
    ) -> Result<()> {
        let origin = POINT {
            x: origin.0,
            y: origin.1,
        };
        let source = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(),
            pptDst: &origin,
            psize: &size,
            hdcSrc: self.surface.dc,
            pptSrc: &source,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: dirty,
        };
        // SAFETY: every pointer in `info` is to a local or to `dirty`, all alive for the call;
        // the memory DC holds the DIB of exactly `size`.
        unsafe { UpdateLayeredWindowIndirect(hwnd, &info) }.ok()
    }
}

/// [`copy_rgba_to_bgra`] that also writes alpha-0 pixels as black at alpha 1 when `floor`.
fn copy_rgba_to_bgra_floor(src: &[u8], dst: &mut [u8], floor: bool) {
    if !floor {
        copy_rgba_to_bgra(src, dst);
        return;
    }
    for (s, d) in src.as_chunks::<4>().0.iter().zip(dst.as_chunks_mut::<4>().0) {
        let rgba = u32::from_le_bytes(*s);
        let bgra = if rgba >> 24 == 0 {
            0x0100_0000
        } else {
            (rgba & 0xFF00_FF00) | ((rgba & 0xFF) << 16) | ((rgba >> 16) & 0xFF)
        };
        *d = bgra.to_le_bytes();
    }
}

/// Sets Per-Monitor V2 awareness for the process if nothing set it yet; never fails.
/// The release build sets it in the manifest, and then this call does nothing. Call it
/// before creating any window.
pub fn ensure_dpi_awareness() {
    // SAFETY: plain value argument. It fails with access denied when the manifest or an
    // earlier call already set the awareness, which is the case we want to ignore.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

fn register_class(instance: HINSTANCE) -> Result<()> {
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: CLASS,
        ..Default::default()
    };
    // SAFETY: `class` points only to static data and a `'static` function.
    if unsafe { RegisterClassW(&class) } == 0 {
        let error = Error::from_thread();
        // A second overlay in the same process finds the class already registered.
        if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(error);
        }
    }
    Ok(())
}

/// Touches no Rust state except a thread-local flag, so re-entrant calls (for example
/// `WM_WINDOWPOSCHANGING` sent from inside `SetWindowPos`) are harmless.
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DISPLAYCHANGE => {
            DISPLAY_CHANGED.with(|flag| flag.set(true));
            LRESULT(0)
        }
        // Do not resize to the suggested rectangle: `present` sets the size.
        WM_DPICHANGED => {
            DISPLAY_CHANGED.with(|flag| flag.set(true));
            LRESULT(0)
        }
        WM_SETTINGCHANGE if w.0 == SPI_SETWORKAREA.0 as usize => {
            DISPLAY_CHANGED.with(|flag| flag.set(true));
            LRESULT(0)
        }
        // SAFETY: the arguments come unchanged from the system.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// Premultiplied RGBA to premultiplied BGRA: swap bytes 0 and 2 of each pixel.
fn copy_rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    for (s, d) in src.as_chunks::<4>().0.iter().zip(dst.as_chunks_mut::<4>().0) {
        let rgba = u32::from_le_bytes(*s);
        let bgra = (rgba & 0xFF00_FF00) | ((rgba & 0xFF) << 16) | ((rgba >> 16) & 0xFF);
        *d = bgra.to_le_bytes();
    }
}

/// One memory DC for the life of the overlay, with one top-down 32-bit DIB section
/// selected into it. The DIB is replaced only when the size changes.
struct Surface {
    dc: HDC,
    /// The DC's original bitmap, selected back before the DC is deleted.
    original: HGDIOBJ,
    bitmap: HBITMAP,
    bits: *mut u8,
    size: SIZE,
}

impl Surface {
    fn new() -> Result<Surface> {
        // SAFETY: no arguments; the DC is deleted in `Drop`.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(Error::from_thread());
        }
        Ok(Surface {
            dc,
            original: HGDIOBJ::default(),
            bitmap: HBITMAP::default(),
            bits: ptr::null_mut(),
            size: SIZE::default(),
        })
    }

    /// Makes the selected DIB exactly `size`. On error the old DIB stays selected.
    fn resize(&mut self, size: SIZE) -> Result<()> {
        if size == self.size && !self.bitmap.is_invalid() {
            return Ok(());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size.cx,
                biHeight: -size.cy, // top-down, like the tiny-skia pixmap
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = ptr::null_mut();
        // SAFETY: `info` and `bits` are locals; the new bitmap is owned below.
        let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
        // SAFETY: both handles are valid and owned by this surface.
        let previous = unsafe { SelectObject(self.dc, bitmap.into()) };
        if previous.is_invalid() {
            // SAFETY: the new bitmap was never selected, so it can be deleted.
            let _ = unsafe { DeleteObject(bitmap.into()) };
            return Err(Error::from_thread());
        }
        if self.bitmap.is_invalid() {
            self.original = previous;
        } else {
            // SAFETY: the old bitmap is no longer selected into any DC.
            let _ = unsafe { DeleteObject(self.bitmap.into()) };
        }
        self.bitmap = bitmap;
        self.bits = bits.cast();
        self.size = size;
        Ok(())
    }

    /// The DIB's pixels, tightly packed (32 bits per pixel needs no row padding).
    fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: GdiFlush finishes any pending GDI work on the DIB before we write it.
        unsafe {
            let _ = GdiFlush();
        }
        let len = self.size.cx as usize * self.size.cy as usize * 4;
        // SAFETY: `bits` points to the DIB section of `size`, which lives until the next
        // `resize` or `Drop`; both need `&mut self`, which this borrow holds.
        unsafe { std::slice::from_raw_parts_mut(self.bits, len) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the handles are owned by this surface and freed once, the bitmap after
        // it is selected out of the DC.
        unsafe {
            if !self.bitmap.is_invalid() {
                SelectObject(self.dc, self.original);
                let _ = DeleteObject(self.bitmap.into());
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_red_and_blue_and_keeps_alpha() {
        let src = [10, 20, 30, 40, 1, 2, 3, 255];
        let mut dst = [0; 8];
        copy_rgba_to_bgra(&src, &mut dst);
        assert_eq!(dst, [30, 20, 10, 40, 3, 2, 1, 255]);
    }

    #[test]
    fn the_floor_turns_only_transparent_pixels_into_faint_black() {
        let src = [0, 0, 0, 0, 10, 20, 30, 40];
        let mut dst = [9; 8];
        copy_rgba_to_bgra_floor(&src, &mut dst, true);
        assert_eq!(dst, [0, 0, 0, 1, 30, 20, 10, 40]);
        copy_rgba_to_bgra_floor(&src, &mut dst, false);
        assert_eq!(dst, [0, 0, 0, 0, 30, 20, 10, 40]);
    }
}
