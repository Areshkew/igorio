mod check;
mod desktop;
mod gpu;
mod media;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use igorio_core::geom::{Point, Rect};
use igorio_core::viewport::RecordingViewport;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat};
use windows::Win32::Foundation::{FILETIME, S_OK};
use windows::Win32::Graphics::{Direct3D11::ID3D11Texture2D, Dwm::DwmFlush, Dxgi::IDXGIDevice};
use windows::Win32::System::{
    Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler},
    Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    Threading::{GetCurrentProcess, GetProcessTimes},
    WinRT::{
        Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
        Graphics::Capture::IGraphicsCaptureItemInterop,
        RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
    },
};
use windows::core::{BOOL, IInspectable, Interface, factory};

use desktop::{Cursor, Markers, Monitor};
use gpu::{Gpu, Size, Timing};
use media::{Media, Writer};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const FPS: i64 = 60;
pub const SECOND: i64 = 10_000_000;
static STOP: AtomicBool = AtomicBool::new(false);

pub fn require<T>(slot: Option<T>, name: &str) -> Result<T> {
    slot.ok_or_else(|| format!("{name} returned no object").into())
}

#[derive(Clone, Copy, Debug)]
enum CursorMode {
    System,
    Drawn1,
    Drawn2,
}

struct Zoom {
    t: f64,
    zoom: f64,
    center: Point,
}

struct Options {
    seconds: f64,
    output: Option<PathBuf>,
    monitor: Option<usize>,
    zooms: Vec<Zoom>,
    cursor: CursorMode,
    selfcheck: bool,
    keep: bool,
}

fn options() -> Result<Option<Options>> {
    let mut args = std::env::args().skip(1);
    let mut opts = Options {
        seconds: 4.0,
        output: None,
        monitor: None,
        zooms: Vec::new(),
        cursor: CursorMode::System,
        selfcheck: false,
        keep: false,
    };
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--seconds" => opts.seconds = value()?.parse()?,
            "--out" => opts.output = Some(PathBuf::from(value()?)),
            "--monitor" => opts.monitor = Some(value()?.parse()?),
            "--zoom-script" => {
                opts.zooms.clear();
                for action in value()?.split(',') {
                    let values = action
                        .split(':')
                        .map(str::parse::<f64>)
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    let [t, zoom, x, y] = values.as_slice() else {
                        return Err("zoom-script entries must be t:zoom:cx:cy".into());
                    };
                    if values.iter().any(|v| !v.is_finite())
                        || *t < 0.0
                        || !(1.0..=4.0).contains(zoom)
                    {
                        return Err("zoom-script requires finite values, nonnegative times and zoom in [1,4]".into());
                    }
                    opts.zooms.push(Zoom {
                        t: *t,
                        zoom: *zoom,
                        center: Point::new(*x, *y),
                    });
                }
                opts.zooms.sort_by(|a, b| a.t.total_cmp(&b.t));
            }
            "--cursor" => {
                opts.cursor = match value()?.as_str() {
                    "system" => CursorMode::System,
                    "drawn1" => CursorMode::Drawn1,
                    "drawn2" => CursorMode::Drawn2,
                    _ => return Err("cursor must be system, drawn1 or drawn2".into()),
                }
            }
            "--selfcheck" => opts.selfcheck = true,
            "--keep" => opts.keep = true,
            "--help" | "-h" => {
                println!(
                    "recorder [--seconds N] [--out path.mp4] [--monitor index] [--zoom-script \"t:zoom:cx:cy,...\"] [--cursor system|drawn1|drawn2] [--selfcheck] [--keep]"
                );
                println!(
                    "Defaults: 4 seconds, monitor under the cursor, system cursor. Zoom centres are monitor-local physical pixels."
                );
                return Ok(None);
            }
            _ => return Err(format!("unknown option {flag} (see --help)").into()),
        }
    }
    if opts.selfcheck {
        opts.seconds = 4.0;
    }
    if !opts.seconds.is_finite() || !(1.0 / FPS as f64..=86_400.0).contains(&opts.seconds) {
        return Err("seconds must be between 1/60 and 86400".into());
    }
    Ok(Some(opts))
}

fn output_path(opts: &Options) -> Result<PathBuf> {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let default = target.join(format!(
        "{}-{}.mp4",
        if opts.selfcheck {
            "selfcheck"
        } else {
            "recording"
        },
        std::process::id()
    ));
    let path = opts.output.as_deref().unwrap_or(&default);
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
    {
        return Err("output must end in .mp4".into());
    }
    if opts.selfcheck || opts.output.is_none() {
        std::fs::create_dir_all(&target)?;
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = dunce(parent.canonicalize()?);
    if opts.selfcheck && !parent.starts_with(dunce(target.canonicalize()?)) {
        return Err("selfcheck output must be inside the spike's target directory".into());
    }
    Ok(parent.join(path.file_name().ok_or("missing MP4 filename")?))
}

fn dunce(path: PathBuf) -> PathBuf {
    // MF URL creation expects a DOS or UNC path, not a verbatim path.
    let Some(value) = path.to_str() else {
        return path;
    };
    if let Some(network) = value.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{network}"));
    }
    value
        .strip_prefix(r"\\?\")
        .map_or(path.clone(), PathBuf::from)
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("FAIL: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let Some(opts) = options()? else {
        return Ok(());
    };
    let path = output_path(&opts)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let selfcheck = opts.selfcheck;
    let keep = opts.keep;
    let budget = Duration::from_secs_f64(opts.seconds + if selfcheck { 60.0 } else { 20.0 });
    let worker_path = path.clone();

    let result = (|| {
        // SAFETY: handler only sets an atomic. The process owns its handler table entry.
        unsafe { SetConsoleCtrlHandler(Some(stop_on_ctrl), true)? };
        let (tx, rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("capture and encode MTA".into())
            .spawn(move || {
                let _ = tx.send(record_and_check(opts, &worker_path));
            })?;
        match rx.recv_timeout(budget) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                STOP.store(true, Ordering::Relaxed);
                rx.recv_timeout(Duration::from_secs(3))
                    .unwrap_or_else(|_| Err("recording worker deadline expired".into()))
            }
            Err(error) => Err(error.into()),
        }
    })();
    let delete = (selfcheck && !keep) || (!selfcheck && result.is_err());
    if delete {
        match std::fs::remove_file(&path) {
            Ok(()) => println!("Removed {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "{}; could not remove {}: {error}",
                    result
                        .err()
                        .map_or_else(|| "selfcheck finished".into(), |e| e.to_string()),
                    path.display()
                )
                .into());
            }
        }
    } else {
        println!("Output: {}", path.display());
    }
    result
}

unsafe extern "system" fn stop_on_ctrl(signal: u32) -> BOOL {
    let stop = matches!(signal, CTRL_C_EVENT | CTRL_BREAK_EVENT);
    if stop {
        STOP.store(true, Ordering::Relaxed);
    }
    stop.into()
}

struct Apartment;

impl Apartment {
    fn enter() -> Result<Self> {
        // SAFETY: this is a fresh worker thread. WinRT initializes COM as MTA too.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: matches the successful RoInitialize, after all COM objects on this worker.
        unsafe { RoUninitialize() };
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
    fn new(item: GraphicsCaptureItem, gpu: &Gpu, cursor: CursorMode) -> Result<Self> {
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        // SAFETY: dxgi is the live D3D11 device used by both capture and processing.
        let device: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            3,
            item.Size()?,
        )?;
        let session = match pool.CreateCaptureSession(&item) {
            Ok(session) => session,
            Err(e) => {
                let _ = pool.Close();
                return Err(e.into());
            }
        };
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let token = match item.Closed(
            &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                flag.store(true, Ordering::Relaxed);
                Ok(())
            }),
        ) {
            Ok(token) => token,
            Err(e) => {
                let _ = session.Close();
                let _ = pool.Close();
                return Err(e.into());
            }
        };
        let capture = Self {
            item,
            token,
            closed,
            pool,
            session,
        };
        capture
            .session
            .SetIsCursorCaptureEnabled(matches!(cursor, CursorMode::System))?;
        match capture.session.SetIsBorderRequired(false) {
            Ok(()) => {
                println!("Border: requested off (Windows may keep it without borderless consent)")
            }
            Err(e) => println!("Border: Windows rejected suppression ({e})"),
        }
        // Older Windows 11 builds lack this optional frame delivery hint.
        let _ = capture.session.SetMinUpdateInterval(TimeSpan {
            Duration: SECOND / FPS,
        });
        Ok(capture)
    }

    fn poll(&self) -> Result<Option<Direct3D11CaptureFrame>> {
        if self.closed.load(Ordering::Relaxed) {
            return Err("the capture item closed (monitor disconnected or capture ended)".into());
        }
        match self.pool.TryGetNextFrame() {
            Ok(frame) => Ok(Some(frame)),
            // windows-core projects a successful null WinRT reference as Error::empty().
            Err(e) if e.code() == S_OK => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

struct Clock(i64);

impl Clock {
    fn new() -> Result<Self> {
        let mut frequency = 0;
        // SAFETY: writes the frequency into a live i64.
        unsafe { QueryPerformanceFrequency(&mut frequency)? };
        Ok(Self(frequency))
    }

    fn now(&self) -> Result<i64> {
        let mut ticks = 0;
        // SAFETY: writes the current QPC reading into a live i64.
        unsafe { QueryPerformanceCounter(&mut ticks)? };
        Ok((i128::from(ticks) * i128::from(SECOND) / i128::from(self.0)) as i64)
    }

    fn wait_until(&self, time: i64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(100);
        while !STOP.load(Ordering::Relaxed) {
            let left = time - self.now()?;
            if left <= 0 {
                break;
            }
            if Instant::now() >= deadline {
                return Err("capture timestamp could not be reconciled with QPC".into());
            }
            thread::sleep(Duration::from_nanos((left as u64 * 100).min(4_000_000)));
        }
        Ok(())
    }
}

fn copy_frame(frame: &Direct3D11CaptureFrame, gpu: &Gpu, expected: Size) -> Result<i64> {
    let size = frame.ContentSize()?;
    if (size.Width as u32, size.Height as u32) != expected {
        return Err("display size changed during recording".into());
    }
    let source: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    // SAFETY: the WinRT capture surface wraps the frame's live D3D11 texture.
    let texture: ID3D11Texture2D = unsafe { source.GetInterface()? };
    gpu.update_desktop(&texture);
    Ok(frame.SystemRelativeTime()?.Duration)
}

fn frame_time(frame: i64) -> i64 {
    frame * SECOND / FPS
}

fn record_and_check(mut opts: Options, path: &Path) -> Result<()> {
    let _apartment = Apartment::enter()?;
    let _media = Media::start()?;
    if !GraphicsCaptureSession::IsSupported()? {
        return Err("Windows.Graphics.Capture is unavailable in this session".into());
    }
    let monitor = Monitor::select(opts.monitor)?;
    let interop: IGraphicsCaptureItemInterop =
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: monitor.handle came from this desktop's monitor enumeration.
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor.handle)? };
    let size = item.Size()?;
    if size.Width < 2 || size.Height < 2 {
        return Err("capture item is too small".into());
    }
    let source = (size.Width as u32, size.Height as u32);
    let fit = 1.0_f64
        .min(3840.0 / f64::from(source.0))
        .min(2160.0 / f64::from(source.1));
    let output = (
        ((f64::from(source.0) * fit) as u32 & !1).max(2),
        ((f64::from(source.1) * fit) as u32 & !1).max(2),
    );
    println!(
        "Video: {}x{} -> {}x{}, {FPS} fps, cursor {:?}",
        source.0, source.1, output.0, output.1, opts.cursor
    );
    let adapter = monitor.adapter()?;
    let gpu = Gpu::new(&adapter, source, output).map_err(|e| format!("D3D11 setup: {e}"))?;
    let capture = Capture::new(item, &gpu, opts.cursor)?;
    let clock = Clock::new().map_err(|e| format!("QueryPerformanceFrequency: {e}"))?;
    if opts.selfcheck {
        let center = Point::new(f64::from(source.0) / 4.0, f64::from(source.1) / 4.0);
        opts.zooms = vec![
            Zoom {
                t: 1.5,
                zoom: 2.0,
                center,
            },
            Zoom {
                t: 3.0,
                zoom: 1.0,
                center,
            },
        ];
    }
    let display = Rect::new(0.0, 0.0, f64::from(source.0), f64::from(source.1));
    let mut viewport = RecordingViewport::new(display);
    let mut center = viewport.center();
    let mut script = opts.zooms.iter().peekable();
    let mut cursor = match opts.cursor {
        CursorMode::System => None,
        CursorMode::Drawn1 => Some(Cursor::new(1.0)),
        CursorMode::Drawn2 => Some(Cursor::new(2.0)),
    };

    let markers = Markers::start(monitor.bounds, Duration::from_secs_f64(opts.seconds + 8.0))?;
    // SAFETY: wait for the markers to reach DWM on the worker, never on the window thread.
    unsafe { DwmFlush() }.map_err(|e| format!("DwmFlush: {e}"))?;
    let cpu_start = cpu_time().map_err(|e| format!("GetProcessTimes at start: {e}"))?;
    let wall_start = Instant::now();
    capture
        .session
        .StartCapture()
        .map_err(|e| format!("StartCapture: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let origin = loop {
        if let Some(frame) = capture
            .poll()
            .map_err(|e| format!("first frame poll: {e}"))?
        {
            break copy_frame(&frame, &gpu, source)
                .map_err(|e| format!("first frame copy: {e}"))?;
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the first capture frame".into());
        }
        thread::sleep(Duration::from_millis(2));
    };
    let mut last_capture = origin;
    let mut capture_count = 1;
    let mut pending = None::<Direct3D11CaptureFrame>;
    let mut crop_time = Timing::default();
    let end = (opts.seconds * SECOND as f64).round() as i64;
    let frames = (end * FPS + SECOND - 1) / SECOND;
    let mut writer = None;
    let mut written = 0;
    let record_result = (|| -> Result<()> {
        for n in 0..frames {
            let t = frame_time(n);
            if n > 0 {
                clock.wait_until(origin + t)?;
                if STOP.load(Ordering::Relaxed) {
                    break;
                }
                // Keep the newest capture at or before this tick. A future frame remains
                // owned until its tick. No capture delivery is needed to animate the crop.
                for _ in 0..3 {
                    let Some(frame) = pending
                        .take()
                        .map_or_else(|| capture.poll(), |f| Ok(Some(f)))?
                    else {
                        break;
                    };
                    let timestamp = frame.SystemRelativeTime()?.Duration;
                    if timestamp > origin + t {
                        pending = Some(frame);
                        break;
                    }
                    if timestamp > last_capture {
                        last_capture = copy_frame(&frame, &gpu, source)?;
                        capture_count += 1;
                    }
                }
            }
            let elapsed = t as f64 / SECOND as f64;
            while let Some(action) = script.next_if(|z| z.t <= elapsed + 0.000_000_1) {
                viewport.zoom_by((action.zoom / viewport.target_zoom()).ln());
                center = action.center;
            }
            viewport.step(center, true, 1.0 / FPS as f64);
            let layer = cursor
                .as_mut()
                .and_then(|cursor| cursor.sample(&gpu, monitor.bounds));
            let start = Instant::now();
            let rendered = gpu
                .render(viewport.area(), layer)
                .map_err(|e| format!("crop/scale or NV12 conversion: {e}"))?;
            crop_time.add(start.elapsed());
            let duration = end.min(frame_time(n + 1)) - t;
            match &mut writer {
                Some(writer) => Writer::write(writer, &rendered, t, duration)?,
                None => {
                    writer = Some(Writer::open(
                        path,
                        output,
                        &gpu.device,
                        &rendered,
                        duration,
                    )?)
                }
            }
            written += 1;
        }
        Ok(())
    })();
    drop(pending);
    drop(capture);
    drop(markers);
    let finalize_result = writer.as_ref().map_or(Ok(()), Writer::finalize);
    let cpu_used = cpu_time()? - cpu_start;
    crop_time.print("Crop/scale + NV12 (CPU submission)");
    if let Some(writer) = writer {
        writer.timing.print("Sink Writer WriteSample");
    }
    if let Some(cursor) = cursor {
        cursor.timing.print("Drawn cursor poll/upload (CPU)");
        println!(
            "Drawn cursor: {} submitted layers, {} shape uploads, last native size {:?}, {} errors",
            cursor.rendered, cursor.uploads, cursor.source_size, cursor.errors
        );
    }
    println!(
        "Recording: {:.3} s wall, {:.3} s process CPU, {written} output frames, {capture_count} captured updates",
        wall_start.elapsed().as_secs_f64(),
        cpu_used
    );
    drop(gpu);

    if let Err(error) = record_result {
        if opts.selfcheck || written == 0 {
            return Err(error);
        }
        eprintln!("Stopped after {written} frames: {error}");
    }
    finalize_result?;
    let bytes = std::fs::metadata(path)?.len();
    println!(
        "MP4: {bytes} bytes ({:.0} bytes/s)",
        bytes as f64 / (end.min(frame_time(written)) as f64 / SECOND as f64)
    );
    if opts.selfcheck {
        check::verify(path, source, output, end, frames)?;
    }
    Ok(())
}

fn cpu_time() -> Result<f64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the pseudo-handle represents this process, and all output slots are distinct.
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
