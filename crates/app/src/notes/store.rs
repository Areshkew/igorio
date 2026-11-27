//! Sticky notes as plain Markdown files in a folder the caller chooses.
//!
//! Ported from `reference/fairy-cursor/sources/capabilities/notes/NoteStore.swift`. Each file
//! starts with the same short front matter block the macOS app writes (id, when, and where the
//! note was written), so a notes folder can be shared between the two apps. The file is named
//! after the note's first line and renamed as that changes; a blank note has no file until
//! something is typed into it.
//!
//! Two Windows-only changes sit on top of the Swift rules:
//!
//! - A file stem is made safe for Windows (`windows_safe_stem`): characters Windows forbids
//!   become `-`, trailing dots and spaces go, device names such as `CON` get a leading `_`,
//!   and the stem is short enough that the full path stays under `MAX_PATH`.
//! - Times are supplied by the caller. This module never reads a clock and never starts a
//!   timer: `update` marks a note dirty with a deadline, `next_deadline` says when the caller
//!   should call `tick`, and `flush` writes everything at once. `now` has the same units as
//!   the caller's clock (seconds); `SystemTime` values go into the files.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use igorio_core::geom::Rect;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FO_DELETE, SHFILEOPSTRUCTW,
    SHFileOperationW,
};
use windows::core::PCWSTR;

/// How long a change waits before it is written, from `update`'s `DispatchQueue` delay in the
/// Swift store. The caller supplies the clock; this is only a length of time.
pub const DEBOUNCE_SECONDS: f64 = 0.4;

/// Longest full path, in UTF-16 code units, that this store aims to produce. Windows'
/// `MAX_PATH` is 260 including the terminating NUL.
pub const MAX_PATH_UNITS: usize = 259;

/// Longest file stem `Note::file_stem` produces. The store shortens it further when the
/// notes folder is deep enough that `folder\stem.md` would pass [`MAX_PATH_UNITS`].
pub const MAX_STEM_UNITS: usize = 120;

/// Counter that keeps temp names distinct within one process.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A note's identity: a UUID, as `[u8; 16]`. Text form is the canonical 8-4-4-4-12 with
/// uppercase hex digits, which is what Swift's `UUID.uuidString` writes into the file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NoteId([u8; 16]);

impl NoteId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 16 bytes from the caller, with the UUID version 4 and RFC 4122 variant bits set.
    pub fn new_random(seed: [u8; 16]) -> Self {
        let mut bytes = seed;
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        Self(bytes)
    }

    /// Parses the canonical hyphenated form. Hex digits may be upper or lower case, like
    /// `UUID(uuidString:)`.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 36 {
            return None;
        }
        let mut out = [0u8; 16];
        let mut out_index = 0;
        let mut index = 0;
        while index < bytes.len() {
            if matches!(index, 8 | 13 | 18 | 23) {
                if bytes[index] != b'-' {
                    return None;
                }
                index += 1;
                continue;
            }
            let high = hex_value(bytes[index])?;
            let low = hex_value(*bytes.get(index + 1)?)?;
            if out_index == out.len() {
                return None;
            }
            out[out_index] = (high << 4) | low;
            out_index += 1;
            index += 2;
        }
        (out_index == out.len()).then_some(Self(out))
    }
}

impl fmt::Display for NoteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-\
             {:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12],
            b[13], b[14], b[15]
        )
    }
}

impl fmt::Debug for NoteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// The error `str::parse` returns for text that is not a canonical UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidNoteId;

impl fmt::Display for InvalidNoteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a UUID in 8-4-4-4-12 form")
    }
}

impl std::error::Error for InvalidNoteId {}

impl FromStr for NoteId {
    type Err = InvalidNoteId;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        NoteId::parse(text).ok_or(InvalidNoteId)
    }
}

/// Sixteen bytes for [`NoteId::new_random`], mixed from a caller-supplied time, a counter and
/// the process id. The store's own counter makes ids distinct within one store; callers that
/// create several stores at the same instant should pass a different time or start the
/// counter elsewhere.
pub fn seed_bytes(now: SystemTime, counter: u64) -> [u8; 16] {
    let (seconds, nanos) = unix_parts(now);
    let mut state = (seconds as u64)
        ^ ((nanos as u64) << 32)
        ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ u64::from(std::process::id());
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        state = split_mix(state);
        chunk.copy_from_slice(&state.to_le_bytes());
    }
    bytes
}

fn split_mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut x = z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Where a note was started: the Desktop, an app window, or a web page. Field and value names
/// match `NoteOrigin` in Swift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NoteKind {
    #[default]
    Desktop,
    App,
    Web,
}

impl NoteKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            NoteKind::Desktop => "desktop",
            NoteKind::App => "app",
            NoteKind::Web => "web",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "desktop" => Some(NoteKind::Desktop),
            "app" => Some(NoteKind::App),
            "web" => Some(NoteKind::Web),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoteOrigin {
    pub kind: NoteKind,
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub window_title: Option<String>,
    /// The page for [`NoteKind::Web`]; the window's document (a `file:` URL) for
    /// [`NoteKind::App`] when the app exposes one.
    pub url: Option<String>,
}

impl NoteOrigin {
    pub const fn desktop() -> Self {
        NoteOrigin {
            kind: NoteKind::Desktop,
            app_name: None,
            bundle_id: None,
            window_title: None,
            url: None,
        }
    }

    /// Short label for the note's chip and the menu: "Desktop", "Xcode", "github.com", or the
    /// file name for a local document ("paper.pdf").
    pub fn label(&self) -> String {
        if let Some(url) = self.url.as_deref()
            && let Some(name) = file_url_last_component(url)
            && !name.is_empty()
        {
            return name;
        }
        match self.kind {
            NoteKind::Desktop => "Desktop".to_owned(),
            NoteKind::App => self.app_name.clone().unwrap_or_else(|| "App".to_owned()),
            NoteKind::Web => match self.url.as_deref().and_then(url_host) {
                Some(host) => host
                    .strip_prefix("www.")
                    .unwrap_or(host.as_str())
                    .to_owned(),
                None => self.app_name.clone().unwrap_or_else(|| "Web".to_owned()),
            },
        }
    }

    /// Longer description for tooltips.
    pub fn detail(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.kind != NoteKind::Desktop
            && let Some(app_name) = &self.app_name
        {
            parts.push(app_name.clone());
        }
        if let Some(title) = &self.window_title
            && !title.is_empty()
        {
            parts.push(title.clone());
        }
        if let Some(url) = &self.url {
            parts.push(url.clone());
        }
        if parts.is_empty() {
            self.label()
        } else {
            parts.join("\n")
        }
    }
}

/// One sticky note, in memory. Field names follow `Note` in Swift.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub id: NoteId,
    pub text: String,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
    pub origin: NoteOrigin,
    /// Panel frame in screen coordinates (includes the shadow inset).
    pub frame: Rect,
    /// On screen or put away (put-away notes stay listed in the menu).
    pub is_open: bool,
    /// Open but hidden: the window is off screen until it is shown again (from the notes
    /// panel or the menu). Not in the Swift app; kept in the windows file only.
    pub is_hidden: bool,
}

impl Note {
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// First non-empty line without its Markdown markers.
    pub fn title(&self) -> String {
        let line = self
            .text
            .split('\n')
            .map(trim_spaces)
            .find(|line| !line.is_empty());
        let Some(line) = line else {
            return "Empty note".to_owned();
        };
        let stripped = strip_markers(strip_markdown_prefix(line));
        let result = if stripped.is_empty() {
            line.to_owned()
        } else {
            stripped
        };
        if result.chars().count() > 48 {
            format!("{}…", safe_cut(&result, 47))
        } else {
            result
        }
    }

    /// File name (without ".md") for the note: its first line, made safe for Windows.
    ///
    /// This does not know the notes folder, so it uses [`MAX_STEM_UNITS`]; the store shortens
    /// the stem further when the folder path leaves less room under [`MAX_PATH_UNITS`].
    pub fn file_stem(&self) -> String {
        windows_safe_stem(&self.swift_file_stem(), MAX_STEM_UNITS)
    }

    /// The Swift `fileStem` rules, before the Windows-only rules.
    fn swift_file_stem(&self) -> String {
        if self.is_blank() {
            return "Untitled".to_owned();
        }
        let mut stem: String = self
            .title()
            .chars()
            .filter(|c| *c != '…' && *c != ':')
            .map(|c| match c {
                '/' | '\\' | '\n' | '\r' | '\t' => '-',
                _ => c,
            })
            .collect();
        stem = trim_spaces(&stem).to_owned();
        while stem.starts_with('.') {
            stem.remove(0);
        }
        if stem.is_empty() {
            "Untitled".to_owned()
        } else {
            stem
        }
    }
}

/// Applies the Windows file-name rules to a stem (the Swift rules already ran).
///
/// Invalid characters (`< > : " / \ | ? *` and control characters) become `-`; trailing dots
/// and spaces are dropped; a reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`,
/// `LPT1`-`LPT9`, with or without an extension) gets a leading `_`; and the result is cut to
/// `max_units` UTF-16 code units so the caller can keep the full path under `MAX_PATH`.
/// An empty result becomes "Untitled".
pub fn windows_safe_stem(stem: &str, max_units: usize) -> String {
    let mut result: String = stem
        .chars()
        .map(|c| if is_windows_invalid(c) { '-' } else { c })
        .collect();
    result = trim_trailing_dots_and_spaces(&result).to_owned();
    if result.is_empty() {
        result = "Untitled".to_owned();
    }
    result = truncate_utf16(&result, max_units.max(1)).to_owned();
    result = trim_trailing_dots_and_spaces(&result).to_owned();
    if result.is_empty() {
        result = "Untitled".to_owned();
    }
    if is_reserved_device_name(&result) {
        result.insert(0, '_');
        result = truncate_utf16(&result, max_units.max(1)).to_owned();
        result = trim_trailing_dots_and_spaces(&result).to_owned();
    }
    result
}

fn is_windows_invalid(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control()
}

fn trim_trailing_dots_and_spaces(text: &str) -> &str {
    text.trim_end_matches(['.', ' '])
}

/// Cuts `text` to the longest prefix that fits in `max_units` UTF-16 code units, without
/// cutting through a character a reader sees as one (see [`safe_cut`]).
fn truncate_utf16(text: &str, max_units: usize) -> &str {
    let mut units = 0;
    let mut chars = 0;
    for c in text.chars() {
        let width = c.len_utf16();
        if units + width > max_units {
            break;
        }
        units += width;
        chars += 1;
    }
    safe_cut(text, chars)
}

/// The longest prefix of `text` of at most `max_chars` characters that does not end in the
/// middle of what a reader sees as one character.
///
/// This is an approximation of grapheme clusters (there is no `unicode-segmentation` here).
/// It covers the common cases: combining marks (U+0300..=U+036F, U+1AB0..=U+1AFF,
/// U+1DC0..=U+1DFF, U+20D0..=U+20FF, U+FE20..=U+FE2F), variation selectors (U+FE00..=U+FE0F,
/// U+E0100..=U+E01EF), zero-width joiners (U+200D) and the character after one, emoji
/// modifiers (U+1F3FB..=U+1F3FF), and regional indicator pairs, which make a flag
/// (U+1F1E6..=U+1F1FF, paired from the start of the run). Hangul jamo and Indic conjuncts are
/// not covered. When the cut would split one of these, it moves back to the nearest point
/// that does not.
fn safe_cut(text: &str, max_chars: usize) -> &str {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text;
    }
    let mut end = max_chars;
    while end > 0 && !is_safe_cut(&chars, end) {
        end -= 1;
    }
    let bytes = chars[..end].iter().map(|c| c.len_utf8()).sum();
    &text[..bytes]
}

/// May `chars[..end]` end there without splitting a character a reader sees as one?
fn is_safe_cut(chars: &[char], end: usize) -> bool {
    let Some(&next) = chars.get(end) else {
        return true;
    };
    let previous = chars[end - 1];
    if is_combining(next) || is_variation_selector(next) || is_emoji_modifier(next) {
        return false;
    }
    // Neither before nor after a zero-width joiner: the joined sequence must stay together.
    if next == '\u{200D}' || previous == '\u{200D}' {
        return false;
    }
    if is_regional_indicator(next) && is_regional_indicator(previous) {
        // Pair the run from its start; an odd count before `end` means `previous` is the
        // first half of the flag `next` completes.
        let run = chars[..end]
            .iter()
            .rev()
            .take_while(|c| is_regional_indicator(**c))
            .count();
        if run % 2 == 1 {
            return false;
        }
    }
    true
}

/// Unicode general categories Mn, Mc and Me in the ranges this module covers.
fn is_combining(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036F}'
        | '\u{1AB0}'..='\u{1AFF}'
        | '\u{1DC0}'..='\u{1DFF}'
        | '\u{20D0}'..='\u{20FF}'
        | '\u{FE20}'..='\u{FE2F}'
    )
}

fn is_variation_selector(c: char) -> bool {
    matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

fn is_emoji_modifier(c: char) -> bool {
    matches!(c, '\u{1F3FB}'..='\u{1F3FF}')
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

fn is_reserved_device_name(stem: &str) -> bool {
    // Windows compares the part before the first dot, without trailing spaces.
    let first = stem.split('.').next().unwrap_or(stem);
    let name = first.trim_end_matches(' ').to_ascii_uppercase();
    if matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let bytes = name.as_bytes();
    bytes.len() == 4
        && (name.starts_with("COM") || name.starts_with("LPT"))
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

/// The window part of a note, kept in the separate windows file. Matches Swift's nested
/// `NoteStore.WindowState`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowState {
    pub frame: Rect,
    pub is_open: bool,
    /// Open but hidden (see [`Note::is_hidden`]).
    pub is_hidden: bool,
}

/// The on-disk form of a note, from `NoteFile` in Swift: a front matter block, then the
/// Markdown as typed.
///
/// ```text
/// ---
/// id: 6F9A1B2C-3D4E-4F5A-6B7C-8D9EAFB0C1D2
/// created: 2026-11-11T14:32:05Z
/// from: "github.com"
/// kind: web
/// app: "Safari"
/// bundle: "com.apple.Safari"
/// window: "GitHub"
/// url: "https://github.com/rust-lang"
/// ---
///
/// the note…
/// ```
///
/// Values are JSON strings (also valid YAML), so titles with colons or quotes round-trip.
pub struct NoteFile;

/// The metadata read back from a note file. Absent fields keep `NoteFile::parse`'s defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteMeta {
    pub id: Option<NoteId>,
    pub created: Option<SystemTime>,
    pub origin: NoteOrigin,
}

impl NoteFile {
    /// The file's bytes for `note`, as plain UTF-8.
    ///
    /// A UTF-8 byte order mark that [`NoteFile::parse`] skipped is not written back: the note
    /// itself is text, and the mark belongs to the editor that saved it, not to the file.
    pub fn render(note: &Note) -> String {
        let origin = &note.origin;
        let mut lines = vec![
            "---".to_owned(),
            format!("id: {}", note.id),
            format!("created: {}", format_utc(note.created_at)),
            format!("from: {}", quote(&origin.label())),
            format!("kind: {}", origin.kind.as_str()),
        ];
        if let Some(value) = &origin.app_name {
            lines.push(format!("app: {}", quote(value)));
        }
        if let Some(value) = &origin.bundle_id {
            lines.push(format!("bundle: {}", quote(value)));
        }
        if let Some(value) = &origin.window_title
            && !value.is_empty()
        {
            lines.push(format!("window: {}", quote(value)));
        }
        if let Some(value) = &origin.url {
            lines.push(format!("url: {}", quote(value)));
        }
        lines.push("---".to_owned());
        format!("{}\n\n{}", lines.join("\n"), note.text)
    }

    /// Splits a file into its metadata and body. A file without the block is all body and
    /// gets no metadata, which is the Swift rule. One leading UTF-8 byte order mark is
    /// skipped, as `settings.rs` does: an editor on Windows may add one, and it is not part
    /// of the note's text.
    pub fn parse(text: &str) -> (NoteMeta, String) {
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        let mut meta = NoteMeta {
            id: None,
            created: None,
            origin: NoteOrigin::default(),
        };
        if !(text.starts_with("---\n") || text.starts_with("---\r\n")) {
            return (meta, text.to_owned());
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let Some(end) = lines
            .iter()
            .skip(1)
            .position(|line| trim_line(line) == "---")
            .map(|index| index + 1)
        else {
            return (meta, text.to_owned());
        };
        let mut fields: BTreeMap<&str, String> = BTreeMap::new();
        for line in &lines[1..end] {
            if let Some(colon) = line.find(':') {
                let key = trim_line(&line[..colon]);
                let value = unquote(trim_line(&line[colon + 1..]));
                fields.insert(key, value);
            }
        }
        meta.id = fields.get("id").and_then(|value| NoteId::parse(value));
        meta.created = fields.get("created").and_then(|value| parse_utc(value));
        meta.origin = NoteOrigin {
            kind: fields
                .get("kind")
                .and_then(|value| NoteKind::parse(value))
                .unwrap_or_default(),
            app_name: fields.get("app").cloned(),
            bundle_id: fields.get("bundle").cloned(),
            window_title: fields.get("window").cloned(),
            url: fields.get("url").cloned(),
        };
        let mut body = lines[end + 1..].join("\n");
        // `render` puts one blank line between the block and the note.
        if body.starts_with('\n') {
            body.remove(0);
        }
        (meta, body)
    }
}

/// What `delete` did with the note's file. `trashed` is false when the note had no file or
/// when the Recycle Bin call failed; the note itself is removed either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedFile {
    pub path: Option<PathBuf>,
    pub trashed: bool,
}

/// Moves a note's file out of the notes folder. The real implementation sends it to the
/// Recycle Bin; tests inject a fake.
pub trait Trash {
    fn trash(&self, path: &Path) -> io::Result<()>;
}

/// The Recycle Bin, through `SHFileOperationW` with `FOF_ALLOWUNDO` (the Windows counterpart
/// of `FileManager.trashItem`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RecycleBin;

impl Trash for RecycleBin {
    fn trash(&self, path: &Path) -> io::Result<()> {
        let from = shell_delete_buffer(path)?;
        // SAFETY: this is the only unsafe code in the module.
        //
        // `from` is a live, double-NUL-terminated UTF-16 buffer for the whole call. The struct
        // is initialized by the windows crate, `pTo` stays null (unused for a delete) and no
        // pointer escapes; SHFileOperationW only reads `pFrom` and writes into `operation`.
        //
        // COM pairing: SHFileOperationW needs COM on the calling thread. `CoInitializeEx` on
        // S_OK (this call initialized the apartment) and on S_FALSE (the thread already had the
        // same apartment) must each be balanced by exactly one `CoUninitialize` on the same
        // thread, and only after the API call is done. RPC_E_CHANGED_MODE means the thread
        // already lives in another apartment: this call did not initialize anything, so it
        // must not uninitialize, and the operation can still run. Any other failure means COM
        // is unavailable, so return before touching the shell API.
        unsafe {
            let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let must_uninitialize = initialized == S_OK || initialized == S_FALSE;
            if !must_uninitialize && initialized != RPC_E_CHANGED_MODE {
                return Err(io::Error::other(format!(
                    "COM could not be initialized on this thread: {initialized:?}"
                )));
            }
            let mut operation = SHFILEOPSTRUCTW {
                wFunc: FO_DELETE,
                pFrom: PCWSTR(from.as_ptr()),
                fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0) as u16,
                ..Default::default()
            };
            let code = SHFileOperationW(&mut operation);
            let aborted = code == 0 && operation.fAnyOperationsAborted.as_bool();
            if must_uninitialize {
                CoUninitialize();
            }
            if aborted {
                Err(io::Error::other(
                    "the file was not moved to the Recycle Bin",
                ))
            } else if code == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(code))
            }
        }
    }
}

/// The double-NUL-terminated UTF-16 list `SHFileOperationW` wants for one path.
///
/// The API takes an absolute path under `MAX_PATH` and strips trailing spaces and dots from
/// it, so a relative, too-long or so-shaped path is refused here rather than deleting some
/// other file. The caller may rely on the returned buffer having at least two NULs at its end.
fn shell_delete_buffer(path: &Path) -> io::Result<Vec<u16>> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the Recycle Bin needs an absolute path, got {}",
                path.display()
            ),
        ));
    }
    let mut buffer: Vec<u16> = path.as_os_str().encode_wide().collect();
    if buffer.len() > MAX_PATH_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the path is {} UTF-16 units long; the shell API takes at most {MAX_PATH_UNITS}",
                buffer.len()
            ),
        ));
    }
    if buffer
        .last()
        .is_some_and(|unit| *unit == u16::from(b' ') || *unit == u16::from(b'.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the shell API drops a trailing space or dot from {}",
                path.display()
            ),
        ));
    }
    // SHFILEOPSTRUCTW asks for a double-NUL-terminated list of paths.
    buffer.extend([0, 0]);
    Ok(buffer)
}

/// The notes in one folder, plus the window state kept next to the app's other settings.
pub struct NoteStore {
    notes: BTreeMap<NoteId, Note>,
    folder: PathBuf,
    /// The file each note was last written to (absent for a blank note).
    files: BTreeMap<NoteId, PathBuf>,
    /// Notes waiting for their debounce deadline, keyed by note, valued by the deadline on the
    /// caller's clock.
    pending: BTreeMap<NoteId, f64>,
    windows: BTreeMap<NoteId, WindowState>,
    windows_file: PathBuf,
    trash: Box<dyn Trash>,
    /// Counter for ids given to files that have no metadata block yet.
    next_seed: u64,
}

impl NoteStore {
    /// Opens the folder, reads the windows file and reloads the notes in it. `now` is used
    /// only as the fallback timestamp and as the seed for ids of files that have no metadata.
    pub fn new(
        folder: impl Into<PathBuf>,
        windows_file: impl Into<PathBuf>,
        now: SystemTime,
    ) -> Self {
        Self::with_trash(folder, windows_file, now, Box::new(RecycleBin))
    }

    /// Like [`NoteStore::new`], with a replacement for the Recycle Bin.
    pub fn with_trash(
        folder: impl Into<PathBuf>,
        windows_file: impl Into<PathBuf>,
        now: SystemTime,
        trash: Box<dyn Trash>,
    ) -> Self {
        let mut store = NoteStore {
            notes: BTreeMap::new(),
            folder: folder.into(),
            files: BTreeMap::new(),
            pending: BTreeMap::new(),
            windows: BTreeMap::new(),
            windows_file: windows_file.into(),
            trash,
            next_seed: 0,
        };
        store.load_windows();
        let _ = fs::create_dir_all(&store.folder);
        store.reload(now);
        store
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    pub fn note(&self, id: NoteId) -> Option<&Note> {
        self.notes.get(&id)
    }

    pub fn notes(&self) -> impl Iterator<Item = &Note> {
        self.notes.values()
    }

    /// The file a note was last written to, if it has one.
    pub fn file_path(&self, id: NoteId) -> Option<&Path> {
        self.files.get(&id).map(PathBuf::as_path)
    }

    /// Most recently edited first.
    pub fn recent(&self) -> Vec<Note> {
        let mut notes: Vec<Note> = self.notes.values().cloned().collect();
        notes.sort_by_key(|note| std::cmp::Reverse(note.updated_at));
        notes
    }

    /// Point at another folder: save what is pending here, then read the notes there. Window
    /// state is app state and stays with the app.
    pub fn switch_folder(
        &mut self,
        folder: impl Into<PathBuf>,
        now: SystemTime,
    ) -> Vec<(NoteId, io::Error)> {
        let errors = self.flush();
        self.folder = folder.into();
        let _ = fs::create_dir_all(&self.folder);
        self.notes.clear();
        self.files.clear();
        self.reload(now);
        errors
    }

    /// Adds a note and writes it at once, like Swift's `insert`.
    pub fn insert(&mut self, note: Note) -> io::Result<()> {
        let id = note.id;
        self.notes.insert(id, note);
        self.write(id)
    }

    /// Applies a change and saves it [`DEBOUNCE_SECONDS`] later. `now` is on the caller's
    /// clock, the same clock passed to [`NoteStore::tick`] and [`NoteStore::next_deadline`].
    /// Returns false when the note is unknown.
    pub fn update(&mut self, id: NoteId, now: f64, change: impl FnOnce(&mut Note)) -> bool {
        let Some(note) = self.notes.get_mut(&id) else {
            return false;
        };
        change(note);
        self.pending.insert(id, now + DEBOUNCE_SECONDS);
        true
    }

    /// The earliest pending deadline, or `None` when nothing is waiting.
    pub fn next_deadline(&self) -> Option<f64> {
        self.pending
            .values()
            .copied()
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Writes every note whose deadline is at or before `now`. Returns each write that failed;
    /// one failing note does not stop the others.
    pub fn tick(&mut self, now: f64) -> Vec<(NoteId, io::Error)> {
        let due: Vec<NoteId> = self
            .pending
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        self.write_all(due)
    }

    /// Writes everything still waiting (on quit).
    pub fn flush(&mut self) -> Vec<(NoteId, io::Error)> {
        let due: Vec<NoteId> = self.pending.keys().copied().collect();
        self.write_all(due)
    }

    /// Removes the note; its file goes to the Recycle Bin, so a mistake can be undone.
    /// Returns `None` when there was no such note.
    pub fn delete(&mut self, id: NoteId) -> Option<DeletedFile> {
        self.pending.remove(&id);
        let had_note = self.notes.remove(&id).is_some();
        let had_window = self.windows.remove(&id).is_some();
        let path = self.files.remove(&id);
        if !had_note && !had_window && path.is_none() {
            return None;
        }
        let trashed = match &path {
            Some(path) => self.trash.trash(path).is_ok(),
            None => false,
        };
        let _ = self.save_windows();
        Some(DeletedFile { path, trashed })
    }

    /// Picks up edits, new files and deletions made outside the app. Notes in `keeping` (open
    /// on screen) are left as they are in memory: the app's copy is the live one. Files with
    /// changes still waiting to be written are left alone too.
    pub fn refresh(&mut self, keeping: &[NoteId], now: SystemTime) {
        let disk = self.read_folder(now);
        let mut seen = HashSet::new();
        for (note, path) in disk {
            seen.insert(note.id);
            if keeping.contains(&note.id) || self.pending.contains_key(&note.id) {
                continue;
            }
            self.files.insert(note.id, path);
            self.notes.insert(note.id, note);
        }
        let stale: Vec<NoteId> = self
            .notes
            .keys()
            .copied()
            .filter(|id| {
                !seen.contains(id) && !keeping.contains(id) && !self.pending.contains_key(id)
            })
            .collect();
        for id in stale {
            self.notes.remove(&id);
            self.files.remove(&id);
        }
    }

    fn reload(&mut self, now: SystemTime) {
        for (note, path) in self.read_folder(now) {
            self.files.insert(note.id, path);
            self.notes.insert(note.id, note);
        }
    }

    fn write_all(&mut self, ids: Vec<NoteId>) -> Vec<(NoteId, io::Error)> {
        let mut errors = Vec::new();
        for id in ids {
            if let Err(error) = self.write(id) {
                errors.push((id, error));
            }
        }
        errors
    }

    fn write(&mut self, id: NoteId) -> io::Result<()> {
        self.pending.remove(&id);
        let Some(note) = self.notes.get(&id).cloned() else {
            return Ok(());
        };
        self.windows.insert(
            id,
            WindowState {
                frame: note.frame,
                is_open: note.is_open,
                is_hidden: note.is_hidden,
            },
        );
        // Window state is beside the note, not in it; failing to save it must not lose text.
        let _ = self.save_windows();
        // A blank note has no file; one that was emptied out keeps its old file until it is
        // put away (and then deleted), so clearing a note by accident loses nothing yet.
        if note.is_blank() {
            return Ok(());
        }
        let stem = self.stem_for(&note);
        let current = self.files.get(&id).cloned();
        let mut claimed: Option<PathBuf> = None;
        let target = if let Some(current) = current {
            let name = current.file_stem().and_then(OsStr::to_str).unwrap_or("");
            if name_matches(name, &stem) {
                current
            } else {
                // The first line changed: follow it with the file name.
                let renamed = claim_file(&self.folder, &stem, Some(&current))?;
                match fs::rename(&current, &renamed) {
                    Ok(()) => {
                        self.files.insert(id, renamed.clone());
                        renamed
                    }
                    Err(_) => {
                        // Keep the old name; the new one is only an empty claim.
                        let _ = fs::remove_file(&renamed);
                        current
                    }
                }
            }
        } else {
            let path = claim_file(&self.folder, &stem, None)?;
            claimed = Some(path.clone());
            path
        };
        let text = NoteFile::render(&note);
        match write_atomic(&target, text.as_bytes()) {
            Ok(()) => {
                self.files.insert(id, target);
                Ok(())
            }
            Err(error) => {
                if let Some(path) = claimed {
                    let _ = fs::remove_file(path);
                }
                Err(error)
            }
        }
    }

    /// The note's stem, shortened when the folder path leaves less room under `MAX_PATH`.
    fn stem_for(&self, note: &Note) -> String {
        let folder_units = self.folder.as_os_str().to_string_lossy().encode_utf16().count();
        let room = MAX_PATH_UNITS
            .saturating_sub(folder_units + 1 + 3) // separator and ".md"
            .clamp(1, MAX_STEM_UNITS);
        windows_safe_stem(&note.swift_file_stem(), room)
    }

    /// Every `.md` file in the folder (not subfolders), parsed. Files without our front
    /// matter get a fresh id; it is written into the file the first time the note changes.
    /// A zero-byte file is skipped: it is not a note yet.
    fn read_folder(&mut self, now: SystemTime) -> Vec<(Note, PathBuf)> {
        let mut result = Vec::new();
        let Ok(entries) = fs::read_dir(&self.folder) else {
            return result;
        };
        let mut ids = HashSet::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let is_markdown = path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"));
            if !is_markdown || is_hidden(&entry) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            // A zero-byte file is not a note: `claim_file` creates one just before the
            // rename that gives it content, and a crash in between leaves it behind. Leave
            // it alone — another program may be about to write into it.
            if metadata.len() == 0 {
                continue;
            }
            let (mut meta, body) = NoteFile::parse(&text);
            // A file copied in Explorer keeps its twin's id; give the copy its own.
            if let Some(id) = meta.id
                && ids.contains(&id)
            {
                meta.id = None;
            }
            let id = meta
                .id
                .or_else(|| {
                    self.files
                        .iter()
                        .find(|(_, file)| **file == path)
                        .map(|(id, _)| *id)
                })
                .unwrap_or_else(|| {
                    let id = NoteId::new_random(seed_bytes(now, self.next_seed));
                    self.next_seed += 1;
                    id
                });
            ids.insert(id);
            let created = meta
                .created
                .or_else(|| metadata.created().ok())
                .or_else(|| metadata.modified().ok())
                .unwrap_or(now);
            let updated = metadata.modified().unwrap_or(now);
            let window = self.windows.get(&id).copied();
            let note = Note {
                id,
                text: body,
                created_at: created,
                updated_at: updated,
                origin: meta.origin,
                frame: window.map_or_else(Rect::default, |state| state.frame),
                is_open: window.is_some_and(|state| state.is_open),
                is_hidden: window.is_some_and(|state| state.is_hidden),
            };
            result.push((note, path));
        }
        result
    }

    /// Reads the windows file. A missing file gives no state; a file that cannot be parsed is
    /// kept as `<name>.bad` and gives no state, so the notes themselves still load.
    fn load_windows(&mut self) {
        let Ok(bytes) = fs::read(&self.windows_file) else {
            return;
        };
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        match serde_json::from_slice::<BTreeMap<String, WindowFile>>(bytes) {
            Ok(saved) => {
                for (text, state) in saved {
                    if let Some(id) = NoteId::parse(&text) {
                        self.windows.insert(id, state.into());
                    }
                }
            }
            Err(_) => {
                let _ = fs::copy(&self.windows_file, bad_path(&self.windows_file));
            }
        }
    }

    fn save_windows(&self) -> io::Result<()> {
        let saved: BTreeMap<String, WindowFile> = self
            .windows
            .iter()
            .map(|(id, state)| (id.to_string(), WindowFile::from(*state)))
            .collect();
        let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
        write_atomic(&self.windows_file, &bytes)
    }
}

/// Is `name` this stem, or the stem with a " 2"-style suffix `claim_file` added?
fn name_matches(name: &str, stem: &str) -> bool {
    let Some(rest) = name.strip_prefix(stem) else {
        return false;
    };
    rest.is_empty()
        || (rest.starts_with(' ')
            && rest.chars().count() > 1
            && rest[1..].chars().all(|c| c.is_numeric()))
}

/// "Stem.md", or "Stem 2.md", "Stem 3.md"… if taken by another file. Claims the name with a
/// create-new empty file so a racing writer gets the next number instead of overwriting.
fn claim_file(folder: &Path, stem: &str, own: Option<&Path>) -> io::Result<PathBuf> {
    let mut number = 1u64;
    loop {
        let name = if number == 1 {
            format!("{stem}.md")
        } else {
            format!("{stem} {number}.md")
        };
        let candidate = folder.join(name);
        if own == Some(candidate.as_path()) {
            return Ok(candidate);
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => number += 1,
            Err(error) => return Err(error),
        }
    }
}

/// Writes through a fresh temp file in the same folder, then renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let folder = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(folder)?;
    let (temp, mut file) = create_temp(folder)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = match written {
        Ok(()) => fs::rename(&temp, path),
        Err(error) => Err(error),
    };
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn create_temp(folder: &Path) -> io::Result<(PathBuf, File)> {
    loop {
        let path = folder.join(format!(
            ".note.tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

/// The first free copy name for an unparsable file, following `settings.rs`: `<name>.bad`,
/// then `.bad.1` through `.bad.9`; an existing `.bad.9` is reused last.
fn bad_path(path: &Path) -> PathBuf {
    let mut base = path.as_os_str().to_owned();
    base.push(".bad");
    let first = PathBuf::from(&base);
    if !first.exists() {
        return first;
    }
    let mut candidate = first;
    for number in 1..=9 {
        let mut name = base.clone();
        name.push(format!(".{number}"));
        candidate = PathBuf::from(name);
        if number == 9 || !candidate.exists() {
            break;
        }
    }
    candidate
}

/// The windows file's JSON form: an object keyed by the UUID text.
#[derive(Debug, Serialize, Deserialize)]
struct WindowFile {
    frame: FrameFile,
    #[serde(rename = "isOpen")]
    is_open: bool,
    /// Added by the Windows app; files written before it lack it and read as not hidden.
    /// Readers that do not know it ignore it (serde skips unknown fields).
    #[serde(rename = "isHidden", default, skip_serializing_if = "is_false")]
    is_hidden: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl From<WindowState> for WindowFile {
    fn from(state: WindowState) -> Self {
        WindowFile {
            frame: FrameFile {
                x: state.frame.x,
                y: state.frame.y,
                width: state.frame.width,
                height: state.frame.height,
            },
            is_open: state.is_open,
            is_hidden: state.is_hidden,
        }
    }
}

impl From<WindowFile> for WindowState {
    fn from(saved: WindowFile) -> Self {
        WindowState {
            frame: Rect::new(
                saved.frame.x,
                saved.frame.y,
                saved.frame.width,
                saved.frame.height,
            ),
            is_open: saved.is_open,
            is_hidden: saved.is_hidden,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct FrameFile {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn is_hidden(entry: &fs::DirEntry) -> bool {
    if entry.file_name().to_string_lossy().starts_with('.') {
        return true;
    }
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    entry
        .metadata()
        .map(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
        .unwrap_or(false)
}

/// Spaces, tabs and the `\r` of a CRLF line, like Swift's `CharacterSet.whitespaces` plus the
/// carriage return Windows editors leave at the end of a line.
fn trim_spaces(text: &str) -> &str {
    text.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r')
}

/// A metadata line: spaces and tabs at the ends, plus the `\r` of a CRLF file.
fn trim_line(text: &str) -> &str {
    text.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r')
}

/// The leading Markdown marker the Swift title regex removes, if any.
fn strip_markdown_prefix(line: &str) -> &str {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes)
        && let Some(rest) = strip_leading_whitespace(&line[hashes..])
    {
        return rest;
    }
    if let Some(first) = line.chars().next()
        && matches!(first, '-' | '*' | '+')
        && let Some(rest) = strip_leading_whitespace(&line[first.len_utf8()..])
    {
        let rest = strip_task_marker(rest).unwrap_or(rest);
        return rest;
    }
    let digits_end = line
        .char_indices()
        .take_while(|(_, c)| c.is_numeric())
        .map(|(index, c)| index + c.len_utf8())
        .last()
        .unwrap_or(0);
    if digits_end > 0
        && let Some(rest) = line[digits_end..].strip_prefix(['.', ')'])
        && let Some(rest) = strip_leading_whitespace(rest)
    {
        return rest;
    }
    if let Some(rest) = line.strip_prefix('>') {
        return rest.trim_start_matches(char::is_whitespace);
    }
    line
}

/// `\s+`; `None` when there is no whitespace to remove.
fn strip_leading_whitespace(text: &str) -> Option<&str> {
    let rest = text.trim_start_matches(char::is_whitespace);
    (rest.len() != text.len()).then_some(rest)
}

/// The optional `[ ]`, `[x]` or `[X]` task marker and the whitespace after it.
fn strip_task_marker(text: &str) -> Option<&str> {
    for marker in ["[ ]", "[x]", "[X]"] {
        if let Some(rest) = text.strip_prefix(marker) {
            return strip_leading_whitespace(rest);
        }
    }
    None
}

/// Removes every `*`, `_`, `` ` `` and `~`, like the Swift title regex.
fn strip_markers(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~'))
        .collect()
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| text.to_owned())
}

fn unquote(text: &str) -> String {
    if text.starts_with('"')
        && let Ok(value) = serde_json::from_str::<String>(text)
    {
        return value;
    }
    text.to_owned()
}

/// A `file:` URL's last path component, percent-decoded; `None` for other URLs.
fn file_url_last_component(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    let component = path.rsplit('/').next().unwrap_or(path);
    Some(percent_decode(component))
}

/// A URL's host: the authority without user info, port or brackets, lowercased. Only absolute
/// URLs with `://` have one, like `URL.host`.
fn url_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or(stripped)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Seconds and nanoseconds since the Unix epoch; times before it give a negative second count.
fn unix_parts(time: SystemTime) -> (i64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos()),
        Err(error) => {
            let duration = error.duration();
            if duration.subsec_nanos() == 0 {
                (-(duration.as_secs() as i64), 0)
            } else {
                (
                    -(duration.as_secs() as i64) - 1,
                    1_000_000_000 - duration.subsec_nanos(),
                )
            }
        }
    }
}

/// ISO 8601 as Swift's default `ISO8601DateFormatter` writes it: `2026-11-11T14:32:05Z`.
fn format_utc(time: SystemTime) -> String {
    let (seconds, _) = unix_parts(time);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = of_day / 3_600;
    let minute = (of_day % 3_600) / 60;
    let second = of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn parse_utc(text: &str) -> Option<SystemTime> {
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = number(&bytes[0..4])? as i64;
    let month = number(&bytes[5..7])?;
    let day = number(&bytes[8..10])?;
    let hour = number(&bytes[11..13])?;
    let minute = number(&bytes[14..16])?;
    let second = number(&bytes[17..19])?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let offset_minutes = match &bytes[19..] {
        [b'Z'] | [b'z'] => 0,
        [sign, h1, h2, b':', m1, m2] if *sign == b'+' || *sign == b'-' => {
            let hours = i64::from(number(&[*h1, *h2])?);
            let minutes = i64::from(number(&[*m1, *m2])?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let total = hours * 60 + minutes;
            if *sign == b'-' { -total } else { total }
        }
        [sign, h1, h2, m1, m2] if *sign == b'+' || *sign == b'-' => {
            let hours = i64::from(number(&[*h1, *h2])?);
            let minutes = i64::from(number(&[*m1, *m2])?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let total = hours * 60 + minutes;
            if *sign == b'-' { -total } else { total }
        }
        _ => return None,
    };
    let local_seconds = days_from_civil(year, month, day) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second);
    time_from_unix(local_seconds - offset_minutes * 60)
}

fn number(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(byte - b'0');
    }
    Some(value)
}

fn time_from_unix(seconds: i64) -> Option<SystemTime> {
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = (year - era * 400) as u64;
    let month_number = if month > 2 { month - 3 } else { month + 9 } as u64;
    let day_of_year = (153 * month_number + 2) / 5 + u64::from(day) - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era as i64 - 719_468
}

/// The reverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = (days - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year =
        day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_cut_keeps_plain_text_at_exactly_max() {
        assert_eq!(safe_cut("abcdef", 3), "abc");
        assert_eq!(safe_cut("abc", 3), "abc");
        assert_eq!(safe_cut("abc", 0), "");
    }

    #[test]
    fn safe_cut_does_not_split_a_combining_mark_from_its_base() {
        let text = "e\u{301}x";
        assert_eq!(safe_cut(text, 2), "e\u{301}", "the pair is kept whole");
        assert_eq!(safe_cut(text, 1), "", "the pair is dropped whole");
    }

    #[test]
    fn safe_cut_never_splits_a_family_emoji_joined_with_zwj() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let length = family.chars().count();
        assert_eq!(safe_cut(family, length), family);
        for max in 1..length {
            assert_eq!(safe_cut(family, max), "", "cut at {max}");
        }
    }

    #[test]
    fn safe_cut_keeps_a_skin_tone_modifier_with_its_emoji() {
        let thumbs_up = "\u{1F44D}\u{1F3FD}";
        assert_eq!(safe_cut(thumbs_up, 1), "");
        assert_eq!(safe_cut(thumbs_up, 2), thumbs_up);
    }

    #[test]
    fn safe_cut_does_not_split_a_flag_made_of_regional_indicators() {
        let germany = "\u{1F1E9}\u{1F1EA}";
        assert_eq!(safe_cut(germany, 1), "");
        assert_eq!(safe_cut(germany, 2), germany);
    }

    #[test]
    fn safe_cut_cuts_three_flags_after_a_whole_flag() {
        let three = "\u{1F1E9}\u{1F1EA}\u{1F1EB}\u{1F1F7}\u{1F1EE}\u{1F1F9}";
        assert_eq!(safe_cut(three, 3), "\u{1F1E9}\u{1F1EA}");
        assert_eq!(safe_cut(three, 5), "\u{1F1E9}\u{1F1EA}\u{1F1EB}\u{1F1F7}");
    }

    #[test]
    fn safe_cut_leaves_a_short_string_alone() {
        assert_eq!(safe_cut("short", 48), "short");
        assert_eq!(safe_cut("", 47), "");
    }

    #[test]
    fn shell_delete_buffer_is_double_nul_terminated() {
        let path = Path::new(r"C:\notes\Hello.md");
        let buffer = shell_delete_buffer(path).expect("an absolute path");
        let expected: Vec<u16> = path.as_os_str().encode_wide().collect();
        assert_eq!(&buffer[..expected.len()], expected.as_slice());
        assert_eq!(&buffer[expected.len()..], &[0, 0]);
        assert_eq!(buffer.iter().filter(|unit| **unit == 0).count(), 2);
        // The comment in `trash` promises a list, so the copy above must be the path itself.
        assert_eq!(
            String::from_utf16(&buffer[..expected.len()]).unwrap(),
            path.display().to_string()
        );
    }

    #[test]
    fn shell_delete_buffer_refuses_paths_the_shell_api_would_change() {
        let relative = shell_delete_buffer(Path::new("Hello.md")).unwrap_err();
        assert_eq!(relative.kind(), io::ErrorKind::InvalidInput);

        let trailing_space = shell_delete_buffer(Path::new(r"C:\notes\Hello.md ")).unwrap_err();
        assert_eq!(trailing_space.kind(), io::ErrorKind::InvalidInput);

        let trailing_dot = shell_delete_buffer(Path::new(r"C:\notes\Hello.")).unwrap_err();
        assert_eq!(trailing_dot.kind(), io::ErrorKind::InvalidInput);

        let long = format!(r"C:\notes\{}.md", "a".repeat(MAX_PATH_UNITS));
        let too_long = shell_delete_buffer(Path::new(&long)).unwrap_err();
        assert_eq!(too_long.kind(), io::ErrorKind::InvalidInput);
    }
}
