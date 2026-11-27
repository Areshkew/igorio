use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use igorio_app::layout::CardSize;
use igorio_app::settings::{DefaultFolders, Settings, default_settings_path};
use igorio_app::shortcuts::{Action, Chord, Shortcuts};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh folder under the temp directory, removed when the test ends.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "igorio-settings-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn defaults(scratch: &Scratch) -> DefaultFolders {
    DefaultFolders {
        notes: scratch.path("default-notes"),
        captures: scratch.path("default-captures"),
    }
}

#[test]
fn defaults_are_empty_and_off() {
    let settings = Settings::default();
    assert!(settings.capabilities.is_empty());
    assert_eq!(settings.notes_folder, None);
    assert_eq!(settings.captures_folder, None);
    assert!(!settings.keep_every_screenshot);
    assert!(!settings.launch_at_login);
    assert_eq!(settings.card_size, CardSize::Small);
    assert_eq!(
        settings,
        Settings::load(&PathBuf::from("does-not-exist-anywhere.json")).unwrap()
    );
}

#[test]
fn an_unknown_card_size_keeps_the_other_settings() {
    let scratch = Scratch::new("card-size");
    let path = scratch.path("settings.json");
    fs::write(
        &path,
        r#"{"card_size": "huge", "launch_at_login": true}"#,
    )
    .unwrap();

    let settings = Settings::load(&path).unwrap();
    assert_eq!(settings.card_size, CardSize::Small);
    assert!(settings.launch_at_login, "the rest of the file must survive");
    assert!(!scratch.path("settings.json.bad").exists(), "the file was parsed");

    fs::write(&path, r#"{"card_size": "large"}"#).unwrap();
    assert_eq!(Settings::load(&path).unwrap().card_size, CardSize::Large);
}

#[test]
fn unlisted_capability_is_enabled() {
    let mut settings = Settings::default();
    assert!(settings.is_enabled("notes"));
    assert!(settings.is_enabled("screenshots"));

    settings.set_enabled("notes", false);
    assert!(!settings.is_enabled("notes"));
    assert!(settings.is_enabled("screenshots"));

    settings.set_enabled("notes", true);
    assert!(settings.is_enabled("notes"));
}

#[test]
fn save_then_load_round_trips() {
    let scratch = Scratch::new("round-trip");
    let path = scratch.path("nested/settings.json");

    let mut settings = Settings::default();
    settings.capabilities.insert("notes".to_owned(), false);
    settings.capabilities.insert("screenshots".to_owned(), true);
    settings.notes_folder = Some(scratch.path("my notes"));
    settings.captures_folder = Some(scratch.path("my captures"));
    settings.keep_every_screenshot = true;
    settings.launch_at_login = true;

    settings.save(&path).unwrap();
    assert_eq!(Settings::load(&path).unwrap(), settings);
}

#[test]
fn missing_file_gives_defaults() {
    let scratch = Scratch::new("missing");
    assert_eq!(
        Settings::load(&scratch.path("settings.json")).unwrap(),
        Settings::default()
    );
}

#[test]
fn corrupt_file_gives_defaults_and_keeps_a_bad_copy() {
    let scratch = Scratch::new("corrupt");
    let path = scratch.path("settings.json");
    let junk = "{ this is not json";
    fs::write(&path, junk).unwrap();

    assert_eq!(Settings::load(&path).unwrap(), Settings::default());
    let bad = scratch.path("settings.json.bad");
    assert!(bad.is_file(), "expected {:?} to exist", bad);
    assert_eq!(fs::read_to_string(&bad).unwrap(), junk);
}

#[test]
fn older_bad_copies_are_kept_up_to_nine() {
    let scratch = Scratch::new("bad-copies");
    let path = scratch.path("settings.json");
    let junk = "{ this is not json";
    fs::write(&path, junk).unwrap();

    for _ in 0..3 {
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
    }
    for name in ["settings.json.bad", "settings.json.bad.1", "settings.json.bad.2"] {
        assert_eq!(fs::read_to_string(scratch.path(name)).unwrap(), junk, "{name}");
    }

    for n in 3..=8 {
        fs::write(scratch.path(&format!("settings.json.bad.{n}")), "older").unwrap();
    }
    Settings::load(&path).unwrap();
    assert_eq!(
        fs::read_to_string(scratch.path("settings.json.bad.9")).unwrap(),
        junk
    );

    // An existing .bad.9 is the last one and is overwritten, nothing newer is made.
    fs::write(scratch.path("settings.json.bad.9"), "older").unwrap();
    Settings::load(&path).unwrap();
    assert_eq!(
        fs::read_to_string(scratch.path("settings.json.bad.9")).unwrap(),
        junk
    );
    assert_eq!(
        fs::read_to_string(scratch.path("settings.json.bad.1")).unwrap(),
        junk,
        "an older copy must not be overwritten"
    );
    assert!(!scratch.path("settings.json.bad.10").exists());
}

#[test]
fn a_utf8_bom_is_skipped() {
    let scratch = Scratch::new("bom");
    let path = scratch.path("settings.json");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(br#"{"launch_at_login": true}"#);
    fs::write(&path, bytes).unwrap();

    let settings = Settings::load(&path).unwrap();
    assert!(settings.launch_at_login);
    assert!(!scratch.path("settings.json.bad").exists());
}

#[test]
fn unknown_fields_are_ignored_and_missing_fields_default() {
    let scratch = Scratch::new("partial");
    let path = scratch.path("settings.json");
    fs::write(
        &path,
        r#"{"launch_at_login": true, "capabilities": {"notes": false}, "future_setting": [1, 2]}"#,
    )
    .unwrap();

    let settings = Settings::load(&path).unwrap();
    assert!(settings.launch_at_login);
    assert!(!settings.is_enabled("notes"));
    assert!(settings.is_enabled("screenshots"));
    assert_eq!(settings.notes_folder, None);
    assert_eq!(settings.captures_folder, None);
    assert!(!settings.keep_every_screenshot);
    assert_eq!(
        settings.capabilities,
        BTreeMap::from([("notes".to_owned(), false)])
    );
}

#[test]
fn a_locked_file_is_an_error_not_defaults() {
    use std::os::windows::fs::OpenOptionsExt;

    let scratch = Scratch::new("locked");
    let path = scratch.path("settings.json");
    Settings {
        launch_at_login: true,
        ..Settings::default()
    }
    .save(&path)
    .unwrap();

    // Another program (antivirus, a sync tool) holding the file with no sharing.
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    let error = Settings::load(&path).unwrap_err();
    assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
    drop(lock);

    // Once the file is free again, the real settings are still there.
    assert!(Settings::load(&path).unwrap().launch_at_login);
}

#[test]
fn save_creates_the_parent_folder_and_leaves_no_temp_file() {
    let scratch = Scratch::new("atomic");
    let path = scratch.path("deep/deeper/settings.json");

    Settings::default().save(&path).unwrap();

    assert!(path.is_file());
    let names: Vec<String> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["settings.json"]);
}

#[test]
fn save_syncs_the_temp_file_before_the_rename() {
    let scratch = Scratch::new("sync");
    let path = scratch.path("settings.json");
    let settings = Settings {
        launch_at_login: true,
        ..Settings::default()
    };

    settings.save(&path).unwrap();

    // The write handle is closed and the whole document is on disk, not a prefix.
    let bytes = fs::read(&path).unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(Settings::load(&path).unwrap(), settings);
}

#[test]
fn notes_dir_uses_and_creates_the_chosen_folder() {
    let scratch = Scratch::new("notes-dir");
    let chosen = scratch.path("chosen-notes");
    let defaults = defaults(&scratch);
    let settings = Settings {
        notes_folder: Some(chosen.clone()),
        ..Settings::default()
    };

    assert_eq!(settings.notes_dir(&defaults), (chosen.clone(), false));
    assert!(chosen.is_dir());
}

#[test]
fn folders_are_created_without_write_probes() {
    let scratch = Scratch::new("no-probe");
    let chosen = scratch.path("chosen");
    let defaults = defaults(&scratch);
    let settings = Settings {
        notes_folder: Some(chosen.clone()),
        ..Settings::default()
    };

    assert_eq!(settings.notes_dir(&defaults), (chosen.clone(), false));
    // No probe file (or anything else) was left behind.
    assert_eq!(fs::read_dir(&chosen).unwrap().count(), 0);
}

#[test]
fn relative_chosen_folders_are_unusable() {
    let scratch = Scratch::new("relative");
    let defaults = defaults(&scratch);
    let relative = format!("igorio-rel-notes-{}", std::process::id());

    for chosen in [PathBuf::from(&relative), PathBuf::from("C:notes")] {
        let settings = Settings {
            notes_folder: Some(chosen),
            ..Settings::default()
        };
        assert_eq!(settings.notes_dir(&defaults), (defaults.notes.clone(), true));
    }
    // Nothing was created relative to the current directory.
    assert!(!std::env::current_dir().unwrap().join(&relative).exists());
}

#[test]
fn unwritable_chosen_folder_falls_back_and_creates_the_default() {
    let scratch = Scratch::new("fallback");
    let blocker = scratch.path("blocker");
    fs::write(&blocker, b"a file, not a folder").unwrap();
    let defaults = defaults(&scratch);
    let settings = Settings {
        notes_folder: Some(blocker.join("below-a-file")),
        ..Settings::default()
    };

    assert_eq!(settings.notes_dir(&defaults), (defaults.notes.clone(), true));
    assert!(defaults.notes.is_dir());
}

#[test]
fn no_chosen_folder_uses_and_creates_the_default() {
    let scratch = Scratch::new("default-folder");
    let defaults = defaults(&scratch);
    let settings = Settings::default();
    assert!(!defaults.notes.exists());
    assert!(!defaults.captures.exists());

    assert_eq!(settings.notes_dir(&defaults), (defaults.notes.clone(), false));
    assert_eq!(settings.captures_dir(&defaults), (defaults.captures.clone(), false));
    assert!(defaults.notes.is_dir());
    assert!(defaults.captures.is_dir());
}

#[test]
fn pasted_dir_is_captures_pasted_and_created() {
    let scratch = Scratch::new("pasted");
    let defaults = defaults(&scratch);
    let mut settings = Settings::default();
    let pasted = settings.pasted_dir(&defaults);
    assert_eq!(pasted, defaults.captures.join("Pasted"));
    assert!(pasted.is_dir());

    let chosen = scratch.path("chosen-captures");
    settings.captures_folder = Some(chosen.clone());
    let pasted = settings.pasted_dir(&defaults);
    assert_eq!(pasted, chosen.join("Pasted"));
    assert!(pasted.is_dir());
}

#[test]
fn system_defaults_are_absolute() {
    let folders = DefaultFolders::from_system();
    assert!(folders.notes.is_absolute(), "{:?}", folders.notes);
    assert!(folders.captures.is_absolute(), "{:?}", folders.captures);
}

#[test]
fn settings_path_lives_under_igorios_roaming_folder() {
    let path = default_settings_path();
    assert!(path.is_absolute());
    assert_eq!(path.file_name().unwrap(), "settings.json");
    assert_eq!(path.parent().unwrap().file_name().unwrap(), "Igorio");
}

#[test]
fn shortcuts_round_trip_through_the_file() {
    let scratch = Scratch::new("shortcuts-round-trip");
    let path = scratch.path("settings.json");
    let mut settings = Settings::default();
    assert_eq!(settings.shortcuts, Shortcuts::defaults());
    settings
        .shortcuts
        .set(Action::RestoreLast, Some(Chord::parse("Ctrl+Alt+Z").unwrap()))
        .unwrap();
    settings.shortcuts.set(Action::Pen, None).unwrap();
    settings.save(&path).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["shortcuts"]["restore_last"], "Ctrl+Alt+Z");
    assert_eq!(json["shortcuts"]["pen"], serde_json::Value::Null);
    assert_eq!(Settings::load(&path).unwrap(), settings);
}

#[test]
fn missing_shortcuts_give_the_defaults() {
    let scratch = Scratch::new("shortcuts-missing");
    let path = scratch.path("settings.json");
    fs::write(&path, r#"{"launch_at_login": true}"#).unwrap();
    let settings = Settings::load(&path).unwrap();
    assert_eq!(settings.shortcuts, Shortcuts::defaults());
    assert!(settings.launch_at_login);
}

#[test]
fn a_bad_shortcut_keeps_the_rest_of_the_file() {
    let scratch = Scratch::new("shortcuts-lenient");
    let path = scratch.path("settings.json");
    fs::write(
        &path,
        r#"{
            "launch_at_login": true,
            "card_size": "large",
            "shortcuts": {
                "restore_last": "Ctrl+Alt+Z",
                "discard_top": null,
                "discard_all": "Ctrl+Alt+Z",
                "pen": "Ctrl+Nope+7",
                "highlighter": "Win+L",
                "toggle_recording": 6,
                "beam_me_up": "Ctrl+Alt+B"
            }
        }"#,
    )
    .unwrap();
    let settings = Settings::load(&path).unwrap();
    assert!(!scratch.path("settings.json.bad").exists(), "the file was parsed");
    assert!(settings.launch_at_login);
    assert_eq!(settings.card_size, CardSize::Large);
    let s = &settings.shortcuts;
    let chord = |text: &str| Some(Chord::parse(text).unwrap());
    assert_eq!(s.get(Action::RestoreLast), chord("Ctrl+Alt+Z"));
    assert_eq!(s.get(Action::DiscardTop), None);
    assert_eq!(s.get(Action::DiscardAll), None, "the first action keeps a shared chord");
    assert_eq!(s.get(Action::Pen), chord("Ctrl+Shift+7"));
    assert_eq!(s.get(Action::Highlighter), chord("Ctrl+Shift+8"));
    assert_eq!(s.get(Action::ToggleRecording), chord("Ctrl+Shift+6"));
    assert_eq!(s.get(Action::OpenSettings), None);

    fs::write(&path, r#"{"shortcuts": "Ctrl+Alt+Z", "launch_at_login": true}"#).unwrap();
    let settings = Settings::load(&path).unwrap();
    assert_eq!(settings.shortcuts, Shortcuts::defaults());
    assert!(settings.launch_at_login);
}

#[test]
fn the_recording_hint_count_defaults_to_zero_and_round_trips() {
    let scratch = Scratch::new("hints");
    let path = scratch.path("settings.json");
    fs::write(&path, r#"{"launch_at_login": true}"#).unwrap();
    let mut settings = Settings::load(&path).unwrap();
    assert_eq!(settings.recording_hints_shown, 0);
    assert!(settings.launch_at_login);
    settings.recording_hints_shown = 2;
    settings.save(&path).unwrap();
    assert_eq!(Settings::load(&path).unwrap().recording_hints_shown, 2);
}

#[test]
fn counting_the_recording_hint_goes_one_two_three_and_keeps_the_rest() {
    let scratch = Scratch::new("hint-count");
    let path = scratch.path("settings.json");
    Settings {
        launch_at_login: true,
        ..Settings::default()
    }
    .save(&path)
    .unwrap();
    for expected in 1..=3 {
        let saved = Settings::note_recording_hint_shown(&path).unwrap();
        assert_eq!(saved.recording_hints_shown, expected);
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.recording_hints_shown, expected);
        assert!(loaded.launch_at_login);
    }
    // No file yet: it starts from the defaults.
    let fresh = scratch.path("fresh.json");
    assert_eq!(Settings::note_recording_hint_shown(&fresh).unwrap().recording_hints_shown, 1);
}

#[test]
fn counting_the_recording_hint_on_a_locked_file_loses_nothing() {
    use std::os::windows::fs::OpenOptionsExt;

    let scratch = Scratch::new("hint-locked");
    let path = scratch.path("settings.json");
    Settings {
        launch_at_login: true,
        recording_hints_shown: 1,
        ..Settings::default()
    }
    .save(&path)
    .unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    assert!(Settings::note_recording_hint_shown(&path).is_err());
    drop(lock);
    let loaded = Settings::load(&path).unwrap();
    assert!(loaded.launch_at_login, "the other settings are still there");
    assert_eq!(loaded.recording_hints_shown, 1);
    assert!(!scratch.path("settings.json.bad").exists());
}
