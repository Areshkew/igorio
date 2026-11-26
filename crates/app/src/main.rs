#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Wires the modules together: one UI thread owns the overlay, the tray, the state and the
//! drop router. `igorio --demo N` starts with N generated screenshots on the pile.
//! `FAIRY_DEBUG=1` logs every drop decision (see `drop_router::DebugLog`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use igorio_app::draw::Renderer;
use igorio_app::drop_router::{DebugLog, Drops};
use igorio_app::follower::Follower;
use igorio_app::hook::{self, MouseEvent, MouseHook};
use igorio_app::layout::CardSize;
use igorio_app::overlay::{self, Overlay};
use igorio_app::saver;
use igorio_app::settings::{self, DefaultFolders, Settings};
use igorio_app::settings_window::{LiveSettings, SettingsEvent, SettingsWindow};
use igorio_app::shortcuts::{Action, Hotkeys, Registration, RegistrationReport};
use igorio_app::snip::{Snip, SnipEvent};
use igorio_app::state::FairyState;
use igorio_app::tray::{MenuItem, Tray};
use igorio_core::shake::{ShakeGate, ShakeResult};
use igorio_kit::Image;
use tiny_skia::Pixmap;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE,
    SYNCHRONIZATION_SYNCHRONIZE, SetWaitableTimer, TIMER_MODIFY_STATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, PostQuitMessage, QS_ALLINPUT, TranslateMessage, WM_QUIT,
};

const MENU_RESTORE: u32 = 1;
const MENU_ADD_DEMO: u32 = 2;
const MENU_QUIT: u32 = 3;
const MENU_SETTINGS: u32 = 4;

/// Frame interval while something moves, in seconds.
const FRAME: f64 = 0.008;
/// The longest sleep. The loop wakes at least this often to check that the mouse hook is
/// still installed (Windows removes a slow hook without notice).
const HEALTH_CHECK: f64 = 1.0;

struct App {
    state: FairyState,
    shake: ShakeGate,
    follower: Follower,
    renderer: Renderer,
    overlay: Overlay,
    pixmap: Option<Pixmap>,
    origin: (i32, i32),
    scale: f64,
    last_frame: f64,
    /// The scene changed since the last draw.
    dirty: bool,
    /// The last frame was drawn mid-animation, so one more frame shows the end state.
    was_animating: bool,
    demo_count: usize,
}

impl App {
    fn apply_card_size(&mut self, size: CardSize, now: f64) {
        self.state.set_card_size(size, now);
        self.follower.card_size = size;
        self.dirty = true;
    }

    fn add_demo(&mut self) {
        let image = Arc::new(demo_image(self.demo_count));
        self.demo_count += 1;
        self.state.hold(image, 1.0, SystemTime::now(), false, None, None, hook::now());
        self.shake.captured();
        self.dirty = true;
    }

    fn restore_last(&mut self) {
        self.state.restore_last(hook::now());
        self.dirty = true;
    }

    /// Runs a global shortcut. `OpenSettings` is handled by the loop, like the tray item.
    fn shortcut(&mut self, action: Action, now: f64) {
        match action {
            Action::RestoreLast => self.restore_last(),
            Action::DiscardTop | Action::DiscardAll if self.state.is_holding() => {
                if action == Action::DiscardTop {
                    self.state.dissolve(now);
                } else {
                    self.state.discard_all(now);
                }
                // As after a shake: the gesture starts over, so the motion before the key
                // press counts toward nothing. Unlike a shake, no "discard all" is armed.
                self.shake.gesture.reset();
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn capture_ui(&mut self, shown: bool, time: f64) {
        self.state.set_hidden_for_capture(shown, time);
        if shown {
            self.shake.screenshot_ui_shown();
        } else {
            self.shake.screenshot_ui_gone();
        }
        self.dirty = true;
    }

    /// Returns true if a visibility transition may overlap this batch of mouse events.
    fn screenshots(&mut self, events: &mut Vec<SnipEvent>, settings: &Settings) -> bool {
        let mut transitioned = false;
        for event in events.drain(..) {
            match event {
                SnipEvent::CaptureUiShown { time } => {
                    self.capture_ui(true, time);
                    transitioned = true;
                }
                SnipEvent::CaptureUiGone { time } => {
                    self.capture_ui(false, time);
                    transitioned = true;
                }
                SnipEvent::Captured {
                    image,
                    scale,
                    captured_at,
                    saved_file,
                    ..
                } => {
                    let kept = saved_file.is_some();
                    if !kept && settings.keep_every_screenshot {
                        let image = Arc::clone(&image);
                        let settings = settings.clone();
                        // Folder selection can itself hit the filesystem and the Shell.
                        std::thread::spawn(move || {
                            let (folder, _) = settings.captures_dir(&DefaultFolders::from_system());
                            saver::save_screenshot_in_background(
                                image,
                                scale,
                                captured_at,
                                folder,
                                |result| {
                                    if let Err(error) = result {
                                        eprintln!("could not keep screenshot: {error}");
                                    }
                                },
                            );
                        });
                    }
                    self.shake.captured();
                    self.state
                        .hold(image, scale, captured_at, kept, None, None, hook::now());
                    self.dirty = true;
                }
                // The detector already wrote the reason to its debug log.
                SnipEvent::Ignored { .. } => {}
            }
        }
        transitioned
    }

    /// Feeds mouse events to the shake gesture. Returns true when the cursor moved.
    fn mouse(&mut self, events: &[MouseEvent]) -> bool {
        let mut moved = false;
        // The shake thresholds are in logical pixels.
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        for event in events {
            let MouseEvent::Move { x, y, time, injected: false } = *event else {
                continue;
            };
            moved = true;
            let holding = self.state.is_holding();
            match self.shake.move_(x as f64 / scale, y as f64 / scale, time, holding) {
                ShakeResult::DiscardTop => {
                    self.state.dissolve(time);
                    self.dirty = true;
                }
                ShakeResult::DiscardAll => {
                    self.state.discard_all(time);
                    self.dirty = true;
                }
                ShakeResult::None => {}
            }
        }
        moved
    }

    /// True when a frame must run now, whatever the timer says.
    fn needs_frame(&mut self) -> bool {
        if self.overlay.take_display_changed() {
            self.overlay.reassert();
            self.dirty = true;
        }
        self.dirty
    }

    fn pile_visible(&self) -> bool {
        !self.state.stack().is_empty()
    }

    /// Runs one frame and returns the time of the next one; `None` means "when something
    /// happens" (a mouse move, a menu action).
    fn frame(&mut self, now: f64) -> windows::core::Result<Option<f64>> {
        let dt = now - self.last_frame;
        self.last_frame = now;

        let deadline_before = self.state.next_deadline();
        self.state.tick(now);
        if deadline_before.is_some_and(|at| at <= now) {
            self.dirty = true;
        }

        let animating = self.state.is_animating(now);
        if self.state.stack().is_empty() && !animating {
            if self.overlay.is_visible() {
                self.overlay.hide();
            }
            self.follower.reset();
            self.dirty = false;
            self.was_animating = false;
            return Ok(self.state.next_deadline());
        }

        let mouse = overlay::cursor_pos();
        let monitor = overlay::monitor_at(mouse);
        let moved = self
            .follower
            .step(mouse, monitor.work_area, monitor.scale, dt);
        if let Some(origin) = moved {
            self.origin = (origin.x.round() as i32, origin.y.round() as i32);
        }

        let scale_changed = monitor.scale != self.scale;
        self.scale = monitor.scale;
        if self.dirty || animating || self.was_animating || scale_changed {
            let (width, height) = Renderer::pixmap_size(self.scale);
            if self
                .pixmap
                .as_ref()
                .is_none_or(|p| p.width() != width || p.height() != height)
            {
                self.pixmap = Pixmap::new(width, height);
            }
            if let Some(pixmap) = self.pixmap.as_mut() {
                self.renderer
                    .render(&self.state.scene(now, self.scale), pixmap);
                self.overlay.present(pixmap, self.origin)?;
            }
            if !self.overlay.is_visible() {
                self.overlay.show();
            }
            self.dirty = false;
        } else if moved.is_some() {
            self.overlay.move_to(self.origin)?;
        }
        self.was_animating = animating;

        let settled = self
            .follower
            .is_settled(mouse, monitor.work_area, monitor.scale);
        let next_frame = (animating || !settled).then_some(now + FRAME);
        Ok(earlier(next_frame, self.state.next_deadline()))
    }
}

fn earlier(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A generated stand-in for a screenshot: a gradient with a few blocks, a new colour each time.
fn demo_image(index: usize) -> Image {
    const W: u32 = 1600;
    const H: u32 = 1000;
    const PALETTE: [[u8; 3]; 5] = [
        [59, 130, 246],
        [16, 185, 129],
        [245, 158, 11],
        [139, 92, 246],
        [244, 63, 94],
    ];
    let base = PALETTE[index % PALETTE.len()];
    let mut rgba = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let shade = 0.55 + 0.45 * (x + y) as f64 / (W + H) as f64;
            let block = (300..700).contains(&x) && (200..450).contains(&y)
                || (900..1300).contains(&x) && (550..800).contains(&y);
            let lift = if block { 90.0 } else { 0.0 };
            for channel in base {
                rgba.push((channel as f64 * shade + lift).min(255.0) as u8);
            }
            rgba.push(255);
        }
    }
    Image::new(W, H, rgba).expect("the buffer has the declared size")
}

/// A one-shot high-resolution timer, so an 8 ms sleep is not rounded up to the 15.6 ms
/// system tick.
struct Sleeper(HANDLE);

impl Sleeper {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: no pointers are passed; the returned handle is owned by `Sleeper`.
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                (TIMER_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE).0,
            )?
        };
        Ok(Self(handle))
    }

    /// Returns when `wait` has passed, a message is in the queue or one of `wakes` is
    /// signalled, whichever is first.
    fn wait(&self, wait: Duration, wakes: &[HANDLE]) -> windows::core::Result<()> {
        // Negative means relative, in 100 ns units. At least one unit, so the timer fires.
        let due = -((wait.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64);
        let mut handles = Vec::with_capacity(1 + wakes.len());
        handles.push(self.0);
        handles.extend_from_slice(wakes);
        // SAFETY: `self.0` and every handle in `wakes` are live handles, and `due` and
        // `handles` outlive the call.
        unsafe {
            SetWaitableTimer(self.0, &due, 0, None, None, false)?;
            let result = MsgWaitForMultipleObjectsEx(
                Some(&handles),
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            // A failed wait returns at once: end the loop rather than spin.
            if result == WAIT_FAILED {
                return Err(windows::core::Error::from_thread());
            }
        }
        Ok(())
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        // SAFETY: the handle was created in `new` and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Dispatches every queued message. Returns false when WM_QUIT was seen.
fn pump_messages() -> bool {
    let mut message = MSG::default();
    // SAFETY: `message` is a valid MSG for the calls to fill and read; this thread owns
    // the windows whose messages are dispatched.
    unsafe {
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            if message.message == WM_QUIT {
                return false;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    true
}

fn demo_count_from_args() -> usize {
    let args: Vec<String> = std::env::args().collect();
    match args.iter().position(|arg| arg == "--demo") {
        Some(i) => args.get(i + 1).and_then(|n| n.parse().ok()).unwrap_or(1),
        None => 0,
    }
}

/// Serializes detector startup and shutdown off the overlay's frame loop.
#[derive(Default)]
struct Detector {
    active: Option<Snip>,
    transition: Option<JoinHandle<Option<Snip>>>,
    retry_at: f64,
}

impl Detector {
    fn stop(&mut self) {
        if let Some(snip) = self.active.take() {
            self.transition = Some(thread::spawn(move || {
                if !snip.shutdown() {
                    eprintln!("screenshot workers did not stop within their shutdown budget");
                }
                None
            }));
        }
        self.retry_at = 0.0;
    }

    fn update(&mut self, enabled: bool, drops: Option<&Drops>, now: f64) {
        if self.transition.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(transition) = self.transition.take()
        {
            self.active = transition.join().ok().flatten();
        }
        if !enabled {
            self.stop();
        } else if self.active.is_none() && self.transition.is_none() && now >= self.retry_at
            && let Some(clipboard) = drops.and_then(Drops::clipboard)
        {
            self.retry_at = now + HEALTH_CHECK;
            let start = Snip::starter(clipboard);
            self.transition = Some(thread::spawn(move || match start() {
                Ok(snip) => Some(snip),
                Err(error) => {
                    eprintln!("screenshot detection is off: {error}");
                    None
                }
            }));
        }
    }
}

impl Drop for Detector {
    fn drop(&mut self) {
        // Both Snip startup and shutdown have bounded waits and need the clipboard alive.
        drop(self.transition.take().and_then(|thread| thread.join().ok().flatten()));
    }
}

fn main() -> windows::core::Result<()> {
    overlay::ensure_dpi_awareness();
    let settings_path = settings::default_settings_path();
    let mut settings = LiveSettings::from(Settings::load(&settings_path).unwrap_or_default());

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64 | 1);
    let mut overlay = Overlay::new()?;
    if std::env::var_os("FAIRY_CAPTURABLE").is_some_and(|v| v == "1") {
        overlay.set_capturable(true)?;
    }
    let app = Rc::new(RefCell::new(App {
        state: FairyState::new(seed),
        shake: ShakeGate::new(),
        follower: Follower::new(),
        renderer: Renderer::new(),
        overlay,
        pixmap: None,
        origin: (0, 0),
        scale: 0.0,
        last_frame: hook::now(),
        dirty: false,
        was_animating: false,
        demo_count: 0,
    }));
    app.borrow_mut().apply_card_size(settings.card_size, hook::now());
    for _ in 0..demo_count_from_args() {
        app.borrow_mut().add_demo();
    }

    // The tray calls these from inside `DispatchMessageW`, when `app` is not borrowed.
    let menu_app = Rc::clone(&app);
    let action_app = Rc::clone(&app);
    let open_settings = Rc::new(Cell::new(false));
    let action_settings = Rc::clone(&open_settings);
    let _tray = Tray::new(
        "Igorio",
        Box::new(move || {
            let can_restore = menu_app.borrow().state.can_restore();
            vec![
                MenuItem::Action {
                    id: MENU_RESTORE,
                    label: "Restore Last Screenshot".into(),
                    enabled: can_restore,
                    checked: false,
                },
                MenuItem::action(MENU_ADD_DEMO, "Add Demo Screenshot"),
                MenuItem::Separator,
                MenuItem::action(MENU_SETTINGS, "Settings…"),
                MenuItem::action(MENU_QUIT, "Quit"),
            ]
        }),
        Box::new(move |id| match id {
            MENU_RESTORE => action_app.borrow_mut().restore_last(),
            MENU_ADD_DEMO => action_app.borrow_mut().add_demo(),
            MENU_SETTINGS => action_settings.set(true),
            // SAFETY: posts WM_QUIT to this thread's own queue.
            MENU_QUIT => unsafe { PostQuitMessage(0) },
            _ => {}
        }),
    )?;

    let mut mouse_hook = MouseHook::start()?;
    let mut events = Vec::new();
    let sleeper = Sleeper::new()?;

    // The drop router and its workers (UI Automation, clipboard). Without them the app
    // still carries, shakes and restores. Dropping `drops` (on every way out of `main`,
    // errors and panics included) puts the user's clipboard back if a paste left ours there.
    let mut drops = match Drops::start(settings.snapshot.clone(), DebugLog::from_env()) {
        Ok(drops) => Some(drops),
        Err(why) => {
            eprintln!("double-click drops are off: {why}");
            None
        }
    };
    // This local is declared after `drops`, so it stops before clipboard restore and
    // shutdown on normal quit, error returns and unwinding.
    let mut detector = Detector::default();
    let mut hotkeys = match Hotkeys::start(&settings.snapshot.shortcuts) {
        Ok(hotkeys) => Some(hotkeys),
        Err(error) => {
            eprintln!("keyboard shortcuts are off: {error}");
            None
        }
    };
    // Close the recorder before the service on all exit paths.
    let mut settings_window = SettingsWindow::new(settings_path)?;
    let mut recording_shortcut = false;
    let mut presses = Vec::new();
    let mut snip_events = Vec::new();
    let mut wakes = vec![mouse_hook.wake_handle(), settings_window.wake_handle()];
    if let Some(hotkeys) = &hotkeys {
        wakes.push(hotkeys.wake_handle());
    }
    if let Some(drops) = &drops {
        wakes.extend(drops.wake_handles());
    }
    let permanent_wakes = wakes.len();
    // When the next frame runs; `None` waits for a mouse move or a menu action.
    let mut due = Some(hook::now());
    let mut next_health_check = hook::now() + HEALTH_CHECK;

    while pump_messages() {
        let now = hook::now();
        for event in settings_window.events() {
            let saved = match event {
                SettingsEvent::PauseShortcuts(tag) => {
                    recording_shortcut = true;
                    if let Some(service) = &hotkeys {
                        service.set_paused(true);
                    }
                    request_shortcut_status(&settings_window, hotkeys.as_ref(), tag);
                    continue;
                }
                SettingsEvent::CheckShortcuts { tag, proposed } => {
                    if let Some(service) = &hotkeys {
                        service.apply(&proposed);
                        service.set_paused(false);
                    }
                    request_shortcut_status(&settings_window, hotkeys.as_ref(), tag);
                    continue;
                }
                SettingsEvent::ResumeShortcuts => {
                    recording_shortcut = false;
                    if let Some(service) = &hotkeys {
                        service.drain(&mut presses);
                        presses.clear();
                        service.apply(&settings.snapshot.shortcuts);
                        service.set_paused(false);
                    }
                    request_shortcut_status(&settings_window, hotkeys.as_ref(), 0);
                    continue;
                }
                SettingsEvent::Saved(saved) => saved,
            };
            let changed_shortcuts = settings.snapshot.shortcuts != saved.shortcuts;
            settings = saved.into();
            if let Some(drops) = &mut drops {
                drops.set_settings(&settings.snapshot);
            }
            if changed_shortcuts && let Some(service) = &hotkeys {
                service.apply(&settings.snapshot.shortcuts);
                request_shortcut_status(&settings_window, hotkeys.as_ref(), 0);
            }
            let mut app = app.borrow_mut();
            app.apply_card_size(settings.card_size, now);
            if !settings.screenshot_detector {
                detector.stop();
                if app.state.hidden_for_capture() {
                    app.capture_ui(false, now);
                }
            }
        }
        if let Some(hotkeys) = &hotkeys {
            hotkeys.drain(&mut presses);
            for report in hotkeys.reports() {
                settings_window.publish_registration(report);
            }
        }
        if recording_shortcut {
            presses.clear();
        }
        for pressed in presses.drain(..) {
            match pressed.action {
                Action::OpenSettings => open_settings.set(true),
                action => app.borrow_mut().shortcut(action, now),
            }
        }
        if open_settings.replace(false) {
            match settings_window.open() {
                Ok(()) => request_shortcut_status(&settings_window, hotkeys.as_ref(), 0),
                Err(error) => eprintln!("could not queue Settings: {error}"),
            }
        }
        detector.update(settings.screenshot_detector, drops.as_ref(), now);
        mouse_hook.drain(&mut events);
        if let Some(snip) = &detector.active {
            snip.drain(&mut snip_events);
        }
        let mut router_due = None;
        {
            let mut app = app.borrow_mut();
            let app = &mut *app;
            // Treat the whole batch as capture input if visibility changed during
            // it. A shown/gone pair can arrive together with the selection clicks.
            if app.screenshots(&mut snip_events, &settings.snapshot) || app.state.hidden_for_capture() {
                events.clear();
            }
            let moved = app.mouse(&events);
            if let Some(drops) = drops.as_mut() {
                if drops.run(&events, &mut app.state, now) {
                    app.dirty = true;
                }
                router_due = drops.next_deadline();
            }
            events.clear();
            if app.needs_frame() {
                due = Some(now);
            } else if moved && app.pile_visible() {
                // The pile follows the cursor, but not faster than the frame rate.
                due = earlier(due, Some((app.last_frame + FRAME).max(now)));
            }
            if due.is_some_and(|at| at <= now) {
                due = app.frame(now)?;
            }
        }
        if now >= next_health_check {
            next_health_check = now + HEALTH_CHECK;
            if !mouse_hook.is_alive() || mouse_hook.seems_stalled() {
                // On failure the next check tries again.
                let _ = mouse_hook.restart();
            }
            if let Some(snip) = &mut detector.active
                && (!snip.is_alive() || snip.seems_stalled())
            {
                let mut app = app.borrow_mut();
                if app.state.hidden_for_capture() {
                    app.capture_ui(false, now);
                    due = Some(now);
                }
                detector.stop();
            }
        }
        wakes.truncate(permanent_wakes);
        if let Some(snip) = &detector.active {
            wakes.push(snip.wake_handle());
        }
        let transition_due = detector.transition.is_some().then_some(now + FRAME);
        let until = earlier(earlier(due, router_due), earlier(transition_due, Some(next_health_check)))
            .unwrap_or(next_health_check);
        let wait = Duration::from_secs_f64((until - hook::now()).max(0.0));
        sleeper.wait(wait, &wakes)?;
    }
    // Put the user's clipboard back if a paste left ours there, then stop the workers.
    drop(settings_window);
    if let Some(hotkeys) = hotkeys.take() {
        hotkeys.set_paused(false);
        if !hotkeys.shutdown() {
            eprintln!("the shortcut thread did not stop within its shutdown budget");
        }
    }
    drop(detector);
    drop(drops);
    Ok(())
}

fn request_shortcut_status(window: &SettingsWindow, service: Option<&Hotkeys>, tag: usize) {
    if service.is_some_and(|hotkeys| hotkeys.report_after_commands(tag)) {
        return;
    }
    let entries = Action::ALL.iter().copied().map(|action| {
        let status = if action.available() {
            Registration::Failed("The shortcut service is unavailable.".into())
        } else {
            Registration::Unavailable
        };
        (action, status)
    }).collect();
    window.publish_registration(RegistrationReport { tag, entries });
}
