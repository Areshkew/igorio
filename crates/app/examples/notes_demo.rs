//! Try the sticky notes by hand.
//!
//! cargo run -p igorio-app --example notes_demo --release
//!
//! Starts the notes thread with a notes folder under target/notes-demo/, opens a new note at the
//! cursor and brings back the notes left open last time. Type commands in this console:
//!
//!   n          a new note at the cursor
//!   a          put every note away
//!   r          list the recent notes
//!   o N        reopen recent note number N
//!   d          drop a generated test screenshot into the note that was active last
//!   q          quit (saves everything first)
//!
//! Deleted notes go to target/notes-demo/trash, not the Recycle Bin.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use igorio_app::notes::store::{NoteOrigin, Trash};
use igorio_app::notes::window::{Event, Notes, NotesConfig, RecentNote, Theme};
use igorio_app::overlay::{cursor_pos, ensure_dpi_awareness};
use igorio_core::geom::{CardId, Point};
use igorio_kit::held::HeldCard;
use igorio_kit::image::Image;
use windows::Win32::System::Threading::WaitForSingleObject;

/// Moves deleted notes into a folder under target/ instead of the Recycle Bin.
struct DemoTrash(PathBuf);

impl Trash for DemoTrash {
    fn trash(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.0)?;
        std::fs::rename(path, self.0.join(path.file_name().unwrap_or_default()))
    }
}

fn test_card(id: u64) -> HeldCard {
    let (width, height) = (640u32, 360u32);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let checker = (x / 32 + y / 32) % 2 == 0;
            rgba.extend([
                (40 + x * 180 / width) as u8,
                (90 + y * 120 / height) as u8,
                if checker { 230 } else { 190 },
                255,
            ]);
        }
    }
    HeldCard {
        id: CardId(id),
        image: Arc::new(Image::new(width, height, rgba).expect("image")),
        scale: 1.0,
        captured_at: SystemTime::now(),
        kept: false,
        tilt: 0.0,
        movie: None,
        duration: None,
    }
}

fn main() -> std::io::Result<()> {
    ensure_dpi_awareness();
    // The workspace target folder, without the \? prefix canonicalize would add.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .unwrap_or(manifest)
        .join("target")
        .join("notes-demo");
    std::fs::create_dir_all(&root)?;
    let notes = Notes::start(NotesConfig {
        folder: root.join("Notes"),
        windows_file: root.join("windows.json"),
        trash: Box::new(DemoTrash(root.join("trash"))),
        theme: Theme::System,
        restore: true,
        test: None,
    })?;
    println!("Notes folder: {}", root.join("Notes").display());
    println!(
        "Commands: n (new note), a (put all away), r (recent), o N (reopen), d (drop a test screenshot), q (quit)"
    );
    notes.open_new(cursor_pos(), NoteOrigin::desktop());

    let (lines, input) = mpsc::channel();
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    let mut recent: Vec<RecentNote> = Vec::new();
    let mut next_card = 1;
    loop {
        // SAFETY: waits on the notes module's own event, for at most 100 ms.
        let _ = unsafe { WaitForSingleObject(notes.wake_handle(), 100) };
        for event in notes.events() {
            match event {
                Event::Recent(list) => recent = list,
                Event::CardsTaken { ids, hint } => println!("({hint}: {ids:?})"),
                Event::Error(message) => println!("Error: {message}"),
            }
        }
        let Ok(line) = input.try_recv() else { continue };
        let mut words = line.split_whitespace();
        match words.next() {
            Some("n") => notes.open_new(cursor_pos(), NoteOrigin::desktop()),
            Some("a") => notes.put_away_all(),
            Some("r") => {
                notes.refresh();
                thread::sleep(Duration::from_millis(150));
                for event in notes.events() {
                    if let Event::Recent(list) = event {
                        recent = list;
                    }
                }
                if recent.is_empty() {
                    println!("No notes yet.");
                }
                for (index, note) in recent.iter().enumerate() {
                    let open = if note.is_open { " (open)" } else { "" };
                    println!("{index:>3}  {}  · {}{open}", note.title, note.label);
                }
            }
            Some("o") => {
                match words.next().and_then(|n| n.parse::<usize>().ok()).and_then(|n| recent.get(n))
                {
                    Some(note) => notes.reopen(note.id),
                    None => println!("Type r first, then o and a number from the list."),
                }
            }
            Some("d") => {
                let zones = notes.drop_zones().zones();
                match zones.iter().find(|zone| zone.active).or(zones.first()) {
                    Some(zone) => {
                        let at =
                            Point::new(zone.rect.mid_x(), zone.rect.y + zone.rect.height - 40.0);
                        notes.give_cards(at, vec![test_card(next_card)]);
                        next_card += 1;
                    }
                    None => println!("No note is open."),
                }
            }
            Some("q") => break,
            Some(other) => println!("Unknown command {other:?}."),
            None => {}
        }
    }
    let ended = notes.shutdown(Duration::from_secs(1));
    println!(
        "{}",
        if ended { "Saved and closed." } else { "The notes thread did not end in time." }
    );
    Ok(())
}
