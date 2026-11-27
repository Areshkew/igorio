//! Run with cargo run -p igorio-app --example recorder_demo --release -- [seconds] [--drawn].

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use igorio_app::hook;
use igorio_app::recording::recorder::{
    CursorMode, MonitorId, Recorder, RecorderEvent, RecordingConfig,
};
use igorio_core::geom::{Point, Rect};
use igorio_core::viewport::RecordingViewport;
use windows::Win32::Foundation::WAIT_FAILED;
use windows::Win32::System::Threading::WaitForSingleObject;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Recorder demo: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut duration = None;
    let mut mode = CursorMode::System;
    for arg in std::env::args().skip(1) {
        if arg == "--drawn" {
            mode = CursorMode::Drawn { scale: 2.0 };
        } else if duration.is_none() {
            duration = Some(arg.parse::<f64>()?);
        } else {
            return Err("usage: recorder_demo [seconds] [--drawn]".into());
        }
    }
    let duration = duration.unwrap_or(5.0);
    if !(0.1..=86_400.0).contains(&duration) {
        return Err("seconds must be between 0.1 and 86400".into());
    }
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/recorder-demo");
    let mut config = RecordingConfig::new(MonitorId::under_cursor()?, folder);
    config.cursor = mode;
    let mut recorder = Recorder::start(config)?;
    let budget = Duration::from_secs_f64(duration + 15.0);
    let deadline = Instant::now() + budget;
    let mut events = Vec::with_capacity(3);
    let mut script = None::<(f64, f64, RecordingViewport)>;
    let (mut zoomed, mut reset) = (false, false);
    while Instant::now() < deadline {
        recorder.drain(&mut events);
        for event in events.drain(..) {
            match event {
                RecorderEvent::Started {
                    monitor_rect,
                    output_size,
                    scale,
                    time,
                } => {
                    println!(
                        "Started {monitor_rect:?}, {}×{}, scale {scale:.2}",
                        output_size.0, output_size.1
                    );
                    let display = Rect::new(0.0, 0.0, monitor_rect.width, monitor_rect.height);
                    script = Some((time, time, RecordingViewport::new(display)));
                }
                RecorderEvent::Progress {
                    elapsed,
                    frames,
                    dropped,
                } => println!("Progress {elapsed:.2}s, {frames} frames, {dropped} dropped"),
                RecorderEvent::Finished { file, duration, .. } => {
                    println!("Finished {duration:.3}s\n{}", file.display());
                    if !recorder.shutdown(Duration::from_secs(1)) {
                        return Err("worker did not exit after finalizing".into());
                    }
                    return Ok(());
                }
                RecorderEvent::Failed {
                    reason,
                    partial_file,
                } => {
                    if let Some(path) = partial_file {
                        eprintln!("Recovery file {}", path.display());
                    }
                    return Err(reason.into());
                }
            }
        }
        if let Some((start, last, viewport)) = &mut script {
            let now = hook::now();
            let elapsed = now - *start;
            if elapsed >= duration / 3.0 && !zoomed {
                viewport.zoom_by(2.0_f64.ln());
                zoomed = true;
            }
            if elapsed >= duration * 2.0 / 3.0 && !reset {
                viewport.reset();
                reset = true;
            }
            let center = Point::new(viewport.display().mid_x(), viewport.display().mid_y());
            viewport.step(center, false, now - *last);
            *last = now;
            recorder.set_area(viewport.area());
            if elapsed >= duration {
                recorder.stop();
            }
        }
        // SAFETY: this live event is owned by recorder and borrowed for a bounded wait.
        if unsafe { WaitForSingleObject(recorder.wake_handle(), 8) } == WAIT_FAILED {
            return Err(windows::core::Error::from_thread().into());
        }
    }
    recorder.shutdown(Duration::from_secs(2));
    Err("recording exceeded its deadline".into())
}
