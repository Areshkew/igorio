//! Exercises only our guide window: renders every page to PNG at light 100% and dark 125%
//! (and one "Done" state), checks that the buttons are where they are painted, and that the
//! keys move pages. No foreground activation or synthetic system input: key messages are
//! posted to our own test window only. With `IGORIO_TEST_MONITOR=secondary` the windows open
//! on a monitor other than the primary one.
#[path = "common/test_monitor.rs"]
mod test_monitor;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::slice;
use std::thread;
use std::time::{Duration, Instant};

use igorio_app::guide::{
    BACK_ID, DOT_ID, Features, GuideWindow, NEXT_ID, Page, TestMode, Tried, pages,
    test_client_size, test_control_rect,
};
use igorio_app::overlay::ensure_dpi_awareness;
use igorio_app::settings::Settings;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{VIRTUAL_KEY, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT};
use windows::Win32::UI::WindowsAndMessaging::*;

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

/// A captured client area, RGB.
struct Shot {
    width: i32,
    height: i32,
    rgb: Vec<u8>,
}

impl Shot {
    fn at(&self, x: i32, y: i32) -> [u8; 3] {
        let i = ((y * self.width + x) * 3) as usize;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

fn open(guide: &mut GuideWindow, settings: &Settings) -> Result<HWND> {
    guide.open(settings, false)?;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if let Some(hwnd) = guide.test_hwnd() {
            // SAFETY: this HWND was published by our test window only after setup.
            if unsafe { IsWindowVisible(hwnd).as_bool() } {
                return Ok(hwnd);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err("guide window did not become visible".into())
}

/// Posts a key press to our own test window, as its message loop would get it.
fn key(hwnd: HWND, vk: VIRTUAL_KEY) {
    // SAFETY: a posted window message to the self-test's own window only.
    let _ = unsafe { PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(vk.0 as usize), LPARAM(1)) };
}

/// Waits for the posted messages to be handled.
fn page_after(guide: &GuideWindow, expected: usize) -> Option<usize> {
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        let page = guide.test_page();
        if page == Some(expected) || Instant::now() > until {
            return page;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn capture(hwnd: HWND, path: &Path) -> Result<Shot> {
    // SAFETY: wait for the compositor to consume the frozen client paint before PrintWindow.
    let _ = unsafe { DwmFlush() };
    let mut client = RECT::default();
    // SAFETY: this is our visible test HWND and client is writable.
    unsafe { GetClientRect(hwnd, &mut client)? };
    let (w, h) = (client.right, client.bottom);
    let mut bits = std::ptr::null_mut();
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: BITMAPINFO defines a 32-bit top-down DIB; bits is a writable out pointer.
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    // SAFETY: a private memory DC; its original bitmap is restored before deletion.
    unsafe {
        let dc = CreateCompatibleDC(None);
        let previous = SelectObject(dc, bitmap.into());
        const PW_RENDERFULLCONTENT: u32 = 2;
        let _ = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT));
        let _ = GdiFlush();
        SelectObject(dc, previous);
        let _ = DeleteDC(dc);
    }
    // SAFETY: the DIB is top-down, 32-bit, tightly packed, and alive through the copy.
    let rgb: Vec<u8> = unsafe { slice::from_raw_parts(bits.cast::<u8>(), (w * h * 4) as usize) }
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
        .collect();
    // SAFETY: the bitmap is no longer selected into any DC.
    let _ = unsafe { DeleteObject(bitmap.into()) };
    let mut encoder = png::Encoder::new(File::create(path)?, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgb)?;
    Ok(Shot { width: w, height: h, rgb })
}

fn child_rect(hwnd: HWND, id: i32) -> Option<RECT> {
    // SAFETY: a same-process child; its screen rectangle is mapped into the parent's client.
    unsafe {
        let child = GetDlgItem(Some(hwnd), id).ok()?;
        if !IsWindowVisible(child).as_bool() {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(child, &mut rect).ok()?;
        let mut points = [POINT { x: rect.left, y: rect.top }, POINT { x: rect.right, y: rect.bottom }];
        MapWindowPoints(None, Some(hwnd), &mut points);
        Some(RECT { left: points[0].x, top: points[0].y, right: points[1].x, bottom: points[1].y })
    }
}

fn name(page: Page) -> &'static str {
    match page {
        Page::Welcome => "welcome",
        Page::Screenshot => "screenshot",
        Page::Drop => "drop",
        Page::Shake => "shake",
        Page::ToolMenu => "toolmenu",
        Page::Record => "record",
    }
}

/// Every control sits where the layout says, and its middle is painted while the paper
/// around it is not.
fn buttons_where_painted(checks: &mut Checks, hwnd: HWND, shot: &Shot, count: usize, page: usize, dpi: u32, label: &str) {
    let paper = shot.at(2, 2);
    let mut ids = vec![NEXT_ID];
    if page > 0 {
        ids.push(BACK_ID);
    }
    ids.extend((0..count as i32).map(|i| DOT_ID + i));
    for id in ids {
        let expected = test_control_rect(id, count, dpi);
        let actual = child_rect(hwnd, id);
        checks.check(&format!("{label}: control {id} at its layout rectangle"), actual == Some(expected));
        // Beside the label of a button (its text can be the paper color), the middle of a dot.
        let inside_x = if id >= DOT_ID { (expected.left + expected.right) / 2 } else { expected.left + 8 };
        let middle = shot.at(inside_x, (expected.top + expected.bottom) / 2);
        // The dots leave paper between them; the buttons fill most of their box.
        let outside = if id >= DOT_ID {
            shot.at(expected.left, expected.top + 2)
        } else {
            shot.at(expected.left - 4, (expected.top + expected.bottom) / 2)
        };
        checks.check(
            &format!("{label}: control {id} is painted inside its rectangle, paper outside"),
            middle != paper && outside == paper,
        );
    }
    if page == 0 {
        checks.check(&format!("{label}: no Back on the first page"), child_rect(hwnd, BACK_ID).is_none());
    }
}

fn run(out: &Path) -> Result<bool> {
    let mut checks = Checks { ok: true };
    let settings = Settings::default();
    let list = pages(Features::of(&settings));
    let at = test_monitor::test_monitor_point();
    println!("INFO test windows open {}", at.map_or("on the cursor's monitor".into(), |(x, y)| format!("around ({x}, {y})")));
    let count = list.len();
    checks.check(&format!("{count} pages with every capability on"), count >= 4);

    for (dark, dpi, suffix) in [(false, 96, "light-100"), (true, 120, "dark-125")] {
        let mut guide = GuideWindow::for_test(TestMode { dark, dpi, motion: false, at });
        let hwnd = open(&mut guide, &settings)?;
        let mut client = RECT::default();
        // SAFETY: our own test window.
        unsafe { GetClientRect(hwnd, &mut client)? };
        checks.check(&format!("{suffix}: client size"), (client.right, client.bottom) == test_client_size(dpi));
        if let Some((x, y)) = at {
            // SAFETY: monitor lookups for our own window and a point; no pointers kept.
            let same = unsafe {
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) == MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL)
            };
            checks.check(&format!("{suffix}: opens on the test monitor"), same);
        }
        for (index, &page) in list.iter().enumerate() {
            if index > 0 {
                key(hwnd, VK_RIGHT);
                checks.check(&format!("{suffix}: Right moves to page {index}"), page_after(&guide, index) == Some(index));
            }
            guide.test_freeze(None);
            let path = out.join(format!("guide-{}-{suffix}.png", name(page)));
            let shot = capture(hwnd, &path)?;
            println!("INFO wrote {}", path.display());
            let varied = shot.rgb.as_chunks::<3>().0.iter().filter(|p| **p != shot.at(0, 0)).count() > 1000;
            checks.check(&format!("{suffix}: {} page is drawn", name(page)), varied && shot.height > 0);
            buttons_where_painted(&mut checks, hwnd, &shot, count, index, dpi, &format!("{suffix} {}", name(page)));
        }
        key(hwnd, VK_RIGHT);
        checks.check(&format!("{suffix}: Right on the last page stays"), page_after(&guide, count - 1) == Some(count - 1));
        key(hwnd, VK_LEFT);
        checks.check(&format!("{suffix}: Left goes back"), page_after(&guide, count - 2) == Some(count - 2));
        if !dark {
            // A dot jumps straight to its page.
            if let Some(dot) = guide.test_hwnd().and_then(|hwnd| {
                // SAFETY: a child of our own test window.
                unsafe { GetDlgItem(Some(hwnd), DOT_ID + 1) }.ok()
            }) {
                // SAFETY: the same notification a click sends, to our own window.
                unsafe {
                    SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(DOT_ID as usize + 1)), Some(LPARAM(dot.0 as isize)));
                }
            }
            checks.check("a dot opens its page", page_after(&guide, 1) == Some(1));
            // The screenshot page's try line turns into Done when a card lands.
            let before = capture(hwnd, &out.join("guide-screenshot-light-100.png"))?;
            guide.tried(Tried::CardLanded);
            thread::sleep(Duration::from_millis(100));
            guide.test_freeze(None);
            let path = out.join("guide-screenshot-done-light-100.png");
            let after = capture(hwnd, &path)?;
            println!("INFO wrote {}", path.display());
            checks.check("a landed card turns Try it now into Done", before.rgb != after.rgb);
            // Enter on the last page is Done: the window closes.
            key(hwnd, VK_RIGHT);
            for _ in 0..count {
                key(hwnd, VK_RIGHT);
            }
            page_after(&guide, count - 1);
            key(hwnd, VK_RETURN);
        } else {
            key(hwnd, VK_ESCAPE);
        }
        let until = Instant::now() + Duration::from_secs(2);
        // SAFETY: a same-process HWND; this checks that it was destroyed.
        while unsafe { IsWindow(Some(hwnd)) }.as_bool() && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        let gone = !unsafe { IsWindow(Some(hwnd)) }.as_bool();
        checks.check(&format!("{suffix}: {} closes the guide", if dark { "Esc" } else { "Enter on Done" }), gone);
        checks.check(&format!("{suffix}: the thread ends"), guide.close());
    }

    // With motion on the scene plays only on a page that has one.
    let mut guide = GuideWindow::for_test(TestMode { dark: false, dpi: 96, motion: true, at });
    let hwnd = open(&mut guide, &settings)?;
    thread::sleep(Duration::from_millis(400));
    checks.check("motion on: the welcome page goes idle after its fade-in", guide.test_idle());
    key(hwnd, VK_RIGHT);
    page_after(&guide, 1);
    thread::sleep(Duration::from_millis(400));
    checks.check("motion on: the screenshot scene keeps ticking", !guide.test_idle());
    checks.check("motion on: the thread ends", guide.close());
    Ok(checks.ok)
}

fn main() -> Result<()> {
    ensure_dpi_awareness();
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    match run(&out) {
        Ok(true) => Ok(()),
        Ok(false) => Err("guide self-test failed".into()),
        Err(error) => {
            println!("FAIL {error}");
            Err(error)
        }
    }
}
