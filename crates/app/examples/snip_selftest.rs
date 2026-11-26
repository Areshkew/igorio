//! Non-interactive detector smoke test. Uses only PNGs in its private scratch directory.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use igorio_app::clipboard::Clipboard;
use igorio_app::snip::{Snip, SnipEvent, Source};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows::Win32::System::Threading::WaitForSingleObject;

const PAUSE_WITH_WRITER: Duration = Duration::from_millis(350);
const WAIT_FOR_CAPTURE: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Report(bool);

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        println!("{} {name} - {detail}", if ok { "PASS" } else { "FAIL" });
        self.0 |= !ok;
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("igorio-snip-{}-{nonce}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for name in ["first.png", "recovered.png", "restarted.png"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

fn observe(snip: &Snip, path: &Path, timeout: Duration) -> (usize, bool) {
    let start = Instant::now();
    let mut events = Vec::new();
    let mut count = 0;
    let mut correct = true;
    while start.elapsed() < timeout {
        // SAFETY: the detector owns the handle for the duration of this wait.
        unsafe { WaitForSingleObject(snip.wake_handle(), 25) };
        snip.drain(&mut events);
        for event in events.drain(..) {
            if let SnipEvent::Captured {
                image,
                source,
                saved_file: Some(file),
                ..
            } = event
                && file == path
            {
                count += 1;
                correct &= source == Source::FullScreenFile
                    && image.width == 2
                    && image.height == 1
                    && image.rgba == [255, 0, 0, 255, 0, 255, 0, 128];
            }
        }
    }
    (count, correct)
}

fn write_fixture(snip: &Snip, path: &Path) -> io::Result<((usize, usize), (usize, bool))> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder
            .write_header()?
            .write_image_data(&[255, 0, 0, 255, 0, 255, 0, 128])?;
    }
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)?;
    let (prefix, iend) = bytes.split_at(bytes.len() - 12);
    writer.write_all(prefix)?;
    writer.flush()?;
    let partial = observe(snip, path, PAUSE_WITH_WRITER).0;
    writer.write_all(iend)?;
    writer.flush()?;
    let open = observe(snip, path, PAUSE_WITH_WRITER).0;
    drop(writer);
    Ok(((partial, open), observe(snip, path, WAIT_FOR_CAPTURE)))
}

fn check_fixture(report: &mut Report, label: &str, snip: &Snip, path: &Path) {
    match write_fixture(snip, path) {
        Ok(((partial, open), (closed, correct))) => {
            report.check(label, partial == 0 && open == 0 && closed == 1 && correct,
                format!("partial={partial} complete_but_open={open} closed={closed} pixels_match={correct}"));
        }
        Err(error) => report.check(label, false, error),
    }
}

fn recreate(path: &Path) -> io::Result<()> {
    fs::remove_file(path.join("first.png"))?;
    fs::remove_dir(path)?;
    let start = Instant::now();
    loop {
        match fs::create_dir(path) {
            Ok(()) => return Ok(()),
            Err(error) if start.elapsed() >= Duration::from_secs(5) => return Err(error),
            // The removed directory can be delete-pending until the watch closes its handle.
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn run(report: &mut Report) -> Result<(), Box<dyn std::error::Error>> {
    let scratch = Scratch::new()?;
    let clipboard = Clipboard::start(|| {})?;
    let mut snip = Snip::start_in_folder(&clipboard, scratch.0.clone())?;
    report.check(
        "start",
        snip.is_alive(),
        "keyboard, window, folder and controller threads",
    );

    std::thread::sleep(Duration::from_secs(2));
    let stats = snip.hook_stats();
    report.check(
        "keyboard callback <= 1 ms",
        stats.max_callback <= Duration::from_millis(1),
        format!(
            "longest={:?} callbacks={} dropped={}{}",
            stats.max_callback,
            stats.callbacks,
            stats.dropped,
            if stats.callbacks == 0 {
                " (no keyboard activity sampled)"
            } else {
                ""
            }
        ),
    );

    check_fixture(
        report,
        "folder reports one complete PNG",
        &snip,
        &scratch.0.join("first.png"),
    );
    match recreate(&scratch.0) {
        Ok(()) => check_fixture(
            report,
            "folder recovers after deletion and recreation",
            &snip,
            &scratch.0.join("recovered.png"),
        ),
        Err(error) => report.check(
            "folder recovers after deletion and recreation",
            false,
            error,
        ),
    }

    let wake = snip.wake_handle();
    match snip.restart() {
        Ok(()) => {
            report.check(
                "restart",
                snip.is_alive() && snip.wake_handle() == wake,
                "workers alive and wake handle retained",
            );
            check_fixture(
                report,
                "restarted folder reports one PNG",
                &snip,
                &scratch.0.join("restarted.png"),
            );
        }
        Err(error) => report.check("restart", false, error),
    }

    let start = Instant::now();
    let all_joined = snip.shutdown();
    let elapsed = start.elapsed();
    report.check(
        "shutdown joins every detector thread within 2 s",
        all_joined && elapsed < Duration::from_secs(2),
        format!("all_joined={all_joined} elapsed={elapsed:?}"),
    );
    Ok(())
}

fn main() -> ExitCode {
    let mut report = Report::default();
    if let Err(error) = run(&mut report) {
        report.check("start", false, error);
    }
    if report.0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
