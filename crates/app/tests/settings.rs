use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use igorio_app::settings::{DefaultFolders, Settings, default_settings_path};

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
    assert_eq!(settings, Settings::load(&PathBuf::from("does-not-exist-anywhere.json")));
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
    assert_eq!(Settings::load(&path), settings);
}

#[test]
fn missing_file_gives_defaults() {
    let scratch = Scratch::new("missing");
    assert_eq!(Settings::load(&scratch.path("settings.json")), Settings::default());
}

#[test]
fn corrupt_file_gives_defaults_and_keeps_a_bad_copy() {
    let scratch = Scratch::new("corrupt");
    let path = scratch.path("settings.json");
    let junk = "{ this is not json";
    fs::write(&path, junk).unwrap();

    assert_eq!(Settings::load(&path), Settings::default());
    let bad = scratch.path("settings.json.bad");
    assert!(bad.is_file(), "expected {:?} to exist", bad);
    assert_eq!(fs::read_to_string(&bad).unwrap(), junk);
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

    let settings = Settings::load(&path);
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
fn unwritable_chosen_folder_falls_back() {
    let scratch = Scratch::new("fallback");
    let blocker = scratch.path("blocker");
    fs::write(&blocker, b"a file, not a folder").unwrap();
    let defaults = defaults(&scratch);
    let settings = Settings {
        notes_folder: Some(blocker.join("below-a-file")),
        ..Settings::default()
    };

    assert_eq!(settings.notes_dir(&defaults), (defaults.notes.clone(), true));
}

#[test]
fn no_chosen_folder_uses_the_default_without_fallback() {
    let scratch = Scratch::new("default-folder");
    let defaults = defaults(&scratch);
    let settings = Settings::default();

    assert_eq!(settings.notes_dir(&defaults), (defaults.notes.clone(), false));
    assert_eq!(settings.captures_dir(&defaults), (defaults.captures.clone(), false));
}

#[test]
fn pasted_dir_is_captures_pasted() {
    let scratch = Scratch::new("pasted");
    let defaults = defaults(&scratch);
    let mut settings = Settings::default();
    assert_eq!(settings.pasted_dir(&defaults), defaults.captures.join("Pasted"));

    let chosen = scratch.path("chosen-captures");
    settings.captures_folder = Some(chosen.clone());
    assert_eq!(settings.pasted_dir(&defaults), chosen.join("Pasted"));
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
