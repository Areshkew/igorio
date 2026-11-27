//! Non-interactive self-check of `igorio_app::pins`. Sends no input, moves nothing, launches
//! nothing, never takes the foreground: pins are shown without activation and driven with
//! messages sent to their own windows. With `IGORIO_TEST_MONITOR=secondary` they open on the
//! first monitor that is not the primary one. Prints PASS or FAIL per check; exits 0 only if
//! all pass.
//!
//! Pictures (the pin's own pixels on a plain backdrop, corners rounded as DWM rounds them; the
//! DWM shadow is not in them) go to crates/app/examples/pin-*.png.
//!
//! Not checked here: Copy (it writes the real clipboard) and Save as PNG… (a modal dialog);
//! the saver's `save_png_as` has its own test. The native right-click menu is not drawn by us.

#[path = "common/test_monitor.rs"]
mod test_monitor;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use igorio_app::overlay::{cursor_pos, ensure_dpi_awareness, monitor_at, monitor_bounds_at};
use igorio_app::pins::{
    self, Event, MENU_CARRY, MENU_CLOSE, MIN_SIDE, Pins, TestOptions, natural_size, pin_frame,
    pin_size, wheel_frame,
};
use igorio_core::geom::{CardId, Point, Rect};
use igorio_kit::{HeldCard, Image};
use tiny_skia::{Color, FillRule, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Transform};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetForegroundWindow, GetLayeredWindowAttributes, GetWindowDisplayAffinity,
    GetWindowLongPtrW, IsWindowVisible, LAYERED_WINDOW_ATTRIBUTES_FLAGS, SendMessageW, WDA_NONE,
    WM_COMMAND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Checks {
    ok: bool,
}

impl Checks {
    fn check(&mut self, name: &str, pass: bool) {
        self.ok &= pass;
        println!("{} {name}", if pass { "PASS" } else { "FAIL" });
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("pin_selftest: {error}");
            ExitCode::FAILURE
        }
    }
}

/// A stand-in screenshot: a gradient with two lighter blocks, as `igorio --demo` makes.
fn demo_image(width: u32, height: u32, base: [u8; 3]) -> Arc<Image> {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let shade = 0.55 + 0.45 * f64::from(x + y) / f64::from(width + height);
            let block = (width * 3 / 16..width * 7 / 16).contains(&x) && (height / 5..height * 9 / 20).contains(&y)
                || (width * 9 / 16..width * 13 / 16).contains(&x) && (height * 11 / 20..height * 4 / 5).contains(&y);
            let lift = if block { 90.0 } else { 0.0 };
            for channel in base {
                rgba.push((f64::from(channel) * shade + lift).min(255.0) as u8);
            }
            rgba.push(255);
        }
    }
    Arc::new(Image::new(width, height, rgba).expect("the buffer has the declared size"))
}

fn card(id: u64, image: Arc<Image>, movie: bool) -> HeldCard {
    HeldCard {
        id: CardId(id),
        image,
        scale: 1.0,
        captured_at: SystemTime::now(),
        kept: movie,
        tilt: 0.0,
        movie: movie.then(|| PathBuf::from(r"C:\nowhere\recording.mp4")),
        duration: movie.then_some(75.0),
    }
}

fn wait_for(mut what: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if what() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    what()
}

fn events_within(pins: &Pins, wait: Duration) -> Vec<Event> {
    let until = Instant::now() + wait;
    let mut events = Vec::new();
    while Instant::now() < until && events.is_empty() {
        events.extend(pins.events());
        thread::sleep(Duration::from_millis(10));
    }
    events
}

fn hwnd_of(pins: &Pins, id: u64) -> Option<HWND> {
    pins.test_with(id, |pin| pin.hwnd().0 as isize).map(|raw| HWND(raw as *mut _))
}

fn frame_of(pins: &Pins, id: u64) -> Option<Rect> {
    pins.test_with(id, |pin| pin.frame())
}

fn lparam(x: i32, y: i32) -> LPARAM {
    LPARAM(((y as u16 as isize) << 16) | x as u16 as isize)
}

fn send(hwnd: HWND, message: u32, wp: usize, lp: LPARAM) {
    // SAFETY: a message to a window of this process, as its own thread would get it.
    unsafe { SendMessageW(hwnd, message, Some(WPARAM(wp)), Some(lp)) };
}

/// What the window painted (its own surface, not the screen), BGRA rows. Run on the pins
/// thread, between its frames.
fn window_pixels(hwnd: HWND, width: i32, height: i32) -> Option<Vec<u8>> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: GDI objects made, used and freed here; the window is ours.
    unsafe {
        let window_dc = GetDC(Some(hwnd));
        let dc = CreateCompatibleDC(Some(window_dc));
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok();
        let pixels = bitmap.filter(|_| !bits.is_null()).map(|bitmap| {
            let old = SelectObject(dc, bitmap.into());
            let _ = BitBlt(dc, 0, 0, width, height, Some(window_dc), 0, 0, SRCCOPY);
            let _ = GdiFlush();
            let pixels = std::slice::from_raw_parts(bits.cast::<u8>(), (width * height * 4) as usize).to_vec();
            SelectObject(dc, old);
            let _ = DeleteObject(bitmap.into());
            pixels
        });
        let _ = DeleteDC(dc);
        ReleaseDC(Some(hwnd), window_dc);
        pixels
    }
}

fn gone(pins: &Pins, id: u64) -> bool {
    wait_for(|| !pins.test_ids().contains(&id))
}

/// The pin's picture with corners rounded at 8 DIPs on a plain backdrop, as RGB.
fn save_picture(folder: &Path, name: &str, picture: &Pixmap, dpi: u32, dark: bool) -> Result<bool> {
    let pad = (24 * dpi / 96) as i32;
    let (w, h) = (picture.width() as i32 + 2 * pad, picture.height() as i32 + 2 * pad);
    let mut canvas = Pixmap::new(w as u32, h as u32).ok_or("canvas")?;
    canvas.fill(if dark { Color::from_rgba8(0x1f, 0x23, 0x29, 255) } else { Color::from_rgba8(0xf3, 0xf4, 0xf6, 255) });
    let radius = 8.0 * dpi as f32 / 96.0;
    let rect = tiny_skia::Rect::from_xywh(pad as f32, pad as f32, picture.width() as f32, picture.height() as f32)
        .ok_or("rect")?;
    let mut mask = Mask::new(w as u32, h as u32).ok_or("mask")?;
    let mut path = PathBuilder::new();
    path.push_rect(rect);
    let path = rounded(rect, radius).or_else(|| path.finish()).ok_or("path")?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    canvas.draw_pixmap(pad, pad, picture.as_ref(), &PixmapPaint::default(), Transform::identity(), Some(&mask));
    let mut outline = Paint::default();
    outline.set_color(Color::from_rgba8(28, 38, 58, 36));
    outline.anti_alias = true;
    canvas.stroke_path(&path, &outline, &tiny_skia::Stroke::default(), Transform::identity(), None);
    let rgb: Vec<u8> = canvas.data().as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut encoder = png::Encoder::new(File::create(folder.join(name))?, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgb)?;
    let pixels = rgb.as_chunks::<3>().0;
    Ok(pixels.iter().filter(|pixel| **pixel != pixels[0]).count() > 200)
}

fn rounded(rect: tiny_skia::Rect, r: f32) -> Option<tiny_skia::Path> {
    let (l, t, rt, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let k = r * 0.447_715;
    let mut p = PathBuilder::new();
    p.move_to(l + r, t);
    p.line_to(rt - r, t);
    p.cubic_to(rt - k, t, rt, t + k, rt, t + r);
    p.line_to(rt, b - r);
    p.cubic_to(rt, b - k, rt - k, b, rt - r, b);
    p.line_to(l + r, b);
    p.cubic_to(l + k, b, l, b - k, l, b - r);
    p.line_to(l, t + r);
    p.cubic_to(l, t + k, l + k, t, l + r, t);
    p.close();
    p.finish()
}

fn picture(pins: &Pins, id: u64) -> Option<Pixmap> {
    pins.test_with(id, |pin| pin.picture()).flatten()
}

/// The pin and the pin hovered with its close button lit, at one DPI and theme.
fn pictures(checks: &mut Checks, folder: &Path, pins: &Pins, id: u64, dpi: u32, dark: bool, tag: &str) -> Result<()> {
    pins.test_with(id, |pin| pin.hover(false, false));
    let plain = picture(pins, id);
    let saved = match &plain {
        Some(p) => save_picture(folder, &format!("pin-{tag}.png"), p, dpi, dark)?,
        None => false,
    };
    checks.check(&format!("picture pin-{tag}.png"), saved);
    pins.test_with(id, |pin| pin.hover(true, true));
    let hovered = picture(pins, id);
    let saved = match &hovered {
        Some(p) => save_picture(folder, &format!("pin-hover-{tag}.png"), p, dpi, dark)?,
        None => false,
    };
    checks.check(&format!("picture pin-hover-{tag}.png"), saved);
    checks.check(
        &format!("{tag}: the close button shows only while hovered"),
        matches!((&plain, &hovered), (Some(a), Some(b)) if a.data() != b.data()),
    );
    pins.test_with(id, |pin| pin.hover(false, false));
    Ok(())
}

fn run() -> Result<bool> {
    ensure_dpi_awareness();
    let mut checks = Checks { ok: true };
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let at = match test_monitor::test_monitor_point() {
        Some((x, y)) => Point::new(f64::from(x), f64::from(y)),
        None => {
            let work = monitor_at(cursor_pos()).work_area;
            Point::new(work.mid_x(), work.mid_y())
        }
    };
    let work = monitor_at(at).work_area;
    // SAFETY: plain query.
    let foreground = unsafe { GetForegroundWindow() };

    // Light, 100%, no motion.
    let options = TestOptions { dpi: Some(96), motion: Some(false), dark: Some(false), no_activate: true };
    let pins = Pins::start(None, Some(options))?;
    let image = demo_image(1600, 1000, [59, 130, 246]);
    let id = pins.pin(card(1, image.clone(), false), at);
    checks.check("a pin opens", wait_for(|| pins.test_ids().contains(&id)) && pins.count() == 1);
    let hwnd = hwnd_of(&pins, id).ok_or("no pin window")?;
    let expected = pin_frame(at, pin_size(natural_size((1600, 1000), 1.0, 1.0), work, MIN_SIDE), work);
    let frame = frame_of(&pins, id).unwrap_or_default();
    checks.check(
        &format!("it is centred where it was pinned, at most 40% of the work area ({frame:?}, expected {expected:?})"),
        frame == expected && frame.width <= work.width * 0.4 + 1.0 && frame.height <= work.height * 0.4 + 1.0,
    );
    // SAFETY: plain queries about our own window.
    let (ex, visible, affinity) = unsafe {
        let mut affinity = 0u32;
        let _ = GetWindowDisplayAffinity(hwnd, &mut affinity);
        (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32, IsWindowVisible(hwnd).as_bool(), affinity)
    };
    checks.check(
        "it is a visible topmost tool window (no taskbar button) that screenshots include",
        visible
            && ex & WS_EX_TOPMOST.0 != 0
            && ex & WS_EX_TOOLWINDOW.0 != 0
            && ex & WS_EX_APPWINDOW.0 == 0
            && affinity == WDA_NONE.0,
    );
    // SAFETY: plain query.
    let front = unsafe { GetForegroundWindow() };
    checks.check("it did not take the foreground", front != hwnd);
    if front != foreground {
        println!("INFO the foreground window changed meanwhile (the person at the desktop may have switched)");
    }
    pictures(&mut checks, &folder, &pins, id, 96, false, "light-100")?;

    // Drag anywhere to move it.
    // Each message carries the pointer in the window's coordinates at that moment, as
    // Windows would send them while the window follows the pointer.
    let before = frame_of(&pins, id).unwrap_or_default();
    let pointer = |pins: &Pins, (x, y): (f64, f64)| {
        let now = frame_of(pins, id).unwrap_or_default();
        lparam((before.x + x - now.x) as i32, (before.y + y - now.y) as i32)
    };
    send(hwnd, WM_LBUTTONDOWN, 1, pointer(&pins, (60.0, 60.0)));
    send(hwnd, WM_MOUSEMOVE, 1, pointer(&pins, (110.0, 80.0)));
    send(hwnd, WM_MOUSEMOVE, 1, pointer(&pins, (160.0, 100.0)));
    send(hwnd, WM_LBUTTONUP, 0, pointer(&pins, (160.0, 100.0)));
    let moved = frame_of(&pins, id).unwrap_or_default();
    checks.check(
        &format!("a drag moves it by the pointer's way ({before:?} -> {moved:?})"),
        moved == Rect::new(before.x + 100.0, before.y + 40.0, before.width, before.height),
    );

    // The wheel resizes it around the cursor; Ctrl+wheel changes its opacity.
    let cursor = Point::new((moved.x + moved.width / 4.0).round(), (moved.y + moved.height / 4.0).round());
    let bounds = monitor_bounds_at(cursor).bounds;
    let settled = |pins: &Pins| wait_for(|| pins.test_with(id, |pin| pin.zooming()) == Some(false));
    send(hwnd, WM_MOUSEWHEEL, 120 << 16, lparam(cursor.x as i32, cursor.y as i32));
    settled(&pins);
    let grown = frame_of(&pins, id).unwrap_or_default();
    let want = wheel_frame(moved, cursor, 1.0, MIN_SIDE, (bounds.width, bounds.height));
    checks.check(&format!("a wheel notch grows it around the cursor ({grown:?}, expected {want:?})"), grown == want);
    for _ in 0..60 {
        send(hwnd, WM_MOUSEWHEEL, (-120i16 as u16 as usize) << 16, lparam(cursor.x as i32, cursor.y as i32));
    }
    settled(&pins);
    let small = frame_of(&pins, id).unwrap_or_default();
    checks.check(
        &format!("turning it down stops at the minimum side ({small:?})"),
        (small.width.min(small.height) - MIN_SIDE).abs() <= 1.0,
    );
    send(hwnd, WM_MOUSEWHEEL, ((-240i16 as u16 as usize) << 16) | 0x0008, lparam(cursor.x as i32, cursor.y as i32));
    // SAFETY: plain query about our own window.
    let alpha = unsafe {
        let mut alpha = 0u8;
        let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
        let _ = GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags));
        alpha
    };
    checks.check(
        &format!("Ctrl+wheel makes it see-through (opacity {:?}, alpha {alpha})", pins.test_with(id, |pin| pin.opacity())),
        pins.test_with(id, |pin| pin.opacity()) == Some(0.8) && alpha == 204,
    );

    // A double-click asks to carry it again; the app takes it back.
    send(hwnd, WM_LBUTTONDBLCLK, 1, lparam(30, 30));
    let events = events_within(&pins, Duration::from_secs(1));
    let asked = matches!(events.as_slice(), [Event::Carry { pin, double_click: true, card }] if *pin == id && Arc::ptr_eq(&card.image, &image));
    checks.check("a double-click asks to carry the card again, the pin's own card", asked);
    checks.check("the pin stays until the app takes it", pins.test_ids().contains(&id));
    pins.take(id);
    checks.check("taken, the pin closes and frees its slot", gone(&pins, id) && wait_for(|| pins.count() == 0));

    // The menu's Carry again and Close.
    let id = pins.pin(card(2, image.clone(), false), at);
    wait_for(|| pins.test_ids().contains(&id));
    let hwnd = hwnd_of(&pins, id).ok_or("no pin window")?;
    send(hwnd, WM_COMMAND, MENU_CARRY, LPARAM(0));
    let events = events_within(&pins, Duration::from_secs(1));
    checks.check(
        "the menu's Carry again asks too",
        matches!(events.as_slice(), [Event::Carry { pin, double_click: false, .. }] if *pin == id),
    );
    send(hwnd, WM_COMMAND, MENU_CLOSE, LPARAM(0));
    checks.check("the menu's Close closes it", gone(&pins, id));

    // The close button, then Esc.
    let id = pins.pin(card(3, image.clone(), false), at);
    wait_for(|| pins.test_ids().contains(&id));
    let hwnd = hwnd_of(&pins, id).ok_or("no pin window")?;
    let width = frame_of(&pins, id).unwrap_or_default().width as i32;
    let button = pins::close_rect(width, 96);
    let (bx, by) = ((button.left + button.right) / 2, (button.top + button.bottom) / 2);
    send(hwnd, WM_LBUTTONDOWN, 1, lparam(bx, by));
    send(hwnd, WM_LBUTTONUP, 0, lparam(bx, by));
    checks.check("a click on the close button closes it", gone(&pins, id));
    let id = pins.pin(card(4, image.clone(), false), at);
    wait_for(|| pins.test_ids().contains(&id));
    let hwnd = hwnd_of(&pins, id).ok_or("no pin window")?;
    send(hwnd, WM_KEYDOWN, usize::from(VK_ESCAPE.0), LPARAM(0));
    checks.check("Esc closes it", gone(&pins, id) && wait_for(|| pins.count() == 0));

    // A recording shows its first frame with the duration badge.
    let poster = demo_image(1280, 720, [139, 92, 246]);
    let id = pins.pin(card(5, poster, true), at);
    wait_for(|| pins.test_ids().contains(&id));
    // The pointer may rest over it on the test monitor: the picture is taken without hover.
    pins.test_with(id, |pin| pin.hover(false, false));
    let saved = match picture(&pins, id) {
        Some(p) => save_picture(&folder, "pin-movie-light-100.png", &p, 96, false)?,
        None => false,
    };
    checks.check("picture pin-movie-light-100.png", saved);
    let several = pins.pin(card(6, image.clone(), false), Point::new(at.x - 200.0, at.y - 150.0));
    checks.check("several pins can be on screen", wait_for(|| pins.test_ids() == vec![id, several]));
    checks.check("the pins thread stops with its pins", pins.shutdown(Duration::from_secs(1)));

    // Dark, 125%.
    let options = TestOptions { dpi: Some(120), motion: Some(false), dark: Some(true), no_activate: true };
    let pins = Pins::start(None, Some(options))?;
    let id = pins.pin(card(7, demo_image(1600, 1000, [16, 185, 129]), false), at);
    wait_for(|| pins.test_ids().contains(&id));
    let expected = pin_frame(at, pin_size(natural_size((1600, 1000), 1.0, 1.25), work, MIN_SIDE * 1.25), work);
    checks.check(
        "at 125% it is sized for the scale",
        frame_of(&pins, id) == Some(expected) && pins.test_with(id, |pin| pin.dpi()) == Some(120),
    );
    pictures(&mut checks, &folder, &pins, id, 120, true, "dark-125")?;
    checks.check("the dark pins thread stops", pins.shutdown(Duration::from_secs(1)));

    // With motion: it grows in and ends at its frame.
    let options = TestOptions { dpi: Some(96), motion: Some(true), dark: Some(false), no_activate: true };
    let pins = Pins::start(None, Some(options))?;
    let id = pins.pin(card(8, demo_image(800, 500, [245, 158, 11]), false), at);
    wait_for(|| pins.test_ids().contains(&id));
    let expected = pin_frame(at, pin_size(natural_size((800, 500), 1.0, 1.0), work, MIN_SIDE), work);
    let mut growing = Vec::new();
    let settled = wait_for(|| {
        growing.extend(frame_of(&pins, id).filter(|frame| *frame != expected));
        pins.test_with(id, |pin| pin.still()) == Some(true)
    });
    checks.check(
        &format!("it grows in ({} frames on the way, first {:?}) and settles at its frame", growing.len(), growing.first()),
        !growing.is_empty() && settled && frame_of(&pins, id) == Some(expected),
    );
    // With motion the wheel zooms smoothly. Quick notches add up and end where the instant
    // steps did.
    let hwnd = hwnd_of(&pins, id).ok_or("no pin window")?;
    pins.test_with(id, |pin| pin.hover(false, false));
    let start = frame_of(&pins, id).unwrap_or_default();
    let cursor = Point::new((start.x + start.width / 3.0).round(), (start.y + start.height / 3.0).round());
    let bounds = monitor_bounds_at(cursor).bounds;
    let mut want = start;
    let mut on_the_way = Vec::new();
    pins.test_with(id, |pin| pin.paint_stats());
    let began = Instant::now();
    for _ in 0..4 {
        send(hwnd, WM_MOUSEWHEEL, 120 << 16, lparam(cursor.x as i32, cursor.y as i32));
        want = wheel_frame(want, cursor, 1.0, MIN_SIDE, (bounds.width, bounds.height));
        let until = Instant::now() + Duration::from_millis(25);
        while Instant::now() < until {
            on_the_way.extend(frame_of(&pins, id));
            thread::sleep(Duration::from_millis(2));
        }
    }
    let settled = wait_for(|| {
        on_the_way.extend(frame_of(&pins, id));
        pins.test_with(id, |pin| pin.zooming()) == Some(false)
    });
    let took = began.elapsed().as_secs_f64();
    let (paints, total, longest) = pins.test_with(id, |pin| pin.paint_stats()).unwrap_or_default();
    println!(
        "INFO four notches: {paints} paints in {:.0} ms ({:.0} a second), {:.2} ms each on average, {longest:.2} ms at most",
        took * 1000.0,
        f64::from(paints) / took,
        total / f64::from(paints.max(1)),
    );
    on_the_way.dedup();
    let end = frame_of(&pins, id).unwrap_or_default();
    checks.check(
        &format!("four quick notches zoom smoothly ({} frames on the way) and end where the instant steps did ({end:?}, expected {want:?})", on_the_way.len()),
        settled && end == want && on_the_way.len() >= 3,
    );
    checks.check(
        "after a zoom the picture is made sharp at the new size",
        wait_for(|| pins.test_with(id, |pin| pin.sharp()) == Some(true)),
    );

    // A frame in the middle of a zoom is painted all over at its size. Growing, so a window
    // not painted yet would show a black strip.
    send(hwnd, WM_MOUSEWHEEL, 240 << 16, lparam(cursor.x as i32, cursor.y as i32));
    let target = [1.0, 1.0].iter().fold(end, |frame, notches| {
        wheel_frame(frame, cursor, *notches, MIN_SIDE, (bounds.width, bounds.height))
    });
    let mut middle = None;
    wait_for(|| {
        let seen = pins.test_with(id, |pin| {
            // Right after a frame, before anything else can paint the window.
            let moving = pin.zoom_frame();
            let frame = pin.frame();
            (moving, frame, window_pixels(pin.hwnd(), frame.width as i32, frame.height as i32))
        });
        if let Some((true, frame, pixels)) = seen
            && frame != end
            && frame != target
        {
            middle = Some((frame, pixels));
        }
        middle.is_some()
    });
    let gaps = middle.as_ref().map(|(_, pixels)| {
        pixels.as_deref().map_or(usize::MAX, |pixels| {
            pixels.as_chunks::<4>().0.iter().filter(|p| p[..3] == [0, 0, 0] || p[..3] == [255, 255, 255]).count()
        })
    });
    checks.check(
        &format!("a frame mid-zoom ({:?}) has no black or white gaps ({gaps:?} such pixels)", middle.as_ref().map(|(frame, _)| frame)),
        gaps == Some(0),
    );
    wait_for(|| pins.test_with(id, |pin| pin.zooming()) == Some(false));

    // A press during a zoom stops it where it is.
    let before = frame_of(&pins, id).unwrap_or_default();
    send(hwnd, WM_MOUSEWHEEL, (-360i16 as u16 as usize) << 16, lparam(cursor.x as i32, cursor.y as i32));
    wait_for(|| frame_of(&pins, id).is_some_and(|frame| frame != before));
    send(hwnd, WM_LBUTTONDOWN, 1, lparam(20, 20));
    let stopped = pins.test_with(id, |pin| pin.zooming()) == Some(false);
    let held = frame_of(&pins, id).unwrap_or_default();
    thread::sleep(Duration::from_millis(150));
    let later = frame_of(&pins, id).unwrap_or_default();
    send(hwnd, WM_LBUTTONUP, 0, lparam(20, 20));
    checks.check(
        &format!("a press during a zoom stops it where it is ({held:?}, then {later:?})"),
        stopped && held == later && held != before,
    );

    // Paint time while zooming a big card: eight quick notches up, then eight down.
    let big = pins.pin(card(9, demo_image(2560, 1440, [59, 130, 246]), false), at);
    wait_for(|| pins.test_ids().contains(&big) && pins.test_with(big, |pin| pin.still()) == Some(true));
    let big_hwnd = hwnd_of(&pins, big).ok_or("no pin window")?;
    pins.test_with(big, |pin| pin.hover(false, false));
    let start = frame_of(&pins, big).unwrap_or_default();
    let middle = lparam((start.x + start.width / 2.0) as i32, (start.y + start.height / 2.0) as i32);
    pins.test_with(big, |pin| pin.paint_stats());
    let began = Instant::now();
    for notch in [120i16, -120] {
        for _ in 0..8 {
            send(big_hwnd, WM_MOUSEWHEEL, (notch as u16 as usize) << 16, middle);
            thread::sleep(Duration::from_millis(40));
        }
        wait_for(|| pins.test_with(big, |pin| pin.zooming() || !pin.sharp()) == Some(false));
    }
    let took = began.elapsed().as_secs_f64();
    let (count, total, longest) = pins.test_with(big, |pin| pin.paint_stats()).unwrap_or_default();
    println!(
        "INFO zooming a 2560x1440 card from {start:?}: {count} paints in {took:.2} s, {:.2} ms each on average, {longest:.2} ms at most",
        total / f64::from(count.max(1))
    );
    send(big_hwnd, WM_COMMAND, MENU_CLOSE, LPARAM(0));
    gone(&pins, big);

    send(hwnd, WM_COMMAND, MENU_CLOSE, LPARAM(0));
    checks.check("with motion it fades out and closes", gone(&pins, id));
    checks.check("the last pins thread stops", pins.shutdown(Duration::from_secs(1)));

    println!("{}", if checks.ok { "ALL PASS" } else { "SOME FAILED" });
    Ok(checks.ok)
}
