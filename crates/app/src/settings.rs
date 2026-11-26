//! The fairy's own settings: which capabilities are on, where notes and captures go, and
//! a couple of switches. Stored as `%APPDATA%\Igorio\settings.json`.
//!
//! Ported in spirit from `AppSettings.swift` and `CapabilityRegistry.swift`: a capability
//! that is not listed in the file is ON, so new capabilities arrive enabled.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Documents, FOLDERID_Screenshots, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath,
};

/// Sub-folder of Documents where notes are kept by default.
const NOTES_FOLDER_NAME: &str = "Fairy Notes";
/// Counter used to give temp files distinct names within one process.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Capability id -> switched on. A capability that is not listed is ON (new ones arrive enabled).
    pub capabilities: BTreeMap<String, bool>,
    /// None means "use the default folder".
    pub notes_folder: Option<PathBuf>,
    pub captures_folder: Option<PathBuf>,
    /// Save every screenshot to the captures folder as it is taken. Default false.
    pub keep_every_screenshot: bool,
    pub launch_at_login: bool,
}

impl Settings {
    pub fn is_enabled(&self, capability_id: &str) -> bool {
        self.capabilities.get(capability_id).copied().unwrap_or(true)
    }

    pub fn set_enabled(&mut self, capability_id: &str, on: bool) {
        self.capabilities.insert(capability_id.to_owned(), on);
    }

    /// Reads the file. A missing file gives defaults. A file that cannot be parsed gives
    /// defaults and is copied to `<name>.bad` so the user's data is not lost on the next save.
    pub fn load(path: &Path) -> Settings {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return Settings::default(),
        };
        match serde_json::from_slice(&bytes) {
            Ok(settings) => settings,
            Err(_) => {
                let mut bad = path.as_os_str().to_owned();
                bad.push(".bad");
                let _ = fs::copy(path, PathBuf::from(bad));
                Settings::default()
            }
        }
    }

    /// Writes pretty JSON to a temp file in the same folder, then renames it over `path`
    /// (so a crash never leaves half a file). Creates the parent folder.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temp = temp_path(path);
        fs::write(&temp, json)?;
        if let Err(error) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        Ok(())
    }

    /// The folder to use for notes: the chosen one if it exists or can be created and is
    /// writable, otherwise the default. Returns (folder, used_fallback).
    pub fn notes_dir(&self, defaults: &DefaultFolders) -> (PathBuf, bool) {
        pick_folder(self.notes_folder.as_deref(), &defaults.notes)
    }

    pub fn captures_dir(&self, defaults: &DefaultFolders) -> (PathBuf, bool) {
        pick_folder(self.captures_folder.as_deref(), &defaults.captures)
    }

    /// `<captures>\Pasted`: screenshots pasted into a terminal as a file path are kept here.
    pub fn pasted_dir(&self, defaults: &DefaultFolders) -> PathBuf {
        self.captures_dir(defaults).0.join("Pasted")
    }
}

/// The chosen folder when it is usable, else the default.
fn pick_folder(chosen: Option<&Path>, default: &Path) -> (PathBuf, bool) {
    match chosen {
        Some(chosen) => match usable_folder(chosen) {
            Some(folder) => (folder, false),
            None => (default.to_path_buf(), true),
        },
        None => (default.to_path_buf(), false),
    }
}

/// Creates the folder if needed and checks that a small file can be written there.
fn usable_folder(path: &Path) -> Option<PathBuf> {
    if fs::create_dir_all(path).is_err() || !path.is_dir() {
        return None;
    }
    let probe = path.join(format!(
        ".igorio-write-probe-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            Some(path.to_path_buf())
        }
        Err(_) => None,
    }
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
    /// Documents\Fairy Notes and the Screenshots known folder, through SHGetKnownFolderPath
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
    // SAFETY: SHGetKnownFolderPath returns a NUL-terminated UTF-16 path allocated with
    // CoTaskMemAlloc, or an error. We copy the wide string into an OsString and then free
    // that exact pointer once with CoTaskMemFree, as the API requires. `id` points at a
    // `'static` GUID provided by the windows crate.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None).ok()?;
        let result = OsString::from_wide(path.as_wide());
        CoTaskMemFree(Some(path.0.cast()));
        Some(PathBuf::from(result))
    }
}
