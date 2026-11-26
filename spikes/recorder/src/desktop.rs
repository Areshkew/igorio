use std::ffi::c_void;
use std::mem::size_of;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use igorio_core::geom::Rect;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, POINT, RECT, SIZE};
use windows::Win32::Graphics::{
    Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1},
    Gdi::*,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, Error, w};

use crate::Result;
use crate::gpu::{CursorLayer, Gpu, Size, Timing};

pub struct Monitor {
    pub handle: HMONITOR,
    pub bounds: RECT,
}

impl Monitor {
    pub fn select(index: Option<usize>) -> Result<Self> {
        let mut monitors = Vec::<Monitor>::new();
        // SAFETY: the callback runs synchronously and borrows the vector exclusively.
        if !unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(collect_monitor),
                LPARAM((&mut monitors as *mut Vec<Monitor>) as isize),
            )
        }
        .as_bool()
        {
            return Err(Error::from_thread().into());
        }
        // The numbering is by physical origin, with negative coordinates preserved.
        monitors.sort_by_key(|m| (m.bounds.left, m.bounds.top));
        let selected = match index {
            Some(index) => index,
            None => {
                let mut cursor = POINT::default();
                // SAFETY: GetCursorPos writes a single POINT. Use the primary monitor if unavailable.
                let handle = if unsafe { GetCursorPos(&mut cursor) }.is_ok() {
                    // SAFETY: the point and fallback mode are value arguments.
                    unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) }
                } else {
                    // SAFETY: value arguments, requesting the primary monitor.
                    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
                };
                monitors
                    .iter()
                    .position(|m| m.handle == handle)
                    .ok_or("cursor monitor is unavailable")?
            }
        };
        let count = monitors.len();
        if selected >= count {
            return Err(format!("monitor index {selected} is out of range (found {count})").into());
        }
        println!(
            "Monitor {selected}/{count}, bounds {:?}",
            monitors[selected].bounds
        );
        Ok(monitors.remove(selected))
    }

    pub fn adapter(&self) -> Result<IDXGIAdapter> {
        // SAFETY: creates a DXGI factory owned by this call.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        for index in 0..32 {
            // SAFETY: DXGI bounds-checks adapter and output indices.
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
                break;
            };
            for output_index in 0..32 {
                // SAFETY: DXGI bounds-checks the output index.
                let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                    break;
                };
                // SAFETY: this is a live DXGI output.
                if unsafe { output.GetDesc()? }.Monitor == self.handle {
                    use windows::core::Interface;
                    return Ok(adapter.cast()?);
                }
            }
        }
        Err("no DXGI adapter owns the selected monitor".into())
    }
}

unsafe extern "system" fn collect_monitor(
    handle: HMONITOR,
    _: HDC,
    bounds: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: EnumDisplayMonitors provides bounds and the live Vec passed in LPARAM.
    unsafe {
        (&mut *(data.0 as *mut Vec<Monitor>)).push(Monitor {
            handle,
            bounds: *bounds,
        })
    };
    true.into()
}

pub const BLOCK: u32 = 52;
pub const INCLUDED_OFFSET: (i32, i32) = (40, 56);
pub const EXCLUDED_OFFSET: (i32, i32) = (124, 56);
pub const INCLUDED_RGB: [u8; 3] = [23, 245, 104];
pub const EXCLUDED_RGB: [u8; 3] = [247, 22, 211];

pub struct Markers(Sender<()>);

impl Markers {
    pub fn start(bounds: RECT, max_lifetime: Duration) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("marker windows".into())
            .spawn(move || {
                let windows = (|| -> Result<_> {
                    Ok((
                        marker(bounds, INCLUDED_OFFSET, INCLUDED_RGB, false)
                            .map_err(|e| format!("included marker: {e}"))?,
                        marker(bounds, EXCLUDED_OFFSET, EXCLUDED_RGB, true)
                            .map_err(|e| format!("excluded marker: {e}"))?,
                    ))
                })();
                let _windows = match windows {
                    Ok(windows) => windows,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let end = Instant::now() + max_lifetime;
                while Instant::now() < end {
                    let mut msg = MSG::default();
                    for _ in 0..64 {
                        // SAFETY: msg is a writable MSG. Dispatch uses the static window class.
                        if !unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                            break;
                        }
                        // SAFETY: msg was initialized by PeekMessageW on this thread.
                        unsafe { DispatchMessageW(&msg) };
                    }
                    if !matches!(
                        stop_rx.recv_timeout(Duration::from_millis(8)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
            })?;
        let markers = Self(stop_tx);
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| format!("marker windows did not become ready: {e}"))??;
        Ok(markers)
    }
}

impl Drop for Markers {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

struct Window(HWND);

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the UI thread destroys its own window exactly once.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

fn marker(bounds: RECT, offset: (i32, i32), rgb: [u8; 3], excluded: bool) -> Result<Window> {
    // SAFETY: returns the module for this process. No library load or ownership transfer.
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    // SAFETY: the built-in STATIC class and empty title are static strings. There is no creation data.
    let window = Window(
        unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                0,
                0,
                BLOCK as i32,
                BLOCK as i32,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW: {e}"))?,
    );
    if excluded {
        // SAFETY: this top-level window is owned by this process and is still hidden.
        unsafe { SetWindowDisplayAffinity(window.0, WDA_EXCLUDEFROMCAPTURE) }
            .map_err(|e| format!("SetWindowDisplayAffinity: {e}"))?;
    }
    let dib = Dib::new((BLOCK, BLOCK)).map_err(|e| format!("marker DIB: {e}"))?;
    // SAFETY: new DIB, no pending GDI operations. Fill each BGRA pixel with premultiplied opaque RGB.
    for pixel in unsafe { std::slice::from_raw_parts_mut(dib.bits, dib.len) }.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
    }
    let pos = POINT {
        x: bounds.left + offset.0,
        y: bounds.top + offset.1,
    };
    let size = SIZE {
        cx: BLOCK as i32,
        cy: BLOCK as i32,
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
        BlendFlags: 0,
    };
    // SAFETY: the selected DIB covers the requested area and each pointer is live through the call.
    unsafe {
        UpdateLayeredWindow(
            window.0,
            None,
            Some(&pos),
            Some(&size),
            Some(dib.dc),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    }
    .map_err(|e| format!("UpdateLayeredWindow: {e}"))?;
    // SAFETY: the window and its content are live. It must never take focus.
    unsafe {
        SetWindowPos(
            window.0,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
    .map_err(|e| format!("SetWindowPos: {e}"))?;
    Ok(window)
}

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u8,
    len: usize,
}

impl Dib {
    fn new(size: Size) -> Result<Self> {
        // SAFETY: a private memory DC, used only by this thread.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(format!("CreateCompatibleDC: {}", Error::from_thread()).into());
        }
        let mut dib = Self {
            dc,
            bitmap: HBITMAP::default(),
            original: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            len: size.0 as usize * size.1 as usize * 4,
        };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size.0 as i32,
                biHeight: -(size.1 as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: info describes a top-down 32-bit DIB. bits is a live output slot.
        dib.bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
        dib.bits = bits.cast();
        // SAFETY: both handles are valid GDI objects owned by the DIB.
        dib.original = unsafe { SelectObject(dib.dc, dib.bitmap.into()) };
        if dib.original.is_invalid() {
            return Err(format!("SelectObject: {}", Error::from_thread()).into());
        }
        Ok(dib)
    }

    fn draw_cursor(&mut self, icon: HICON, size: Size, background: u8) -> Result<&[u8]> {
        // SAFETY: this DIB owns len writable bytes. Previous GDI work was flushed before return.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len).fill(background) };
        // SAFETY: the private DC has a DIB of the given size selected and icon is a live cursor.
        let result = unsafe {
            DrawIconEx(
                self.dc,
                0,
                0,
                icon,
                size.0 as i32,
                size.1 as i32,
                0,
                None,
                DI_NORMAL,
            )
        };
        // SAFETY: flush this thread's GDI batch before reading DIB bytes, even on a draw error.
        let flushed = unsafe { GdiFlush() };
        result?;
        if !flushed.as_bool() {
            return Err("GDI failed to flush cursor bitmap".into());
        }
        // SAFETY: GDI has finished accessing the DIB, and &mut self prevents another draw.
        Ok(unsafe { std::slice::from_raw_parts(self.bits, self.len) })
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: the original bitmap belongs to the DC; deselect ours before freeing it.
        unsafe {
            if !self.original.is_invalid() {
                SelectObject(self.dc, self.original);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

struct IconInfo(ICONINFO);

impl Drop for IconInfo {
    fn drop(&mut self) {
        // SAFETY: GetIconInfo allocated both bitmaps if present and transferred ownership.
        unsafe {
            if !self.0.hbmColor.is_invalid() {
                let _ = DeleteObject(self.0.hbmColor.into());
            }
            if !self.0.hbmMask.is_invalid() {
                let _ = DeleteObject(self.0.hbmMask.into());
            }
        }
    }
}

struct CursorImage {
    cursor: HCURSOR,
    size: Size,
    hotspot: (u32, u32),
    layer: CursorLayer,
}

pub struct Cursor {
    scale: f64,
    image: Option<CursorImage>,
    pub timing: Timing,
    pub rendered: u64,
    pub uploads: u64,
    pub source_size: Option<Size>,
    pub errors: u64,
}

impl Cursor {
    pub fn new(scale: f64) -> Self {
        Self {
            scale,
            image: None,
            timing: Timing::default(),
            rendered: 0,
            uploads: 0,
            source_size: None,
            errors: 0,
        }
    }

    pub fn sample(&mut self, gpu: &Gpu, monitor: RECT) -> Option<CursorLayer> {
        let start = Instant::now();
        let layer = self.read(gpu, monitor).unwrap_or_else(|_| {
            self.errors += 1;
            None
        });
        self.timing.add(start.elapsed());
        if layer.is_some() {
            self.rendered += 1;
        }
        layer
    }

    fn read(&mut self, gpu: &Gpu, monitor: RECT) -> Result<Option<CursorLayer>> {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: info is writable and cbSize names the correct ABI layout.
        unsafe { GetCursorInfo(&mut info)? };
        let p = info.ptScreenPos;
        if info.flags != CURSOR_SHOWING
            || p.x < monitor.left
            || p.y < monitor.top
            || p.x >= monitor.right
            || p.y >= monitor.bottom
        {
            return Ok(None);
        }
        if self
            .image
            .as_ref()
            .is_none_or(|image| image.cursor != info.hCursor)
        {
            let image = load_cursor(gpu, info.hCursor)?;
            self.source_size = Some(image.size);
            self.image = Some(image);
            self.uploads += 1;
        }
        let image = self.image.as_ref().ok_or("missing cursor image")?;
        let rect = Rect::new(
            f64::from(p.x - monitor.left) - f64::from(image.hotspot.0) * self.scale,
            f64::from(p.y - monitor.top) - f64::from(image.hotspot.1) * self.scale,
            f64::from(image.size.0) * self.scale,
            f64::from(image.size.1) * self.scale,
        );
        Ok(Some(CursorLayer {
            rect: gpu.normalize(rect),
            views: image.layer.views.clone(),
        }))
    }
}

fn load_cursor(gpu: &Gpu, cursor: HCURSOR) -> Result<CursorImage> {
    let icon = HICON(cursor.0);
    let mut info = IconInfo(ICONINFO::default());
    // SAFETY: icon borrows the system HCURSOR. The output owns only the allocated bitmaps.
    unsafe { GetIconInfo(icon, &mut info.0)? };
    let monochrome = info.0.hbmColor.is_invalid();
    let bitmap = if monochrome {
        info.0.hbmMask
    } else {
        info.0.hbmColor
    };
    let mut desc = BITMAP::default();
    // SAFETY: bitmap is from GetIconInfo and desc is a writable BITMAP of the advertised size.
    let copied = unsafe {
        GetObjectW(
            bitmap.into(),
            size_of::<BITMAP>() as i32,
            Some((&mut desc as *mut BITMAP).cast()),
        )
    };
    let height = if monochrome {
        desc.bmHeight / 2
    } else {
        desc.bmHeight
    };
    if copied == 0 || !(1..=1024).contains(&desc.bmWidth) || !(1..=1024).contains(&height) {
        return Err("invalid cursor dimensions".into());
    }
    let size = (desc.bmWidth as u32, height as u32);
    let mut dib = Dib::new(size)?;
    let black = gpu.upload_cursor(size, dib.draw_cursor(icon, size, 0)?)?;
    let white = gpu.upload_cursor(size, dib.draw_cursor(icon, size, 255)?)?;
    Ok(CursorImage {
        cursor,
        size,
        hotspot: (info.0.xHotspot, info.0.yHotspot),
        layer: CursorLayer {
            rect: [0.0; 4],
            views: [Some(black), Some(white)],
        },
    })
}
