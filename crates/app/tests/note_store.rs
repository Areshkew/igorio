//! Integration tests for the note store: file format, debounce deadline, renaming without
//! clobbering, delete, folder switching, refresh, windows file, and the value types.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use igorio_app::notes::store::{
    DEBOUNCE_SECONDS, MAX_STEM_UNITS, Note, NoteFile, NoteId, NoteKind, NoteOrigin, NoteStore,
    Trash, seed_bytes, windows_safe_stem,
};
use igorio_core::geom::Rect;

/// 2026-11-11T14:32:05Z, the timestamp used by the format test.
const CREATED: u64 = 1_794_407_525;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh folder under the temp directory, removed when the test ends.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "igorio-notes-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn folder(&self, name: &str) -> PathBuf {
        let folder = self.path(name);
        fs::create_dir_all(&folder).unwrap();
        folder
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A note whose fields are distinct enough to notice a mix-up.
fn note(id: NoteId, text: &str) -> Note {
    Note {
        id,
        text: text.to_owned(),
        created_at: at(CREATED),
        updated_at: at(CREATED),
        origin: NoteOrigin::desktop(),
        frame: Rect::new(10.0, 20.0, 320.0, 240.0),
        is_open: false,
        is_hidden: false,
    }
}

fn id(byte: u8) -> NoteId {
    let mut bytes = [0u8; 16];
    bytes[0] = byte;
    bytes[15] = 0xFF - byte;
    NoteId::from_bytes(bytes)
}

fn md_files(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(folder)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect();
    files.sort();
    files
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// Records what was trashed and removes the file, like the Recycle Bin moving it away.
#[derive(Clone, Default)]
struct FakeTrash {
    paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl Trash for FakeTrash {
    fn trash(&self, path: &Path) -> io::Result<()> {
        self.paths.lock().unwrap().push(path.to_path_buf());
        fs::remove_file(path)
    }
}

#[test]
fn a_new_note_is_written_when_tick_reaches_its_deadline() {
    let scratch = Scratch::new("debounce");
    let folder = scratch.folder("notes");
    let mut store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    let note_id = id(1);

    // A blank note has no file until something is typed.
    store.insert(note(note_id, " \n\t")).unwrap();
    assert!(md_files(&folder).is_empty());
    assert_eq!(store.next_deadline(), None);

    assert!(store.update(note_id, 10.0, |note| note.text = "Hello".to_owned()));
    assert_eq!(store.next_deadline(), Some(10.0 + DEBOUNCE_SECONDS));

    assert!(store.tick(10.0 + DEBOUNCE_SECONDS - 0.001).is_empty());
    assert!(md_files(&folder).is_empty(), "must wait for the deadline");

    assert!(store.tick(10.0 + DEBOUNCE_SECONDS).is_empty());
    let files = md_files(&folder);
    assert_eq!(files.len(), 1);
    assert!(fs::read_to_string(&files[0]).unwrap().ends_with("\n\nHello"));
    assert_eq!(store.next_deadline(), None);
}

#[test]
fn flush_writes_at_once() {
    let scratch = Scratch::new("flush");
    let folder = scratch.folder("notes");
    let mut store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    let note_id = id(2);
    store.insert(note(note_id, " ")).unwrap();
    store.update(note_id, 1.0, |note| note.text = "Saved now".to_owned());
    assert!(md_files(&folder).is_empty());

    assert!(store.flush().is_empty());
    let files = md_files(&folder);
    assert_eq!(files.len(), 1);
    assert!(fs::read_to_string(&files[0]).unwrap().contains("Saved now"));
    assert_eq!(store.next_deadline(), None);
}

#[test]
fn the_file_format_is_exactly_the_swift_format() {
    let scratch = Scratch::new("format");
    let folder = scratch.folder("notes");
    let windows = scratch.path("note-windows.json");
    let mut store = NoteStore::new(&folder, &windows, at(CREATED));
    let note_id = NoteId::from_bytes([
        0x6F, 0x9A, 0x1B, 0x2C, 0x3D, 0x4E, 0x4F, 0x5A, 0x6B, 0x7C, 0x8D, 0x9E, 0xAF, 0xB0,
        0xC1, 0xD2,
    ]);
    let known = Note {
        id: note_id,
        text: "# Groceries\nMilk\nEggs".to_owned(),
        created_at: at(CREATED),
        updated_at: at(CREATED),
        origin: NoteOrigin {
            kind: NoteKind::Web,
            app_name: Some("Safari".to_owned()),
            bundle_id: Some("com.apple.Safari".to_owned()),
            window_title: Some("GitHub".to_owned()),
            url: Some("https://github.com/rust-lang".to_owned()),
        },
        frame: Rect::new(10.5, 20.5, 320.0, 240.0),
        is_open: true,
        is_hidden: false,
    };
    store.insert(known.clone()).unwrap();

    let files = md_files(&folder);
    assert_eq!(files.len(), 1);
    let text = fs::read_to_string(&files[0]).unwrap();
    let expected = "\
---
id: 6F9A1B2C-3D4E-4F5A-6B7C-8D9EAFB0C1D2
created: 2026-11-11T14:32:05Z
from: \"github.com\"
kind: web
app: \"Safari\"
bundle: \"com.apple.Safari\"
window: \"GitHub\"
url: \"https://github.com/rust-lang\"
---

# Groceries
Milk
Eggs";
    assert_eq!(text, expected);
    assert_eq!(file_name(&files[0]), "Groceries.md");

    let modified = fs::metadata(&files[0]).unwrap().modified().unwrap();
    drop(store);
    let reopened = NoteStore::new(&folder, &windows, at(CREATED));
    let mut expected_note = known.clone();
    // Like Swift, `updatedAt` comes from the file's modification date, not from the content.
    expected_note.updated_at = modified;
    assert_eq!(reopened.note(note_id), Some(&expected_note));
}

#[test]
fn a_byte_order_mark_is_skipped_and_never_written_back() {
    let scratch = Scratch::new("bom");
    let folder = scratch.folder("notes");
    let note_id = id(43);
    let plain = NoteFile::render(&note(note_id, "Bom note"));
    let with_mark = format!("\u{FEFF}{plain}");

    // The same file with and without the mark gives the same metadata and body.
    assert_eq!(NoteFile::parse(&with_mark), NoteFile::parse(&plain));

    fs::write(folder.join("Bom note.md"), with_mark.as_bytes()).unwrap();
    let store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    let loaded = store.note(note_id).expect("the id comes from the block");
    assert_eq!(loaded.text, "Bom note");
    assert_eq!(loaded.origin, NoteOrigin::desktop());
}

#[test]
fn a_note_read_with_a_mark_is_written_back_without_one() {
    let scratch = Scratch::new("bom-write");
    let folder = scratch.folder("notes");
    let note_id = id(44);
    let with_mark = format!("\u{FEFF}{}", NoteFile::render(&note(note_id, "Bom note")));
    fs::write(folder.join("Bom note.md"), with_mark.as_bytes()).unwrap();

    let mut store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    store.update(note_id, 0.0, |note| {
        note.text = "Bom note edited".to_owned()
    });
    assert!(store.flush().is_empty());

    let bytes = fs::read(store.file_path(note_id).unwrap()).unwrap();
    assert!(
        !bytes.starts_with(b"\xEF\xBB\xBF"),
        "the byte order mark is not written back"
    );
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(NoteFile::parse(&text).0.id, Some(note_id));

    let reopened = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    assert_eq!(reopened.note(note_id).unwrap().text, "Bom note edited");
}

#[test]
fn a_note_without_a_mark_round_trips_byte_for_byte() {
    let scratch = Scratch::new("round-trip");
    let folder = scratch.folder("notes");
    let windows = scratch.path("note-windows.json");
    let note_id = id(45);
    {
        let mut store = NoteStore::new(&folder, &windows, at(CREATED));
        store
            .insert(note(note_id, "# Heading\nMilk\nEggs"))
            .unwrap();
    }
    let path = md_files(&folder).pop().unwrap();
    let before = fs::read(&path).unwrap();

    let mut reopened = NoteStore::new(&folder, &windows, at(CREATED));
    // Mark the note dirty without changing it: a flush right after loading must rewrite the
    // same bytes.
    assert!(reopened.update(note_id, 0.0, |_| {}));
    assert!(reopened.flush().is_empty());
    assert_eq!(
        fs::read(reopened.file_path(note_id).unwrap()).unwrap(),
        before
    );
}

#[test]
fn a_zero_byte_markdown_file_is_not_a_note() {
    let scratch = Scratch::new("zero-byte");
    let folder = scratch.folder("notes");
    let empty = folder.join("Empty.md");
    fs::write(&empty, b"").unwrap();

    // A crash between `claim_file` and the write leaves an empty file behind; it is not a
    // note, and it is left in place because another program may be about to write it.
    let mut store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    assert!(store.notes().next().is_none());
    store.refresh(&[], at(CREATED));
    assert!(store.notes().next().is_none());
    assert!(empty.exists(), "the empty file is not deleted");

    // Whitespace alone is still a note, blank because it has no metadata block.
    fs::write(&empty, " \n\t").unwrap();
    store.refresh(&[], at(CREATED));
    assert!(store.notes().next().expect("a blank note").is_blank());

    // A file that starts empty and gets content is picked up by the next refresh.
    let later = folder.join("Later.md");
    fs::write(&later, b"").unwrap();
    store.refresh(&[], at(CREATED));
    assert_eq!(store.notes().count(), 1);
    fs::write(&later, NoteFile::render(&note(id(46), "Later"))).unwrap();
    store.refresh(&[], at(CREATED));
    assert_eq!(store.note(id(46)).unwrap().text, "Later");
}

#[test]
fn title_uses_the_first_non_empty_line_without_markdown_markers() {
    let cases = [
        ("\n\n# Heading\nbody", "Heading"),
        ("## **Bold** title", "Bold title"),
        ("- item", "item"),
        ("- [ ] task", "task"),
        ("* [x] done", "done"),
        ("1. one", "one"),
        ("2) two", "two"),
        ("> quote", "quote"),
        (">quote", "quote"),
        ("`code` and _style_", "code and style"),
        ("plain first line", "plain first line"),
        ("***", "***"),
    ];
    for (text, expected) in cases {
        assert_eq!(note(id(3), text).title(), expected, "text {text:?}");
    }
    assert_eq!(note(id(3), " \n\t\n").title(), "Empty note");
}

#[test]
fn a_blank_note_is_untitled() {
    let blank = note(id(4), " \n\t\n");
    assert!(blank.is_blank());
    assert_eq!(blank.title(), "Empty note");
    assert_eq!(blank.file_stem(), "Untitled");
}

#[test]
fn a_long_first_line_is_cut_to_48_characters() {
    let text = "a".repeat(60);
    let title = note(id(5), &text).title();
    assert_eq!(title.chars().count(), 48);
    assert_eq!(title.chars().take(47).collect::<String>(), "a".repeat(47));
    assert!(title.ends_with('…'));

    let stem = note(id(5), &text).file_stem();
    assert_eq!(stem, "a".repeat(47));
}

#[test]
fn windows_invalid_characters_never_appear_in_a_file_stem() {
    // The Swift rules remove ':' and turn '/' and '\' into '-'; the Windows rules must catch
    // the rest and every control character.
    for character in ['<', '>', '"', '/', '\\', '|', '?', '\u{1}', '\u{1f}'] {
        let stem = note(id(6), &format!("a{character}b")).file_stem();
        assert_eq!(stem, "a-b", "character {character:?}");
        assert!(
            !stem.chars().any(|c| matches!(
                c,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) || c.is_control()),
            "character {character:?} survived as {stem:?}"
        );
    }
    assert_eq!(note(id(6), "a:b").file_stem(), "ab", "Swift removes ':'");
    assert_eq!(note(id(6), "a*b").file_stem(), "ab", "Swift strips Markdown markers");
    assert_eq!(note(id(6), "c:\\temp").file_stem(), "c-temp");
}

#[test]
fn reserved_device_names_get_a_prefix() {
    for name in ["CON", "PRN", "AUX", "NUL", "COM1", "COM9", "LPT1", "LPT9"] {
        assert_eq!(
            note(id(7), name).file_stem(),
            format!("_{name}"),
            "device name {name}"
        );
    }
    assert_eq!(note(id(7), "con.txt").file_stem(), "_con.txt");
    // Only 1 through 9 are reserved, and the check is case-insensitive.
    assert_eq!(windows_safe_stem("COM0", 100), "COM0");
    assert_eq!(windows_safe_stem("LPT10", 100), "LPT10");
    assert_eq!(windows_safe_stem("console", 100), "console");
}

#[test]
fn trailing_dots_and_spaces_are_removed() {
    assert_eq!(windows_safe_stem("Draft...", 100), "Draft");
    assert_eq!(windows_safe_stem("name. . ", 100), "name");
    assert_eq!(windows_safe_stem("...", 100), "Untitled");
    assert_eq!(note(id(8), "Draft... ").file_stem(), "Draft");
}

#[test]
fn a_stem_is_capped_for_a_usable_full_path() {
    let long = "b".repeat(500);
    assert_eq!(windows_safe_stem(&long, MAX_STEM_UNITS).chars().count(), MAX_STEM_UNITS);
    assert_eq!(windows_safe_stem(&long, 10).chars().count(), 10);
    assert_eq!(windows_safe_stem("CON", 3), "_CO", "reserved check runs after the cut");
    assert_eq!(windows_safe_stem("", 100), "Untitled");
}

#[test]
fn two_notes_with_the_same_title_get_numbered_files() {
    let scratch = Scratch::new("same-title");
    let folder = scratch.folder("notes");
    let mut store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    let first = id(10);
    let second = id(11);
    let third = id(12);
    store.insert(note(first, "Same")).unwrap();
    store.insert(note(second, "Same")).unwrap();
    store.insert(note(third, "Same")).unwrap();

    assert_eq!(file_name(store.file_path(first).unwrap()), "Same.md");
    assert_eq!(file_name(store.file_path(second).unwrap()), "Same 2.md");
    assert_eq!(file_name(store.file_path(third).unwrap()), "Same 3.md");
    assert_eq!(md_files(&folder).len(), 3);
}

#[test]
fn changing_the_title_renames_the_file_without_clobbering_another_note() {
    let scratch = Scratch::new("rename");
    let folder = scratch.folder("notes");
    let mut store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    let changing = id(20);
    let other = id(21);
    store.insert(note(changing, "One")).unwrap();
    store.insert(note(other, "Two")).unwrap();
    let other_path = store.file_path(other).unwrap().to_path_buf();

    store.update(changing, 0.0, |note| note.text = "Two".to_owned());
    assert!(store.flush().is_empty());

    assert_eq!(file_name(store.file_path(changing).unwrap()), "Two 2.md");
    assert_eq!(store.file_path(other).unwrap(), other_path);
    assert!(fs::read_to_string(store.file_path(other).unwrap())
        .unwrap()
        .ends_with("\n\nTwo"));
    assert!(fs::read_to_string(store.file_path(changing).unwrap())
        .unwrap()
        .contains(&changing.to_string()));

    // The " 2" suffix belongs to this note, so its later edits keep the same file.
    store.update(changing, 1.0, |note| note.text = "Two\nmore".to_owned());
    assert!(store.flush().is_empty());
    assert_eq!(file_name(store.file_path(changing).unwrap()), "Two 2.md");
}

#[test]
fn delete_removes_the_note_and_trashes_its_file() {
    let scratch = Scratch::new("delete");
    let folder = scratch.folder("notes");
    let windows = scratch.path("note-windows.json");
    let fake = FakeTrash::default();
    let mut store = NoteStore::with_trash(&folder, &windows, at(CREATED), Box::new(fake.clone()));
    let note_id = id(30);
    store.insert(note(note_id, "Throw me away")).unwrap();
    let path = store.file_path(note_id).unwrap().to_path_buf();

    let deleted = store.delete(note_id).expect("the note was there");
    assert_eq!(deleted.path.as_deref(), Some(path.as_path()));
    assert!(deleted.trashed);
    assert!(!path.exists());
    assert!(store.note(note_id).is_none());
    assert_eq!(fake.paths.lock().unwrap().as_slice(), &[path]);
    assert_eq!(store.delete(note_id), None);

    let saved = fs::read_to_string(&windows).unwrap();
    assert!(!saved.contains(&note_id.to_string()));
}

#[test]
fn switch_folder_flushes_the_old_one_and_reloads_the_new_one() {
    let scratch = Scratch::new("switch");
    let folder_a = scratch.folder("a");
    let folder_b = scratch.folder("b");
    let windows = scratch.path("note-windows.json");

    // Put a note in folder B with a separate store and windows file.
    {
        let mut other = NoteStore::new(&folder_b, scratch.path("b-windows.json"), at(CREATED));
        other.insert(note(id(41), "Beta")).unwrap();
    }

    let mut store = NoteStore::new(&folder_a, &windows, at(CREATED));
    let alpha = id(40);
    store.insert(note(alpha, "Alpha")).unwrap();
    store.update(alpha, 0.0, |note| note.text = "Alpha edited".to_owned());

    assert!(store.switch_folder(&folder_b, at(CREATED)).is_empty());
    assert_eq!(store.folder(), folder_b);
    assert!(store.note(alpha).is_none());
    assert_eq!(store.note(id(41)).unwrap().text, "Beta");

    // The pending edit in folder A was flushed before switching.
    let alpha_file = md_files(&folder_a).pop().unwrap();
    assert!(fs::read_to_string(alpha_file).unwrap().contains("Alpha edited"));

    assert!(store.switch_folder(&folder_a, at(CREATED)).is_empty());
    assert_eq!(store.note(alpha).unwrap().text, "Alpha edited");
    assert!(store.note(id(41)).is_none());
}

#[test]
fn refresh_picks_up_outside_edits_additions_and_deletions() {
    let scratch = Scratch::new("refresh");
    let folder = scratch.folder("notes");
    let mut store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    let edited = id(50);
    let removed = id(51);
    let open = id(52);
    store.insert(note(edited, "Alpha")).unwrap();
    store.insert(note(removed, "Bravo")).unwrap();
    store.insert(note(open, "Charlie")).unwrap();
    let edited_path = store.file_path(edited).unwrap().to_path_buf();
    let removed_path = store.file_path(removed).unwrap().to_path_buf();
    let open_path = store.file_path(open).unwrap().to_path_buf();

    let mut changed = note(edited, "Alpha edited");
    changed.origin = NoteOrigin {
        kind: NoteKind::App,
        app_name: Some("Notepad".to_owned()),
        ..NoteOrigin::default()
    };
    fs::write(&edited_path, NoteFile::render(&changed)).unwrap();

    let added = id(53);
    fs::write(
        folder.join("Added.md"),
        NoteFile::render(&note(added, "Delta")),
    )
    .unwrap();
    fs::write(folder.join("plain.md"), "Just text, no metadata").unwrap();

    fs::remove_file(&removed_path).unwrap();
    fs::remove_file(&open_path).unwrap();

    store.refresh(&[open], at(CREATED));

    let edited_note = store.note(edited).unwrap();
    assert_eq!(edited_note.text, "Alpha edited");
    assert_eq!(edited_note.origin.app_name.as_deref(), Some("Notepad"));
    assert!(store.note(removed).is_none(), "a vanished file is dropped");
    assert!(
        store.note(open).is_some(),
        "an open note stays when its file vanishes"
    );
    assert_eq!(store.note(added).unwrap().text, "Delta");
    let plain = store
        .notes()
        .find(|note| note.text == "Just text, no metadata")
        .expect("a file without metadata is a note whose whole text is the body");
    assert_eq!(plain.title(), "Just text, no metadata");
}

#[test]
fn window_state_round_trips_through_the_windows_file() {
    let scratch = Scratch::new("windows");
    let folder = scratch.folder("notes");
    let windows = scratch.path("note-windows.json");
    let note_id = id(60);
    let placed = Note {
        frame: Rect::new(12.5, 34.5, 300.0, 240.0),
        is_open: true,
        ..note(note_id, "Remember my place")
    };
    {
        let mut store = NoteStore::new(&folder, &windows, at(CREATED));
        store.insert(placed.clone()).unwrap();
    }

    let reopened = NoteStore::new(&folder, &windows, at(CREATED));
    let loaded = reopened.note(note_id).expect("the note file is there");
    assert_eq!(loaded.frame, placed.frame);
    assert!(loaded.is_open);
    assert!(!loaded.is_hidden);
    assert_eq!(loaded.text, placed.text);
}

#[test]
fn the_hidden_state_survives_a_restart_and_old_files_read_as_shown() {
    let scratch = Scratch::new("hidden");
    let folder = scratch.folder("notes");
    let windows = scratch.path("note-windows.json");
    let (hidden, shown) = (id(61), id(62));
    {
        let mut store = NoteStore::new(&folder, &windows, at(CREATED));
        store.insert(Note { is_open: true, ..note(shown, "On screen") }).unwrap();
        store.insert(Note { is_open: true, ..note(hidden, "Out of the way") }).unwrap();
        store.update(hidden, 1.0, |note| note.is_hidden = true);
        assert!(store.flush().is_empty());
    }
    let file = fs::read_to_string(&windows).unwrap();
    // Only a hidden note carries the key; the others stay as the Swift app wrote them.
    assert_eq!(file.matches("isHidden").count(), 1, "{file}");

    let mut reopened = NoteStore::new(&folder, &windows, at(CREATED));
    assert!(reopened.note(hidden).unwrap().is_hidden);
    assert!(reopened.note(hidden).unwrap().is_open);
    assert!(!reopened.note(shown).unwrap().is_hidden);

    // Shown again: the key goes away.
    reopened.update(hidden, 2.0, |note| note.is_hidden = false);
    assert!(reopened.flush().is_empty());
    assert!(!fs::read_to_string(&windows).unwrap().contains("isHidden"));

    // A windows file without the key (as the Swift app and older builds write it) reads as
    // open and not hidden.
    let older = NoteStore::new(&folder, &windows, at(CREATED));
    assert!(!older.note(hidden).unwrap().is_hidden);
    assert!(older.note(hidden).unwrap().is_open);
}

#[test]
fn a_corrupt_windows_file_does_not_lose_notes() {
    let scratch = Scratch::new("corrupt-windows");
    let folder = scratch.folder("notes");
    let windows = scratch.path("note-windows.json");
    {
        let mut store = NoteStore::new(&folder, &windows, at(CREATED));
        store.insert(note(id(70), "Still here")).unwrap();
    }
    fs::write(&windows, b"not json at all").unwrap();

    let reopened = NoteStore::new(&folder, &windows, at(CREATED));
    assert_eq!(reopened.note(id(70)).unwrap().text, "Still here");
    assert!(
        scratch.path("note-windows.json.bad").exists(),
        "the unparsable file is kept"
    );
}

#[test]
fn recent_is_sorted_by_updated_at_newest_first() {
    let scratch = Scratch::new("recent");
    let folder = scratch.folder("notes");
    let mut store = NoteStore::new(&folder, scratch.path("note-windows.json"), at(CREATED));
    let oldest = id(80);
    let middle = id(81);
    let newest = id(82);
    store.insert(note(oldest, "Oldest")).unwrap();
    store.insert(note(middle, "Middle")).unwrap();
    store.insert(note(newest, "Newest")).unwrap();
    store.update(oldest, 0.0, |note| note.updated_at = at(CREATED + 10));
    store.update(middle, 0.0, |note| note.updated_at = at(CREATED + 20));
    store.update(newest, 0.0, |note| note.updated_at = at(CREATED + 30));

    let ids: Vec<NoteId> = store.recent().iter().map(|note| note.id).collect();
    assert_eq!(ids, vec![newest, middle, oldest]);
}

#[test]
fn origin_labels_and_details_match_swift() {
    assert_eq!(NoteOrigin::desktop().label(), "Desktop");
    assert_eq!(NoteOrigin::desktop().detail(), "Desktop");

    let app = NoteOrigin {
        kind: NoteKind::App,
        app_name: Some("Xcode".to_owned()),
        bundle_id: Some("com.apple.dt.Xcode".to_owned()),
        window_title: Some("MyProject".to_owned()),
        url: None,
    };
    assert_eq!(app.label(), "Xcode");
    assert_eq!(app.detail(), "Xcode\nMyProject");

    let document = NoteOrigin {
        kind: NoteKind::App,
        app_name: Some("Preview".to_owned()),
        window_title: None,
        url: Some("file:///Users/me/Documents/paper%20one.pdf".to_owned()),
        ..NoteOrigin::default()
    };
    assert_eq!(document.label(), "paper one.pdf");
    assert_eq!(
        document.detail(),
        "Preview\nfile:///Users/me/Documents/paper%20one.pdf"
    );

    let web = NoteOrigin {
        kind: NoteKind::Web,
        app_name: Some("Safari".to_owned()),
        bundle_id: Some("com.apple.Safari".to_owned()),
        window_title: Some("GitHub".to_owned()),
        url: Some("https://www.github.com/rust-lang".to_owned()),
    };
    assert_eq!(web.label(), "github.com");
    assert_eq!(
        web.detail(),
        "Safari\nGitHub\nhttps://www.github.com/rust-lang"
    );

    let web_without_url = NoteOrigin {
        kind: NoteKind::Web,
        app_name: Some("Firefox".to_owned()),
        ..NoteOrigin::default()
    };
    assert_eq!(web_without_url.label(), "Firefox");
    assert_eq!(web_without_url.detail(), "Firefox");
    assert_eq!(
        NoteOrigin {
            kind: NoteKind::Web,
            ..NoteOrigin::default()
        }
        .label(),
        "Web"
    );
}

#[test]
fn note_ids_format_and_parse_canonically() {
    let note_id = NoteId::from_bytes([
        0x6F, 0x9A, 0x1B, 0x2C, 0x3D, 0x4E, 0x4F, 0x5A, 0x6B, 0x7C, 0x8D, 0x9E, 0xAF, 0xB0,
        0xC1, 0xD2,
    ]);
    let text = "6F9A1B2C-3D4E-4F5A-6B7C-8D9EAFB0C1D2";
    assert_eq!(note_id.to_string(), text);
    assert_eq!(NoteId::parse(text), Some(note_id));
    assert_eq!(
        "6f9a1b2c-3d4e-4f5a-6b7c-8d9eafb0c1d2".parse::<NoteId>(),
        Ok(note_id)
    );
    assert_eq!(NoteId::parse("6F9A1B2C3D4E4F5A6B7C8D9EAFB0C1D2"), None);
    assert_eq!(NoteId::parse("6F9A1B2C-3D4E-4F5A-6B7C-8D9EAFB0C1D"), None);

    let random = NoteId::new_random([0xFF; 16]);
    assert_eq!(random.as_bytes()[6] & 0xF0, 0x40);
    assert_eq!(random.as_bytes()[8] & 0xC0, 0x80);
    assert_ne!(seed_bytes(at(CREATED), 0), seed_bytes(at(CREATED), 1));
}
