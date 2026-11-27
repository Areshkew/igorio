//! Self-test of hiding notes and of the notes panel: the header's hide button, the panel's
//! list, filter, keys, row actions, "Hide all" and "Show all", and the hidden state across a
//! restart. Runs against a folder under target/ with a fake Recycle Bin; every window is shown
//! without activation and every key goes to our own controls as messages. Saves the panel's
//! reference pictures (light and dark) in crates/app/examples/.
//!
//! cargo run -p igorio-app --example notes_panel_selftest --release

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use igorio_app::notes::panel::NoteState;
use igorio_app::notes::store::{Note, NoteFile, NoteId, NoteOrigin, Trash};
use igorio_app::notes::window::{HIDE_ID, Notes, NotesConfig, TestOptions, Theme};
use igorio_app::overlay::{ensure_dpi_awareness, monitor_at};
use igorio_core::geom::{Point, Rect};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP};
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

fn start(root: &Path, theme: Theme, moved: &Arc<Mutex<Vec<PathBuf>>>) -> Result<Notes> {
    Ok(Notes::start(NotesConfig {
        folder: root.join("Notes"),
        windows_file: root.join("windows.json"),
        trash: Box::new(FakeTrash { folder: root.join("trash"), moved: moved.clone() }),
        theme,
        restore: true,
        test: Some(TestOptions { dpi: Some(96), motion: Some(true), no_activate: true }),
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

type Rows = (Vec<(NoteId, NoteState)>, Option<NoteId>);

fn rows(notes: &Notes) -> Option<Rows> {
    notes.test_panel(|panel| panel.rows())
}

fn state_of(notes: &Notes, id: NoteId) -> Option<NoteState> {
    rows(notes)?.0.into_iter().find(|(row, _)| *row == id).map(|(_, state)| state)
}

/// Waits until the panel lists `id` in `state` (the list follows the notes while it is open).
fn wait_state(notes: &Notes, id: NoteId, state: NoteState) -> bool {
    wait(Duration::from_secs(3), || state_of(notes, id) == Some(state))
}

fn filter_hwnd(notes: &Notes) -> HWND {
    HWND(notes.test_panel(|panel| panel.filter_hwnd().0 as isize).unwrap_or(0) as *mut _)
}

/// A key to the panel's filter field, as the keyboard sends it: key down, then its character.
fn key(notes: &Notes, vk: u16, ch: Option<u16>) {
    let edit = filter_hwnd(notes);
    // SAFETY: messages to our own control (on the notes thread, which handles them).
    unsafe {
        SendMessageW(edit, WM_KEYDOWN, Some(WPARAM(usize::from(vk))), Some(LPARAM(1)));
        if let Some(ch) = ch {
            SendMessageW(edit, WM_CHAR, Some(WPARAM(usize::from(ch))), Some(LPARAM(1)));
        }
        SendMessageW(edit, WM_KEYUP, Some(WPARAM(usize::from(vk))), Some(LPARAM(1)));
    }
}

/// Typing into the filter: one character message per character, as a keyboard gives them.
fn type_filter(notes: &Notes, text: &str) {
    let edit = filter_hwnd(notes);
    for unit in text.encode_utf16() {
        // SAFETY: a character message to our own control.
        unsafe { SendMessageW(edit, WM_CHAR, Some(WPARAM(usize::from(unit))), Some(LPARAM(1))) };
    }
}

fn click(button: HWND) {
    // SAFETY: a click message to our own button (no input is synthesized).
    unsafe { SendMessageW(button, BM_CLICK, None, None) };
}

fn hide_button(notes: &Notes, id: NoteId) -> HWND {
    let handle = notes
        .test_with(id, |note| {
            // SAFETY: a child lookup in our own window.
            unsafe { GetDlgItem(Some(note.hwnd()), HIDE_ID) }.map_or(0, |hwnd| hwnd.0 as isize)
        })
        .unwrap_or(0);
    HWND(handle as *mut _)
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

fn shot(checks: &mut Checks, hwnd: HWND, folder: &Path, name: &str) -> Result<()> {
    let saved = match capture(hwnd) {
        Some((w, h, bgra)) => {
            let rgb: Vec<u8> =
                bgra.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]).collect();
            let mut encoder = png::Encoder::new(File::create(folder.join(name))?, w as u32, h as u32);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header()?.write_image_data(&rgb)?;
            let pixels = rgb.as_chunks::<3>().0;
            pixels.iter().filter(|pixel| **pixel != pixels[0]).count() > 200
        }
        None => false,
    };
    checks.check(&format!("picture {name}"), saved);
    Ok(())
}

/// A note file written before the app starts, changed `age` seconds ago.
fn seed(folder: &Path, byte: u8, text: &str, age: u64) -> Result<NoteId> {
    let id = NoteId::from_bytes([byte; 16]);
    let when = SystemTime::now() - Duration::from_secs(age);
    let note = Note {
        id,
        text: text.to_owned(),
        created_at: when,
        updated_at: when,
        origin: NoteOrigin::desktop(),
        frame: Rect::default(),
        is_open: false,
        is_hidden: false,
    };
    let title: String = text.lines().next().unwrap_or("note").to_owned();
    let path = folder.join(format!("{title}.md"));
    fs::write(&path, NoteFile::render(&note))?;
    File::options().write(true).open(&path)?.set_modified(when)?;
    Ok(id)
}

fn run() -> Result<bool> {
    let mut checks = Checks { ok: true };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(format!("notes-panel-selftest-{}", std::process::id()));
    fs::create_dir_all(root.join("Notes"))?;
    let root = fs::canonicalize(root)?;
    let pictures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let moved = Arc::new(Mutex::new(Vec::new()));
    let folder = root.join("Notes");
    let groceries = seed(&folder, 1, "Groceries\nmilk, eggs and bread", 3 * 60)?;
    let meeting = seed(&folder, 2, "Meeting notes\nDiscuss the **budget** for spring", 2 * 3600)?;
    let budget = seed(&folder, 3, "Budget 2027\n- rent\n- food", 3 * 86_400)?;

    let mut notes = start(&root, Theme::Light, &moved)?;
    let work = monitor_at(Point::new(0.0, 0.0)).work_area;

    // --- Hide through the note's header button -------------------------------------------
    notes.open_new(Point::new(work.x + work.width * 0.3, work.y + work.height * 0.3), NoteOrigin::desktop());
    let mut trip = None;
    wait(Duration::from_secs(3), || {
        trip = notes.test_open_ids().into_iter().next();
        trip.is_some()
    });
    let Some(trip) = trip else {
        checks.check("a new note opens", false);
        return Ok(false);
    };
    wait(Duration::from_secs(3), || notes.test_idle());
    notes.test_with(trip, |note| {
        note.editor().set_text("Trip plan\npack the tent");
    });
    notes.test_with(trip, |note| note.hover_button(Some(0)));
    if let Some(hwnd) = notes.test_with(trip, |note| note.hwnd().0 as isize) {
        shot(&mut checks, HWND(hwnd as *mut _), &pictures, "notes-header-hide-hover.png")?;
    }
    notes.test_with(trip, |note| note.hover_button(None));
    let button = hide_button(&notes, trip);
    checks.check("the header has a hide button", !button.is_invalid());
    click(button);
    let gone = wait(Duration::from_secs(3), || !notes.test_open_ids().contains(&trip));
    checks.check("the hide button takes the note off screen", gone);
    let windows = fs::read_to_string(root.join("windows.json")).unwrap_or_default();
    checks.check("hiding saves the hidden state at once", windows.contains("isHidden"));
    let saved = fs::read_to_string(folder.join("Trip plan.md")).unwrap_or_default();
    checks.check("hiding saves the text first", saved.ends_with("Trip plan\npack the tent"));
    wait(Duration::from_secs(3), || notes.test_idle());

    // --- The panel: every note, newest edit first, with its state ------------------------
    notes.toggle_panel(None);
    checks.check("the panel opens", wait(Duration::from_secs(3), || rows(&notes).is_some()));
    let listed = rows(&notes).unwrap_or_default();
    let order: Vec<NoteId> = listed.0.iter().map(|(id, _)| *id).collect();
    checks.check(
        &format!("the panel lists every note, newest edit first ({} rows)", order.len()),
        order == [trip, groceries, meeting, budget],
    );
    checks.check(
        "each row says whether the note is shown, hidden or closed",
        listed.0[0].1 == NoteState::Hidden && listed.0[1..].iter().all(|(_, s)| *s == NoteState::Closed),
    );
    checks.check("the first row is selected", listed.1 == Some(trip));
    let idle = wait(Duration::from_secs(2), || notes.test_panel(|panel| !panel.animating()) == Some(true));
    checks.check("the panel runs no timer once it has opened", idle);
    if let Some(hwnd) = notes.test_panel(|panel| panel.hwnd().0 as isize) {
        shot(&mut checks, HWND(hwnd as *mut _), &pictures, "notes-panel-light.png")?;
    }

    // --- Filter as you type --------------------------------------------------------------
    type_filter(&notes, "BUDGET");
    let found: Vec<NoteId> = rows(&notes).unwrap_or_default().0.iter().map(|(id, _)| *id).collect();
    checks.check("the filter matches title and text, ignoring case", found == [meeting, budget]);
    type_filter(&notes, " spring");
    let found: Vec<NoteId> = rows(&notes).unwrap_or_default().0.iter().map(|(id, _)| *id).collect();
    checks.check("every word of the filter must match", found == [meeting]);
    if let Some(hwnd) = notes.test_panel(|panel| panel.hwnd().0 as isize) {
        shot(&mut checks, HWND(hwnd as *mut _), &pictures, "notes-panel-filtered.png")?;
    }
    key(&notes, VK_ESCAPE.0, Some(0x1b));
    let all = rows(&notes).unwrap_or_default().0.len();
    checks.check("Esc clears the filter and keeps the panel open", all == 4);

    // --- Keys: Up and Down move, Enter opens ---------------------------------------------
    key(&notes, VK_DOWN.0, None);
    key(&notes, VK_DOWN.0, None);
    let selected = rows(&notes).and_then(|rows| rows.1);
    checks.check("Down moves the selection", selected == Some(meeting));
    key(&notes, VK_UP.0, None);
    let selected = rows(&notes).and_then(|rows| rows.1);
    checks.check("Up moves it back", selected == Some(groceries));
    for _ in 0..10 {
        key(&notes, VK_DOWN.0, None);
    }
    let selected = rows(&notes).and_then(|rows| rows.1);
    checks.check("Down stops at the last row", selected == Some(budget));
    type_filter(&notes, "trip");
    key(&notes, VK_RETURN.0, Some(0x0d));
    let shown = wait(Duration::from_secs(3), || notes.test_open_ids().contains(&trip));
    checks.check("Enter shows the selected hidden note", shown);
    checks.check("the list follows: the note is listed as shown", wait_state(&notes, trip, NoteState::Shown));
    let filter_kept = rows(&notes).map(|rows| rows.0.len()) == Some(1);
    checks.check("the filter stays while the list changes", filter_kept);
    key(&notes, VK_ESCAPE.0, Some(0x1b));

    // --- Row actions: show or hide, delete -----------------------------------------------
    wait(Duration::from_secs(3), || notes.test_idle());
    let index = |notes: &Notes, id: NoteId| {
        rows(notes).and_then(|rows| rows.0.iter().position(|(row, _)| *row == id)).unwrap_or(0)
    };
    let row = index(&notes, trip);
    notes.test_panel(move |panel| panel.row_action(row, 0));
    checks.check("a row's hide action hides a shown note", wait_state(&notes, trip, NoteState::Hidden));
    checks.check("and its window goes", !notes.test_open_ids().contains(&trip));
    let row = index(&notes, groceries);
    notes.test_panel(move |panel| panel.row_action(row, 0));
    checks.check("a row's show action shows a closed note", wait_state(&notes, groceries, NoteState::Shown));
    checks.check("and its window opens", notes.test_open_ids().contains(&groceries));
    let row = index(&notes, budget);
    notes.test_panel(move |panel| panel.row_action(row, 1));
    let deleted = wait(Duration::from_secs(3), || {
        rows(&notes).is_some_and(|rows| rows.0.iter().all(|(id, _)| *id != budget))
    });
    checks.check("a row's delete action removes the note from the list", deleted);
    let trashed = moved.lock().map(|moved| moved.iter().any(|path| path.ends_with("Budget 2027.md"))).unwrap_or(false);
    checks.check("and its file goes to the (fake) Recycle Bin", trashed);

    // --- Hide all, Show all --------------------------------------------------------------
    wait(Duration::from_secs(3), || notes.test_idle());
    let row = index(&notes, trip);
    notes.test_panel(move |panel| panel.row_action(row, 0));
    wait_state(&notes, trip, NoteState::Shown);
    wait(Duration::from_secs(3), || notes.test_idle());
    let buttons = notes.test_panel(|panel| panel.button_hwnds().map(|hwnd| hwnd.0 as isize)).unwrap_or_default();
    click(HWND(buttons[0] as *mut _));
    let hidden = wait(Duration::from_secs(3), || {
        notes.test_open_ids().is_empty()
            && state_of(&notes, trip) == Some(NoteState::Hidden)
            && state_of(&notes, groceries) == Some(NoteState::Hidden)
    });
    checks.check("Hide all hides every shown note", hidden);
    checks.check("Hide all leaves closed notes closed", state_of(&notes, meeting) == Some(NoteState::Closed));
    wait(Duration::from_secs(3), || notes.test_idle());
    click(HWND(buttons[1] as *mut _));
    let shown = wait(Duration::from_secs(3), || {
        let open = notes.test_open_ids();
        open.contains(&trip) && open.contains(&groceries) && !open.contains(&meeting)
    });
    checks.check("Show all shows every hidden note (closed ones stay closed)", shown);

    // The two buttons one after the other, faster than the notes' motion.
    for pause in [0u64, 40, 150, 300] {
        for round in 0..3 {
            click(HWND(buttons[0] as *mut _));
            std::thread::sleep(Duration::from_millis(pause));
            click(HWND(buttons[1] as *mut _));
            std::thread::sleep(Duration::from_millis(pause));
            let _ = round;
        }
        let shown = wait(Duration::from_secs(3), || {
            let open = notes.test_open_ids();
            open.contains(&trip)
                && open.contains(&groceries)
                && state_of(&notes, trip) == Some(NoteState::Shown)
                && state_of(&notes, groceries) == Some(NoteState::Shown)
        });
        checks.check(&format!("Hide all then Show all, {pause} ms apart, three times: all shown"), shown);
        click(HWND(buttons[0] as *mut _));
        std::thread::sleep(Duration::from_millis(pause));
        click(HWND(buttons[1] as *mut _));
        std::thread::sleep(Duration::from_millis(pause));
        click(HWND(buttons[0] as *mut _));
        let hidden = wait(Duration::from_secs(3), || {
            notes.test_open_ids().is_empty()
                && state_of(&notes, trip) == Some(NoteState::Hidden)
                && state_of(&notes, groceries) == Some(NoteState::Hidden)
        });
        checks.check(&format!("then Hide, Show, Hide, {pause} ms apart: all hidden"), hidden);
        wait(Duration::from_secs(3), || notes.test_idle());
        click(HWND(buttons[1] as *mut _));
        wait_state(&notes, trip, NoteState::Shown);
        wait(Duration::from_secs(3), || notes.test_idle());
    }

    // --- Dark theme while open -----------------------------------------------------------
    wait(Duration::from_secs(3), || notes.test_idle());
    notes.set_theme(Theme::Dark);
    thread::sleep(Duration::from_millis(100));
    if let Some(hwnd) = notes.test_panel(|panel| panel.hwnd().0 as isize) {
        shot(&mut checks, HWND(hwnd as *mut _), &pictures, "notes-panel-dark.png")?;
    }

    // --- Esc closes, the tray item toggles -----------------------------------------------
    key(&notes, VK_ESCAPE.0, Some(0x1b));
    checks.check("Esc with an empty filter closes the panel", wait(Duration::from_secs(3), || rows(&notes).is_none()));
    notes.toggle_panel(None);
    wait(Duration::from_secs(3), || rows(&notes).is_some());
    notes.toggle_panel(None);
    checks.check("the tray item closes an open panel", wait(Duration::from_secs(3), || rows(&notes).is_none()));

    // --- A hidden note stays hidden after a restart --------------------------------------
    notes.hide(trip);
    wait(Duration::from_secs(3), || !notes.test_open_ids().contains(&trip));
    wait(Duration::from_secs(3), || notes.test_idle());
    checks.check("the app's notes stop in time", notes.shutdown(Duration::from_secs(2)));
    notes = start(&root, Theme::Light, &moved)?;
    wait(Duration::from_secs(3), || notes.test_open_ids().contains(&groceries));
    let open = notes.test_open_ids();
    checks.check(
        &format!("after a restart the shown note is back and the hidden one is not ({} open)", open.len()),
        open.contains(&groceries) && !open.contains(&trip),
    );
    notes.toggle_panel(None);
    wait(Duration::from_secs(3), || rows(&notes).is_some());
    checks.check("after a restart the panel lists it as hidden", state_of(&notes, trip) == Some(NoteState::Hidden));
    notes.reopen(trip);
    let back = wait(Duration::from_secs(3), || notes.test_open_ids().contains(&trip));
    checks.check("choosing a hidden note in the tray menu shows it", back);
    checks.check("and the panel says so", wait_state(&notes, trip, NoteState::Shown));
    wait(Duration::from_secs(3), || notes.test_idle());
    let windows = fs::read_to_string(root.join("windows.json")).unwrap_or_default();
    checks.check("shown again, nothing is hidden in the windows file", !windows.contains("isHidden"));
    checks.check("the notes stop in time", notes.shutdown(Duration::from_secs(2)));
    let _ = fs::remove_dir_all(&root);
    Ok(checks.ok)
}

fn main() {
    ensure_dpi_awareness();
    igorio_app::crash::install_at(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/notes-panel-selftest-crash.log"),
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
