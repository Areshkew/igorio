mod desktop {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::sync::mpsc::{self, Sender};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, POINT, RECT, SIZE};
    use windows::Win32::Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::*,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{Error, w};

    use crate::Result;
    use crate::Size;

    const BLOCK: u32 = 52;
    const INCLUDED_OFFSET: (i32, i32) = (40, 56);
    const EXCLUDED_OFFSET: (i32, i32) = (124, 56);
    pub const INCLUDED_RGB: [u8; 3] = [23, 245, 104];
    pub const EXCLUDED_RGB: [u8; 3] = [247, 22, 211];

    pub struct Markers(Sender<()>);

    impl Markers {
        pub fn start(max_lifetime: Duration) -> Result<(Self, [RECT; 2])> {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (stop_tx, stop_rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("marker windows".into())
                .spawn(move || {
                    let windows = (|| -> Result<_> {
                        let mut point = POINT::default();
                        // SAFETY: Win32 writes to a live POINT and validates the monitor handle.
                        unsafe { GetCursorPos(&mut point)? };
                        // SAFETY: this monitor is selected in the window thread's coordinate space.
                        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
                        let mut info = MONITORINFO {
                            cbSize: size_of::<MONITORINFO>() as u32,
                            ..Default::default()
                        };
                        // SAFETY: info has the documented size and lives throughout the call.
                        unsafe { GetMonitorInfoW(monitor, &mut info) }.ok()?;
                        let included =
                            marker(info.rcMonitor, INCLUDED_OFFSET, INCLUDED_RGB, false)?;
                        let excluded = marker(info.rcMonitor, EXCLUDED_OFFSET, EXCLUDED_RGB, true)?;
                        let bounds = [included.rect()?, excluded.rect()?];
                        Ok(((included, excluded), bounds))
                    })();
                    let (_windows, bounds) = match windows {
                        Ok(windows) => windows,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(bounds));
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
            let bounds = ready_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| format!("marker windows did not become ready: {e}"))??;
            Ok((markers, bounds))
        }
    }

    impl Drop for Markers {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    struct Window(HWND);

    impl Window {
        fn rect(&self) -> Result<RECT> {
            let mut rect = RECT::default();
            // SAFETY: this window is live, and rect has the required size for this attribute.
            unsafe {
                DwmGetWindowAttribute(
                    self.0,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    (&mut rect as *mut RECT).cast(),
                    size_of::<RECT>() as u32,
                )?
            };
            Ok(rect)
        }
    }

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
        for pixel in unsafe { std::slice::from_raw_parts_mut(dib.bits, dib.len) }
            .as_chunks_mut::<4>()
            .0
        {
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
            dib.bitmap =
                unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
            dib.bits = bits.cast();
            // SAFETY: both handles are valid GDI objects owned by the DIB.
            dib.original = unsafe { SelectObject(dib.dc, dib.bitmap.into()) };
            if dib.original.is_invalid() {
                return Err(format!("SelectObject: {}", Error::from_thread()).into());
            }
            Ok(dib)
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
}

mod check {
    use std::{path::Path, ptr};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToInt64};
    use windows::core::{HSTRING, Interface};

    use crate::Size;
    use crate::desktop::{EXCLUDED_RGB, INCLUDED_RGB};
    use windows::Win32::Foundation::RECT;

    use crate::{Result, SECOND, require};

    fn pair(a: u32, b: u32) -> u64 {
        (u64::from(a) << 32) | u64::from(b)
    }

    const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    pub fn verify(
        path: &Path,
        source: Size,
        output: Size,
        duration: i64,
        expected: i64,
        [included, excluded_marker]: [RECT; 2],
    ) -> Result<()> {
        let mut attrs = None;
        // SAFETY: a writable output slot on the MF worker.
        unsafe { MFCreateAttributes(&mut attrs, 2)? };
        let attrs = require(attrs, "reader attributes")?;
        // SAFETY: enable basic software RGB conversion and use the software decoder.
        unsafe {
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)?;
        }
        let path = HSTRING::from(path.to_str().ok_or("MP4 path is not Unicode")?);
        // SAFETY: the path and attributes live through the call.
        let reader = unsafe { MFCreateSourceReaderFromURL(&path, &attrs) }
            .map_err(|e| format!("MP4 opens: {e}"))?;
        println!("PASS MP4 opens with Media Foundation");

        // SAFETY: asks the live reader for its first native video media type and attributes.
        let (codec, dimensions) = unsafe {
            let native = reader.GetNativeMediaType(VIDEO, 0)?;
            (
                native.GetGUID(&MF_MT_SUBTYPE)?,
                native.GetUINT64(&MF_MT_FRAME_SIZE)?,
            )
        };
        let mut ok = true;
        report(
            &mut ok,
            codec == MFVideoFormat_H264 && dimensions == pair(output.0, output.1),
            format!(
                "native H.264 video, {}x{} (expected {}x{})",
                dimensions >> 32,
                dimensions as u32,
                output.0,
                output.1
            ),
        );
        // SAFETY: MF_PD_DURATION is a presentation attribute, returned as an owned variant.
        let mut value = unsafe {
            reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)?
        };
        // SAFETY: this initialized variant lives through conversion and is then cleared exactly once.
        let measured = unsafe {
            let converted = PropVariantToInt64(&value);
            PropVariantClear(&mut value)?;
            converted?
        };
        report(
            &mut ok,
            (measured - duration).abs() <= SECOND / 5,
            format!(
                "duration {:.3} s (expected {:.3} s)",
                measured as f64 / SECOND as f64,
                duration as f64 / SECOND as f64
            ),
        );

        // SAFETY: configure the reader's selected video stream to decode to RGB32.
        let format = unsafe {
            let ty = MFCreateMediaType()?;
            ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            reader.SetCurrentMediaType(VIDEO, None, &ty)?;
            reader.GetCurrentMediaType(VIDEO)?
        };
        // SAFETY: reads metadata on the configured format. RGB32 has a DWORD-aligned row size.
        let stride = unsafe {
            format
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|v| v as i32)
                .or_else(|_| MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, output.0))?
        };

        let (mut count, mut max_gap) = (0, 0);
        let (mut first, mut previous) = (None, None);
        let (mut monotonic, mut eos) = (true, false);
        let (mut normal, mut excluded, mut zoomed, mut restored) = (None, None, None, None);
        for _ in 0..expected + 120 {
            let (mut flags, mut time, mut sample) = (0, 0_i64, None);
            // SAFETY: reader is synchronous and every requested output has a writable slot.
            unsafe {
                reader.ReadSample(
                    VIDEO,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )?
            };
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err("source reader signalled a decode error".into());
            }
            if let Some(sample) = sample {
                count += 1;
                first.get_or_insert(time);
                if let Some(prior) = previous {
                    let gap = time - prior;
                    monotonic &= gap > 0;
                    max_gap = max_gap.max(gap);
                }
                previous = Some(time);
                if normal.is_none() && (SECOND / 2..SECOND).contains(&time) {
                    let blocks = with_frame(&sample, output, stride, |pixels| {
                        [
                            pixels.block(source, included, INCLUDED_RGB, 1.0),
                            pixels.block(source, excluded_marker, EXCLUDED_RGB, 1.0),
                        ]
                    })?;
                    [normal, excluded] = blocks.map(Some);
                } else if zoomed.is_none() && (22 * SECOND / 10..23 * SECOND / 10).contains(&time) {
                    zoomed = Some(with_frame(&sample, output, stride, |pixels| {
                        pixels.block(source, included, INCLUDED_RGB, 2.0)
                    })?);
                } else if restored.is_none() && (33 * SECOND / 10..34 * SECOND / 10).contains(&time)
                {
                    restored = Some(with_frame(&sample, output, stride, |pixels| {
                        pixels.block(source, included, INCLUDED_RGB, 1.0)
                    })?);
                }
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                eos = true;
                break;
            }
        }
        report(
            &mut ok,
            eos && (count - expected).abs() <= 12,
            format!("decoded frame count {count} (expected {expected})"),
        );
        report(
            &mut ok,
            monotonic
                && first.is_some_and(|t| (0..=SECOND / 20).contains(&t))
                && previous.is_some_and(|t| (0..=SECOND / 20).contains(&(measured - t)))
                && max_gap <= SECOND / 20,
            format!(
                "strictly increasing timestamps, maximum gap {:.3} ms",
                max_gap as f64 / 10_000.0
            ),
        );

        let expected_width =
            f64::from(included.right - included.left) * f64::from(output.0) / f64::from(source.0);
        let expected_height =
            f64::from(included.bottom - included.top) * f64::from(output.1) / f64::from(source.1);
        let shape = normal.unwrap_or_default();
        report(
            &mut ok,
            (0.8..1.2).contains(&(f64::from(shape.width) / expected_width))
                && (0.8..1.2).contains(&(f64::from(shape.height) / expected_height))
                && shape.solid(),
            format!(
                "included green block {}x{} in first second (expected {:.0}x{:.0})",
                shape.width, shape.height, expected_width, expected_height
            ),
        );
        report(
            &mut ok,
            excluded.as_ref().is_some_and(|block| {
                f64::from(block.pixels) < expected_width * expected_height * 0.02
            }),
            format!(
                "excluded magenta block absent ({} matching pixels in its region)",
                excluded.unwrap_or_default().pixels
            ),
        );
        let large = zoomed.unwrap_or_default();
        let ratio_w = f64::from(large.width) / f64::from(shape.width);
        let ratio_h = f64::from(large.height) / f64::from(shape.height);
        report(
            &mut ok,
            (1.8..=2.2).contains(&ratio_w) && (1.8..=2.2).contains(&ratio_h) && large.solid(),
            format!(
                "zoomed green block {}x{} around 2 s ({ratio_w:.2}x wide, {ratio_h:.2}x tall)",
                large.width, large.height
            ),
        );
        report(
            &mut ok,
            restored.is_some_and(|block| {
                block.solid()
                    && block.width.abs_diff(shape.width) <= 3
                    && block.height.abs_diff(shape.height) <= 3
            }),
            "green marker returns to its first size after zoom reset".into(),
        );
        if ok {
            Ok(())
        } else {
            Err("selfcheck hard checks failed".into())
        }
    }

    fn report(ok: &mut bool, passed: bool, message: String) {
        *ok &= passed;
        println!("{} {message}", if passed { "PASS" } else { "FAIL" });
    }

    #[derive(Default)]
    struct Block {
        width: u32,
        height: u32,
        pixels: u32,
    }

    impl Block {
        fn solid(&self) -> bool {
            self.width > 0
                && self.height > 0
                && f64::from(self.pixels) / f64::from(self.width * self.height) > 0.7
        }
    }

    struct Pixels<'a> {
        size: Size,
        data: &'a [u8],
        first: isize,
        stride: isize,
    }

    impl Pixels<'_> {
        fn block(&self, source: Size, bounds: RECT, rgb: [u8; 3], zoom: f64) -> Block {
            let sx = f64::from(self.size.0) * zoom / f64::from(source.0);
            let sy = f64::from(self.size.1) * zoom / f64::from(source.1);
            let margin = if zoom > 1.0 { 48 } else { 24 };
            let x0 = ((f64::from(bounds.left) * sx).floor() as u32).saturating_sub(margin);
            let y0 = ((f64::from(bounds.top) * sy).floor() as u32).saturating_sub(margin);
            let x1 = ((f64::from(bounds.right) * sx).ceil() as u32 + 12).min(self.size.0);
            let y1 = ((f64::from(bounds.bottom) * sy).ceil() as u32 + 12).min(self.size.1);
            let (mut min_x, mut min_y, mut max_x, mut max_y, mut matches) = (x1, y1, x0, y0, 0);
            for y in y0..y1 {
                let line = (self.first + y as isize * self.stride) as usize;
                for x in x0..x1 {
                    let i = line + 4 * x as usize;
                    let pixel = &self.data[i..i + 3];
                    if pixel
                        .iter()
                        .zip(rgb.iter().rev())
                        .all(|(a, b)| a.abs_diff(*b) <= 32)
                    {
                        matches += 1;
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x);
                        max_y = max_y.max(y);
                    }
                }
            }
            if matches == 0 {
                Block::default()
            } else {
                Block {
                    width: max_x - min_x + 1,
                    height: max_y - min_y + 1,
                    pixels: matches,
                }
            }
        }
    }

    fn with_frame<T>(
        sample: &IMFSample,
        size: Size,
        stride: i32,
        check: impl FnOnce(Pixels<'_>) -> T,
    ) -> Result<T> {
        // SAFETY: sample is an owned RGB32 frame, and the reader selected its current format.
        let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
        if let Ok(buffer) = buffer.cast::<IMF2DBuffer2>() {
            let (mut row, mut start, mut stride, mut len) =
                (ptr::null_mut(), ptr::null_mut(), 0, 0);
            // SAFETY: each output slot is valid for the lifetime of this lock.
            unsafe {
                buffer.Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut row,
                    &mut stride,
                    &mut start,
                    &mut len,
                )?
            };
            // SAFETY: buffer describes len readable bytes between Lock2DSize and Unlock2D.
            let result = unsafe {
                scan(
                    start,
                    len,
                    row.addr() as isize - start.addr() as isize,
                    stride,
                    size,
                    check,
                )
            };
            // SAFETY: matches the one successful read lock above.
            unsafe { buffer.Unlock2D()? };
            return result;
        }
        let (mut data, mut len) = (ptr::null_mut(), 0);
        // SAFETY: both output slots are live. This legacy reader buffer is contiguous.
        unsafe { buffer.Lock(&mut data, None, Some(&mut len))? };
        let first = if stride < 0 {
            (size.1 - 1) as isize * -(stride as isize)
        } else {
            0
        };
        // SAFETY: buffer describes len readable bytes during the current Lock.
        let result = unsafe { scan(data, len, first, stride, size, check) };
        // SAFETY: matches the successful lock above.
        unsafe { buffer.Unlock()? };
        result
    }

    unsafe fn scan<T>(
        start: *const u8,
        len: u32,
        first: isize,
        stride: i32,
        size: Size,
        check: impl FnOnce(Pixels<'_>) -> T,
    ) -> Result<T> {
        let stride = stride as isize;
        let last = first + (size.1 - 1) as isize * stride;
        let row_bytes = size.0 as isize * 4;
        if start.is_null()
            || first.min(last) < 0
            || first.max(last) + row_bytes > len as isize
            || stride.unsigned_abs() < row_bytes as usize
        {
            return Err(format!(
                "invalid decoded RGB32 layout (offset {first}, stride {stride}, bytes {len})"
            )
            .into());
        }
        // SAFETY: caller holds the MF read lock, and each referenced row lies inside the reported allocation.
        let data = unsafe { std::slice::from_raw_parts(start, len as usize) };
        Ok(check(Pixels {
            size,
            data,
            first,
            stride,
        }))
    }

    pub fn playable(path: &Path) -> Result<()> {
        let path = HSTRING::from(path.as_os_str());
        // SAFETY: the path is a live UTF-16 string and MF is initialized on this thread.
        let mut attrs = None;
        // SAFETY: attrs is writable and MF is initialized.
        unsafe { MFCreateAttributes(&mut attrs, 1)? };
        let attrs = require(attrs, "playback attributes")?;
        // SAFETY: live attribute store, path, reader and media type. Configure decoded NV12.
        let reader = unsafe {
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)?;
            let reader = MFCreateSourceReaderFromURL(&path, &attrs)?;
            let native = reader.GetNativeMediaType(VIDEO, 0)?;
            if native.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_H264 {
                return Err("reopened file does not contain H.264".into());
            }
            let ty = MFCreateMediaType()?;
            ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            reader.SetCurrentMediaType(VIDEO, None, &ty)?;
            reader
        };
        for _ in 0..8 {
            let (mut flags, mut sample) = (0, None);
            // SAFETY: reader is synchronous and both output slots are writable.
            unsafe {
                reader.ReadSample(VIDEO, 0, None, Some(&mut flags), None, Some(&mut sample))?
            };
            if sample.is_some() {
                return Ok(());
            }
            if flags & ((MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32) != 0 {
                break;
            }
        }
        Err("reopened H.264 file did not yield a decoded sample".into())
    }
}

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use igorio_app::hook;
use igorio_app::recording::recorder::{
    CursorMode, FrameTiming, MonitorId, Recorder, RecorderEvent, RecordingConfig,
};
use igorio_core::geom::Rect;
use windows::Win32::Foundation::{FILETIME, RECT, WAIT_FAILED};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, WaitForSingleObject};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Size = (u32, u32);
const SECOND: i64 = 10_000_000;
const SHUTDOWN: Duration = Duration::from_secs(8);

fn main() -> ExitCode {
    let (tx, rx) = mpsc::sync_channel(1);
    let keep = std::env::args().any(|arg| arg == "--keep");
    std::thread::spawn(move || {
        let result = run(keep);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(Duration::from_secs(50)) {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) => {
            eprintln!("FAIL {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("FAIL self-test exceeded its deadline: {e}");
            ExitCode::FAILURE
        }
    }
}

fn require<T>(slot: Option<T>, name: &str) -> Result<T> {
    slot.ok_or_else(|| format!("{name} returned no object").into())
}

fn hard(condition: bool, name: &str) -> Result<()> {
    if condition {
        println!("PASS {name}");
        Ok(())
    } else {
        Err(name.to_string().into())
    }
}

struct Scratch {
    folder: PathBuf,
    keep: bool,
}

impl Scratch {
    fn new(keep: bool) -> Result<Self> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/recorder-selftest");
        fs::create_dir_all(&root)?;
        let folder = root.join(format!(
            "run-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir(&folder)?;
        Ok(Self { folder, keep })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.keep {
            println!("Kept recordings in {}", self.folder.display());
        } else if let Ok(entries) = fs::read_dir(&self.folder) {
            for file in entries.flatten() {
                let _ = fs::remove_file(file.path());
            }
            let _ = fs::remove_dir(&self.folder);
        }
    }
}

struct Media;

impl Media {
    fn start() -> Result<Self> {
        // SAFETY: this is a fresh verification thread with no initialized apartment.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        // SAFETY: COM is initialized on this thread.
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            // SAFETY: balances the successful RoInitialize above.
            unsafe { RoUninitialize() };
            return Err(e.into());
        }
        Ok(Self)
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        // SAFETY: balances this thread's startup after dropping all readers.
        unsafe {
            let _ = MFShutdown();
            RoUninitialize();
        }
    }
}

fn run(keep: bool) -> Result<()> {
    let media = Media::start()?;
    let scratch = Scratch::new(keep)?;
    let monitor = MonitorId::under_cursor()?;
    let bounds = monitor.bounds()?;
    let config = RecordingConfig::new(monitor, scratch.folder.clone());
    let (markers, marker_rects) = desktop::Markers::start(Duration::from_secs(9))?;
    let marker_rects = marker_rects.map(|rect| RECT {
        left: rect.left - bounds.x as i32,
        top: rect.top - bounds.y as i32,
        right: rect.right - bounds.x as i32,
        bottom: rect.bottom - bounds.y as i32,
    });
    let cpu_start = cpu_time()?;
    let wall = Instant::now();
    let mut recorder = Recorder::start(config.clone())?;
    hard(
        Recorder::start(config.clone()).is_err(),
        "second start is refused",
    )?;
    let (events, sent_zoom, sent_reset) = scripted(&recorder, bounds)?;
    hard(recorder.shutdown(SHUTDOWN), "recording worker joined")?;
    drop(markers);
    let cpu_elapsed = cpu_time()? - cpu_start;
    let wall_elapsed = wall.elapsed();
    let (size, progress, file, duration) = match events.split_first() {
        Some((
            RecorderEvent::Started {
                output_size,
                monitor_rect,
                scale,
                ..
            },
            rest,
        )) if *monitor_rect == bounds => {
            let (last, progress) = rest.split_last().ok_or("missing Finished event")?;
            match last {
                RecorderEvent::Finished {
                    file,
                    poster,
                    scale: poster_scale,
                    duration,
                } => {
                    hard(
                        *output_size == (poster.width, poster.height)
                            && *scale == *poster_scale
                            && poster.rgba.len() == (poster.width * poster.height * 4) as usize,
                        "poster has output size, scale and RGBA length",
                    )?;
                    (*output_size, progress, file, *duration)
                }
                RecorderEvent::Failed { reason, .. } => {
                    return Err(format!("recording failed: {reason}").into());
                }
                _ => return Err("last event must be Finished".into()),
            }
        }
        _ => return Err(format!("missing Started: {events:?}").into()),
    };
    hard(
        !progress.is_empty()
            && progress
                .iter()
                .all(|event| matches!(event, RecorderEvent::Progress { .. })),
        "Started, Progress and Finished arrived in order",
    )?;
    hard(
        sent_zoom && sent_reset,
        "script sent full, 2× and full areas",
    )?;
    hard(
        (duration - 4.0).abs() <= 0.2,
        "Finished duration is within 0.2 s of 4 s",
    )?;
    hard(valid_name(file), "final name follows the naming pattern")?;
    hard(
        fs::read_dir(&scratch.folder)?.count() == 1,
        "finalization left no temporary file",
    )?;
    hard(fast_start(file)?, "MP4 has ftyp and moov before mdat")?;

    let stats = recorder.metrics().ok_or("missing recording measurements")?;
    println!(
        "Encoder: {} ({}), {} fps, border request rejected: {}",
        stats.encoder.as_deref().unwrap_or("unidentified H.264 MFT"),
        if stats.hardware_encoder {
            "hardware requested"
        } else {
            "software"
        },
        stats.fps,
        stats.border_request_rejected,
    );
    timing("Crop/scale + NV12 CPU submission", stats.crop);
    timing("Sink Writer WriteSample", stats.write);
    println!(
        "Recording: {:.3} s wall, {cpu_elapsed:.3} s process CPU, {} output frames, {} capture updates, {} dropped",
        wall_elapsed.as_secs_f64(),
        stats.write.frames,
        stats.captured_updates,
        stats.dropped
    );
    let bytes = fs::metadata(file)?.len();
    println!(
        "MP4: {bytes} bytes ({:.0} bytes/s)",
        bytes as f64 / duration
    );
    check::verify(
        file,
        (bounds.width as u32, bounds.height as u32),
        size,
        4 * SECOND,
        4 * i64::from(stats.fps),
        marker_rects,
    )?;

    let mut early = Recorder::start(config.clone())?;
    hard(
        early.shutdown(SHUTDOWN),
        "immediate stop joined within its deadline",
    )?;
    let mut early_events = Vec::new();
    early.drain(&mut early_events);
    match early_events.last() {
        Some(RecorderEvent::Failed {
            reason,
            partial_file: None,
        }) => hard(
            reason.contains("stopped before the first"),
            "immediate stop gave a clean Failed",
        )?,
        Some(RecorderEvent::Finished { file, .. }) => {
            check::playable(file)?;
            println!("PASS immediate stop finalized a playable short file");
        }
        _ => return Err(format!("unexpected immediate-stop events: {early_events:?}").into()),
    }

    let mut drawn_config = config;
    drawn_config.cursor = CursorMode::Drawn { scale: 2.0 };
    let mut drawn = Recorder::start(drawn_config)?;
    let deadline = Instant::now() + SHUTDOWN;
    let mut events = Vec::new();
    let time = loop {
        drawn.drain(&mut events);
        match events.first() {
            Some(RecorderEvent::Started { time, .. }) => break *time,
            Some(RecorderEvent::Failed { reason, .. }) => return Err(reason.clone().into()),
            _ => wait_event(&drawn, deadline)?,
        }
    };
    while hook::now() - time < 0.65 {
        wait_event(&drawn, deadline)?;
        drawn.drain(&mut events);
    }
    hard(
        drawn.shutdown(SHUTDOWN),
        "shutdown during drawn-cursor recording joined within its deadline",
    )?;
    drawn.drain(&mut events);
    let drawn_file = match events.last() {
        Some(RecorderEvent::Finished { file, .. }) => file,
        Some(RecorderEvent::Failed { reason, .. }) => return Err(reason.clone().into()),
        _ => return Err("shutdown did not publish Finished".into()),
    };
    check::playable(drawn_file)?;
    hard(
        fast_start(drawn_file)?,
        "shutdown finalized playable H.264 with fast start",
    )?;
    let stats = drawn.metrics().ok_or("missing drawn cursor measurements")?;
    hard(
        stats.cursor.frames > 0,
        "drawn-cursor mode polled the cursor",
    )?;
    println!(
        "Drawn cursor: {} composited frames",
        stats.drawn_cursor_frames
    );
    timing("Drawn cursor poll/upload CPU", stats.cursor);
    hard(
        fs::read_dir(&scratch.folder)?.count() == 2,
        "all attempts left only the two final MP4 files",
    )?;
    drop(media);
    Ok(())
}

fn scripted(recorder: &Recorder, bounds: Rect) -> Result<(Vec<RecorderEvent>, bool, bool)> {
    let deadline = Instant::now() + Duration::from_secs(17);
    let mut events = Vec::new();
    let (mut time, mut zoom, mut reset) = (None, false, false);
    loop {
        recorder.drain(&mut events);
        if matches!(
            events.last(),
            Some(RecorderEvent::Finished { .. } | RecorderEvent::Failed { .. })
        ) {
            return Ok((events, zoom, reset));
        }
        if let Some(RecorderEvent::Started { time: start, .. }) = events.first() {
            time = Some(*start);
        }
        if let Some(time) = time {
            let elapsed = hook::now() - time;
            if elapsed >= 1.5 && !zoom {
                recorder.set_area(Rect::new(0.0, 0.0, bounds.width / 2.0, bounds.height / 2.0));
                zoom = true;
            }
            if elapsed >= 3.0 && !reset {
                recorder.set_area(Rect::new(0.0, 0.0, bounds.width, bounds.height));
                reset = true;
            }
            if elapsed >= 4.0 {
                recorder.stop();
            }
        }
        wait_event(recorder, deadline)?;
    }
}

fn wait_event(recorder: &Recorder, deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err("timed out waiting for recorder events".into());
    }
    // SAFETY: Recorder owns this event and is borrowed through the bounded wait.
    if unsafe { WaitForSingleObject(recorder.wake_handle(), 5) } == WAIT_FAILED {
        return Err(windows::core::Error::from_thread().into());
    }
    Ok(())
}

fn valid_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let Some(date) = name
        .strip_prefix("Screen Recording ")
        .and_then(|s| s.strip_suffix(".mp4"))
    else {
        return false;
    };
    let pattern = "0000-00-00 at 00.00.00";
    date.len() == pattern.len()
        && date.bytes().zip(pattern.bytes()).all(|(actual, expected)| {
            if expected == b'0' {
                actual.is_ascii_digit()
            } else {
                actual == expected
            }
        })
}

fn fast_start(path: &Path) -> Result<bool> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    let (mut ftyp, mut moov) = (false, false);
    let mut offset = 0;
    for _ in 0..32 {
        if offset + 8 > len {
            break;
        }
        f.seek(SeekFrom::Start(offset))?;
        let mut header = [0; 8];
        f.read_exact(&mut header)?;
        let size = u64::from(u32::from_be_bytes(header[..4].try_into()?));
        let size = if size == 1 {
            let mut extended = [0; 8];
            f.read_exact(&mut extended)?;
            u64::from_be_bytes(extended)
        } else if size == 0 {
            len - offset
        } else {
            size
        };
        match &header[4..] {
            b"ftyp" => ftyp = true,
            b"moov" => moov = true,
            b"mdat" => return Ok(ftyp && moov),
            _ => {}
        }
        if size < 8 || size > len - offset {
            break;
        }
        offset += size;
    }
    Ok(false)
}

fn cpu_time() -> Result<f64> {
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: the pseudo-handle represents this process and all outputs are writable.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )?
    };
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Ok((ticks(kernel) + ticks(user)) as f64 / SECOND as f64)
}

fn timing(label: &str, timing: FrameTiming) {
    let average = timing.total.as_secs_f64() * 1000.0 / timing.frames.max(1) as f64;
    println!(
        "{label}: {average:.3} ms avg, {:.3} ms max ({} frames)",
        timing.maximum.as_secs_f64() * 1000.0,
        timing.frames
    );
}
