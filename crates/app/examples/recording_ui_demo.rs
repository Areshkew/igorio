//! Try the recording UI by hand, without recording anything: the outline, the pill and the
//! ink window on the monitor under the cursor, driven by the real input.
//!
//! `cargo run -p igorio-app --example recording_ui_demo --release -- [options]`
//!
//! - Zoom: hold Alt and turn the wheel (the outline shrinks and grows), hold Alt and move
//!   to move the zoomed area, let go to leave it, tap Alt twice for the whole screen. The
//!   demo steps a `RecordingViewport` with the events, as the recorder will.
//! - The strip at the top: click the pen or highlighter button to pick it up or put it down,
//!   click the `2.0x` badge for the whole screen, drag the dot or the time to move the strip
//!   along the top, hover a button for its tooltip. The stop button ends the demo.
//! - Tools also by console line `pen` or `hl` (or start with `--chords` to use Ctrl+Shift+7
//!   and Ctrl+Shift+8, registered only while the demo runs). Draw with the mouse; Ctrl+Z
//!   undoes, Backspace or Delete clears, Esc puts the pen down. `undo`, `clear` and `done`
//!   work as console lines too.
//! - `q` and Enter quits.
//!
//! Options: `--ctrl-alt` (Ctrl+Alt instead of Alt), `--no-mask` (do not send the mask key, so
//! a lone Alt opens menus again), `--chords` (see above), `--seconds N` (quit after N s),
//! `--debug` (log lines).
//!
//! Killed hard (Ctrl+C, Task Manager): Windows removes the process's hooks and hotkeys and its
//! windows with it. No key can be left down: the demo never swallows a key release, and the
//! mask key goes down and up in one `SendInput` call.

use std::io::BufRead;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use igorio_app::hook::MouseHook;
use igorio_app::overlay::{cursor_pos, ensure_dpi_awareness, monitor_bounds_at};
use igorio_app::recording::frame::Labels;
use igorio_app::recording::ink_overlay::{RecordingUi, RecordingUiConfig, UiEvent};
use igorio_app::recording::input::ZoomModifier;
use igorio_core::geom::Rect;
use igorio_core::ink::{InkKey, InkTool};
use igorio_core::viewport::RecordingViewport;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, QS_ALLINPUT, WM_HOTKEY,
};

const PEN_HOTKEY: i32 = 0x7701;
const HIGHLIGHTER_HOTKEY: i32 = 0x7702;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let seconds = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<f64>().ok());
    let modifier = if has("--ctrl-alt") {
        ZoomModifier::CtrlAlt
    } else {
        ZoomModifier::Alt
    };
    ensure_dpi_awareness();
    let monitor = monitor_bounds_at(cursor_pos());
    println!(
        "Monitor {}x{} at {},{} ({}%).",
        monitor.bounds.width,
        monitor.bounds.height,
        monitor.bounds.x,
        monitor.bounds.y,
        (monitor.scale * 100.0).round()
    );

    // The process's one mouse hook: the wheel gate decides in its callback.
    let hook = MouseHook::start().map_err(|e| format!("mouse hook: {e}"))?;
    let chords = has("--chords") && register_chords();
    let config = RecordingUiConfig {
        modifier,
        labels: if chords {
            Labels::default()
        } else {
            Labels {
                toggle: None,
                pen: None,
                highlighter: None,
            }
        },
        inject_mask: !has("--no-mask"),
        debug: has("--debug"),
        ..RecordingUiConfig::default()
    };
    let mut ui = RecordingUi::start(monitor, config)?;
    ui.set_elapsed(0.0);

    let label = modifier.label();
    println!(
        "Hold {label} + wheel to zoom, hold {label} and move to move, tap {label} twice for the whole screen."
    );
    if chords {
        println!("Ctrl+Shift+7 pen, Ctrl+Shift+8 highlighter (registered for this demo only).");
    }
    println!("Console: pen | hl | undo | clear | done | q");

    let (lines_tx, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut viewport = RecordingViewport::new(monitor.bounds);
    let mut following = false;
    let mut area: Option<Rect> = None;
    let mut last = Instant::now();
    let started = Instant::now();
    let mut mouse = Vec::new();
    let mut events = Vec::new();
    'main: loop {
        let moving = following || (viewport.zoom() - viewport.target_zoom()).abs() > 1e-4;
        let handles = [ui.wake_handle(), hook.wake_handle()];
        // SAFETY: both handles stay open while `ui` and `hook` live.
        unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&handles),
                if moving { 8 } else { 50 },
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        for id in hotkeys() {
            match id {
                PEN_HOTKEY => ui.tool_shortcut(InkTool::Pen),
                HIGHLIGHTER_HOTKEY => ui.tool_shortcut(InkTool::Highlighter),
                _ => {}
            }
        }
        mouse.clear();
        hook.drain(&mut mouse);
        ui.feed(&mouse);
        events.clear();
        ui.drain(&mut events);
        for event in &events {
            match event {
                UiEvent::Zoom(amount) => viewport.zoom_by(*amount),
                UiEvent::Follow { on, .. } => following = *on,
                UiEvent::ResetZoom => viewport.reset(),
                UiEvent::ToolChanged(tool) => println!("tool: {tool:?}"),
                UiEvent::Log(line) => println!("{line}"),
                UiEvent::StopRequested => {
                    println!("stop button");
                    break 'main;
                }
                UiEvent::Hidden => {}
            }
        }
        while let Ok(line) = lines.try_recv() {
            match line.trim() {
                "pen" => ui.tool_shortcut(InkTool::Pen),
                "hl" | "highlighter" => ui.tool_shortcut(InkTool::Highlighter),
                "undo" => ui.ink_key(InkKey::Undo),
                "clear" => ui.ink_key(InkKey::Clear),
                "done" => ui.ink_key(InkKey::Done),
                "q" | "quit" => break 'main,
                "" => {}
                other => println!("unknown: {other}"),
            }
        }
        let now = Instant::now();
        let dt = (now - last).as_secs_f64();
        last = now;
        viewport.step(cursor_pos(), following, dt);
        let next = viewport.area();
        if area != Some(next) {
            area = Some(next);
            ui.set_area(next);
        }
        if seconds.is_some_and(|s| started.elapsed().as_secs_f64() >= s) {
            break;
        }
    }

    ui.stop();
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut events = Vec::new();
    while Instant::now() < deadline && !events.contains(&UiEvent::Hidden) {
        ui.drain(&mut events);
        std::thread::sleep(Duration::from_millis(10));
    }
    let ended = ui.shutdown(Duration::from_secs(1));
    if chords {
        unregister_chords();
    }
    drop(hook);
    println!(
        "bye ({})",
        if ended {
            "clean"
        } else {
            "thread left running"
        }
    );
    Ok(())
}

fn register_chords() -> bool {
    let mods: HOT_KEY_MODIFIERS = MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT;
    // SAFETY: thread hotkeys (no window); unregistered at exit, freed by Windows if killed.
    unsafe {
        let pen = RegisterHotKey(None, PEN_HOTKEY, mods, 0x37).is_ok();
        let highlighter = RegisterHotKey(None, HIGHLIGHTER_HOTKEY, mods, 0x38).is_ok();
        if !(pen && highlighter) {
            println!("Ctrl+Shift+7 or 8 is taken by another program; use the console.");
            let _ = UnregisterHotKey(None, PEN_HOTKEY);
            let _ = UnregisterHotKey(None, HIGHLIGHTER_HOTKEY);
            return false;
        }
    }
    true
}

fn unregister_chords() {
    // SAFETY: the thread hotkeys registered above.
    unsafe {
        let _ = UnregisterHotKey(None, PEN_HOTKEY);
        let _ = UnregisterHotKey(None, HIGHLIGHTER_HOTKEY);
    }
}

/// Thread messages: the hotkey ids pressed since the last call.
fn hotkeys() -> Vec<i32> {
    let mut ids = Vec::new();
    let mut msg = MSG::default();
    // SAFETY: `msg` is a local; this thread owns no windows.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_HOTKEY {
                ids.push(msg.wParam.0 as i32);
            } else {
                DispatchMessageW(&msg);
            }
        }
    }
    ids
}
