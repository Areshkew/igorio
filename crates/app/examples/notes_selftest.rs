//! Notes self-test: our own note windows, files under target/ only, a fake Recycle Bin, no
//! synthetic input (keys are messages to our own editor control) and no clipboard writes.
//!
//! cargo run -p igorio-app --example notes_selftest --release
//!
//! Prints PASS or FAIL per check and exits 0 only when every check passes. Pictures go to
//! crates/app/examples/notes-*.png.

use std::fs::{self, File};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use igorio_app::notes::editor::{
    self, DARK, Editor, EditorCommand, LIGHT, set_global, text_data_object,
};
use igorio_app::notes::markdown::{BlockCommand, Inline, SlashItem};
use igorio_app::notes::store::{Note, NoteFile, NoteId, NoteOrigin, Trash};
use igorio_app::notes::window::{Event, NoteZone, Notes, NotesConfig, TestOptions, Theme};
use igorio_app::overlay::{ensure_dpi_awareness, monitor_at};
use igorio_core::geom::{CardId, Point, Rect};
use igorio_kit::held::HeldCard;
use igorio_kit::image::Image;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::Controls::RichEdit::{EM_GETSCROLLPOS, ITextDocument, tomTrue};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_ESCAPE, VK_RETURN, VkKeyScanW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::Interface;

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

/// Moves "deleted" files into a folder of ours and remembers them.
struct FakeTrash {
    folder: PathBuf,
    moved: Arc<Mutex<Vec<PathBuf>>>,
}

impl Trash for FakeTrash {
    fn trash(&self, path: &Path) -> std::io::Result<()> {
        fs::create_dir_all(&self.folder)?;
        let target = self.folder.join(path.file_name().unwrap_or_default());
        fs::rename(path, &target)?;
        self.moved.lock().expect("trash").push(target);
        Ok(())
    }
}

fn start(
    root: &Path,
    theme: Theme,
    dpi: u32,
    moved: &Arc<Mutex<Vec<PathBuf>>>,
    restore: bool,
) -> Result<Notes> {
    Ok(Notes::start(NotesConfig {
        folder: root.join("Notes"),
        windows_file: root.join("windows.json"),
        trash: Box::new(FakeTrash { folder: root.join("trash"), moved: moved.clone() }),
        theme,
        restore,
        test: Some(TestOptions { dpi: Some(dpi), motion: Some(true), no_activate: true }),
    })?)
}

fn wait(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

fn zone(notes: &Notes, id: NoteId) -> Option<NoteZone> {
    notes.drop_zones().zones().into_iter().find(|zone| zone.id == id)
}

/// Opens a new note at `at` and waits until it has grown in.
fn new_note(notes: &Notes, at: Point) -> Option<NoteId> {
    let before = notes.test_open_ids();
    notes.open_new(at, NoteOrigin::desktop());
    let mut id = None;
    wait(Duration::from_secs(3), || {
        id = notes.test_open_ids().into_iter().find(|id| !before.contains(id));
        id.is_some()
    });
    let id = id?;
    wait(Duration::from_secs(3), || notes.test_idle() && zone(notes, id).is_some());
    Some(id)
}

fn on<R: Send + 'static>(
    notes: &Notes,
    id: NoteId,
    f: impl FnOnce(&Editor) -> R + Send + 'static,
) -> R {
    notes.test_with(id, move |note| f(&note.editor())).expect("note is open")
}

fn hwnd_of(notes: &Notes, id: NoteId) -> HWND {
    HWND(zone(notes, id).map_or(0, |zone| zone.hwnd) as *mut _)
}

/// Types text the way a keyboard does: key down, the character, key up, to our own control.
fn type_text(editor: &Editor, text: &str) {
    for ch in text.chars() {
        if ch == '\n' {
            editor.send_key(VK_RETURN.0, "\r");
        } else {
            let mut buffer = [0u16; 2];
            let units = ch.encode_utf16(&mut buffer);
            // SAFETY: a pure keyboard layout lookup.
            let vk = unsafe { VkKeyScanW(units[0]) } as u16 & 0xff;
            editor.send_key(vk, &ch.to_string());
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Font {
    size: f32,
    weight: i32,
    hidden: bool,
    color: u32,
    name: String,
    italic: bool,
    strike: bool,
    underline: bool,
}

/// The format of one UTF-16 character, read back from the control through TOM.
fn font_at(editor: &Editor, at: usize) -> Font {
    let doc: ITextDocument = editor.rich_edit_ole().cast().expect("TOM");
    // SAFETY: TOM calls on the control's own thread (inside a test closure).
    unsafe {
        let range = doc.Range(at as i32, at as i32 + 1).expect("range");
        let font = range.GetFont().expect("font");
        Font {
            size: font.GetSize().unwrap_or(0.0),
            weight: font.GetWeight().unwrap_or(0),
            hidden: font.GetHidden().unwrap_or(0) == tomTrue.0,
            color: font.GetForeColor().unwrap_or(0) as u32,
            name: font.GetName().map(|name| name.to_string()).unwrap_or_default(),
            italic: font.GetItalic().unwrap_or(0) == tomTrue.0,
            strike: font.GetStrikeThrough().unwrap_or(0) == tomTrue.0,
            underline: font.GetUnderline().unwrap_or(0) != 0,
        }
    }
}

fn u16_index(text: &str, byte: usize) -> usize {
    text[..byte].encode_utf16().count()
}

fn capture(hwnd: HWND) -> Option<(i32, i32, Vec<u8>)> {
    // SAFETY: wait for composition, then print our own window's client area into our DIB.
    unsafe {
        let _ = DwmFlush();
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client).ok()?;
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
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dc = CreateCompatibleDC(None);
        let previous = SelectObject(dc, bitmap.into());
        const PW_RENDERFULLCONTENT: u32 = 2;
        let printed =
            PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT))
                .as_bool();
        let _ = GdiFlush();
        SelectObject(dc, previous);
        let _ = DeleteDC(dc);
        let pixels = slice::from_raw_parts(bits.cast::<u8>(), (w * h * 4) as usize).to_vec();
        let _ = DeleteObject(bitmap.into());
        printed.then_some((w, h, pixels))
    }
}

fn save_png(path: &Path, w: i32, h: i32, bgra: &[u8]) -> Result<bool> {
    let rgb: Vec<u8> = bgra.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]).collect();
    let mut encoder = png::Encoder::new(File::create(path)?, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgb)?;
    let pixels = rgb.as_chunks::<3>().0;
    Ok(pixels.iter().filter(|pixel| **pixel != pixels[0]).count() > 200)
}

fn shot(checks: &mut Checks, hwnd: HWND, folder: &Path, name: &str) -> Result<()> {
    let saved = match capture(hwnd) {
        Some((w, h, pixels)) => save_png(&folder.join(name), w, h, &pixels)?,
        None => false,
    };
    checks.check(&format!("picture {name}"), saved);
    Ok(())
}

/// The note with the slash menu drawn where it is on screen (a layered popup cannot be printed,
/// so its own bitmap is composed over the note's capture).
fn shot_with_popup(
    checks: &mut Checks,
    notes: &Notes,
    id: NoteId,
    folder: &Path,
    name: &str,
) -> Result<()> {
    let hwnd = hwnd_of(notes, id);
    let popup = on(notes, id, |editor| editor.popup_image());
    let Some((w, h, mut pixels)) = capture(hwnd) else {
        checks.check(&format!("picture {name}"), false);
        return Ok(());
    };
    let mut origin = POINT::default();
    // SAFETY: our window, a writable local.
    let _ = unsafe { ClientToScreen(hwnd, &mut origin) };
    // Extend the canvas downwards so the whole menu fits.
    let (mut width, mut height) = (w, h);
    if let Some((at, pw, ph, _)) = &popup {
        width = width.max(at.x - origin.x + *pw as i32);
        height = height.max(at.y - origin.y + *ph as i32);
    }
    let mut canvas = vec![0x30u8; (width * height * 4) as usize];
    for y in 0..h {
        let from = (y * w * 4) as usize;
        let to = (y * width * 4) as usize;
        canvas[to..to + (w * 4) as usize].copy_from_slice(&pixels[from..from + (w * 4) as usize]);
    }
    if let Some((at, pw, ph, popup_pixels)) = popup {
        let (dx, dy) = (at.x - origin.x, at.y - origin.y);
        for y in 0..ph as i32 {
            for x in 0..pw as i32 {
                let (cx, cy) = (dx + x, dy + y);
                if cx < 0 || cy < 0 || cx >= width || cy >= height {
                    continue;
                }
                let s = ((y * pw as i32 + x) * 4) as usize;
                let d = ((cy * width + cx) * 4) as usize;
                let alpha = u32::from(popup_pixels[s + 3]);
                for c in 0..3 {
                    // Premultiplied source over the capture.
                    canvas[d + c] = (u32::from(popup_pixels[s + c])
                        + u32::from(canvas[d + c]) * (255 - alpha) / 255)
                        .min(255) as u8;
                }
            }
        }
    }
    pixels.clear();
    let saved = save_png(&folder.join(name), width, height, &canvas)?;
    checks.check(&format!("picture {name}"), saved);
    Ok(())
}

fn memory() -> (usize, usize) {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: the EX struct with its size; the pseudo-handle of this process.
    let _ = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            counters.cb,
        )
    };
    (counters.WorkingSetSize, counters.PrivateUsage)
}

fn process_cpu() -> u64 {
    let (mut a, mut b, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: four writable FILETIMEs and our own process.
    let _ = unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut kernel, &mut user) };
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    ticks(kernel) + ticks(user)
}

fn test_image(width: u32, height: u32) -> Image {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let r = (60 + x * 150 / width) as u8;
            let g = (120 + y * 100 / height) as u8;
            let stripe = if (x / 24 + y / 24) % 2 == 0 { 235 } else { 200 };
            rgba.extend([r, g, stripe, 255]);
        }
    }
    Image::new(width, height, rgba).expect("image")
}

fn card(id: u64) -> HeldCard {
    HeldCard {
        id: CardId(id),
        image: Arc::new(test_image(480, 270)),
        scale: 1.0,
        captured_at: SystemTime::now(),
        kept: false,
        tilt: 0.0,
        movie: None,
        duration: None,
    }
}

const SHOWCASE: &str = "# Weekly notes\nA paragraph with **bold**, *italic* and `code`.\n- First point\n- Second point\n  - Nested point\n1. One\n2. Two\n- [ ] Write the report\n- [x] Book the room\n> A quote worth keeping\nSee [the docs](https://example.com/docs)\n---\nThe end";

fn big_note() -> String {
    let block = "## Section heading\nSome text with **bold** and *italic* words, a `code span` and a [link](https://example.com).\n- A bullet point with enough words to wrap\n- [ ] A to-do item\n1. A numbered item\n> A quote line\n\n";
    let mut text = String::new();
    while text.len() < 20_000 {
        text.push_str(block);
    }
    text
}

fn run() -> Result<bool> {
    let mut checks = Checks { ok: true };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(format!("notes-selftest-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let root = fs::canonicalize(root)?;
    let pictures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let moved = Arc::new(Mutex::new(Vec::new()));
    let monitor = monitor_at(Point::new(0.0, 0.0));
    let work = monitor.work_area;
    let spot = |dx: f64, dy: f64| Point::new(work.x + work.width * dx, work.y + work.height * dy);

    // A note written by the macOS app (markers visible in the file) is in the folder already.
    let mac_id = NoteId::parse("6F9A1B2C-3D4E-4F5A-6B7C-8D9EAFB0C1D2").expect("id");
    let mac_note = Note {
        id: mac_id,
        text: "# From the Mac\nThis is **bold** and *slanted*.\n- [x] done item".to_owned(),
        created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000),
        updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000),
        origin: NoteOrigin::desktop(),
        frame: Rect::default(),
        is_open: false,
    };
    fs::create_dir_all(root.join("Notes"))?;
    let mac_path = root.join("Notes").join("From the Mac.md");
    fs::write(&mac_path, NoteFile::render(&mac_note))?;
    let mac_bytes = fs::read(&mac_path)?;

    let notes = start(&root, Theme::Light, 96, &moved, true)?;
    let (_, private_start) = memory();

    // --- One note, typing, saving --------------------------------------------------------
    let Some(first) = new_note(&notes, spot(0.3, 0.3)) else {
        checks.check("a new note opens", false);
        return Ok(false);
    };
    checks.check("a new note opens and grows in", zone(&notes, first).is_some());
    let empty = on(&notes, first, |editor| editor.text());
    checks.check("a new note starts empty", empty.is_empty());
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-blank.png")?;

    on(&notes, first, |editor| type_text(editor, "# Hello\nworld"));
    let text = on(&notes, first, |editor| editor.text());
    checks.check(
        &format!("typing through messages gives the text ({text:?})"),
        text == "# Hello\nworld",
    );
    thread::sleep(Duration::from_millis(700));
    let path = notes.test_with(first, |note| note.file_path()).flatten();
    let stored = path
        .as_ref()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|file| NoteFile::parse(&file).1);
    checks.check(
        &format!(
            "after the debounce the file holds the text ({:?})",
            path.as_ref().map(|p| p.file_name())
        ),
        stored.as_deref() == Some("# Hello\nworld"),
    );
    checks.check(
        "the file is named after the first line",
        path.as_ref().is_some_and(|p| p.ends_with("Hello.md")),
    );

    // --- Visible formatting, read back from the control ----------------------------------
    let fonts = on(&notes, first, |editor| {
        editor.set_text(SHOWCASE);
        editor.select(0..0);
        editor.set_focused(false);
        let at = |needle: &str, offset: usize| {
            font_at(editor, u16_index(SHOWCASE, SHOWCASE.find(needle).expect("needle") + offset))
        };
        vec![
            ("heading marker", at("# Weekly", 0)),
            ("heading text", at("Weekly", 0)),
            ("body", at("A paragraph", 0)),
            ("bold marker", at("**bold**", 0)),
            ("bold text", at("bold**", 0)),
            ("italic text", at("italic", 0)),
            ("code text", at("code`", 0)),
            ("list marker", at("- First", 0)),
            ("done text", at("Book the room", 0)),
            ("link text", at("the docs", 0)),
            ("link target", at("(https", 0)),
        ]
    });
    let get =
        |name: &str| fonts.iter().find(|(n, _)| *n == name).map(|(_, f)| f.clone()).expect("font");
    let body = get("body");
    let heading = get("heading text");
    checks.check(
        &format!("body is Segoe UI Variable Text 10.5 pt regular ({body:?})"),
        body.name == "Segoe UI Variable Text"
            && (body.size - 10.5).abs() < 0.1
            && body.weight == 400
            && !body.hidden,
    );
    checks.check(
        "a heading is larger and semibold",
        heading.size > body.size + 3.0 && heading.weight == 600,
    );
    checks.check(
        "hidden markers: heading, bold, list, link target",
        get("heading marker").hidden
            && get("bold marker").hidden
            && get("list marker").hidden
            && get("link target").hidden,
    );
    checks.check("bold text is semibold", get("bold text").weight == 600);
    checks.check("italic text is italic", get("italic text").italic);
    checks.check("code is monospace", get("code text").name == "Consolas");
    let done = get("done text");
    checks.check(
        "a checked to-do is struck through and muted",
        done.strike && done.color == LIGHT.muted.0,
    );
    let link = get("link text");
    checks.check(
        "a link shows its text in the link colour, underlined",
        link.color == LIGHT.accent.0 && link.underline && !link.hidden,
    );
    let decos = on(&notes, first, |editor| editor.decos());
    let kinds: Vec<String> = decos.iter().map(|deco| format!("{:?}", deco.kind)).collect();
    checks.check(
        &format!("bullets, numbers, boxes, quote bar and divider are drawn ({})", kinds.len()),
        ["Bullet", "Number(1)", "Number(2)", "Check(false)", "Check(true)", "QuoteBar", "Divider"]
            .iter()
            .all(|k| kinds.iter().any(|d| d.starts_with(k))),
    );
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-blocks-light-100.png")?;

    // Markers reveal, dimmed, on the caret's line.
    let revealed = on(&notes, first, |editor| {
        editor.set_focused(true);
        let bold = SHOWCASE.find("bold**").expect("bold") + 2;
        editor.select(bold..bold);
        thread::sleep(Duration::from_millis(5));
        editor.freeze(Some(editor.now() + 10_000.0));
        let marker = font_at(editor, u16_index(SHOWCASE, SHOWCASE.find("**bold").expect("bold")));
        let heading = font_at(editor, 0);
        (marker, heading)
    });
    checks.check(
        &format!("with the caret in bold text its markers show, dimmed ({:?})", revealed.0.color),
        !revealed.0.hidden && revealed.0.color == LIGHT.mark.0,
    );
    checks.check("other lines keep their markers hidden", revealed.1.hidden);
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-reveal-bold.png")?;
    let heading_marker = on(&notes, first, |editor| {
        editor.freeze(None);
        editor.select(3..3);
        editor.freeze(Some(editor.now() + 10_000.0));
        font_at(editor, 0)
    });
    checks.check(
        "with the caret on a heading its # shows, dimmed",
        !heading_marker.hidden && heading_marker.color == LIGHT.mark.0,
    );
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-reveal-heading.png")?;
    let copied = on(&notes, first, |editor| {
        editor.freeze(None);
        editor.set_focused(false);
        let end = SHOWCASE[..SHOWCASE.find('\n').expect("line")].encode_utf16().count();
        (
            editor.copy_text(0..SHOWCASE.find('\n').unwrap() + 1),
            editor.copy_through_callback(0..end),
        )
    });
    checks.check(
        &format!("copy gives the Markdown source ({:?})", copied.1),
        copied.0 == "# Weekly notes\r\n" && copied.1.as_deref() == Some("# Weekly notes"),
    );

    placeholder_checks(&mut checks, &notes, first, "light 100%", Some(&pictures))?;
    // --- Blocks that behave, undo -------------------------------------------------------
    let list = on(&notes, first, |editor| {
        editor.set_text("- one");
        editor.select(5..5);
        editor.send_key(VK_RETURN.0, "\r");
        let after = editor.text();
        editor::undo(editor.hwnd());
        (after, editor.text())
    });
    checks.check(&format!("Return in a list continues it ({:?})", list.0), list.0 == "- one\n- ");
    checks.check(
        &format!("one undo removes the whole continuation ({:?})", list.1),
        list.1 == "- one",
    );
    let undo_steps = on(&notes, first, |editor| {
        editor.set_text("");
        type_text(editor, "plain words");
        let mut seen = vec![editor.text()];
        for _ in 0..6 {
            if !editor::can_undo(editor.hwnd()) {
                break;
            }
            editor::undo(editor.hwnd());
            seen.push(editor.text());
        }
        seen
    });
    let every_undo_changes_text = undo_steps.windows(2).all(|pair| pair[0] != pair[1]);
    checks.check(
        &format!("undo never undoes a formatting pass ({undo_steps:?})"),
        every_undo_changes_text && undo_steps.last().is_some_and(String::is_empty),
    );
    let conversions = on(&notes, first, |editor| {
        let mut out = Vec::new();
        for (typed, _) in [
            ("[] task", ""),
            ("``", ""),
            ("--", ""),
            ("# Big", ""),
            ("**b**", ""),
            ("> q", ""),
            ("1. a", ""),
        ] {
            editor.set_text("");
            type_text(editor, typed);
            if typed == "``" {
                type_text(editor, "`");
            }
            if typed == "--" {
                type_text(editor, "-");
            }
            out.push(editor.text());
        }
        editor.set_text("");
        type_text(editor, "[] ");
        let converted = editor.text();
        editor::undo(editor.hwnd());
        out.push(converted);
        out.push(editor.text());
        out
    });
    checks
        .check(&format!("[] makes a to-do ({:?})", conversions[0]), conversions[0] == "- [ ] task");
    checks.check(
        &format!("``` makes a code block ({:?})", conversions[1]),
        conversions[1] == "```\n\n```",
    );
    checks.check(
        &format!("--- makes a divider and a new line ({:?})", conversions[2]),
        conversions[2] == "---\n",
    );
    checks.check(
        "# , **, > and 1. stay Markdown as typed",
        conversions[3] == "# Big"
            && conversions[4] == "**b**"
            && conversions[5] == "> q"
            && conversions[6] == "1. a",
    );
    checks.check(
        &format!("one undo after a conversion gives back what was typed ({:?})", conversions[8]),
        conversions[7] == "- [ ] " && conversions[8] == "[] ",
    );
    let converted_fonts = on(&notes, first, |editor| {
        editor.set_text("");
        type_text(editor, "# Big");
        let heading = font_at(editor, 2);
        editor.set_text("");
        type_text(editor, "**b** ");
        (heading, font_at(editor, 2), font_at(editor, 0))
    });
    checks.check(
        "typed # becomes a heading at once",
        converted_fonts.0.size > 13.0 && converted_fonts.0.weight == 600,
    );
    checks.check("typed **b** is bold at once", converted_fonts.1.weight == 600);
    let backspace = on(&notes, first, |editor| {
        editor.set_text("- item");
        editor.select(2..2);
        editor.send_key(0x08, "\u{8}");
        editor.text()
    });
    checks.check(
        &format!("Backspace at an item's start makes it text ({backspace:?})"),
        backspace == "item",
    );
    let tabbed = on(&notes, first, |editor| {
        editor.set_text("1. a\n2. b");
        editor.select(9..9);
        editor.send_key(0x09, "\t");
        editor.text()
    });
    checks.check(
        &format!("Tab nests a list item and renumbers ({tabbed:?})"),
        tabbed == "1. a\n   1. b",
    );
    let keys = on(&notes, first, |editor| {
        editor.set_text("make this bold");
        editor.select(5..9);
        editor.command(EditorCommand::Inline(Inline::Bold));
        let bold = editor.text();
        editor.command(EditorCommand::Inline(Inline::Bold));
        let plain = editor.text();
        editor.command(EditorCommand::Block(BlockCommand::Heading(2)));
        (bold, plain, editor.text())
    });
    checks.check(
        &format!("Ctrl+B toggles bold ({:?})", keys.0),
        keys.0 == "make **this** bold" && keys.1 == "make this bold",
    );
    checks.check("Ctrl+Alt+2 makes a heading", keys.2 == "## make this bold");

    // The checkbox, through the function a click calls, half way through its tick.
    let checked = on(&notes, first, |editor| {
        editor.set_text("- [ ] Water the plants\n- [ ] Call home");
        editor.select(0..0);
        let start = editor.now();
        editor.click_check(0);
        editor.freeze(Some(start + 27.0));
        editor.text()
    });
    checks.check(
        &format!("a click on the box checks the to-do ({checked:?})"),
        checked.starts_with("- [x] Water"),
    );
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-todo-check-half.png")?;
    on(&notes, first, |editor| editor.freeze(None));

    // --- Slash menu ----------------------------------------------------------------------
    let slash = on(&notes, first, |editor| {
        editor.set_text("");
        editor.set_focused(true);
        type_text(editor, "/");
        thread::sleep(Duration::from_millis(5));
        let open = editor.slash_state();
        editor.freeze(Some(editor.now() + 10_000.0));
        open.map(|(items, selected, hwnd)| (items, selected, hwnd.0 as isize))
    });
    let popup = HWND(slash.as_ref().map_or(0, |(_, _, hwnd)| *hwnd) as *mut _);
    checks.check(
        "typing / opens the slash menu with every block type",
        slash.as_ref().is_some_and(|(items, _, _)| items.len() == SlashItem::ALL.len()),
    );
    // SAFETY: plain window queries.
    let (visible, foreground, ex) = unsafe {
        (
            IsWindowVisible(popup).as_bool(),
            GetForegroundWindow(),
            GetWindowLongPtrW(popup, GWL_EXSTYLE),
        )
    };
    checks.check(
        "the menu is shown, never the foreground window, and cannot be activated",
        visible && foreground != popup && ex & WS_EX_NOACTIVATE.0 as isize != 0,
    );
    shot_with_popup(&mut checks, &notes, first, &pictures, "notes-slash-open.png")?;
    let filtered = on(&notes, first, |editor| {
        editor.freeze(None);
        type_text(editor, "he");
        editor.send_key(VK_DOWN.0, "");
        thread::sleep(Duration::from_millis(5));
        editor.freeze(Some(editor.now() + 10_000.0));
        editor.slash_state().map(|(items, selected, hwnd)| (items, selected, hwnd.0 as isize))
    });
    checks.check(
        "typing filters the menu and Down moves the selection",
        filtered.as_ref().is_some_and(|(items, selected, _)| {
            items == &[SlashItem::Heading1, SlashItem::Heading2, SlashItem::Heading3]
                && *selected == 1
        }),
    );
    shot_with_popup(&mut checks, &notes, first, &pictures, "notes-slash-filtered.png")?;
    let applied = on(&notes, first, |editor| {
        editor.freeze(None);
        editor.send_key(VK_RETURN.0, "\r");
        let text = editor.text();
        let closed = editor.slash_state().is_none();
        type_text(editor, "x /");
        editor.send_key(VK_ESCAPE.0, "\u{1b}");
        (text, closed, editor.slash_state().is_none(), editor.text())
    });
    checks.check(
        &format!("Enter applies the entry and removes the /text ({:?})", applied.0),
        applied.0 == "## " && applied.1,
    );
    checks.check(
        &format!("Esc closes the menu and keeps the text ({:?})", applied.3),
        applied.2 && applied.3 == "## x /",
    );

    // --- Paste: plain text only --------------------------------------------------------------
    let pasted = on(&notes, first, |editor| {
        editor.set_text("start ");
        editor.select(6..6);
        let rich = text_data_object("pasted words").expect("data");
        // SAFETY: registers a clipboard format name; returns its id.
        let rtf = unsafe {
            windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
                "Rich Text Format"
            ))
        } as u16;
        set_global(&rich, rtf, b"{\\rtf1\\ansi{\\b BOLD RTF}}\0").expect("rtf");
        let ours = editor.paste_data(&rich);
        let after_ours = editor.text();
        let weight = font_at(editor, 8).weight;
        // RichEdit's own import path (the one a drop takes): whatever format RichEdit picks,
        // the restyle that follows every change overwrites its formatting.
        editor.select(after_ours.len()..after_ours.len());
        let other = text_data_object("more").expect("data");
        set_global(&other, rtf, b"{\\rtf1\\ansi{\\b\\fs48 more}}\0").expect("rtf");
        // SAFETY: TOM/OLE import on the control's thread with an in-memory object.
        let imported =
            unsafe { editor.rich_edit_ole().ImportDataObject(&other, 0, Default::default()) }
                .is_ok();
        let after_import = editor.text();
        let imported_font = font_at(editor, after_import.encode_utf16().count() - 2);
        let imported_weight =
            if (imported_font.size - 10.5).abs() < 0.1 { imported_font.weight } else { -1 };
        // An image-only object brings nothing in.
        let only_image = {
            // SAFETY: an empty shell data object.
            let object: IDataObject =
                unsafe { windows::Win32::UI::Shell::SHCreateDataObject(None, None, None) }
                    .expect("data");
            set_global(&object, 8 /* CF_DIB */, &[0u8; 64]).expect("dib");
            object
        };

        // SAFETY: as above.
        let image_import =
            unsafe { editor.rich_edit_ole().ImportDataObject(&only_image, 0, Default::default()) }
                .is_ok();
        let image_paste = editor.paste_data(&only_image);
        (
            ours,
            after_ours,
            weight,
            imported,
            after_import,
            imported_weight,
            image_import,
            image_paste,
            editor.text(),
        )
    });
    checks.check(
        &format!("pasting rich text gives its plain text ({:?})", pasted.1),
        pasted.0 && pasted.1 == "start pasted words" && pasted.2 == 400,
    );
    checks.check(
        &format!("RichEdit's import path (drops) keeps no rich formatting ({:?})", pasted.4),
        pasted.3 && pasted.4 == "start pasted wordsmore" && pasted.5 == 400,
    );
    checks.check(
        &format!(
            "an image-only paste or drop brings nothing in (import {}, paste {})",
            pasted.6, pasted.7
        ),
        !pasted.7 && pasted.8 == "start pasted wordsmore",
    );

    // --- Selection and scroll survive a restyle ---------------------------------------------
    let kept = on(&notes, first, |editor| {
        let text = big_note();
        editor.set_text(&text);
        let at = text.len() / 2;
        editor.select(at..at + 5);
        let mut before = POINT::default();
        // SAFETY: EM_GETSCROLLPOS writes into the local.
        unsafe {
            SendMessageW(
                editor.hwnd(),
                EM_GETSCROLLPOS,
                None,
                Some(windows::Win32::Foundation::LPARAM(&mut before as *mut _ as isize)),
            )
        };
        let sel_before = editor.selection();
        let full = editor.restyle_all();
        editor.set_palette(DARK);
        editor.set_palette(LIGHT);
        let mut after = POINT::default();
        // SAFETY: as above.
        unsafe {
            SendMessageW(
                editor.hwnd(),
                EM_GETSCROLLPOS,
                None,
                Some(windows::Win32::Foundation::LPARAM(&mut after as *mut _ as isize)),
            )
        };
        let can_undo = editor::can_undo(editor.hwnd());
        // Typing one character at the end of the long note.
        let end = text.len();
        editor.select(end..end);
        let typed = Instant::now();
        type_text(editor, "x");
        let keystroke = typed.elapsed();
        let (apply, runs) = editor.last_apply();
        (
            sel_before,
            editor.selection(),
            before,
            after,
            full,
            can_undo,
            keystroke,
            apply,
            runs,
            text.len(),
        )
    });
    checks.check(
        &format!("the selection survives a restyle ({:?})", kept.1),
        kept.0 == kept.1 || kept.1.start == kept.9 + 1,
    );
    checks.check(
        &format!(
            "the scroll position survives a restyle ({:?} -> {:?})",
            (kept.2.x, kept.2.y),
            (kept.3.x, kept.3.y)
        ),
        kept.2 == kept.3,
    );
    checks.check("formatting passes left nothing to undo", !kept.5);
    println!(
        "INFO full restyle of a {}-byte note: {:.1} ms",
        kept.9,
        kept.4.as_secs_f64() * 1000.0
    );
    println!(
        "INFO one keystroke at the end of it: {:.2} ms in all, formatting pass {:.2} ms for {} runs",
        kept.6.as_secs_f64() * 1000.0,
        kept.7.as_secs_f64() * 1000.0,
        kept.8
    );
    checks.check(
        "a full restyle of a 20,000-character note takes under 250 ms",
        kept.4 < Duration::from_millis(250),
    );
    checks.check("a keystroke in it takes under 30 ms", kept.6 < Duration::from_millis(30));

    // --- The showcase, an image block, the header ----------------------------------------
    on(&notes, first, |editor| {
        editor.set_text(SHOWCASE);
        editor.select(0..0);
        editor.set_focused(false);
    });
    let center = zone(&notes, first)
        .map(|zone| Point::new(zone.rect.mid_x(), zone.rect.y + zone.rect.height * 0.8))
        .unwrap_or_default();
    let _ = notes.events();
    on(&notes, first, |editor| {
        editor.set_text("Screenshot of the build:\n");
        let end = editor.text().len();
        editor.select(end..end);
    });
    notes.give_cards(Point::new(center.x, center.y), vec![card(41)]);
    let mut taken = None;
    wait(Duration::from_secs(3), || {
        for event in notes.events() {
            if let Event::CardsTaken { ids, hint } = event {
                taken = Some((ids, hint));
            }
        }
        taken.is_some()
    });
    checks.check(
        &format!("a drop reports the card as taken ({taken:?})"),
        taken.as_ref().is_some_and(|(ids, hint)| ids == &[CardId(41)] && hint == "added to note"),
    );
    let dropped = on(&notes, first, |editor| editor.text());
    let attachment = dropped
        .split("](attachments/")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .map(str::to_owned);
    let file_ok = attachment.as_ref().is_some_and(|name| {
        name.starts_with("shot-")
            && name.len() == "shot-12345678.png".len()
            && root.join("Notes").join("attachments").join(name).is_file()
    });
    checks.check(
        &format!("the PNG is at Notes/attachments/shot-xxxxxxxx.png ({attachment:?})"),
        file_ok,
    );
    checks.check(
        &format!("the Markdown link is the Swift one ({dropped:?})"),
        dropped.contains("![screenshot](attachments/shot-")
            && dropped.starts_with("Screenshot of the build:\n"),
    );
    thread::sleep(Duration::from_millis(300));
    let images = on(&notes, first, |editor| {
        editor.decos().iter().filter(|deco| matches!(deco.kind, editor::DecoKind::Image(_))).count()
    });
    checks.check("the screenshot shows as an image block", images == 1);
    on(&notes, first, |editor| editor.select(0..0));
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-image-block.png")?;
    let source = on(&notes, first, |editor| {
        editor.set_focused(true);
        let at = editor.text().find("![").unwrap_or(0) + 3;
        editor.select(at..at);
        editor.freeze(Some(editor.now() + 10_000.0));
        let text = editor.text();
        (
            font_at(editor, u16_index(&text, at - 3)),
            editor
                .decos()
                .into_iter()
                .find(|deco| matches!(deco.kind, editor::DecoKind::Image(_)))
                .map(|deco| deco.revealed),
        )
    });
    checks.check(
        &format!("with the caret on an image its Markdown shows ({source:?})"),
        !source.0.hidden && source.1 == Some(true),
    );
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-image-source.png")?;
    on(&notes, first, |editor| {
        editor.freeze(None);
        editor.set_focused(false);
        editor.set_text(SHOWCASE);
    });
    notes.test_with(first, |note| note.hover_button(Some(1)));
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-header-close-hover.png")?;
    notes.test_with(first, |note| note.hover_button(None));

    // --- Two notes, drop zones -----------------------------------------------------------
    let Some(second) = new_note(&notes, spot(0.62, 0.3)) else {
        checks.check("a second note opens", false);
        return Ok(false);
    };
    on(&notes, second, |editor| type_text(editor, "Second note"));
    let texts =
        (on(&notes, first, |editor| editor.text()), on(&notes, second, |editor| editor.text()));
    checks.check(
        "two notes open at once keep separate text",
        texts.0 == SHOWCASE && texts.1 == "Second note",
    );
    let zones = notes.drop_zones();
    let inside = zone(&notes, second).map(|zone| Point::new(zone.rect.mid_x(), zone.rect.mid_y()));
    checks.check(
        "the drop-zone snapshot answers for a point inside a note",
        inside.and_then(|p| zones.note_at(p)) == Some(second),
    );
    checks.check(
        "and for a point outside every note",
        zones.note_at(Point::new(work.x + 2.0, work.y + work.height - 2.0)).is_none(),
    );

    // --- External edits ---------------------------------------------------------------------
    thread::sleep(Duration::from_millis(700));
    let second_path = notes.test_with(second, |note| note.file_path()).flatten();
    if let Some(path) = &second_path {
        let file = fs::read_to_string(path)?;
        fs::write(path, file.replace("Second note", "Second note, edited outside"))?;
    }
    let reloaded = notes.test_with(second, |note| {
        note.refresh();
        note.editor().text()
    });
    checks.check(
        &format!("a file changed outside reloads into its open note ({reloaded:?})"),
        reloaded.as_deref() == Some("Second note, edited outside"),
    );
    let kept_local = notes.test_with(second, |note| {
        type_text(&note.editor(), "!");
        note.editor().text()
    });
    if let Some(path) = &second_path {
        let file = fs::read_to_string(path)?;
        fs::write(path, file.replace("edited outside", "edited twice"))?;
    }
    let after_conflict = notes.test_with(second, |note| {
        note.refresh();
        note.editor().text()
    });
    checks.check(
        &format!("when both changed the editor's text stays ({after_conflict:?})"),
        kept_local.is_some()
            && after_conflict == kept_local
            && after_conflict.as_deref().is_some_and(|text| !text.contains("twice")),
    );

    // --- Put away and reopen keeps the place; delete uses the trash ----------------------
    let before = zone(&notes, second).map(|zone| zone.rect);
    notes.put_away(second);
    wait(Duration::from_secs(2), || zone(&notes, second).is_none() && notes.test_idle());
    notes.reopen(second);
    wait(Duration::from_secs(2), || zone(&notes, second).is_some());
    let after = zone(&notes, second).map(|zone| zone.rect);
    checks.check(
        &format!("position and size come back after put away and reopen ({after:?})"),
        before.is_some() && before == after,
    );
    thread::sleep(Duration::from_millis(700));
    let doomed = notes.test_with(second, |note| note.file_path()).flatten();
    notes.delete(second);
    wait(Duration::from_secs(2), || zone(&notes, second).is_none());
    let trashed = moved.lock().map(|moved| moved.clone()).unwrap_or_default();
    checks.check(
        &format!("delete moves the file through the trash ({} files)", trashed.len()),
        doomed.as_ref().is_some_and(|path| !path.exists())
            && trashed
                .iter()
                .any(|path| path.file_name() == doomed.as_ref().and_then(|p| p.file_name())),
    );

    // --- The macOS note -------------------------------------------------------------------
    notes.reopen(mac_id);
    wait(Duration::from_secs(2), || zone(&notes, mac_id).is_some());
    let mac = on(&notes, mac_id, |editor| {
        editor.set_focused(false);
        (editor.text(), font_at(editor, 0).hidden, font_at(editor, 3).weight)
    });
    checks.check(
        &format!("a note in the macOS format loads formatted ({:?})", mac.0),
        mac.0 == mac_note.text && mac.1 && mac.2 == 600,
    );
    shot(&mut checks, hwnd_of(&notes, mac_id), &pictures, "notes-mac-note.png")?;
    notes.put_away(mac_id);
    wait(Duration::from_secs(2), || zone(&notes, mac_id).is_none() && notes.test_idle());
    thread::sleep(Duration::from_millis(600));
    checks.check(
        "load, edit nothing, put away: the file is byte for byte the same",
        fs::read(&mac_path).ok().as_deref() == Some(&mac_bytes[..]),
    );

    // --- Idle, memory ----------------------------------------------------------------------
    wait(Duration::from_secs(3), || notes.test_idle());
    let idle = notes.test_idle();
    let cpu_before = process_cpu();
    thread::sleep(Duration::from_secs(2));
    let cpu = process_cpu() - cpu_before;
    checks.check(
        &format!(
            "idle: no timer runs, process CPU {:.3} ms over two seconds",
            cpu as f64 / 10_000.0
        ),
        idle && notes.test_idle() && cpu == 0,
    );
    let (working_one, private_one) = memory();
    println!(
        "INFO memory with 1 note open: working set {:.1} MB, private {:.1} MB (start {:.1} MB)",
        working_one as f64 / 1e6,
        private_one as f64 / 1e6,
        private_start as f64 / 1e6
    );
    let mut extra = Vec::new();
    for index in 0..9 {
        let at = spot(0.15 + 0.07 * f64::from(index), 0.45);
        if let Some(id) = new_note(&notes, at) {
            on(&notes, id, move |editor| type_text(editor, &format!("Note number {index}")));
            extra.push(id);
        }
    }
    let (working_ten, private_ten) = memory();
    println!(
        "INFO memory with 10 notes open: working set {:.1} MB, private {:.1} MB",
        working_ten as f64 / 1e6,
        private_ten as f64 / 1e6
    );
    checks.check("ten notes open", extra.len() == 9);
    checks.check(
        "each extra note costs under 4 MB of private memory",
        private_ten.saturating_sub(private_one) < 9 * 4_000_000,
    );
    for id in &extra {
        notes.delete(*id);
    }
    wait(Duration::from_secs(3), || notes.test_open_ids().len() == 1 && notes.test_idle());

    // --- The appear animation half way ----------------------------------------------------
    let before = notes.test_open_ids();
    notes.open_new(spot(0.5, 0.35), NoteOrigin::desktop());
    let mut grow = None;
    wait(Duration::from_secs(2), || {
        grow = notes.test_open_ids().into_iter().find(|id| !before.contains(id));
        grow.is_some()
    });
    if let Some(grow) = grow {
        let hwnd = notes.test_with(grow, |note| {
            note.freeze_appear(Some(0.09));
            note.hwnd().0 as isize
        });
        thread::sleep(Duration::from_millis(30));
        if let Some(hwnd) = hwnd {
            shot(&mut checks, HWND(hwnd as *mut _), &pictures, "notes-appear-half.png")?;
        }
        notes.test_with(grow, |note| note.freeze_appear(None));
        wait(Duration::from_secs(2), || notes.test_idle());
        notes.put_away(grow);
    }

    // --- The mouse wheel and the drawn thumb ------------------------------------------------
    let long_text = (1..80).map(|n| format!("Scrolling line {n}")).collect::<Vec<_>>().join("\n");
    on(&notes, first, move |editor| {
        editor.set_focused(false);
        editor.set_text(&long_text);
        editor.select(0..0);
    });
    let note_hwnd = hwnd_of(&notes, first);
    // SAFETY: children of our own note window.
    let (editor_hwnd, close_hwnd) = unsafe {
        (
            GetDlgItem(Some(note_hwnd), igorio_app::notes::window::EDITOR_ID).unwrap_or_default(),
            GetDlgItem(Some(note_hwnd), igorio_app::notes::window::CLOSE_ID).unwrap_or_default(),
        )
    };
    let wheel = |target: HWND, delta: i16| {
        // SAFETY: a wheel message to our own window, as Windows delivers one (no focus needed).
        unsafe {
            SendMessageW(
                target,
                WM_MOUSEWHEEL,
                Some(WPARAM((delta as u16 as usize) << 16)),
                Some(LPARAM(0)),
            );
        }
        thread::sleep(Duration::from_millis(250));
    };
    for (name, target) in
        [("editor", editor_hwnd), ("note window", note_hwnd), ("header button", close_hwnd)]
    {
        let before = on(&notes, first, |editor| editor.scroll_state());
        wheel(target, -120);
        let after = on(&notes, first, |editor| editor.scroll_state());
        let moved = after.0 > before.0 && after.1.zip(before.1).is_some_and(|(a, b)| a.top > b.top);
        checks.check(&format!("a wheel notch over the {name} scrolls the note and the thumb follows ({} -> {} px)", before.0, after.0), moved);
    }
    let before = on(&notes, first, |editor| editor.scroll_state());
    wheel(editor_hwnd, -40);
    let after = on(&notes, first, |editor| editor.scroll_state());
    checks.check(
        &format!(
            "a high-resolution delta of 40 scrolls a third of a notch ({} -> {} px)",
            before.0, after.0
        ),
        after.0 > before.0,
    );
    wheel(editor_hwnd, 120 * 30);
    let top = on(&notes, first, |editor| editor.scroll_state());
    checks.check("scrolling up stops at the top", top.0 == 0);
    // A click on the track below the thumb pages down; dragging the thumb scrolls.
    let (_, thumb) = top;
    if let Some(thumb) = thumb {
        let x = (thumb.left + thumb.right) / 2;
        let click = |message: u32, y: i32| {
            let point = ((y as u16 as usize) << 16) | (x as u16 as usize);
            // SAFETY: mouse messages to our own editor control, client coordinates.
            unsafe {
                SendMessageW(editor_hwnd, message, Some(WPARAM(1)), Some(LPARAM(point as isize)))
            };
        };
        click(WM_LBUTTONDOWN, thumb.bottom + 20);
        click(WM_LBUTTONUP, thumb.bottom + 20);
        thread::sleep(Duration::from_millis(250));
        let paged = on(&notes, first, |editor| editor.scroll_state());
        checks.check(&format!("a click on the track pages down ({} px)", paged.0), paged.0 > 0);
        let thumb = paged.1.unwrap_or(thumb);
        click(WM_LBUTTONDOWN, thumb.top + 2);
        click(WM_MOUSEMOVE, thumb.top + 42);
        click(WM_LBUTTONUP, thumb.top + 42);
        let dragged = on(&notes, first, |editor| editor.scroll_state());
        checks.check(
            &format!("dragging the thumb scrolls ({} -> {} px)", paged.0, dragged.0),
            dragged.0 > paged.0,
        );
    } else {
        checks.check("a long note draws a scroll thumb", false);
    }
    on(&notes, first, |editor| {
        editor.set_text("Short note");
        editor.select(0..0);
    });
    wheel(editor_hwnd, -120);
    let short = on(&notes, first, |editor| editor.scroll_state());
    checks.check(
        &format!("a note shorter than its window does not scroll and has no thumb ({short:?})"),
        short.0 == 0 && short.1.is_none(),
    );

    // --- Resizing through the real message path, down to nothing and back --------------------
    let attachment = fs::read_dir(root.join("Notes").join("attachments"))?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.ends_with(".png"))
        .unwrap_or_default();
    let stress_text = format!(
        "# Resize me\nA long line that wraps many times when the note is narrow, with **bold** and `code` in it.\n- [ ] A to-do\n- [x] Done\n![screenshot](attachments/{attachment})\n{}",
        (1..40).map(|n| format!("Line {n} of text")).collect::<Vec<_>>().join("\n")
    );
    if let Some(stress) = new_note(&notes, spot(0.4, 0.3)) {
        let text = stress_text.clone();
        on(&notes, stress, move |editor| {
            editor.set_text(&text);
            editor.select(0..0);
        });
        let mut smallest = (i32::MAX, i32::MAX);
        let mut steps = 0;
        for _ in 0..3 {
            let (count, small) = stress_resize(&notes, stress);
            steps += count;
            smallest = (smallest.0.min(small.0), smallest.1.min(small.1));
        }
        let still_there = zone(&notes, stress).is_some();
        checks.check(
            &format!("resizing through {steps} sizes on every edge and corner paints and survives"),
            still_there,
        );
        checks.check(
            &format!(
                "asked for 0x0 to 1400x1000, the window never went below 190x120 (smallest {}x{})",
                smallest.0, smallest.1
            ),
            smallest.0 >= 190 && smallest.1 >= 120,
        );
        // With the slash menu open.
        on(&notes, stress, |editor| {
            editor.set_focused(true);
            editor.select(0..0);
            type_text(editor, "\n/");
        });
        let open = on(&notes, stress, |editor| editor.slash_state().is_some());
        let (steps, _) = stress_resize(&notes, stress);
        checks.check(
            &format!("resizing with the slash menu open ({steps} sizes, menu was open: {open})"),
            open && zone(&notes, stress).is_some(),
        );
        // Caret visible, drawings on their lines, saved size the final one.
        let hwnd = hwnd_of(&notes, stress);
        let final_rect = RECT { left: 200, top: 150, right: 200 + 330, bottom: 150 + 260 };
        resize_to(hwnd, final_rect, 8);
        let (caret_visible, aligned) = on(&notes, stress, |editor| {
            editor.set_focused(false);
            let text = editor.text();
            let at = text.find("A to-do").unwrap_or(0);
            editor.select(at..at);
            (editor.caret_visible(), editor.decos_aligned())
        });
        checks.check("after a resize the caret is visible", caret_visible);
        checks.check(
            &format!(
                "checkboxes, bullets and images sit on their lines after re-wrapping ({aligned:?})"
            ),
            aligned.is_none(),
        );
        thread::sleep(Duration::from_millis(700));
        let saved = notes.test_with(stress, |note| note.saved_frame()).flatten();
        let actual = rect_of_win(window_rect(hwnd));
        checks
            .check(&format!("the saved size is the final one ({saved:?})"), saved == Some(actual));
        // While the note is still growing in.
        let before = notes.test_open_ids();
        notes.open_new(spot(0.5, 0.5), NoteOrigin::desktop());
        let mut grower = None;
        wait(Duration::from_secs(2), || {
            grower = notes.test_open_ids().into_iter().find(|id| !before.contains(id));
            grower.is_some()
        });
        if let Some(grower) = grower {
            let (steps, _) = stress_resize(&notes, grower);
            checks.check(
                &format!("resizing while the note grows in ({steps} sizes)"),
                steps > 0 && notes.test_open_ids().contains(&grower),
            );
            wait(Duration::from_secs(2), || notes.test_idle());
            notes.delete(grower);
        }
        notes.delete(stress);
        wait(Duration::from_secs(2), || notes.test_idle());
    } else {
        checks.check("a note for the resize test opens", false);
    }

    // --- UI Automation reaches the editor with its text -------------------------------------
    let editor_hwnd = {
        // SAFETY: a child of our own note window.
        unsafe { GetDlgItem(Some(hwnd_of(&notes, first)), igorio_app::notes::window::EDITOR_ID) }
            .unwrap_or_default()
    };
    let expected_text = on(&notes, first, |editor| editor.text());
    let (name, uia_text) = uia_text(editor_hwnd);
    checks.check(
        &format!("UI Automation reads the note's Markdown text and its name ({name:?})"),
        uia_text.as_deref().is_some_and(|text| {
            !expected_text.is_empty()
                && text.replace('\r', "\n").trim_end() == expected_text.trim_end()
        }) && name.as_deref() == Some("Note text"),
    );

    // A look at spell checking (not a check: the underlines come from Windows' spell checker
    // and only show after an idle moment). Saved under target/ for a person to look at.
    on(&notes, first, |editor| {
        editor.set_text("Teh quikc brwn fox jumps over the lazy dog.");
        editor.select(0..0);
    });
    thread::sleep(Duration::from_millis(1500));
    if let Some((w, h, pixels)) = capture(hwnd_of(&notes, first)) {
        let path = root.join("..").join("notes-spellcheck.png");
        let _ = save_png(&path, w, h, &pixels);
        println!("INFO spell-check picture: {}", path.display());
    }

    // --- Shutdown with the window state saved ---------------------------------------------
    let first_rect = zone(&notes, first).map(|zone| zone.rect);
    let stopped = Instant::now();
    let ended = notes.shutdown(Duration::from_secs(1));
    checks.check(
        &format!("shutdown ends the thread in {:.0} ms", stopped.elapsed().as_secs_f64() * 1000.0),
        ended,
    );

    // --- Restore, clamped, dark at 125% ---------------------------------------------------
    let windows_file = root.join("windows.json");
    let mut saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&windows_file)?)?;
    let key = first.to_string();
    let open_flag = saved[&key]["isOpen"].as_bool();
    checks.check(
        &format!("the open note is saved as open with its frame ({open_flag:?})"),
        open_flag == Some(true)
            && first_rect.is_some_and(|rect| saved[&key]["frame"]["x"].as_f64() == Some(rect.x)),
    );
    saved[&key]["frame"]["x"] = serde_json::json!(99_999.0);
    saved[&key]["frame"]["y"] = serde_json::json!(-99_999.0);
    fs::write(&windows_file, serde_json::to_string_pretty(&saved)?)?;
    let notes = start(&root, Theme::Dark, 120, &moved, true)?;
    wait(Duration::from_secs(3), || zone(&notes, first).is_some() && notes.test_idle());
    let restored = zone(&notes, first);
    let works = {
        let monitor = monitor_at(Point::new(99_999.0, -99_999.0));
        monitor.work_area
    };
    checks.check(
        &format!(
            "an off-screen saved frame is brought back onto a work area ({:?})",
            restored.as_ref().map(|z| z.rect)
        ),
        restored
            .as_ref()
            .is_some_and(|zone| works.contains(Point::new(zone.rect.x + 1.0, zone.rect.y + 1.0))),
    );
    let dpi = notes.test_with(first, |note| note.dpi());
    checks.check("the dark run draws at 125%", dpi == Some(120));
    on(&notes, first, |editor| {
        editor.set_text(SHOWCASE);
        editor.select(0..0);
        editor.set_focused(false);
    });
    thread::sleep(Duration::from_millis(100));
    shot(&mut checks, hwnd_of(&notes, first), &pictures, "notes-blocks-dark-125.png")?;
    placeholder_checks(&mut checks, &notes, first, "dark 125%", None)?;

    // --- Shutdown with a pending write ----------------------------------------------------
    on(&notes, first, |editor| {
        editor.set_text("Pending text");
        let end = editor.text().len();
        editor.select(end..end);
        type_text(editor, " saved on quit");
    });
    let stopped = Instant::now();
    let ended = notes.shutdown(Duration::from_secs(1));
    let elapsed = stopped.elapsed();
    let on_disk = fs::read_dir(root.join("Notes"))?
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .any(|file| NoteFile::parse(&file).1 == "Pending text saved on quit");
    checks.check(
        &format!(
            "shutdown with a pending write ends in {:.0} ms and the write is on disk",
            elapsed.as_secs_f64() * 1000.0
        ),
        ended && elapsed < Duration::from_secs(1) && on_disk,
    );
    let _ = fs::remove_dir_all(&root);
    Ok(checks.ok)
}

fn main() {
    ensure_dpi_awareness();
    igorio_app::crash::install_at(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/notes-selftest-crash.log"),
    );
    let ok = match run() {
        Ok(ok) => ok,
        Err(error) => {
            println!("FAIL {error}");
            false
        }
    };
    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}

#[allow(dead_code)]
fn unused(_: Range<usize>) {}

/// The UI Automation name and document text of a window (this thread owns no windows).
fn uia_text(hwnd: HWND) -> (Option<String>, Option<String>) {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
    };
    // SAFETY: COM for this test thread, then in-process UI Automation calls on our own window.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(automation) =
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
        else {
            return (None, None);
        };
        let Ok(element) = automation.ElementFromHandle(hwnd) else { return (None, None) };
        let name = element.CurrentName().ok().map(|name| name.to_string());
        let text = element
            .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            .and_then(|pattern| pattern.DocumentRange())
            .and_then(|range| range.GetText(-1))
            .ok()
            .map(|text| text.to_string());
        (name, text)
    }
}

fn window_rect(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    // SAFETY: a window of this process; writes the local.
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    rect
}

fn rect_of_win(r: RECT) -> Rect {
    Rect::new(
        f64::from(r.left),
        f64::from(r.top),
        f64::from(r.right - r.left),
        f64::from(r.bottom - r.top),
    )
}

/// One drag step the way the sizing loop does it: WM_SIZING with the edge, then the move, then
/// a paint of the window and its children.
fn resize_step(hwnd: HWND, edge: u32, mut rect: RECT) -> (i32, i32) {
    // SAFETY: messages to our own note window (owned by the notes thread; SendMessage waits
    // for it), with a local RECT for the call.
    unsafe {
        SendMessageW(
            hwnd,
            WM_SIZING,
            Some(WPARAM(edge as usize)),
            Some(LPARAM(&mut rect as *mut RECT as isize)),
        );
        let _ = SetWindowPos(
            hwnd,
            None,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // A raw WM_SIZE with the asked size too (smaller than the window may really get).
        let (w, h) = (
            (rect.right - rect.left).clamp(0, 0xffff) as usize,
            (rect.bottom - rect.top).clamp(0, 0xffff) as usize,
        );
        SendMessageW(hwnd, WM_SIZE, Some(WPARAM(0)), Some(LPARAM((w | h << 16) as isize)));
        let _ =
            RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN);
    }
    let real = window_rect(hwnd);
    (real.right - real.left, real.bottom - real.top)
}

fn resize_to(hwnd: HWND, rect: RECT, edge: u32) {
    // SAFETY: as in resize_step.
    unsafe { SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None) };
    let _ = resize_step(hwnd, edge, rect);
    // SAFETY: as above.
    unsafe { SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None) };
}

/// Drags every edge and corner from large to nothing and back, painting at each step. Returns
/// the steps taken and the smallest window size seen.
fn stress_resize(notes: &Notes, id: NoteId) -> (usize, (i32, i32)) {
    let sizes: [(i32, i32); 16] = [
        (1400, 1000),
        (900, 700),
        (400, 300),
        (304, 224),
        (220, 140),
        (190, 120),
        (189, 119),
        (150, 200),
        (400, 40),
        (40, 400),
        (120, 31),
        (60, 20),
        (10, 10),
        (1, 1),
        (0, 0),
        (500, 380),
    ];
    let mut steps = 0;
    let mut smallest = (i32::MAX, i32::MAX);
    // The window itself (a note still growing in has no drop zone yet).
    let hwnd = HWND(notes.test_with(id, |note| note.hwnd().0 as isize).unwrap_or(0) as *mut _);
    if hwnd.0.is_null() {
        return (0, smallest);
    }
    for edge in 1..=8u32 {
        let start = window_rect(hwnd);
        // SAFETY: as in resize_step.
        unsafe { SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None) };
        for (w, h) in sizes {
            // WMSZ_LEFT 1, RIGHT 2, TOP 3, TOPLEFT 4, TOPRIGHT 5, BOTTOM 6, BOTTOMLEFT 7, BOTTOMRIGHT 8.
            let left_moves = matches!(edge, 1 | 4 | 7);
            let top_moves = matches!(edge, 3..=5);
            let horizontal = !matches!(edge, 3 | 6);
            let vertical = !matches!(edge, 1 | 2);
            let width = if horizontal { w } else { start.right - start.left };
            let height = if vertical { h } else { start.bottom - start.top };
            let left = if left_moves { start.right - width } else { start.left };
            let top = if top_moves { start.bottom - height } else { start.top };
            let (w, h) = resize_step(
                hwnd,
                edge,
                RECT { left, top, right: left + width, bottom: top + height },
            );
            smallest = (smallest.0.min(w), smallest.1.min(h));
            steps += 1;
        }
        // SAFETY: as above.
        unsafe { SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None) };
        let _ = capture(hwnd);
        let _ = resize_step(hwnd, 8, start);
    }
    (steps, smallest)
}

/// For an empty line of each block type with the caret on it: the placeholder sits after the
/// revealed marker and the caret, on the marker's baseline. Saves the heading and quote
/// pictures when `pictures` is given.
fn placeholder_checks(
    checks: &mut Checks,
    notes: &Notes,
    id: NoteId,
    suffix: &str,
    pictures: Option<&Path>,
) -> Result<()> {
    let cases: [(&str, &str, Option<&str>); 8] = [
        ("Heading 1", "# ", Some("notes-placeholder-heading.png")),
        ("Heading 2", "## ", None),
        ("Heading 3", "### ", None),
        ("Quote", "> ", Some("notes-placeholder-quote.png")),
        ("To-do", "- [ ] ", None),
        ("List (bullet)", "- ", None),
        ("List (numbered)", "1. ", None),
        ("Type / for commands", "Some text\n", None),
    ];
    for (name, text, picture) in cases {
        let geometry = on(notes, id, move |editor| {
            editor.set_text(text);
            editor.set_focused(true);
            let end = text.len();
            editor.select(end..end);
            editor.freeze(Some(editor.now() + 10_000.0));
            editor.placeholder_geometry()
        });
        let ok = geometry.is_some_and(|g| {
            let after_caret = g.placeholder.left > g.caret_x;
            match g.marker {
                Some((marker, base)) => {
                    after_caret
                        && g.placeholder.left >= marker.right
                        && (g.baseline - base).abs() <= 1
                }
                None => after_caret,
            }
        });
        checks.check(&format!("{suffix}: the {name} placeholder sits after the marker and the caret, on its baseline ({geometry:?})"), ok);
        if let (Some(folder), Some(picture)) = (pictures, picture) {
            shot(checks, hwnd_of(notes, id), folder, picture)?;
        }
        on(notes, id, |editor| editor.freeze(None));
    }
    // A code block shows no placeholder at all.
    let code = on(notes, id, |editor| {
        editor.set_text("```\n\n```");
        editor.select(4..4);
        editor.placeholder_geometry()
    });
    checks.check(&format!("{suffix}: an empty code line has no placeholder"), code.is_none());
    on(notes, id, |editor| editor.set_focused(false));
    Ok(())
}
