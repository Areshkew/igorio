//! Non-interactive tray self-test. Adds the icon for a moment, checks it, inspects a
//! built menu without showing it, writes enlarged logo PNGs to target/tray-selftest/,
//! and removes the icon. Prints PASS / FAIL / INFO per check; exits 0 only if every
//! hard check passes.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::time::{Duration, Instant};

use igorio_app::tray::{MARK_BLUE, MARK_BLUE_DARK, MenuItem, PopupMenu, TRAY_UID, Tray, logo_pixmap};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMenuItemCount, GetMenuItemInfoW, HMENU, IsWindow, MENUITEMINFOW,
    MFS_CHECKED, MFS_DISABLED, MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
    MIIM_SUBMENU, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            self.failed += 1;
        }
    }
}

fn main() {
    let mut report = Report { failed: 0 };

    // 1. Add the icon.
    let tray = match Tray::new(
        "Igorio tray self-test",
        Box::new(Vec::new),
        Box::new(|_| {}),
    ) {
        Ok(tray) => tray,
        Err(e) => {
            report.check(
                false,
                &format!("Shell_NotifyIconW NIM_ADD + NIM_SETVERSION: {e}"),
            );
            std::process::exit(1);
        }
    };
    report.check(
        true,
        "Shell_NotifyIconW NIM_ADD + NIM_SETVERSION (NOTIFYICON_VERSION_4)",
    );
    pump(Duration::from_millis(400));

    // 2. Where is it? Soft check: an icon in the overflow area may have no rect.
    match tray.icon_rect() {
        Ok(r) if r.right > r.left && r.bottom > r.top => println!(
            "PASS Shell_NotifyIconGetRect: ({}, {}) - ({}, {}), {}x{} px",
            r.left,
            r.top,
            r.right,
            r.bottom,
            r.right - r.left,
            r.bottom - r.top
        ),
        Ok(r) => println!(
            "INFO Shell_NotifyIconGetRect returned an empty rect {r:?} (icon probably in the overflow area)"
        ),
        Err(e) => println!(
            "INFO Shell_NotifyIconGetRect failed: {e} (icon probably in the overflow area; not a hard failure)"
        ),
    }

    // 3. Tooltip change, including one longer than the shell's 127-unit limit.
    let mut tray = tray;
    report.check(
        tray.set_tooltip("Igorio: 3 screenshots held").is_ok(),
        "set_tooltip (short)",
    );
    report.check(
        tray.set_tooltip(&"long tooltip ".repeat(20)).is_ok(),
        "set_tooltip (cut to 127 units)",
    );

    // 4. Menu structure, read back through GetMenuItemInfoW without showing it.
    check_menu(&mut report);

    // 5. Logo PNGs.
    write_logos(&mut report);

    // 6. Remove the icon.
    let hwnd = tray.hwnd();
    drop(tray);
    // SAFETY: IsWindow accepts any handle value, including destroyed ones.
    let window_gone = !unsafe { IsWindow(Some(hwnd)) }.as_bool();
    report.check(window_gone, "Drop destroyed the hidden window");
    report.check(
        icon_gone(hwnd),
        "Drop removed the icon (Shell_NotifyIconGetRect now fails)",
    );

    println!("{} hard check(s) failed", report.failed);
    std::process::exit(if report.failed == 0 { 0 } else { 1 });
}

fn pump(duration: Duration) {
    let end = Instant::now() + duration;
    let mut msg = MSG::default();
    while Instant::now() < end {
        // SAFETY: `msg` is a live MSG; this thread owns the tray window.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: `msg` was filled by PeekMessageW.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn icon_gone(hwnd: HWND) -> bool {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    // SAFETY: `id` is fully initialised and outlives the call.
    unsafe { Shell_NotifyIconGetRect(&id) }.is_err()
}

fn test_menu() -> Vec<MenuItem> {
    vec![
        MenuItem::Label("Holding 2 screenshots".into()),
        MenuItem::action(100, "Discard Top Screenshot"),
        MenuItem::Action {
            id: 101,
            label: "Restore Last Screenshot".into(),
            enabled: false,
            checked: false,
        },
        MenuItem::Separator,
        MenuItem::Submenu {
            label: "Recent Notes".into(),
            items: vec![
                MenuItem::action(200, "Groceries & errands"),
                MenuItem::Submenu {
                    label: "Older".into(),
                    items: vec![MenuItem::Action {
                        id: 300,
                        label: "Pinned".into(),
                        enabled: true,
                        checked: true,
                    }],
                },
            ],
        },
        MenuItem::Action {
            id: 102,
            label: "Keep Every Screenshot".into(),
            enabled: true,
            checked: true,
        },
        MenuItem::Separator,
        MenuItem::action(u32::MAX, "Quit"),
    ]
}

/// One item read back from a Win32 menu.
struct Read {
    separator: bool,
    disabled: bool,
    checked: bool,
    command: u32,
    submenu: Option<HMENU>,
    text: String,
}

fn read_item(hmenu: HMENU, pos: u32) -> Option<Read> {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_STRING,
        ..Default::default()
    };
    // SAFETY: `info` is live; with a null dwTypeData the call only reports the length.
    unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut info) }.ok()?;
    let mut buf = vec![0u16; info.cch as usize + 1];
    info.dwTypeData = windows::core::PWSTR(buf.as_mut_ptr());
    info.cch = buf.len() as u32;
    // SAFETY: `buf` holds `cch` units and outlives the call.
    unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut info) }.ok()?;
    let len = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    Some(Read {
        separator: info.fType.0 & MFT_SEPARATOR.0 != 0,
        disabled: info.fState.0 & MFS_DISABLED.0 != 0,
        checked: info.fState.0 & MFS_CHECKED.0 != 0,
        command: info.wID,
        submenu: (!info.hSubMenu.is_invalid()).then_some(info.hSubMenu),
        text: String::from_utf16_lossy(&buf[..len]).replace("&&", "&"),
    })
}

/// Compares a Win32 menu with the items it was built from. Returns a list of problems.
fn compare(
    menu: &PopupMenu,
    hmenu: HMENU,
    items: &[MenuItem],
    path: &str,
    problems: &mut Vec<String>,
) {
    // SAFETY: `hmenu` is a live menu owned by `menu`.
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    if count != items.len() as i32 {
        problems.push(format!("{path}: {count} items, expected {}", items.len()));
        return;
    }
    for (pos, item) in items.iter().enumerate() {
        let at = format!("{path}[{pos}]");
        let Some(read) = read_item(hmenu, pos as u32) else {
            problems.push(format!("{at}: GetMenuItemInfoW failed"));
            continue;
        };
        let mut expect = |ok: bool, what: &str| {
            if !ok {
                problems.push(format!("{at}: {what}"));
            }
        };
        match item {
            MenuItem::Action {
                id,
                label,
                enabled,
                checked,
            } => {
                expect(
                    read.text == *label,
                    &format!("label {:?}, expected {label:?}", read.text),
                );
                expect(read.disabled == !enabled, "enabled state");
                expect(read.checked == *checked, "checked state");
                expect(read.submenu.is_none(), "unexpected submenu");
                expect(
                    menu.action_id(read.command) == Some(*id),
                    &format!("command {} does not map to {id}", read.command),
                );
            }
            MenuItem::Submenu { label, items } => {
                expect(
                    read.text == *label,
                    &format!("label {:?}, expected {label:?}", read.text),
                );
                match read.submenu {
                    Some(sub) => compare(menu, sub, items, &at, problems),
                    None => expect(false, "missing submenu"),
                }
            }
            MenuItem::Label(label) => {
                expect(
                    read.text == *label,
                    &format!("label {:?}, expected {label:?}", read.text),
                );
                expect(read.disabled, "label is not greyed");
                expect(
                    menu.action_id(read.command).is_none(),
                    "label maps to an action",
                );
            }
            MenuItem::Separator => expect(read.separator, "not a separator"),
        }
    }
}

fn check_menu(report: &mut Report) {
    let items = test_menu();
    let menu = match PopupMenu::build(&items) {
        Ok(menu) => menu,
        Err(e) => {
            report.check(false, &format!("PopupMenu::build: {e}"));
            return;
        }
    };
    report.check(true, "PopupMenu::build (nested test menu)");
    let mut problems = Vec::new();
    compare(&menu, menu.handle(), &items, "menu", &mut problems);
    for p in &problems {
        println!("     {p}");
    }
    report.check(
        problems.is_empty(),
        "menu read back via GetMenuItemInfoW: counts, labels (with &), enabled, checked, submenus, ids",
    );
    report.check(
        menu.action_id(0).is_none(),
        "command 0 (menu dismissed) maps to no action",
    );
}

fn write_logos(report: &mut Report) {
    // The workspace target dir: crates/app/../../target.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest
        .parent()
        .and_then(Path::parent)
        .unwrap_or(manifest)
        .join("target")
        .join("tray-selftest");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        report.check(false, &format!("create {}: {e}", dir.display()));
        return;
    }
    for (name, color) in [("light", MARK_BLUE), ("dark", MARK_BLUE_DARK)] {
        for size in [16u32, 20, 24, 32] {
            let path = dir.join(format!("logo-{size}-{name}.png"));
            let result = write_enlarged(&path, &logo_pixmap(size, color), 8);
            report.check(
                result.is_ok(),
                &format!(
                    "wrote {} {}",
                    path.display(),
                    result.err().unwrap_or_default()
                ),
            );
        }
    }
}

/// Writes the pixmap scaled up `scale` times (nearest neighbour) over mid-grey.
fn write_enlarged(path: &Path, pixmap: &tiny_skia::Pixmap, scale: u32) -> Result<(), String> {
    let (w, h) = (pixmap.width() * scale, pixmap.height() * scale);
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let p = pixmap.pixel(x / scale, y / scale).expect("in bounds");
            // Premultiplied source over grey 128.
            let under = 128 * (255 - p.alpha() as u32) / 255;
            for c in [p.red(), p.green(), p.blue()] {
                rgb.push((c as u32 + under).min(255) as u8);
            }
        }
    }
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgb).map_err(|e| e.to_string())
}
