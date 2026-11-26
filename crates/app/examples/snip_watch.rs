//! Prints detector events and new tuning-log lines until Ctrl+C.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use igorio_app::clipboard::Clipboard;
use igorio_app::snip::{Snip, SnipEvent, debug_log_path};
use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::BOOL;

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn console_handler(event: u32) -> BOOL {
    let stop = matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT);
    if stop {
        STOP.store(true, Relaxed);
    }
    stop.into()
}

fn print_event(event: SnipEvent) {
    match event {
        SnipEvent::CaptureUiShown { time } => println!("CaptureUiShown time={time:.6}"),
        SnipEvent::CaptureUiGone { time } => println!("CaptureUiGone time={time:.6}"),
        SnipEvent::Captured {
            image,
            scale,
            captured_at,
            source,
            saved_file,
        } => println!(
            "Captured source={source:?} size={}x{} scale={scale} file={saved_file:?} captured_at={captured_at:?}",
            image.width, image.height
        ),
        SnipEvent::Ignored { reason } => println!("Ignored reason={reason:?}"),
    }
}

struct LogTail {
    path: PathBuf,
    offset: u64,
}

impl LogTail {
    fn read(&mut self) -> io::Result<()> {
        let file = File::open(&self.path)?;
        if file.metadata()?.len() < self.offset {
            self.offset = 0;
        }
        let mut input = BufReader::new(file);
        input.seek(SeekFrom::Start(self.offset))?;
        let mut line = Vec::new();
        for _ in 0..128 {
            line.clear();
            let bytes = input.read_until(b'\n', &mut line)?;
            if !line.ends_with(b"\n") {
                break;
            }
            self.offset += bytes as u64;
            print!("LOG {}", String::from_utf8_lossy(&line));
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    igorio_app::overlay::ensure_dpi_awareness();
    let until = std::env::args()
        .nth(1)
        .map(|arg| arg.parse::<u32>())
        .transpose()?
        .map(|seconds| Instant::now() + Duration::from_secs(u64::from(seconds)));
    let mut log = debug_log_path().map(|path| {
        let offset = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        println!("Log file {path:?}");
        LogTail { path, offset }
    });
    let clipboard = Clipboard::start(|| {})?;
    let mut snip = Snip::start(&clipboard)?;
    // SAFETY: console_handler is static and does only an atomic store.
    unsafe { SetConsoleCtrlHandler(Some(console_handler), true) }?;
    println!("START detector, Ctrl+C to stop");
    let mut events = Vec::new();
    let mut health_at = Instant::now() + Duration::from_secs(1);
    while !STOP.load(Relaxed) && until.is_none_or(|until| Instant::now() < until) {
        // SAFETY: the detector owns the handle across the wait.
        unsafe { WaitForSingleObject(snip.wake_handle(), 100) };
        snip.drain(&mut events);
        events.drain(..).for_each(print_event);
        if let Some(log) = &mut log {
            let _ = log.read();
        }
        if Instant::now() >= health_at {
            if !snip.is_alive() || snip.seems_stalled() {
                snip.restart()?;
                println!("RESTART detector");
            }
            health_at = Instant::now() + Duration::from_secs(1);
        }
    }
    // SAFETY: unregister the handler installed above.
    unsafe { SetConsoleCtrlHandler(Some(console_handler), false) }?;
    if !snip.shutdown() {
        return Err("detector worker did not join".into());
    }
    println!("STOP detector");
    Ok(())
}
