//! The app's own settings: which capabilities are on, where notes and captures go, and
//! a couple of switches. Stored as `%APPDATA%\Igorio\settings.json`.
//!
//! A capability that is not listed in the file is ON, so new capabilities arrive enabled.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::windows::{ffi::OsStringExt, fs::OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use windows::Win32::Storage::FileSystem::{FILE_ADD_FILE, FILE_FLAG_BACKUP_SEMANTICS};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Documents, FOLDERID_Screenshots, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath,
};

use crate::layout::CardSize;
use crate::notes::summoner::ToolMenu;
use crate::shortcuts::Shortcuts;

/// Sub-folder of Documents where notes are kept by default.
const NOTES_FOLDER_NAME: &str = "Igorio Notes";
/// Migration: the default notes folder before the app was renamed. Only
/// [`Settings::keep_old_notes_folder`] reads it.
const OLD_NOTES_FOLDER_NAME: &str = "Fairy Notes";
/// Migration: capability ids started with this before the app was renamed. Only
/// `capabilities_with_old_ids` reads it.
const OLD_CAPABILITY_PREFIX: &str = "fairy.";
/// Counter used to give temp files distinct names within one process.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Capability id -> switched on. A capability that is not listed is ON (new ones arrive enabled).
    #[serde(deserialize_with = "capabilities_with_old_ids")]
    pub capabilities: BTreeMap<String, bool>,
    /// None means "use the default folder".
    pub notes_folder: Option<PathBuf>,
    pub captures_folder: Option<PathBuf>,
    /// Save every screenshot to the captures folder as it is taken. Default false.
    pub keep_every_screenshot: bool,
    pub launch_at_login: bool,
    /// Size of the screenshot cards. An unknown value is ignored and gives the default.
    #[serde(default, deserialize_with = "card_size_or_default")]
    pub card_size: CardSize,
    /// Action id -> chord text or null. Read leniently: a bad entry gives that action its
    /// default and leaves the rest alone (see `Shortcuts::from_json`).
    pub shortcuts: Shortcuts,
    /// The tools of the Alt tool menu in the user's order, each shown or hidden. Read
    /// leniently: missing gives the default order, all shown (see `ToolMenu::from_json`).
    pub tool_menu: ToolMenu,
    /// How many recordings showed the "Hold Alt and scroll to zoom" hint; it shows while
    /// this is below `recording::feature::START_HINT_RECORDINGS`. Missing: 0. To see the hint
    /// again, set it to 0 or delete it in settings.json.
    pub recording_hints_shown: u32,
    /// The first-run guide was shown once. Missing: false, so the guide opens on the next
    /// start. The tray's "Guide…" opens it any time.
    pub guide_seen: bool,
}

/// Reads one card size, falling back to the default for an unknown or malformed value so a
/// bad `card_size` does not make the whole settings file fall back to defaults.
fn card_size_or_default<'de, D>(deserializer: D) -> Result<CardSize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// Reads the capability switches. Migration: ids were `fairy.*` before the app was renamed
/// and are `igorio.*` now. An old id is read once as its new id; when the file has both, the
/// new id wins. Saving writes only what is in the map, so only new ids are saved.
fn capabilities_with_old_ids<'de, D>(deserializer: D) -> Result<BTreeMap<String, bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut capabilities = BTreeMap::new();
    let mut old = Vec::new();
    for (id, on) in BTreeMap::<String, bool>::deserialize(deserializer)? {
        match id.strip_prefix(OLD_CAPABILITY_PREFIX) {
            Some(rest) => old.push((format!("igorio.{rest}"), on)),
            None => {
                capabilities.insert(id, on);
            }
        }
    }
    for (id, on) in old {
        capabilities.entry(id).or_insert(on);
    }
    Ok(capabilities)
}

impl Settings {
    pub fn is_enabled(&self, capability_id: &str) -> bool {
        self.capabilities.get(capability_id).copied().unwrap_or(true)
    }

    pub fn set_enabled(&mut self, capability_id: &str, on: bool) {
        self.capabilities.insert(capability_id.to_owned(), on);
    }

    /// Reads the file. A missing file gives defaults. A UTF-8 byte order mark is skipped.
    /// A file that cannot be parsed gives defaults after a copy was kept as `<name>.bad`
    /// (or `.bad.1`, `.bad.2`, ... so older copies survive). Any other read error, or a
    /// failed copy, is returned as `Err`; the caller must not save then.
    pub fn load(path: &Path) -> io::Result<Settings> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(error) => return Err(error),
        };
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        match serde_json::from_slice(bytes) {
            Ok(settings) => Ok(settings),
            Err(_) => {
                fs::copy(path, bad_path(path))?;
                Ok(Settings::default())
            }
        }
    }

    /// Writes pretty JSON to a temp file in the same folder, flushes it to disk, then renames
    /// it over `path` (so a crash never leaves half a file). Creates the parent folder.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temp = temp_path(path);
        let result = (|| -> io::Result<()> {
            {
                let mut file = fs::File::create(&temp)?;
                file.write_all(&json)?;
                file.sync_all()?;
            }
            fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// Counts one more recording that showed the zoom hint, in the file at `path`: loads,
    /// adds one, saves, and returns what was saved. A file that cannot be read (locked) is an
    /// error and is left alone, as `load` says; nothing else in it changes.
    pub fn note_recording_hint_shown(path: &Path) -> io::Result<Settings> {
        let mut settings = Settings::load(path)?;
        settings.recording_hints_shown = settings.recording_hints_shown.saturating_add(1);
        settings.save(path)?;
        Ok(settings)
    }

    /// Writes that the guide was shown (`guide_seen`) in the file at `path`, like
    /// [`note_recording_hint_shown`](Self::note_recording_hint_shown): loads, sets, saves,
    /// and returns what was saved.
    pub fn note_guide_seen(path: &Path) -> io::Result<Settings> {
        let mut settings = Settings::load(path)?;
        settings.guide_seen = true;
        settings.save(path)?;
        Ok(settings)
    }

    /// Migration from before the app was renamed: when no notes folder is chosen, the old
    /// default (`Fairy Notes` next to the new default) exists and the new default does not,
    /// the old folder becomes the chosen one, so existing notes stay in use. Nothing on disk
    /// changes. Call it before anything creates the default folder. Returns true when the
    /// setting changed; the caller saves.
    pub fn keep_old_notes_folder(&mut self, defaults: &DefaultFolders) -> bool {
        if self.notes_folder.is_some() || defaults.notes.exists() {
            return false;
        }
        let Some(old) = defaults.notes.parent().map(|parent| parent.join(OLD_NOTES_FOLDER_NAME))
        else {
            return false;
        };
        if !old.is_dir() {
            return false;
        }
        self.notes_folder = Some(old);
        true
    }

    /// The folder to use for notes: a writable absolute path, otherwise the default.
    /// The returned folder is created when possible.
    /// Returns (folder, used_fallback).
    pub fn notes_dir(&self, defaults: &DefaultFolders) -> (PathBuf, bool) {
        pick_folder(self.notes_folder.as_deref(), &defaults.notes)
    }

    pub fn captures_dir(&self, defaults: &DefaultFolders) -> (PathBuf, bool) {
        pick_folder(self.captures_folder.as_deref(), &defaults.captures)
    }

    /// `<captures>\Pasted`: screenshots pasted into a terminal as a file path are kept here.
    pub fn pasted_dir(&self, defaults: &DefaultFolders) -> PathBuf {
        let folder = self.captures_dir(defaults).0.join("Pasted");
        let _ = fs::create_dir_all(&folder);
        folder
    }
}

/// The chosen folder when it is absolute and can be created, else the default. The returned
/// folder is created; if even the default cannot be created it is still returned, so the
/// caller can report the error when writing.
fn pick_folder(chosen: Option<&Path>, default: &Path) -> (PathBuf, bool) {
    match chosen {
        Some(chosen) if chosen.is_absolute() && writable_folder(chosen).is_ok() => {
            (chosen.to_path_buf(), false)
        }
        Some(_) => {
            let _ = fs::create_dir_all(default);
            (default.to_path_buf(), true)
        }
        None => {
            let _ = fs::create_dir_all(default);
            (default.to_path_buf(), false)
        }
    }
}

fn writable_folder(folder: &Path) -> io::Result<()> {
    fs::create_dir_all(folder)?;
    // Request permission to add files through a directory handle. This writes no probe file.
    fs::OpenOptions::new()
        .access_mode(FILE_ADD_FILE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(folder)?;
    Ok(())
}

/// The first free name for the copy of an unparsable file: `<file name>.bad`, then
/// `.bad.1` through `.bad.9`; an existing `.bad.9` is reused last.
fn bad_path(path: &Path) -> PathBuf {
    let mut base = path.as_os_str().to_owned();
    base.push(".bad");
    let first = PathBuf::from(&base);
    if !first.exists() {
        return first;
    }
    let mut candidate = first;
    for n in 1..=9 {
        let mut name = base.clone();
        name.push(format!(".{n}"));
        candidate = PathBuf::from(name);
        if n == 9 || !candidate.exists() {
            break;
        }
    }
    candidate
}

/// A sibling temp file used by `save` before the rename.
fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_owned());
    let folder = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    folder.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

pub struct DefaultFolders {
    pub notes: PathBuf,
    pub captures: PathBuf,
}

impl DefaultFolders {
    /// Documents\Igorio Notes and the Screenshots known folder, through SHGetKnownFolderPath
    /// (FOLDERID_Documents, FOLDERID_Screenshots; never hard-code user paths, they may be
    /// redirected to OneDrive). Falls back to %USERPROFILE% sub-folders if a call fails.
    pub fn from_system() -> DefaultFolders {
        let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
        let documents = known_folder(&FOLDERID_Documents)
            .or_else(|| profile.as_ref().map(|home| home.join("Documents")))
            .unwrap_or_else(|| std::env::temp_dir().join("Documents"));
        let captures = known_folder(&FOLDERID_Screenshots)
            .or_else(|| profile.as_ref().map(|home| home.join("Pictures").join("Screenshots")))
            .unwrap_or_else(|| std::env::temp_dir().join("Screenshots"));
        DefaultFolders {
            notes: documents.join(NOTES_FOLDER_NAME),
            captures,
        }
    }
}

/// `%APPDATA%\Igorio\settings.json`.
pub fn default_settings_path() -> PathBuf {
    let roaming = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| PathBuf::from(home).join("AppData").join("Roaming"))
        })
        .unwrap_or_else(std::env::temp_dir);
    roaming.join("Igorio").join("settings.json")
}

/// Resolves a known folder. The only unsafe code in this module: one Win32 call and freeing
/// its result.
fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    // SAFETY: `id` points at a `'static` GUID from the windows crate. On success the call
    // returns the single out pointer, a NUL-terminated UTF-16 path allocated with
    // CoTaskMemAlloc. We copy the text into an OsString and free that exact pointer once
    // with CoTaskMemFree. The windows crate wrapper only exposes the out pointer in its Ok
    // result, so on failure there is no pointer to free and nothing leaks.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None).ok()?;
        let result = OsString::from_wide(path.as_wide());
        CoTaskMemFree(Some(path.0.cast()));
        Some(PathBuf::from(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_probe_creates_the_directory_without_leaving_a_file() {
        let folder = std::env::temp_dir().join(format!(
            "igorio-folder-check-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        writable_folder(&folder).unwrap();
        assert!(fs::read_dir(&folder).unwrap().next().is_none());
        fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn the_tool_menu_list_is_read_leniently_and_never_spoils_the_file() {
        use crate::notes::summoner::{MenuEntry, ToolId};
        let read = |text: &str| serde_json::from_str::<Settings>(text).unwrap();
        assert_eq!(read(r#"{"launch_at_login": true}"#).tool_menu, ToolMenu::default());
        let bad = read(r#"{"tool_menu": {"id": "note"}, "launch_at_login": true}"#);
        assert!(bad.launch_at_login && bad.tool_menu == ToolMenu::default());
        let partial = read(r#"{"tool_menu": [{"id": "record", "shown": false}, {"id": "spell"}]}"#);
        assert_eq!(
            partial.tool_menu.entries(),
            &[
                MenuEntry { tool: ToolId::RecordScreen, shown: false },
                MenuEntry { tool: ToolId::NewNote, shown: true },
            ]
        );
        let json = serde_json::to_value(&partial).unwrap();
        assert_eq!(json["tool_menu"], serde_json::json!([{"id": "record", "shown": false}, {"id": "note", "shown": true}]));
        assert_eq!(serde_json::from_value::<Settings>(json).unwrap(), partial);
    }
}
