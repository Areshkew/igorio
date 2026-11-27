//! Try the tool menu: hold Alt (or Ctrl+Alt) over empty space, a small menu of tools opens
//! under the cursor, click a row and the tool "runs" (printed here as
//! `TOOL <id> at x,y: origin`; no note window, no recording). Every state change, every
//! lookup, every swallowed click and every injected key is printed.
//!
//! ```text
//! cargo run -p igorio-app --example summon_demo --release [-- --modifier alt|ctrl-alt]
//! cargo run -p igorio-app --example summon_demo --release -- --tools note,record
//! cargo run -p igorio-app --example summon_demo --release -- --render crates/app/examples
//! ```
//!
//! `--tools` names the tools offered (default `note`, as in the app today, where recording
//! is not built yet); `note,record` shows both rows. The rows show the default shortcuts.
//! Ctrl+C stops it: the gate is disarmed, the menu hidden, both hooks removed. `--seconds N`
//! stops it after N seconds. `--render DIR` only writes the menu PNGs and exits.
//!
//! A hard kill (Task Manager, closing the console) cannot leave a key down: the gesture
//! never swallows a key, and its one injected key is a down and up sent in one call. If it
//! dies between a swallowed press and its release, the release reaches the app below on its
//! own (harmless: the app never saw the press). The click gate is a flag in this process's
//! memory, read only by this process's mouse hook: a killed process takes both with it. See
//! `crates/app/examples/SUMMON_TEST.md`.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use igorio_app::hook::{self, MouseHook};
use igorio_app::notes::summoner::{
    Bounds, MenuMotion, MenuPicture, Modifier, OPEN_MS, SummonConfig, Summoner, SummonerEvent,
    ToolContext, ToolId, apply_alpha, menu_layout, menu_width, render_menu, tool_chips,
};
use igorio_app::overlay::ensure_dpi_awareness;
use igorio_app::shortcuts::Shortcuts;
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
    let mut tools = vec![ToolId::NewNote];
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
            ("--tools", Some(value)) => {
                let parsed: Option<Vec<ToolId>> = value.split(',').map(ToolId::parse).collect();
                match parsed {
                    Some(list) => tools = list,
                    None => return usage(),
                }
            }
            ("--render", Some(value)) => render = Some(PathBuf::from(value)),
            _ => return usage(),
        }
        i += 2;
    }
    let result = match render {
        Some(dir) => write_pngs(&dir),
        None => run(modifier, &tools, seconds),
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
    eprintln!(
        "usage: summon_demo [--modifier alt|ctrl-alt] [--tools note,record] [--seconds N] [--render DIR]"
    );
    ExitCode::FAILURE
}

fn run(modifier: Modifier, tools: &[ToolId], seconds: Option<f64>) -> Result<(), String> {
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
    let record = tools.contains(&ToolId::RecordScreen);
    summoner.set_tool_context(ToolContext {
        notes: tools.contains(&ToolId::NewNote),
        recording_available: record,
        recording_enabled: record,
        ..ToolContext::default()
    });
    summoner.set_shortcuts(&Shortcuts::defaults());
    let names: Vec<&str> = tools.iter().map(|t| t.id()).collect();
    println!(
        "Hold {} over empty space; the menu offers [{}]. Click a row. Ctrl+C stops.",
        modifier.label(),
        names.join(", ")
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
                SummonerEvent::ToolChosen(chosen) => println!(
                    "TOOL {} at {},{} (t={:.3}): {} | {}",
                    chosen.tool.id(),
                    chosen.point.0,
                    chosen.point.1,
                    chosen.time,
                    chosen.origin.label(),
                    chosen.origin.detail().replace('\n', " | ")
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

// --- PNGs of the menu ---

const ZOOM: u32 = 2;

struct Shot {
    name: &'static str,
    scale: f64,
    dark: bool,
    /// The cursor's row: 0 (just opened) or 1 (hovered).
    row: usize,
    /// Half way through the open animation (half faded in), not settled.
    opening: bool,
    /// Opened near the bottom of the screen (upward).
    upward: bool,
}

fn write_pngs(dir: &Path) -> Result<(), String> {
    let shots = [
        Shot {
            name: "summon-menu-light-100.png",
            scale: 1.0,
            dark: false,
            row: 0,
            opening: false,
            upward: false,
        },
        Shot {
            name: "summon-menu-dark-100.png",
            scale: 1.0,
            dark: true,
            row: 0,
            opening: false,
            upward: false,
        },
        Shot {
            name: "summon-menu-light-125.png",
            scale: 1.25,
            dark: false,
            row: 0,
            opening: false,
            upward: false,
        },
        Shot {
            name: "summon-menu-dark-125.png",
            scale: 1.25,
            dark: true,
            row: 0,
            opening: false,
            upward: false,
        },
        Shot {
            name: "summon-menu-hover-light-100.png",
            scale: 1.0,
            dark: false,
            row: 1,
            opening: false,
            upward: false,
        },
        Shot {
            name: "summon-menu-hover-dark-125.png",
            scale: 1.25,
            dark: true,
            row: 1,
            opening: false,
            upward: false,
        },
        Shot {
            name: "summon-menu-open-half-light-100.png",
            scale: 1.0,
            dark: false,
            row: 0,
            opening: true,
            upward: false,
        },
        Shot {
            name: "summon-menu-upward-light-100.png",
            scale: 1.0,
            dark: false,
            row: 0,
            opening: false,
            upward: true,
        },
    ];
    let shortcuts = Shortcuts::defaults();
    let rows: Vec<(ToolId, Vec<String>)> = [ToolId::NewNote, ToolId::RecordScreen]
        .into_iter()
        .map(|id| (id, tool_chips(id, &shortcuts)))
        .collect();
    let work = Bounds {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    for shot in shots {
        let opened_at = if shot.upward { (400, 1020) } else { (400, 300) };
        let width = menu_width(&rows, shot.scale);
        let layout = menu_layout(opened_at, rows.len(), width, work, shot.scale);
        // Where the cursor is now: on the first row where the menu opened, or on the second.
        let cursor = if shot.row == 0 {
            opened_at
        } else {
            let row = layout.row(shot.row);
            (
                opened_at.0,
                (row.top + row.bottom) / 2 - (4.0 * shot.scale) as i32,
            )
        };
        let mut motion = MenuMotion::hidden();
        motion.open(Some(0), 0.0, true);
        if shot.row != 0 {
            motion.hover(Some(shot.row), 1000.0);
        }
        // Half way: the moment the menu is half faded in (the easing is fast at first, so
        // that is early in the OPEN_MS).
        let mut at = 5000.0;
        if shot.opening {
            at = 0.0;
            while motion.frame(at).alpha < 0.5 && at < OPEN_MS {
                at += 0.25;
            }
        }
        let frame = motion.frame(at);
        let mut menu = render_menu(&MenuPicture {
            rows: &rows,
            layout: &layout,
            dark: shot.dark,
            highlight: frame.highlight,
            lit: frame.lit,
            flash: frame.flash,
        })
        .ok_or("render")?;
        apply_alpha(&mut menu, frame.alpha);
        let rise = (frame.rise * shot.scale).round() as i32;
        // The window's area plus a margin, on a window background, with the arrow cursor.
        let margin = (16.0 * shot.scale) as i32;
        let (left, top) = (layout.window.left - margin, layout.window.top - margin);
        let (w, h) = (
            (layout.window.width() + 2 * margin) as u32,
            (layout.window.height() + 2 * margin) as u32,
        );
        let mut picture = Pixmap::new(w, h).ok_or("pixmap")?;
        picture.fill(if shot.dark {
            Color::from_rgba8(0x20, 0x20, 0x20, 255)
        } else {
            Color::from_rgba8(0xf3, 0xf3, 0xf3, 255)
        });
        picture.draw_pixmap(
            layout.window.left - left,
            layout.window.top - top + rise,
            menu.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        draw_cursor(
            &mut picture,
            f64::from(cursor.0 - left),
            f64::from(cursor.1 - top),
            shot.scale,
        );
        let path = dir.join(shot.name);
        save_zoomed(&picture, &path)?;
        println!(
            "{} ({}x{} px window at {}%, alpha {:.2}, rise {rise} px, highlight {:.2}{}, shown {ZOOM}x)",
            path.display(),
            layout.window.width(),
            layout.window.height(),
            shot.scale * 100.0,
            frame.alpha,
            frame.highlight,
            if layout.upward { ", upward" } else { "" },
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
