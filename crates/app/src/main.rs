#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Wires the modules together: one UI thread owns the overlay, the tray and the state.
//! `igorio --demo N` starts with N generated screenshots on the pile.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use igorio_app::draw::Renderer;
use igorio_app::follower::Follower;
use igorio_app::overlay::{self, Overlay};
use igorio_app::state::FairyState;
use igorio_app::tray::{MenuItem, Tray};
use igorio_kit::Image;
use tiny_skia::Pixmap;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
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

/// Frame interval while something moves.
const FRAME: Duration = Duration::from_millis(8);
/// How often the cursor is checked while the pile is at rest. Until the mouse hook of
/// milestone M4 exists, polling is the only way to notice that the cursor moved.
const IDLE_POLL: Duration = Duration::from_millis(33);

struct App {
    started: Instant,
    state: FairyState,
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
    fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn add_demo(&mut self) {
        let now = self.now();
        let image = Arc::new(demo_image(self.demo_count));
        self.demo_count += 1;
        self.state.hold(image, 1.0, SystemTime::now(), false, None, None, now);
        self.dirty = true;
    }

    fn restore_last(&mut self) {
        let now = self.now();
        self.state.restore_last(now);
        self.dirty = true;
    }

    /// Runs one frame and returns how long to sleep; `None` means "until a message arrives".
    fn frame(&mut self) -> windows::core::Result<Option<Duration>> {
        let now = self.now();
        let dt = now - self.last_frame;
        self.last_frame = now;

        let deadline_before = self.state.next_deadline();
        self.state.tick(now);
        if deadline_before.is_some_and(|at| at <= now) {
            self.dirty = true;
        }
        if self.overlay.take_display_changed() {
            self.overlay.reassert();
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
            return Ok(self.until_deadline(now, None));
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
        let pace = if animating || !settled { FRAME } else { IDLE_POLL };
        Ok(self.until_deadline(now, Some(pace)))
    }

    /// The shorter of `pace` and the time to the state's next deadline.
    fn until_deadline(&self, now: f64, pace: Option<Duration>) -> Option<Duration> {
        let deadline = self
            .state
            .next_deadline()
            .map(|at| Duration::from_secs_f64((at - now).max(0.0)));
        match (pace, deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
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

    /// Returns when `wait` has passed or a message is in the queue, whichever is first.
    fn wait(&self, wait: Option<Duration>) -> windows::core::Result<()> {
        let Some(wait) = wait else {
            // SAFETY: no handles are passed; the call only blocks this thread.
            unsafe { MsgWaitForMultipleObjectsEx(None, INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
            return Ok(());
        };
        // Negative means relative, in 100 ns units. At least one unit, so the timer fires.
        let due = -((wait.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64);
        // SAFETY: `self.0` is a live timer handle and `due` outlives the call.
        unsafe {
            SetWaitableTimer(self.0, &due, 0, None, None, false)?;
            MsgWaitForMultipleObjectsEx(Some(&[self.0]), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
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

fn main() -> windows::core::Result<()> {
    overlay::ensure_dpi_awareness();

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64 | 1);
    let mut overlay = Overlay::new()?;
    if std::env::var_os("FAIRY_CAPTURABLE").is_some_and(|v| v == "1") {
        overlay.set_capturable(true)?;
    }
    let app = Rc::new(RefCell::new(App {
        started: Instant::now(),
        state: FairyState::new(seed),
        follower: Follower::new(),
        renderer: Renderer::new(),
        overlay,
        pixmap: None,
        origin: (0, 0),
        scale: 0.0,
        last_frame: 0.0,
        dirty: false,
        was_animating: false,
        demo_count: 0,
    }));
    for _ in 0..demo_count_from_args() {
        app.borrow_mut().add_demo();
    }

    // The tray calls these from inside `DispatchMessageW`, when `app` is not borrowed.
    let menu_app = Rc::clone(&app);
    let action_app = Rc::clone(&app);
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
                MenuItem::action(MENU_QUIT, "Quit"),
            ]
        }),
        Box::new(move |id| match id {
            MENU_RESTORE => action_app.borrow_mut().restore_last(),
            MENU_ADD_DEMO => action_app.borrow_mut().add_demo(),
            // SAFETY: posts WM_QUIT to this thread's own queue.
            MENU_QUIT => unsafe { PostQuitMessage(0) },
            _ => {}
        }),
    )?;

    let sleeper = Sleeper::new()?;
    while pump_messages() {
        let wait = app.borrow_mut().frame()?;
        sleeper.wait(wait)?;
    }
    Ok(())
}
