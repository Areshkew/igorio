//! Try the note gesture: hold Alt (or Ctrl+Alt) over empty space, a pill rings the cursor,
//! click and a note is "made" (printed here; no note window yet). Every state change, every
//! lookup, every swallowed click and every injected key is printed.
//!
//! ```text
//! cargo run -p igorio-app --example summon_demo --release [-- --modifier alt|ctrl-alt]
//! cargo run -p igorio-app --example summon_demo --release -- --render crates/app/examples
//! ```
//!
//! Ctrl+C stops it: the gate is disarmed, the pill hidden, both hooks removed. `--seconds N`
//! stops it after N seconds. `--render DIR` only writes the pill PNGs and exits.
//!
//! A hard kill (Task Manager, closing the console) cannot leave a key down: the gesture
//! never swallows a key, and its one injected key is a down and up sent in one call. If it
//! dies between a swallowed press and its release, the release reaches the app below on its
//! own (harmless: the app never saw the press). See `crates/app/examples/SUMMON_TEST.md`.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use igorio_app::hook::{self, MouseHook};
use igorio_app::notes::summoner::{
    APPEAR, Modifier, SummonConfig, Summoner, SummonerEvent, pill_window_size, render_pill, spring,
};
use igorio_app::overlay::ensure_dpi_awareness;
use igorio_app::uia::Uia;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::WaitForMultipleObjects;
use windows::core::BOOL;

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(_kind: u32) -> BOOL {
    STOP.store(true, Ordering::SeqCst);
    BOOL(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut modifier = Modifier::Alt;
    let mut seconds = None;
    let mut render = None;
    let mut i = 0;
    while i < args.len() {
        match (args[i].as_str(), args.get(i + 1)) {
            ("--modifier", Some(value)) => match Modifier::parse(value) {
                Some(m) => modifier = m,
                None => return usage(),
            },
            ("--seconds", Some(value)) => match value.parse::<f64>() {
                Ok(s) => seconds = Some(s),
                Err(_) => return usage(),
            },
            ("--render", Some(value)) => render = Some(PathBuf::from(value)),
            _ => return usage(),
        }
        i += 2;
    }
    let result = match render {
        Some(dir) => write_pngs(&dir),
        None => run(modifier, seconds),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("summon_demo: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: summon_demo [--modifier alt|ctrl-alt] [--seconds N] [--render DIR]");
    ExitCode::FAILURE
}

fn run(modifier: Modifier, seconds: Option<f64>) -> Result<(), String> {
    ensure_dpi_awareness();
    // SAFETY: `on_ctrl` is a 'static function that only stores an atomic.
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl), true) }.map_err(|e| e.to_string())?;
    let mut mouse = MouseHook::start().map_err(|e| format!("mouse hook: {e}"))?;
    let uia = Uia::start().map_err(|e| format!("UI Automation: {e}"))?;
    let requester = uia.requester().map_err(|e| format!("lane: {e}"))?;
    let config = SummonConfig {
        modifier,
        enabled: true,
        debug: true,
        capturable: std::env::var_os("FAIRY_CAPTURABLE").is_some_and(|v| v == "1"),
    };
    let mut summoner = Summoner::start(config, requester)?;
    println!(
        "Hold {} over empty space, click while the pill shows. Ctrl+C stops.",
        modifier.label()
    );
    let started = Instant::now();
    let mut events = Vec::new();
    let mut out = Vec::new();
    let mut next_check = Instant::now() + Duration::from_secs(1);
    while !STOP.load(Ordering::SeqCst)
        && seconds.is_none_or(|s| started.elapsed().as_secs_f64() < s)
    {
        let handles: [HANDLE; 2] = [mouse.wake_handle(), summoner.wake_handle()];
        // SAFETY: both handles live as long as their owners, which outlive the call.
        let result = unsafe { WaitForMultipleObjects(&handles, false, 100) };
        if result == WAIT_FAILED {
            return Err("wait failed".into());
        }
        events.clear();
        mouse.drain(&mut events);
        summoner.feed(&events);
        out.clear();
        summoner.drain(&mut out);
        for event in &out {
            match event {
                SummonerEvent::Log(line) => println!("FAIRY_DEBUG {line}"),
                SummonerEvent::Summon(s) => println!(
                    "SUMMON at {},{} (t={:.3}): {} | {}",
                    s.point.0,
                    s.point.1,
                    s.time,
                    s.origin.label(),
                    s.origin.detail().replace('\n', " | ")
                ),
            }
        }
        if Instant::now() >= next_check {
            next_check = Instant::now() + Duration::from_secs(1);
            if !mouse.is_alive() || mouse.seems_stalled() {
                println!("FAIRY_DEBUG {:10.3} mouse hook restarted", hook::now());
                let _ = mouse.restart();
            }
            if !summoner.is_alive() {
                println!("FAIRY_DEBUG {:10.3} summoner restarted", hook::now());
                summoner.restart()?;
            }
        }
    }
    let stopping = Instant::now();
    let ended = summoner.shutdown();
    drop(mouse);
    let _ = uia.shutdown();
    println!(
        "stopped (summoner thread ended: {ended}, in {} ms)",
        stopping.elapsed().as_millis()
    );
    Ok(())
}

// --- PNGs of the pill ---

const ZOOM: u32 = 4;

fn write_pngs(dir: &Path) -> Result<(), String> {
    // The time the spring is half way.
    let mut half = 0.0;
    while spring(half) < 0.5 && half < APPEAR {
        half += 0.001;
    }
    let shots: [(&str, f64, f64, bool); 6] = [
        ("summon-light-100.png", 1.0, 1.0, false),
        ("summon-dark-100.png", 1.0, 1.0, true),
        ("summon-light-125.png", 1.25, 1.0, false),
        ("summon-dark-125.png", 1.25, 1.0, true),
        ("summon-appear-half-light-100.png", 1.0, spring(half), false),
        ("summon-appear-half-dark-125.png", 1.25, spring(half), true),
    ];
    for (name, scale, value, dark) in shots {
        let (w, h) = pill_window_size(scale);
        let mut pill = Pixmap::new(w, h).ok_or("pixmap")?;
        render_pill(&mut pill, value, scale, dark);
        // On a window background, with the arrow cursor at the hot spot, zoomed so a person
        // can judge the edges.
        let mut shot = Pixmap::new(w, h).ok_or("pixmap")?;
        shot.fill(if dark {
            Color::from_rgba8(0x20, 0x20, 0x20, 255)
        } else {
            Color::from_rgba8(0xf3, 0xf3, 0xf3, 255)
        });
        shot.draw_pixmap(
            0,
            0,
            pill.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        draw_cursor(&mut shot, f64::from(w) / 2.0, f64::from(h) / 2.0, scale);
        let path = dir.join(name);
        save_zoomed(&shot, &path)?;
        println!(
            "{} ({w}x{h} px at {}%, value {value:.2}, shown {ZOOM}x)",
            path.display(),
            scale * 100.0
        );
    }
    Ok(())
}

/// The standard arrow, tip at (x, y).
fn draw_cursor(pixmap: &mut Pixmap, x: f64, y: f64, scale: f64) {
    let s = scale as f32;
    let (x, y) = (x as f32, y as f32);
    let points = [
        (0.0, 0.0),
        (0.0, 16.0),
        (4.0, 12.5),
        (6.5, 18.0),
        (9.0, 17.0),
        (6.5, 11.5),
        (11.5, 11.5),
    ];
    let mut path = PathBuilder::new();
    path.move_to(x, y);
    for (px, py) in &points[1..] {
        path.line_to(x + px * s, y + py * s);
    }
    path.close();
    let Some(path) = path.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(255, 255, 255, 255);
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    paint.set_color_rgba8(0, 0, 0, 255);
    let stroke = Stroke {
        width: s,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn save_zoomed(pixmap: &Pixmap, path: &Path) -> Result<(), String> {
    let (w, h) = (pixmap.width(), pixmap.height());
    let mut data = Vec::with_capacity((w * h * ZOOM * ZOOM * 4) as usize);
    for y in 0..h * ZOOM {
        for x in 0..w * ZOOM {
            let p = pixmap
                .pixel(x / ZOOM, y / ZOOM)
                .ok_or("pixel")?
                .demultiply();
            data.extend_from_slice(&[p.red(), p.green(), p.blue(), p.alpha()]);
        }
    }
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), w * ZOOM, h * ZOOM);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&data).map_err(|e| e.to_string())
}
