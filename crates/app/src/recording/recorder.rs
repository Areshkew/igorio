//! Screen recording for one monitor per process.
//!
//! The caller owns the viewport and its visual overlays. A single worker initializes
//! WinRT/COM MTA and Media Foundation, then owns WGC, D3D11, and the Sink Writer.
//! WGC delivers to its own free-threaded pool. Its Closed callback only sets an atomic.
//! MF may use its own threads. The D3D11 immediate context has multithread protection.
//! The worker drops held frames, closes capture, finalizes/releases the writer,
//! releases D3D11, shuts down MF, and uninitializes COM, in that order.
//! Application waits are bounded. Synchronous OS/driver calls can stall internally.
//! Shutdown's deadline bounds the caller's wait. A timed-out worker retains the
//! process-wide reservation until it exits so another recording cannot race it.
//! Windows may retain the system capture border and may black out protected content.

use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::{ManuallyDrop, size_of};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use igorio_core::geom::{Point, Rect};
use igorio_kit::image::Image;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat};
use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, E_INVALIDARG, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, HANDLE, LPARAM,
    POINT, RECT, S_OK, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::{
    Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob,
        ID3DInclude,
    },
    Direct3D11::*,
    Dxgi::{
        Common::*, CreateDXGIFactory1, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_NOT_FOUND,
        DXGI_ERROR_WAS_STILL_DRAWING, IDXGIAdapter, IDXGIDevice, IDXGIFactory1,
    },
    Gdi::*,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
use windows::Win32::System::{
    Com::IPersist,
    Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, SetEvent,
        SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
    },
    WinRT::{
        Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
        Graphics::Capture::IGraphicsCaptureItemInterop,
        RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
    },
};
use windows::Win32::UI::{
    HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
    Shell::GetScaleFactorForMonitor,
    WindowsAndMessaging::*,
};
use windows::core::{
    BOOL, Error, GUID, HSTRING, IInspectable, Interface, PCSTR, PCWSTR, Result, factory, s,
};

const SECOND: i64 = 10_000_000;
const DEFAULT_FPS: u32 = 60;
const DEFAULT_CAP: Size = (3840, 2160);
const SOFTWARE_CAP: Size = (1920, 1080);
const SOFTWARE_FPS: u32 = 30;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
type Size = (u32, u32);

fn error(message: impl AsRef<str>) -> Error {
    Error::new(E_FAIL, message.as_ref())
}

fn require<T>(slot: Option<T>, name: &str) -> Result<T> {
    slot.ok_or_else(|| error(format!("{name} returned no object")))
}

/// A monitor selected in global physical pixels. Valid for the current display topology.
#[derive(Clone, Copy, Debug)]
pub struct MonitorId(usize);

impl MonitorId {
    pub fn at(point: Point) -> Result<Self> {
        let mut monitors = Vec::<Self>::new();
        // SAFETY: the callback runs synchronously and borrows this Vec exclusively.
        if !unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(collect_monitor),
                LPARAM((&mut monitors as *mut Vec<Self>) as isize),
            )
        }
        .as_bool()
        {
            return Err(Error::from_thread());
        }
        monitors
            .into_iter()
            .filter_map(|monitor| {
                let bounds = monitor.bounds().ok()?;
                let dx = (bounds.x - point.x).max(point.x - bounds.max_x()).max(0.0);
                let dy = (bounds.y - point.y).max(point.y - bounds.max_y()).max(0.0);
                Some((monitor, !bounds.contains(point), dx * dx + dy * dy))
            })
            .min_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)))
            .map(|(monitor, _, _)| monitor)
            .ok_or_else(|| error("no monitor for point"))
    }

    pub fn under_cursor() -> Result<Self> {
        let mut point = POINT::default();
        // SAFETY: point is a writable Win32 POINT.
        unsafe { GetPhysicalCursorPos(&mut point)? };
        Self::at(Point::new(point.x.into(), point.y.into()))
    }

    fn handle(self) -> HMONITOR {
        HMONITOR(self.0 as _)
    }

    /// Full monitor bounds in global physical pixels, including negative origins.
    pub fn bounds(self) -> Result<Rect> {
        self.geometry().map(|geometry| geometry.rect())
    }

    fn geometry(self) -> Result<MonitorGeometry> {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: cbSize advertises the extended allocation, which contains the device name.
        if !unsafe { GetMonitorInfoW(self.handle(), &mut info.monitorInfo) }.as_bool() {
            return Err(error("recording monitor is unavailable"));
        }
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: the device name is NUL-terminated and the DEVMODEW has the specified size.
        if !unsafe {
            EnumDisplaySettingsW(
                PCWSTR(info.szDevice.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            )
        }
        .as_bool()
        {
            return Err(error("recording monitor has no current display mode"));
        }
        // SAFETY: a display DEVMODEW with DM_POSITION uses the display member of this union.
        if !mode.dmFields.contains(DM_POSITION) {
            return Err(error("recording monitor has no current desktop position"));
        }
        let origin = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
        let (mut x, mut y) = (0, 0);
        // SAFETY: both DPI outputs are live and distinct.
        unsafe { GetDpiForMonitor(self.handle(), MDT_EFFECTIVE_DPI, &mut x, &mut y)? };
        // Unmanifested tools receive 96 from GetDpiForMonitor even on a scaled desktop.
        // SAFETY: the monitor is live; this query does not change DPI awareness.
        if x == 96
            && let Ok(scale) = unsafe { GetScaleFactorForMonitor(self.handle()) }
        {
            x = (96.0 * f64::from(scale.0) / 100.0).round() as u32;
            y = x;
        }
        Ok(MonitorGeometry {
            bounds: RECT {
                left: origin.x,
                top: origin.y,
                right: origin.x + mode.dmPelsWidth as i32,
                bottom: origin.y + mode.dmPelsHeight as i32,
            },
            dpi: (x, y),
        })
    }

    fn adapter(self) -> Result<IDXGIAdapter> {
        // SAFETY: creates a reference-counted DXGI factory on the worker.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        for index in 0..32 {
            // SAFETY: factory validates the adapter index.
            let adapter = match unsafe { factory.EnumAdapters1(index) } {
                Ok(adapter) => adapter,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e),
            };
            for index in 0..32 {
                // SAFETY: adapter validates the output index.
                let output = match unsafe { adapter.EnumOutputs(index) } {
                    Ok(output) => output,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e),
                };
                // SAFETY: output is a live DXGI interface.
                if unsafe { output.GetDesc()? }.Monitor == self.handle() {
                    return adapter.cast();
                }
            }
        }
        Err(error("no DXGI adapter owns the selected monitor"))
    }
}

unsafe extern "system" fn collect_monitor(
    handle: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: EnumDisplayMonitors passes the live Vec from MonitorId::at in LPARAM.
    unsafe { (&mut *(data.0 as *mut Vec<MonitorId>)).push(MonitorId(handle.0 as usize)) };
    true.into()
}

#[derive(Clone, Copy, PartialEq)]
struct MonitorGeometry {
    bounds: RECT,
    dpi: Size,
}

impl MonitorGeometry {
    fn size(self) -> Size {
        (
            (self.bounds.right - self.bounds.left).max(0) as u32,
            (self.bounds.bottom - self.bounds.top).max(0) as u32,
        )
    }

    fn rect(self) -> Rect {
        let (w, h) = self.size();
        Rect::new(
            self.bounds.left.into(),
            self.bounds.top.into(),
            w.into(),
            h.into(),
        )
    }

    fn validate(self, current: Self, frame: Size) -> Result<()> {
        if current != self || frame != self.size() {
            return Err(error(
                "monitor bounds, resolution or DPI changed during recording",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub enum CursorMode {
    #[default]
    System,
    Drawn {
        scale: f64,
    },
}

#[derive(Clone, Debug)]
pub struct RecordingConfig {
    pub monitor: MonitorId,
    pub folder: PathBuf,
    pub cursor: CursorMode,
    pub max_size: Size,
    pub fps: u32,
}

impl RecordingConfig {
    pub fn new(monitor: MonitorId, folder: PathBuf) -> Self {
        Self {
            monitor,
            folder,
            cursor: CursorMode::System,
            max_size: DEFAULT_CAP,
            fps: DEFAULT_FPS,
        }
    }
}

#[derive(Debug)]
pub enum RecorderEvent {
    /// Monitor bounds are in global physical pixels. Time uses the hook clock.
    Started {
        monitor_rect: Rect,
        output_size: Size,
        scale: f64,
        time: f64,
    },
    /// Most recent progress, at most four updates per second. Dropped counts missed output ticks.
    Progress {
        elapsed: f64,
        frames: u64,
        dropped: u64,
    },
    Finished {
        file: PathBuf,
        poster: Arc<Image>,
        scale: f64,
        duration: f64,
    },
    /// A nonempty recovery file may remain under a temporary name. It may need repair.
    Failed {
        reason: String,
        partial_file: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTiming {
    pub frames: u64,
    pub total: Duration,
    pub maximum: Duration,
}

impl FrameTiming {
    fn add(&mut self, elapsed: Duration) {
        self.frames += 1;
        self.total += elapsed;
        self.maximum = self.maximum.max(elapsed);
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecordingMetrics {
    pub hardware_encoder: bool,
    pub encoder: Option<String>,
    pub fps: u32,
    pub border_request_rejected: bool,
    pub captured_updates: u64,
    pub drawn_cursor_frames: u64,
    pub dropped: u64,
    pub crop: FrameTiming,
    pub write: FrameTiming,
    pub cursor: FrameTiming,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Reservation;

impl Reservation {
    fn acquire() -> Result<Self> {
        ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| {
                Error::new(
                    ERROR_ALREADY_EXISTS.to_hresult(),
                    "a recording is already active",
                )
            })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

struct OwnedHandle(HANDLE);

// SAFETY: this owns a kernel synchronization object. Win32 supports concurrent
// waits and signals, and Arc keeps the handle alive until all users release it.
unsafe impl Send for OwnedHandle {}
// SAFETY: the handle is never mutated or closed through a shared reference.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    fn event(manual: bool) -> Result<Self> {
        // SAFETY: no security attributes or event name are supplied.
        Ok(Self(unsafe { CreateEventW(None, manual, false, None)? }))
    }

    fn signal(&self) {
        // SAFETY: this is a live event handle owned by self.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: self owns this valid handle and closes it exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct Shared {
    stopping: AtomicBool,
    recording: AtomicBool,
    stop: OwnedHandle,
    wake: OwnedHandle,
    area: Mutex<Option<Rect>>,
    events: Mutex<Vec<RecorderEvent>>,
    metrics: Mutex<Option<RecordingMetrics>>,
}

impl Shared {
    fn emit(&self, event: RecorderEvent) {
        let mut events = self.events.lock().unwrap();
        if matches!(event, RecorderEvent::Progress { .. })
            && matches!(events.last(), Some(RecorderEvent::Progress { .. }))
        {
            events.pop();
        }
        events.push(event);
        drop(events);
        self.wake.signal();
    }

    fn stopped(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    fn request_stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.stop.signal();
    }
}

pub struct Recorder {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts the worker. Setup results arrive through Started or Failed.
    pub fn start(config: RecordingConfig) -> Result<Self> {
        let reservation = Reservation::acquire()?;
        let shared = Arc::new(Shared {
            stopping: AtomicBool::new(false),
            recording: AtomicBool::new(true),
            stop: OwnedHandle::event(true)?,
            wake: OwnedHandle::event(false)?,
            area: Mutex::new(None),
            events: Mutex::new(Vec::with_capacity(3)),
            metrics: Mutex::new(None),
        });
        let state = Arc::clone(&shared);
        let started_at = SystemTime::now();
        let worker = thread::Builder::new()
            .name("screen recorder MTA".into())
            .spawn(move || {
                let event = run(config, &state, started_at);
                state.recording.store(false, Ordering::Release);
                drop(reservation);
                state.emit(event);
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Replaces the crop in monitor-local physical pixels. The worker never waits for it.
    pub fn set_area(&self, area: Rect) {
        *self.shared.area.lock().unwrap() = Some(area);
    }

    pub fn stop(&self) {
        self.shared.request_stop();
    }

    pub fn drain(&self, out: &mut Vec<RecorderEvent>) {
        let mut events = self.shared.events.lock().unwrap();
        out.append(&mut events);
    }

    /// Borrowed auto-reset event for the caller's wait loop. Stays live until Recorder drops.
    pub fn wake_handle(&self) -> HANDLE {
        self.shared.wake.0
    }

    /// True during startup, capture and finalization.
    pub fn is_recording(&self) -> bool {
        self.shared.recording.load(Ordering::Acquire)
    }

    /// Available when the recording attempt returns from the pipeline.
    pub fn metrics(&self) -> Option<RecordingMetrics> {
        self.shared.metrics.lock().unwrap().clone()
    }

    /// Request stop and wait at most the given duration. Events remain available to drain.
    /// A caller can retry after a timeout. Dropping Recorder requests stop without waiting.
    pub fn shutdown(&mut self, within: Duration) -> bool {
        self.stop();
        let Some(worker) = &self.worker else {
            return true;
        };
        let millis = within.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        // SAFETY: the join handle owns this Windows thread handle throughout the wait.
        let done = unsafe { WaitForSingleObject(HANDLE(worker.as_raw_handle()), millis) };
        done == WAIT_OBJECT_0 && self.worker.take().unwrap().join().is_ok()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn output_size(source: Size, cap: Size) -> Result<Size> {
    if source.0 < 2 || source.1 < 2 || cap.0 < 2 || cap.1 < 2 {
        return Err(Error::new(
            E_INVALIDARG,
            "capture and output cap must be at least 2×2",
        ));
    }
    let fit = 1.0_f64
        .min(f64::from(cap.0) / f64::from(source.0))
        .min(f64::from(cap.1) / f64::from(source.1));
    let even = |side| ((f64::from(side) * fit).round() as u32 & !1).max(2);
    Ok((even(source.0), even(source.1)))
}

fn clamp_area(area: Rect, source: Size) -> Rect {
    let (w, h) = (f64::from(source.0), f64::from(source.1));
    if ![area.x, area.y, area.width, area.height]
        .iter()
        .all(|v| v.is_finite())
        || area.is_empty()
    {
        return Rect::new(0.0, 0.0, w, h);
    }
    let width = area.width.clamp(1.0, w);
    let height = area.height.clamp(1.0, h);
    Rect::new(
        area.x.clamp(0.0, w - width),
        area.y.clamp(0.0, h - height),
        width,
        height,
    )
}

/// Index and timestamps of QPC-aligned CFR slots (100 ns units).
struct Schedule(u32);

impl Schedule {
    fn time(&self, slot: i64) -> i64 {
        slot * SECOND / i64::from(self.0)
    }

    fn due(&self, elapsed: i64) -> i64 {
        ((elapsed.max(0) + 1) * i64::from(self.0) - 1) / SECOND
    }

    fn accepts(&self, capture: i64, current: i64, slot: i64) -> bool {
        current < capture && capture <= self.time(slot)
    }
}

struct Clock {
    frequency: i64,
    timer: OwnedHandle,
}

impl Clock {
    fn new() -> Result<Self> {
        let mut frequency = 0;
        // SAFETY: frequency points to a writable i64.
        unsafe { QueryPerformanceFrequency(&mut frequency)? };
        // SAFETY: the timer is unnamed; no security attributes are passed.
        let timer = OwnedHandle(unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )?
        });
        Ok(Self { frequency, timer })
    }

    fn now(&self) -> Result<i64> {
        let mut ticks = 0;
        // SAFETY: ticks points to a writable i64.
        unsafe { QueryPerformanceCounter(&mut ticks)? };
        Ok((i128::from(ticks) * i128::from(SECOND) / i128::from(self.frequency)) as i64)
    }

    fn wait(&self, shared: &Shared, until: i64) -> Result<()> {
        let relative = self.now()? - until;
        if relative >= 0 || shared.stopped() {
            return Ok(());
        }
        // SAFETY: the negative due time is relative and no callbacks are registered.
        unsafe { SetWaitableTimer(self.timer.0, &relative, 0, None, None, false)? };
        // SAFETY: both handles outlive this bounded wait.
        let result = unsafe { WaitForMultipleObjects(&[shared.stop.0, self.timer.0], false, 100) };
        if result == WAIT_FAILED {
            return Err(Error::from_thread());
        }
        Ok(())
    }
}

fn recording_name(
    (year, month, day, hour, minute, second): (u16, u16, u16, u16, u16, u16),
    number: u32,
) -> String {
    let suffix = if number == 1 {
        String::new()
    } else {
        format!(" {number}")
    };
    format!(
        "Screen Recording {year:04}-{month:02}-{day:02} at {hour:02}.{minute:02}.{second:02}{suffix}.mp4"
    )
}

struct TempFile(PathBuf);

impl TempFile {
    fn claim(folder: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..1000 {
            let path = folder.join(format!(
                ".recording-{}-{}.partial.mp4",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "too many recording temp files",
        ))
    }

    fn publish(&self, time: SystemTime) -> Result<PathBuf> {
        let folder = self.0.parent().unwrap();
        let source = HSTRING::from(self.0.as_os_str());
        let parts = crate::saver::local_parts(time);
        for number in 1..=10_000 {
            let path = folder.join(recording_name(parts, number));
            let destination = HSTRING::from(path.as_os_str());
            // SAFETY: both paths are live UTF-16 strings on the same volume.
            // With no flags MoveFileExW atomically refuses an existing destination.
            match unsafe { MoveFileExW(&source, &destination, MOVE_FILE_FLAGS(0)) } {
                Ok(()) => return Ok(path),
                Err(e)
                    if [
                        ERROR_ALREADY_EXISTS.to_hresult(),
                        ERROR_FILE_EXISTS.to_hresult(),
                    ]
                    .contains(&e.code()) => {}
                Err(e) => return Err(e),
            }
        }
        Err(error("too many recordings with the same name"))
    }

    fn recover(self, had_samples: bool) -> Option<PathBuf> {
        if (!had_samples || fs::metadata(&self.0).is_ok_and(|meta| meta.len() == 0))
            && fs::remove_file(&self.0).is_ok()
        {
            return None;
        }
        self.0.try_exists().unwrap_or(true).then_some(self.0)
    }
}

// MF's default file has moov after mdat. On supported Windows versions, its
// opt-in front-moov mode can fail during finalization or emit invalid data.
// Relocate the index of our single-mdat file after MF releases it. The copy
// leaves the complete original available if writing or patching the copy fails.
#[derive(Clone, Copy)]
struct Mp4Box {
    start: u64,
    data: u64,
    end: u64,
    kind: [u8; 4],
}

fn mp4_box(reader: &mut (impl Read + Seek), start: u64, end: u64) -> io::Result<Mp4Box> {
    if end - start < 8 {
        return Err(invalid_mp4("truncated MP4 box header"));
    }
    reader.seek(SeekFrom::Start(start))?;
    let mut header = [0; 8];
    reader.read_exact(&mut header)?;
    let short = u32::from_be_bytes(header[..4].try_into().unwrap());
    let mut header_len = 8;
    let len = match short {
        0 => end - start,
        1 => {
            let mut extended = [0; 8];
            reader.read_exact(&mut extended)?;
            header_len = 16;
            u64::from_be_bytes(extended)
        }
        size => u64::from(size),
    };
    if len < header_len || len > end - start {
        return Err(invalid_mp4("MP4 box exceeds its enclosing box"));
    }
    Ok(Mp4Box {
        start,
        data: start + header_len,
        end: start + len,
        kind: header[4..].try_into().unwrap(),
    })
}

fn invalid_mp4(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn make_fast_start(path: &Path) -> io::Result<Option<TempFile>> {
    let mut source = File::open(path)?;
    let len = source.metadata()?.len();
    let (mut moov, mut mdat) = (None, None);
    let mut start = 0;
    while start < len {
        let header = mp4_box(&mut source, start, len)?;
        match &header.kind {
            b"moov" if moov.replace(header).is_some() => {
                return Err(invalid_mp4("multiple moov boxes"));
            }
            b"mdat" if mdat.replace(header).is_some() => {
                return Err(invalid_mp4("multiple mdat boxes"));
            }
            _ => {}
        }
        start = header.end;
    }
    let moov = moov.ok_or_else(|| invalid_mp4("missing moov"))?;
    let mdat = mdat.ok_or_else(|| invalid_mp4("missing mdat"))?;
    if moov.start < mdat.start {
        return Ok(None);
    }
    let shift = moov.end - moov.start;
    // A zero-sized top-level box extends to EOF and cannot be relocated unchanged.
    // The MF file sink emits sized moov boxes. Bound the index read independently of mdat.
    if shift > 64 * 1024 * 1024 {
        return Ok(None);
    }
    let mut index = vec![0; shift as usize];
    source.seek(SeekFrom::Start(moov.start))?;
    source.read_exact(&mut index)?;
    if index[..4] == [0; 4] {
        index[..4].copy_from_slice(&(shift as u32).to_be_bytes());
    }
    // A file close to the stco limit can require conversion to co64. Its original
    // mux is still playable and uploadable, so keep that layout in this case.
    if !shift_chunks(
        &mut index,
        (moov.data - moov.start) as usize,
        0,
        mdat,
        shift,
    )? {
        return Ok(None);
    }

    let ready = TempFile::claim(path.parent().unwrap())?;
    let copy = (|| -> io::Result<()> {
        use std::io::Write;
        let mut output = OpenOptions::new().write(true).open(&ready.0)?;
        copy_range(&mut source, &mut output, 0, mdat.start)?;
        output.write_all(&index)?;
        copy_range(&mut source, &mut output, mdat.start, moov.start)?;
        copy_range(&mut source, &mut output, moov.end, len)?;
        output.sync_all()
    })();
    if let Err(e) = copy {
        ready.recover(false);
        return Err(e);
    }
    Ok(Some(ready))
}

fn copy_range(source: &mut File, output: &mut File, start: u64, end: u64) -> io::Result<()> {
    source.seek(SeekFrom::Start(start))?;
    if io::copy(&mut source.take(end - start), output)? != end - start {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "MP4 truncated during remux",
        ));
    }
    Ok(())
}

fn shift_chunks(
    index: &mut [u8],
    mut start: usize,
    depth: usize,
    mdat: Mp4Box,
    shift: u64,
) -> io::Result<bool> {
    const TRACK_PATH: [[u8; 4]; 4] = [*b"trak", *b"mdia", *b"minf", *b"stbl"];
    while start < index.len() {
        let header = mp4_box(
            &mut io::Cursor::new(&*index),
            start as u64,
            index.len() as u64,
        )?;
        let end = header.end as usize;
        let data = header.data as usize;
        if TRACK_PATH.get(depth) == Some(&header.kind) {
            if !shift_chunks(&mut index[..end], data, depth + 1, mdat, shift)? {
                return Ok(false);
            }
        } else if depth == TRACK_PATH.len() && matches!(&header.kind, b"stco" | b"co64") {
            let size = if &header.kind == b"stco" { 4 } else { 8 };
            let payload = &mut index[data..end];
            if payload.len() < 8 {
                return Err(invalid_mp4("truncated chunk offset box"));
            }
            let count = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as usize;
            if payload.len() - 8 != count * size {
                return Err(invalid_mp4("malformed chunk offset table"));
            }
            for entry in payload[8..].chunks_exact_mut(size) {
                let mut bytes = [0; 8];
                bytes[8 - size..].copy_from_slice(entry);
                let old = u64::from_be_bytes(bytes);
                if !(mdat.data..mdat.end).contains(&old) {
                    return Err(invalid_mp4("chunk offset is outside mdat"));
                }
                let new = old + shift;
                if size == 4 && u32::try_from(new).is_err() {
                    return Ok(false);
                }
                entry.copy_from_slice(&new.to_be_bytes()[8 - size..]);
            }
        }
        start = end;
    }
    Ok(true)
}

struct CursorLayer {
    pub rect: [f32; 4],
    pub views: [Option<ID3D11ShaderResourceView>; 2],
}

struct Gpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    input_size: Size,
    output_size: Size,
    desktop: ID3D11Texture2D,
    desktop_view: ID3D11ShaderResourceView,
    target: ID3D11RenderTargetView,
    target_tex: ID3D11Texture2D,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    video: Video,
}

impl Gpu {
    fn new(adapter: &IDXGIAdapter, input_size: Size, output_size: Size, fps: u32) -> Result<Self> {
        let (mut device, mut ctx) = (None, None);
        // SAFETY: the selected adapter is live and both output slots are writable.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
        }
        let device = require(device, "D3D device")?;
        let ctx = require(ctx, "D3D immediate context")?;
        let multithread: ID3D11Multithread = ctx.cast()?;
        // SAFETY: Media Foundation may share this device. Protect immediate-context access.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };

        let desktop = texture(
            &device,
            input_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE,
            None,
        )?;
        let desktop_view = view(&device, &desktop)?;
        let target_tex = texture(
            &device,
            output_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET,
            None,
        )?;
        let mut target = None;
        // SAFETY: the BGRA texture has render-target bind flags and the default view is valid.
        unsafe { device.CreateRenderTargetView(&target_tex, None, Some(&mut target))? };
        let video = Video::new(&device, &ctx, &target_tex, output_size, fps)?;

        let mut constants = None;
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<[f32; 8]>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        // SAFETY: the 32-byte constant buffer descriptor and output slot are local.
        unsafe { device.CreateBuffer(&desc, None, Some(&mut constants))? };

        let mut sampler = None;
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxAnisotropy: 1,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        // SAFETY: local sampler descriptor and output slot stay alive through the call.
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler))? };

        let vs_bytecode = compile(s!("vs"), s!("vs_5_0"))?;
        let ps_bytecode = compile(s!("ps"), s!("ps_5_0"))?;
        let (mut vs, mut ps) = (None, None);
        // SAFETY: the compiled bytecode is alive and matches each shader stage.
        unsafe {
            device.CreateVertexShader(bytes(&vs_bytecode), None, Some(&mut vs))?;
            device.CreatePixelShader(bytes(&ps_bytecode), None, Some(&mut ps))?;
        }
        Ok(Self {
            device,
            ctx,
            input_size,
            output_size,
            desktop,
            desktop_view,
            video,
            target_tex,
            target: require(target, "BGRA render-target view")?,
            constants: require(constants, "constant buffer")?,
            sampler: require(sampler, "bilinear sampler")?,
            vs: require(vs, "vertex shader")?,
            ps: require(ps, "pixel shader")?,
        })
    }

    fn update_desktop(&self, source: &ID3D11Texture2D) {
        // SAFETY: the capture surface and retained texture have the same size, device, and format.
        unsafe { self.ctx.CopyResource(&self.desktop, source) };
    }

    fn normalize(&self, area: Rect) -> [f32; 4] {
        let w = f64::from(self.input_size.0);
        let h = f64::from(self.input_size.1);
        [
            (area.x / w) as f32,
            (area.y / h) as f32,
            (area.width / w) as f32,
            (area.height / h) as f32,
        ]
    }

    fn render(
        &self,
        area: Rect,
        cursor: Option<CursorLayer>,
        surface: &ID3D11Texture2D,
        slice: u32,
    ) -> Result<()> {
        let mut params = [0.0; 8];
        params[..4].copy_from_slice(&self.normalize(area));
        let mut views = [Some(self.desktop_view.clone()), None, None];
        if let Some(cursor) = cursor {
            params[4..].copy_from_slice(&cursor.rect);
            let [black, white] = cursor.views;
            views[1] = black;
            views[2] = white;
        }
        let viewport = D3D11_VIEWPORT {
            Width: self.output_size.0 as f32,
            Height: self.output_size.1 as f32,
            MaxDepth: 1.0,
            ..Default::default()
        };
        // SAFETY: all bound objects belong to this device and the constants fill the buffer.
        unsafe {
            self.ctx
                .UpdateSubresource(&self.constants, 0, None, params.as_ptr().cast(), 0, 0);
            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.RSSetViewports(Some(&[viewport]));
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(&self.ps, None);
            self.ctx
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            self.ctx
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.ctx.PSSetShaderResources(0, Some(&views));
            self.ctx
                .OMSetRenderTargets(Some(&[Some(self.target.clone())]), None);
            self.ctx.Draw(3, 0);
            self.ctx.OMSetRenderTargets(None, None);
            self.ctx.PSSetShaderResources(0, Some(&[None, None, None]));
        }
        self.video.convert(surface, slice)
    }

    fn upload_cursor(&self, size: Size, bgra: &[u8]) -> Result<ID3D11ShaderResourceView> {
        if bgra.len() != size.0 as usize * size.1 as usize * 4 {
            return Err(error("invalid cursor bitmap size"));
        }
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr().cast(),
            SysMemPitch: size.0 * 4,
            SysMemSlicePitch: 0,
        };
        let image = texture(
            &self.device,
            size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE,
            Some(&data),
        )?;
        view(&self.device, &image)
    }
}

struct Video {
    device: ID3D11VideoDevice,
    ctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
}

impl Video {
    fn new(
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        bgra: &ID3D11Texture2D,
        size: Size,
        fps: u32,
    ) -> Result<Self> {
        let device: ID3D11VideoDevice = device.cast()?;
        let ctx: ID3D11VideoContext = ctx.cast()?;
        let colors: ID3D11VideoContext1 = ctx.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            OutputFrameRate: rate,
            InputWidth: size.0,
            InputHeight: size.1,
            OutputWidth: size.0,
            OutputHeight: size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: descriptor remains live for the call. This is a progressive 1:1 conversion.
        let enumerator = unsafe { device.CreateVideoProcessorEnumerator(&desc)? };
        // SAFETY: rate conversion index zero is the baseline mode of this enumerator.
        let processor = unsafe { device.CreateVideoProcessor(&enumerator, 0)? };
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut input = None;
        // SAFETY: bgra is the same device's single-slice render target.
        unsafe {
            device.CreateVideoProcessorInputView(bgra, &enumerator, &desc, Some(&mut input))?
        };
        // SAFETY: configure only stream zero, with matching SDR RGB and YUV color spaces.
        unsafe {
            ctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            colors.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            colors.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
        }
        Ok(Self {
            device,
            ctx,
            enumerator,
            processor,
            input: require(input, "video processor input view")?,
        })
    }

    fn convert(&self, nv12: &ID3D11Texture2D, slice: u32) -> Result<()> {
        let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: the allocator returned a live D3D11 texture.
        unsafe { nv12.GetDesc(&mut texture_desc) };
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: if texture_desc.ArraySize == 1 {
                D3D11_VPOV_DIMENSION_TEXTURE2D
            } else {
                D3D11_VPOV_DIMENSION_TEXTURE2DARRAY
            },
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_VPOV {
                    MipSlice: 0,
                    FirstArraySlice: slice,
                    ArraySize: 1,
                },
            },
        };
        let mut output = None;
        // SAFETY: the single-slice NV12 render target matches the enumerator's dimensions.
        unsafe {
            self.device.CreateVideoProcessorOutputView(
                nv12,
                &self.enumerator,
                &desc,
                Some(&mut output),
            )?
        };
        let output = require(output, "video processor output view")?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        // SAFETY: both surfaces are live. All optional past/future streams are empty.
        let result = unsafe {
            self.ctx
                .VideoProcessorBlt(&self.processor, &output, 0, std::slice::from_ref(&stream))
        };
        drop(ManuallyDrop::into_inner(stream.pInputSurface));
        result
    }
}

fn texture(
    device: &ID3D11Device,
    size: Size,
    format: DXGI_FORMAT,
    bind: D3D11_BIND_FLAG,
    data: Option<&D3D11_SUBRESOURCE_DATA>,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    // SAFETY: the descriptors and output slot are live. Optional initial data covers the texture.
    unsafe { device.CreateTexture2D(&desc, data.map(std::ptr::from_ref), Some(&mut texture))? };
    require(texture, "D3D texture")
}

fn view(device: &ID3D11Device, texture: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
    let mut view = None;
    // SAFETY: this device owns the shader-resource texture and can infer its view format.
    unsafe { device.CreateShaderResourceView(texture, None, Some(&mut view))? };
    require(view, "shader-resource view")
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let source = COMPOSITE.as_bytes();
    let (mut code, mut diagnostics) = (None, None);
    // SAFETY: the source is sized explicitly, entry and target are C strings, and outputs are local.
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("composite.hlsl"),
            None,
            None::<&ID3DInclude>,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut diagnostics),
        )
    };
    if let Err(err) = result {
        return Err(error(format!(
            "{err}: {}",
            diagnostics
                .as_ref()
                .map(|blob| String::from_utf8_lossy(bytes(blob)))
                .unwrap_or_default()
        )));
    }
    require(code, "compiled shader")
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: D3DCompiler owns the nonempty byte buffer for the lifetime of its blob.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

const COMPOSITE: &str = r#"
Texture2D desktop : register(t0);
Texture2D cursor_black : register(t1);
Texture2D cursor_white : register(t2);
SamplerState linear_clamp : register(s0);

cbuffer Frame : register(b0) {
    float4 crop;   // origin and size, normalized to the input texture
    float4 cursor; // origin and size in the same coordinates
};

struct Vertex {
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

Vertex vs(uint id : SV_VertexID) {
    Vertex v;
    v.uv = float2((id << 1) & 2, id & 2);
    v.position = float4(v.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return v;
}

float4 ps(Vertex v) : SV_Target {
    float2 uv = crop.xy + v.uv * crop.zw;
    float3 color = desktop.Sample(linear_clamp, uv).rgb;
    if (cursor.z > 0 && cursor.w > 0) {
        float2 pos = (uv - cursor.xy) / cursor.zw;
        if (all(pos >= 0) && all(pos < 1)) {
            float3 black = cursor_black.Sample(linear_clamp, pos).rgb;
            float3 white = cursor_white.Sample(linear_clamp, pos).rgb;
            // Evaluate the cursor against the desktop. Black/white also represent
            // the two endpoints for legacy cursor pixels that invert the background.
            color = lerp(black, white, color);
        }
    }
    return float4(color, 1);
}
"#;

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
            return Err(Error::from_thread());
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
            return Err(Error::from_thread());
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
            return Err(error("GDI failed to flush cursor bitmap"));
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

struct Cursor {
    scale: f64,
    image: Option<CursorImage>,
}

impl Cursor {
    fn new(scale: f64) -> Self {
        Self { scale, image: None }
    }

    fn read(&mut self, gpu: &Gpu, monitor: RECT) -> Result<Option<CursorLayer>> {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: info is writable and cbSize names the correct ABI layout.
        unsafe { GetCursorInfo(&mut info)? };
        let mut p = POINT::default();
        // SAFETY: p is writable; use physical pixels even in an unmanifested example.
        unsafe { GetPhysicalCursorPos(&mut p)? };
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
            self.image = Some(image);
        }
        let image = self.image.as_ref().unwrap();
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
        return Err(error("invalid cursor dimensions"));
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

struct Apartment;

impl Apartment {
    fn enter() -> Result<Self> {
        // SAFETY: recorder worker is a fresh thread, and WinRT also initializes COM as MTA.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: matches this worker's successful RoInitialize.
        unsafe { RoUninitialize() };
    }
}

struct Media;

impl Media {
    fn start() -> Result<Self> {
        // SAFETY: called on the worker with COM initialized.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        Ok(Self)
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        // SAFETY: matches MFStartup, after all MF objects have been dropped.
        let _ = unsafe { MFShutdown() };
    }
}

struct Capture {
    item: GraphicsCaptureItem,
    token: i64,
    closed: Arc<AtomicBool>,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}

impl Capture {
    fn new(
        item: GraphicsCaptureItem,
        gpu: &Gpu,
        mode: CursorMode,
        fps: u32,
        metrics: &mut RecordingMetrics,
    ) -> Result<Self> {
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        // SAFETY: dxgi owns a reference to the D3D11 device used on this worker.
        let device: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            3,
            item.Size()?,
        )?;
        let session = pool.CreateCaptureSession(&item).inspect_err(|_| {
            let _ = pool.Close();
        })?;
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let token = item
            .Closed(
                &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                    flag.store(true, Ordering::Release);
                    Ok(())
                }),
            )
            .inspect_err(|_| {
                let _ = session.Close();
                let _ = pool.Close();
            })?;
        let capture = Self {
            item,
            token,
            closed,
            pool,
            session,
        };
        capture
            .session
            .SetIsCursorCaptureEnabled(matches!(mode, CursorMode::System))?;
        metrics.border_request_rejected = capture.session.SetIsBorderRequired(false).is_err();
        // The delivery hint is optional on older Windows 11 builds.
        let _ = capture.session.SetMinUpdateInterval(TimeSpan {
            Duration: SECOND / i64::from(fps),
        });
        Ok(capture)
    }

    fn poll(&self) -> Result<Option<Direct3D11CaptureFrame>> {
        self.check_open()?;
        match self.pool.TryGetNextFrame() {
            Ok(frame) => Ok(Some(frame)),
            // A successful null WinRT reference projects as an error with code S_OK.
            Err(e) if e.code() == S_OK => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(error(
                "capture item closed (monitor disconnected or capture ended)",
            ));
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn copy_frame(frame: &Direct3D11CaptureFrame, gpu: &Gpu, geometry: MonitorGeometry) -> Result<()> {
    let size = frame.ContentSize()?;
    geometry.validate(geometry, (size.Width as u32, size.Height as u32))?;
    let source: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    // SAFETY: this WinRT surface owns the capture frame's D3D11 texture.
    let texture: ID3D11Texture2D = unsafe { source.GetInterface()? };
    gpu.update_desktop(&texture);
    Ok(())
}

struct Writer {
    inner: IMFSinkWriter,
    stream: u32,
    allocator: IMFVideoSampleAllocatorEx,
}

fn attributes(capacity: u32) -> Result<IMFAttributes> {
    let mut attrs = None;
    // SAFETY: attrs is a writable output slot.
    unsafe { MFCreateAttributes(&mut attrs, capacity)? };
    require(attrs, "Media Foundation attributes")
}

fn pair(a: u32, b: u32) -> u64 {
    (u64::from(a) << 32) | u64::from(b)
}

fn video_type(size: Size, fps: u32, subtype: &GUID) -> Result<IMFMediaType> {
    // SAFETY: creates an owned media type on the initialized MTA.
    let ty = unsafe { MFCreateMediaType()? };
    // SAFETY: these attributes use their documented types and value ranges.
    unsafe {
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        ty.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        ty.SetUINT64(&MF_MT_FRAME_RATE, pair(fps, 1))?;
        ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(ty)
}

impl Writer {
    fn open(path: &Path, gpu: &Gpu, fps: u32, hardware: bool) -> Result<Self> {
        let size = gpu.output_size;
        let attrs = attributes(7)?;
        let (mut token, mut manager) = (0, None);
        // SAFETY: the output slots are writable and distinct.
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
        let manager = require(manager, "DXGI device manager")?;
        // SAFETY: manager owns the token and gpu owns a video-enabled D3D11 device.
        unsafe { manager.ResetDevice(&gpu.device, token)? };
        // SAFETY: the attributes retain any COM reference assigned to them.
        unsafe {
            attrs.SetUINT32(
                &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                u32::from(hardware),
            )?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
        }
        if hardware {
            // SAFETY: the manager is configured and this attribute takes a reference.
            unsafe { attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)? };
        } else {
            // SAFETY: converters may be disabled with hardware transforms disabled.
            unsafe { attrs.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)? };
        }

        let input = video_type(size, fps, &MFVideoFormat_NV12)?;
        let output = video_type(size, fps, &MFVideoFormat_H264)?;
        let bitrate = (u64::from(size.0) * u64::from(size.1) * u64::from(fps) / 10)
            .clamp(2_000_000, u64::from(u32::MAX)) as u32;
        // SAFETY: media types are live and the values match the configured formats.
        unsafe {
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, size.0)?;
        }

        let mut raw = std::ptr::null_mut();
        // SAFETY: the IID matches the typed, empty COM output slot.
        unsafe { MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocatorEx::IID, &mut raw)? };
        if raw.is_null() {
            return Err(error("Media Foundation returned no allocator"));
        }
        // SAFETY: the successful factory call transferred an interface matching the IID.
        let allocator = unsafe { IMFVideoSampleAllocatorEx::from_raw(raw) };
        let alloc_attrs = attributes(1)?;
        // SAFETY: these are this device's allocator, its configured manager and NV12 input.
        unsafe {
            alloc_attrs.SetUINT32(&MF_SA_D3D11_BINDFLAGS, D3D11_BIND_RENDER_TARGET.0 as u32)?;
            allocator.SetDirectXManager(&manager)?;
            allocator.InitializeSampleAllocatorEx(4, 12, &alloc_attrs, &input)?;
        }

        let url = HSTRING::from(path.as_os_str());
        // SAFETY: URL points to our claimed scratch file; MF retains needed attributes.
        let inner = unsafe { MFCreateSinkWriterFromURL(&url, None::<&IMFByteStream>, &attrs)? };
        // SAFETY: the writer is configured in order with live, compatible media types.
        let stream = unsafe { inner.AddStream(&output)? };
        // Audio extension point: add an AAC stream here and feed it WASAPI PCM aligned
        // by packet QPC position to the first capture frame before muxing.
        // SAFETY: stream was returned by this writer and the input type matches the video stream.
        unsafe {
            inner.SetInputMediaType(stream, &input, None::<&IMFAttributes>)?;
            inner.BeginWriting()?;
        }
        Ok(Self {
            inner,
            stream,
            allocator,
        })
    }

    fn encoder(&self) -> Option<(bool, String)> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the IID matches the empty typed COM slot; GUID_NULL queries the encoder.
        unsafe {
            self.inner.GetServiceForStream(
                self.stream,
                &GUID::zeroed(),
                &IMFTransform::IID,
                &mut raw,
            )
        }
        .ok()?;
        if raw.is_null() {
            return None;
        }
        // SAFETY: GetServiceForStream transferred the requested IMFTransform reference.
        let transform = unsafe { IMFTransform::from_raw(raw) };
        // SAFETY: these queries are on the live encoder and return owned values.
        let attrs = unsafe { transform.GetAttributes() }.ok();
        let class = attrs
            .as_ref()
            .and_then(|attrs| {
                // SAFETY: attrs is the live encoder's own attribute store.
                unsafe { attrs.GetGUID(&MFT_TRANSFORM_CLSID_Attribute).ok() }
            })
            .or_else(|| {
                let persist = transform.cast::<IPersist>().ok()?;
                // SAFETY: persist is an owned interface on the encoder.
                unsafe { persist.GetClassID().ok() }
            });
        if class == Some(CMSH264EncoderMFT) {
            return Some((false, "Microsoft H.264 Video Encoder MFT".into()));
        }
        let attrs = attrs?;
        // SAFETY: these are optional string attributes in the live store.
        let (name, hardware) = unsafe {
            let name = attrs
                .GetStringLength(&MFT_FRIENDLY_NAME_Attribute)
                .ok()
                .and_then(|len| {
                    let mut chars = vec![0; len as usize + 1];
                    attrs
                        .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut chars, None)
                        .ok()?;
                    Some(String::from_utf16_lossy(&chars[..len as usize]))
                });
            (
                name,
                attrs
                    .GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute)
                    .is_ok(),
            )
        };
        hardware.then(|| (true, name.unwrap_or_else(|| "hardware H.264 MFT".into())))
    }

    fn sample(&self) -> Result<Option<IMFSample>> {
        // SAFETY: the allocator is live and initialized. MF tracks downstream sample ownership.
        match unsafe { self.allocator.AllocateSample() } {
            Ok(sample) => Ok(Some(sample)),
            Err(e) if e.code() == MF_E_SAMPLEALLOCATOR_EMPTY => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write(&self, sample: &IMFSample, time: i64, duration: i64) -> Result<()> {
        // SAFETY: this sample is owned by the recorder until WriteSample retains it.
        unsafe {
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.inner.WriteSample(self.stream, sample)
        }
    }

    fn finalize(self) -> Result<()> {
        // SAFETY: this consumes the started writer after its final sample.
        unsafe { self.inner.Finalize() }
    }
}

fn surface(sample: &IMFSample) -> Result<(ID3D11Texture2D, u32)> {
    // SAFETY: the video allocator provides one buffer per sample.
    let buffer = unsafe { sample.GetBufferByIndex(0)? };
    let dxgi = buffer.cast::<IMFDXGIBuffer>()?;
    let mut raw = std::ptr::null_mut();
    // SAFETY: the IID matches the output slot. The allocator uses one mip per slice.
    unsafe {
        dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
    }
    if raw.is_null() {
        return Err(error("allocator returned no NV12 surface"));
    }
    // SAFETY: GetResource transferred an ID3D11Texture2D reference.
    let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
    // SAFETY: the allocator's sample buffer has the full NV12 layout.
    let slice = unsafe {
        buffer.SetCurrentLength(buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?)?;
        dxgi.GetSubresourceIndex()?
    };
    Ok((texture, slice))
}

fn seconds(ticks: i64) -> f64 {
    ticks as f64 / SECOND as f64
}

fn stage<T>(label: &str, result: Result<T>) -> Result<T> {
    result.map_err(|e| Error::new(e.code(), format!("{label}: {e}")))
}

fn fit_mode(config: &RecordingConfig, hardware: bool) -> (Size, u32) {
    if hardware {
        (config.max_size, config.fps)
    } else {
        (
            (
                config.max_size.0.min(SOFTWARE_CAP.0),
                config.max_size.1.min(SOFTWARE_CAP.1),
            ),
            config.fps.min(SOFTWARE_FPS),
        )
    }
}

struct Attempt {
    result: Result<(Image, f64, f64)>,
    retry_software: bool,
    had_samples: bool,
    metrics: RecordingMetrics,
}

fn run(config: RecordingConfig, shared: &Shared, started_at: SystemTime) -> RecorderEvent {
    let result = record(&config, shared, started_at);
    match result {
        Ok((file, poster, scale, duration)) => RecorderEvent::Finished {
            file,
            poster: Arc::new(poster),
            scale,
            duration,
        },
        Err((reason, partial_file)) => RecorderEvent::Failed {
            reason,
            partial_file,
        },
    }
}

type RecordResult = std::result::Result<(PathBuf, Image, f64, f64), (String, Option<PathBuf>)>;

fn record(config: &RecordingConfig, shared: &Shared, started_at: SystemTime) -> RecordResult {
    let fail = |e: Error| (e.to_string(), None);
    if shared.stopped() {
        return Err(fail(error("stopped before the first recording frame")));
    }
    if !(1..=120).contains(&config.fps)
        || matches!(config.cursor, CursorMode::Drawn { scale } if !scale.is_finite() || scale <= 0.0)
    {
        return Err(fail(Error::new(
            E_INVALIDARG,
            "invalid frame rate or cursor scale",
        )));
    }
    let _apartment = Apartment::enter().map_err(fail)?;
    let _media = Media::start().map_err(fail)?;
    if !GraphicsCaptureSession::IsSupported().map_err(fail)? {
        return Err(fail(error("Windows.Graphics.Capture is unavailable")));
    }
    let geometry = config.monitor.geometry().map_err(fail)?;
    let adapter = config.monitor.adapter().map_err(fail)?;
    let context = AttemptContext {
        config,
        shared,
        geometry,
        adapter,
        clock: Clock::new().map_err(fail)?,
    };
    output_size(geometry.size(), config.max_size).map_err(fail)?;
    fs::create_dir_all(&config.folder).map_err(|e| fail(e.into()))?;
    let folder = std::path::absolute(&config.folder).map_err(|e| fail(e.into()))?;
    let mut hardware_error = None;
    for hardware in [true, false] {
        let temp = TempFile::claim(&folder).map_err(|e| fail(e.into()))?;
        let attempt = context.capture(&temp.0, hardware);
        if let Ok((poster, scale, duration)) = attempt.result {
            *shared.metrics.lock().unwrap() = Some(attempt.metrics);
            let ready = match make_fast_start(&temp.0) {
                Ok(Some(ready)) => {
                    if let Err(e) = fs::remove_file(&temp.0) {
                        ready.recover(false);
                        return Err((
                            format!("remove original scratch MP4: {e}"),
                            temp.recover(true),
                        ));
                    }
                    ready
                }
                Ok(None) => temp,
                Err(e) => return Err((format!("prepare fast-start MP4: {e}"), temp.recover(true))),
            };
            return match ready.publish(started_at) {
                Ok(file) => Ok((file, poster, scale, duration)),
                Err(e) => Err((format!("publish recording: {e}"), ready.recover(true))),
            };
        }
        let err = attempt.result.unwrap_err();
        let partial = temp.recover(attempt.had_samples);
        if hardware && attempt.retry_software && partial.is_none() && !shared.stopped() {
            hardware_error = Some(err);
            continue;
        }
        *shared.metrics.lock().unwrap() = Some(attempt.metrics);
        let reason = match hardware_error {
            Some(hw) => format!("hardware startup: {hw}; software attempt: {err}"),
            None => err.to_string(),
        };
        return Err((reason, partial));
    }
    unreachable!("software attempt always returns a terminal result")
}

struct AttemptContext<'a> {
    config: &'a RecordingConfig,
    shared: &'a Shared,
    geometry: MonitorGeometry,
    adapter: IDXGIAdapter,
    clock: Clock,
}

impl AttemptContext<'_> {
    fn capture(&self, path: &Path, hardware: bool) -> Attempt {
        let (cap, fps) = fit_mode(self.config, hardware);
        let mut attempt = Attempt {
            result: Err(error("recording did not start")),
            retry_software: false,
            had_samples: false,
            metrics: RecordingMetrics {
                hardware_encoder: hardware,
                fps,
                ..Default::default()
            },
        };
        attempt.result = self.capture_inner(path, cap, &mut attempt);
        attempt
    }

    fn capture_inner(
        &self,
        path: &Path,
        cap: Size,
        attempt: &mut Attempt,
    ) -> Result<(Image, f64, f64)> {
        let shared = self.shared;
        let fps = attempt.metrics.fps;
        let hardware = attempt.metrics.hardware_encoder;
        let source = self.geometry.size();
        let output = output_size(source, cap)?;
        let gpu = stage(
            "create D3D11 pipeline",
            Gpu::new(&self.adapter, source, output, fps),
        )?;
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: handle identifies the monitor whose bounds and adapter were just queried.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(self.config.monitor.handle())? };
        let size = item.Size()?;
        self.geometry.validate(
            self.config.monitor.geometry()?,
            (size.Width as u32, size.Height as u32),
        )?;
        if shared.stopped() {
            return Err(error("stopped before the first recording frame"));
        }
        attempt.retry_software = hardware;
        let writer = stage(
            "configure H.264 Sink Writer",
            Writer::open(path, &gpu, fps, hardware),
        )?;
        if let Some((is_hardware, name)) = writer.encoder() {
            if hardware && !is_hardware {
                return Err(error("Sink Writer selected a software encoder"));
            }
            attempt.metrics.encoder = Some(name);
        }
        attempt.retry_software = false;
        let capture = stage(
            "create capture session",
            Capture::new(item, &gpu, self.config.cursor, fps, &mut attempt.metrics),
        )?;
        if shared.stopped() {
            return Err(error("stopped before the first recording frame"));
        }
        stage("start monitor capture", capture.session.StartCapture())?;

        let mut frames = Frames {
            capture,
            gpu: &gpu,
            context: self,
            cursor: match self.config.cursor {
                CursorMode::System => None,
                CursorMode::Drawn { scale } => Some(Cursor::new(scale)),
            },
            posters: Vec::with_capacity(3),
        };
        let result = frames.encode(&writer, attempt);
        let posters = std::mem::take(&mut frames.posters);
        drop(frames);
        let finalization = if attempt.had_samples {
            stage("finalize MP4", writer.finalize())
        } else {
            drop(writer);
            Ok(())
        };
        let duration = match (result, finalization) {
            (Ok(duration), Ok(())) => duration,
            (Err(capture), Err(finalize)) => return Err(error(format!("{capture}; {finalize}"))),
            (Err(e), _) | (_, Err(e)) => return Err(e),
        };
        let target = (duration / 2).min(3 * SECOND / 10);
        let (_, poster) = posters
            .into_iter()
            .min_by_key(|(time, _)| (time - target).abs())
            .ok_or_else(|| error("no recorded poster frame"))?;
        let poster = stage("read recording poster", gpu.read_poster(&poster))?;
        Ok((poster, self.card_scale(output), seconds(duration)))
    }

    fn card_scale(&self, output: Size) -> f64 {
        // HeldCard divides pixel dimensions by scale. Preserve the recorded monitor's
        // logical width when the video was reduced to meet the encoder's output cap.
        f64::from(self.geometry.dpi.0) / 96.0 * f64::from(output.0)
            / f64::from(self.geometry.size().0)
    }
}

struct Frames<'a> {
    capture: Capture,
    gpu: &'a Gpu,
    context: &'a AttemptContext<'a>,
    cursor: Option<Cursor>,
    posters: Vec<(i64, ID3D11Texture2D)>,
}

impl Frames<'_> {
    fn encode(&mut self, writer: &Writer, attempt: &mut Attempt) -> Result<i64> {
        let clock = &self.context.clock;
        let shared = self.context.shared;
        let wait_until = Instant::now() + FIRST_FRAME_TIMEOUT;
        let origin = loop {
            if shared.stopped() {
                return Err(error("stopped before the first recording frame"));
            }
            if let Some(frame) = self.capture.poll()? {
                copy_frame(&frame, self.gpu, self.context.geometry)?;
                break frame.SystemRelativeTime()?.Duration;
            }
            if Instant::now() >= wait_until {
                return Err(error("timed out waiting for the first capture frame"));
            }
            clock.wait(shared, clock.now()? + SECOND / 500)?;
        };
        let started = crate::hook::now() - seconds(clock.now()? - origin).max(0.0);
        let schedule = Schedule(attempt.metrics.fps);
        let source = self.gpu.input_size;
        let mut area = Rect::new(0.0, 0.0, f64::from(source.0), f64::from(source.1));
        let mut pending = None;
        let mut latest = 0;
        let mut slot = 0;
        let mut duration = 0;
        let mut progress = Instant::now();
        let mut last_geometry = progress;
        let mut poster_selected = false;
        let metrics = &mut attempt.metrics;
        metrics.captured_updates = 1;
        let result = (|| -> Result<()> {
            while !shared.stopped() {
                self.capture.check_open()?;
                clock.wait(shared, origin + schedule.time(slot))?;
                if shared.stopped() {
                    break;
                }
                let elapsed = clock.now()? - origin;
                let due = schedule.due(elapsed);
                if due < slot || elapsed < 0 {
                    continue;
                }
                if slot > 0 && due > slot + 2 {
                    // Bound catch-up work under load. PTS stays on the original timeline.
                    metrics.dropped += (due - slot) as u64;
                    slot = due;
                }
                if last_geometry.elapsed() >= PROGRESS_INTERVAL {
                    self.context
                        .geometry
                        .validate(self.context.config.monitor.geometry()?, source)?;
                    last_geometry = Instant::now();
                }
                // SAFETY: this live device can report asynchronous removal from prior GPU work.
                unsafe { self.gpu.device.GetDeviceRemovedReason()? };
                for _ in 0..3 {
                    let Some(frame) = pending
                        .take()
                        .map_or_else(|| self.capture.poll(), |frame| Ok(Some(frame)))?
                    else {
                        break;
                    };
                    let time = frame.SystemRelativeTime()?.Duration - origin;
                    if time > schedule.time(slot) {
                        pending = Some(frame);
                        break;
                    }
                    if schedule.accepts(time, latest, slot) {
                        copy_frame(&frame, self.gpu, self.context.geometry)?;
                        latest = time;
                        metrics.captured_updates += 1;
                    }
                }

                if let Ok(request) = shared.area.try_lock()
                    && let Some(request) = *request
                {
                    area = clamp_area(request, source);
                }
                let Some(sample) = writer.sample()? else {
                    if schedule.time(slot) - duration >= 2 * SECOND {
                        return Err(error(
                            "encoder did not release video samples for two seconds",
                        ));
                    }
                    metrics.dropped += 1;
                    slot += 1;
                    continue;
                };
                let (surface, slice) = surface(&sample)?;
                let cursor = self.cursor.as_mut().and_then(|cursor| {
                    let start = Instant::now();
                    let layer = cursor
                        .read(self.gpu, self.context.geometry.bounds)
                        .ok()
                        .flatten();
                    metrics.cursor.add(start.elapsed());
                    metrics.drawn_cursor_frames += u64::from(layer.is_some());
                    layer
                });
                let start = Instant::now();
                stage(
                    "render crop and NV12",
                    self.gpu.render(area, cursor, &surface, slice),
                )?;
                metrics.crop.add(start.elapsed());

                let time = schedule.time(slot);
                let end = schedule.time(slot + 1);
                let start = Instant::now();
                let written = writer.write(&sample, time, end - time);
                metrics.write.add(start.elapsed());
                attempt.retry_software = !attempt.had_samples
                    && metrics.hardware_encoder
                    && written
                        .as_ref()
                        .is_err_and(|e| e.code() != DXGI_ERROR_DEVICE_REMOVED);
                stage("write H.264 sample", written)?;
                let first = !attempt.had_samples;
                attempt.had_samples = true;
                duration = end;
                if first {
                    shared.emit(RecorderEvent::Started {
                        monitor_rect: self.context.geometry.rect(),
                        output_size: self.gpu.output_size,
                        scale: self.context.card_scale(self.gpu.output_size),
                        time: started,
                    });
                }
                // Keep bounded GPU candidates near 0, 0.15 and 0.3 seconds.
                if !poster_selected
                    && self.posters.len() < 3
                    && time >= self.posters.len() as i64 * 3 * SECOND / 20
                {
                    self.posters.push((time, self.gpu.snapshot()?));
                }
                if self.posters.len() == 3 && time >= self.posters[2].0 * 2 {
                    self.posters.drain(..2);
                    poster_selected = true;
                }
                if progress.elapsed() >= PROGRESS_INTERVAL {
                    shared.emit(RecorderEvent::Progress {
                        elapsed: seconds(end),
                        frames: metrics.write.frames,
                        dropped: metrics.dropped,
                    });
                    progress = Instant::now();
                }
                slot += 1;
            }
            Ok(())
        })();
        drop(pending);
        result?;
        if !attempt.had_samples {
            return Err(error("stopped before the first recording frame"));
        }
        Ok(duration)
    }
}

impl Gpu {
    fn snapshot(&self) -> Result<ID3D11Texture2D> {
        let snapshot = texture(
            &self.device,
            self.output_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_FLAG(0),
            None,
        )?;
        // SAFETY: both textures belong to this device and have matching sizes and formats.
        unsafe { self.ctx.CopyResource(&snapshot, &self.target_tex) };
        Ok(snapshot)
    }

    fn read_poster(&self, source: &ID3D11Texture2D) -> Result<Image> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: source is an owned snapshot of the BGRA render target.
        unsafe { source.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        // SAFETY: the staging descriptor is live, with no GPU bind flags, and the slot is writable.
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))?
        };
        let staging = require(staging, "poster staging texture")?;
        // SAFETY: the staging texture and source have matching size and format.
        unsafe {
            self.ctx.CopyResource(&staging, source);
            self.ctx.Flush();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        loop {
            // SAFETY: the staging resource is CPU-readable and mapped with a nonblocking flag.
            match unsafe {
                self.ctx.Map(
                    &staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Ok(()) => break,
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(1))
                }
                Err(e) => return Err(e),
            }
        }
        // SAFETY: Map succeeded and reports a readable row pitch for all BGRA rows.
        let data = unsafe {
            std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                mapped.RowPitch as usize * desc.Height as usize,
            )
        };
        let rgba = bgra_to_rgba(data, mapped.RowPitch as usize, self.output_size);
        // SAFETY: pairs with the successful Map; rgba owns its copied pixels.
        unsafe { self.ctx.Unmap(&staging, 0) };
        Image::new(desc.Width, desc.Height, rgba).ok_or_else(|| error("invalid poster pixels"))
    }
}

fn bgra_to_rgba(data: &[u8], pitch: usize, size: Size) -> Vec<u8> {
    let width = size.0 as usize * 4;
    let mut rgba = Vec::with_capacity(width * size.1 as usize);
    for row in data.chunks_exact(pitch).take(size.1 as usize) {
        for pixel in row[..width].as_chunks::<4>().0 {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_fits_cap_with_even_sides() {
        assert_eq!(
            output_size((2560, 1440), DEFAULT_CAP).unwrap(),
            (2560, 1440)
        );
        assert_eq!(output_size((7680, 4320), DEFAULT_CAP).unwrap(), DEFAULT_CAP);
        assert_eq!(
            output_size((1080, 1920), (1920, 1080)).unwrap(),
            (608, 1080)
        );
        assert_eq!(
            output_size((2561, 1441), (2561, 1441)).unwrap(),
            (2560, 1440)
        );
        assert!(output_size((120, 90), (1, 1080)).is_err());
    }

    #[test]
    fn fixed_rate_uses_latest_eligible_capture_and_repeats_on_still_desktop() {
        let schedule = Schedule(60);
        let captures = [0, 40_000, 100_000, 250_000, 670_000];
        let mut latest = 0;
        let samples: Vec<_> = (0..6)
            .map(|slot| {
                for capture in captures {
                    if schedule.accepts(capture, latest, slot) {
                        latest = capture;
                    }
                }
                (schedule.time(slot), latest)
            })
            .collect();
        assert_eq!(
            samples,
            [
                (0, 0),
                (166_666, 100_000),
                (333_333, 250_000),
                (500_000, 250_000),
                (666_666, 250_000),
                (833_333, 670_000),
            ]
        );
        for slot in 1..=120 {
            assert_eq!(schedule.due(schedule.time(slot)), slot);
            assert_eq!(schedule.due(schedule.time(slot) - 1), slot - 1);
        }
        assert_eq!(schedule.time(60 * 60 * 60), 3600 * SECOND);
    }

    #[test]
    fn crop_clamps_and_preserves_fractional_coordinates() {
        let size = (100, 50);
        assert_eq!(
            clamp_area(Rect::new(-5.0, 44.0, 20.5, 20.0), size),
            Rect::new(0.0, 30.0, 20.5, 20.0)
        );
        assert_eq!(
            clamp_area(Rect::new(3.25, 4.5, 90.0, 25.0), size),
            Rect::new(3.25, 4.5, 90.0, 25.0)
        );
        assert_eq!(
            clamp_area(Rect::new(f64::NAN, 0.0, 20.0, 20.0), size),
            Rect::new(0.0, 0.0, 100.0, 50.0)
        );
    }

    #[test]
    fn name_matches_swift_and_collision_suffix() {
        let date = (2026, 11, 6, 3, 4, 5);
        assert_eq!(
            recording_name(date, 1),
            "Screen Recording 2026-11-06 at 03.04.05.mp4"
        );
        assert_eq!(
            recording_name(date, 2),
            "Screen Recording 2026-11-06 at 03.04.05 2.mp4"
        );
        let local = recording_name(crate::saver::local_parts(SystemTime::UNIX_EPOCH), 1);
        assert_eq!(
            local.len(),
            "Screen Recording 1970-01-01 at 00.00.00.mp4".len()
        );
    }

    #[test]
    fn resolution_and_dpi_changes_stop_the_fixed_media_type() {
        let initial = MonitorGeometry {
            bounds: RECT {
                left: -2560,
                top: -100,
                right: 0,
                bottom: 1340,
            },
            dpi: (120, 120),
        };
        assert_eq!(initial.rect(), Rect::new(-2560.0, -100.0, 2560.0, 1440.0));
        assert!(initial.validate(initial, initial.size()).is_ok());
        assert!(initial.validate(initial, (1920, 1080)).is_err());
        assert!(
            initial
                .validate(
                    MonitorGeometry {
                        dpi: (144, 144),
                        ..initial
                    },
                    initial.size()
                )
                .is_err()
        );
    }

    #[test]
    fn software_mode_caps_work_and_honors_smaller_requests() {
        let mut config = RecordingConfig::new(MonitorId(0), PathBuf::new());
        assert_eq!(fit_mode(&config, true), (DEFAULT_CAP, DEFAULT_FPS));
        assert_eq!(fit_mode(&config, false), (SOFTWARE_CAP, SOFTWARE_FPS));
        config.max_size = (800, 600);
        config.fps = 24;
        assert_eq!(fit_mode(&config, false), ((800, 600), 24));
    }

    #[test]
    fn poster_conversion_drops_row_padding_and_uses_opaque_rgba() {
        let source = [1, 2, 3, 0, 99, 99, 99, 99, 4, 5, 6, 128, 99, 99, 99, 99];
        assert_eq!(
            bgra_to_rgba(&source, 8, (1, 2)),
            [3, 2, 1, 255, 6, 5, 4, 255]
        );
    }

    #[test]
    fn monochrome_cursor_preserves_black_white_transparent_and_xor_pixels() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        // Each row has black, white, transparent and invert in its first four bits.
        let and = [0b0011_0000_u8, 0];
        let xor = [0b0101_0000_u8, 0];
        // SAFETY: module handle identifies this executable. Both two-byte scan lines
        // describe one 16-pixel monochrome row and are live through CreateCursor.
        let cursor = unsafe {
            CreateCursor(
                Some(windows::Win32::Foundation::HINSTANCE(
                    GetModuleHandleW(None).unwrap().0,
                )),
                0,
                0,
                16,
                1,
                and.as_ptr().cast(),
                xor.as_ptr().cast(),
            )
        }
        .unwrap();
        let mut dib = Dib::new((16, 1)).unwrap();
        let samples = [0, 255].map(|background| {
            let pixels = dib
                .draw_cursor(HICON(cursor.0), (16, 1), background)
                .unwrap();
            std::array::from_fn::<_, 4, _>(|i| pixels[i * 4])
        });
        // SAFETY: test owns this non-shared cursor and no GDI draw is outstanding.
        unsafe { DestroyCursor(cursor) }.unwrap();
        assert_eq!(samples, [[0, 255, 0, 255], [0, 255, 255, 0]]);
    }

    #[test]
    fn fast_start_shifts_32_and_64_bit_chunk_offsets() {
        fn atom(kind: &[u8; 4], content: Vec<u8>) -> Vec<u8> {
            let len = (content.len() as u32 + 8).to_be_bytes();
            [len.as_slice(), kind, &content].concat()
        }

        let mdat = Mp4Box {
            start: 0,
            data: 8,
            end: 200,
            kind: *b"mdat",
        };
        let table32 = atom(
            b"stco",
            [
                0_u32.to_be_bytes(),
                1_u32.to_be_bytes(),
                100_u32.to_be_bytes(),
            ]
            .concat(),
        );
        let table64 = atom(
            b"co64",
            [
                0_u32.to_be_bytes().as_slice(),
                &1_u32.to_be_bytes(),
                &199_u64.to_be_bytes(),
            ]
            .concat(),
        );
        let mut index = atom(
            b"trak",
            atom(
                b"mdia",
                atom(b"minf", atom(b"stbl", [table32, table64].concat())),
            ),
        );
        assert!(shift_chunks(&mut index, 0, 0, mdat, 40).unwrap());
        assert_eq!(&index[48..52], &140_u32.to_be_bytes());
        assert_eq!(&index[68..76], &239_u64.to_be_bytes());
        let wide = Mp4Box {
            end: u64::MAX - 1,
            ..mdat
        };
        assert!(!shift_chunks(&mut index, 0, 0, wide, u64::from(u32::MAX)).unwrap());
    }

    #[test]
    fn publish_never_replaces_and_missing_temp_leaves_no_placeholder() {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("recorder-unit-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let time = SystemTime::UNIX_EPOCH;
        let one = TempFile::claim(&folder).unwrap();
        let two = TempFile::claim(&folder).unwrap();
        fs::write(&one.0, b"first").unwrap();
        fs::write(&two.0, b"second").unwrap();
        let first = one.publish(time).unwrap();
        let second = two.publish(time).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"first");
        assert_eq!(fs::read(&second).unwrap(), b"second");
        assert!(one.publish(time).is_err());
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 2);
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
        let vanished = TempFile::claim(&folder).unwrap();
        fs::remove_file(&vanished.0).unwrap();
        fs::remove_dir(&folder).unwrap();
        assert!(vanished.publish(time).is_err());
        assert!(vanished.recover(true).is_none());
    }
}
