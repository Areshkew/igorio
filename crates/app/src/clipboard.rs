//! The clipboard for one paste: save the user's clipboard, put a screenshot (or a path, or a
//! file) there, learn whether another app read it, and put the user's clipboard back.
//! From `PasteTarget.swift` (`PromisedPasteboardItem`, `PasteboardSnapshot`). The timing and
//! the decisions are in `igorio_core::paste_policy`; this module reports the facts those rules
//! ask for and performs the writes they decide.
//!
//! # Threads
//!
//! [`Clipboard::start`] spawns one thread. It owns a hidden message-only window, the clipboard
//! owner, and pumps its messages. Every `OpenClipboard` happens on that thread (rule 4), never
//! on the hook or UI thread.
//!
//! Requests ([`Clipboard::snapshot`], [`Clipboard::stage`], [`Clipboard::restage`],
//! [`Clipboard::restore`]) go over a bounded channel and return a [`Ticket`] at once. Each reply
//! arrives on a second channel ([`Clipboard::try_reply`]), and the `wake` callback given to
//! `start` runs on the clipboard thread after every reply and every change in [`Watch`]; the
//! app posts a message to its UI window from it. Requests do not block the caller because a
//! snapshot can wait seconds on another process: reading a format that the source app renders
//! on demand (Excel after a large copy) blocks until that app has rendered it (rule 2). A
//! request that waited in the queue longer than [`STALE_AFTER`] is dropped with
//! [`ClipError::Stale`]; at most [`QUEUE_CAP`] requests can wait. [`Clipboard::wait_reply`]
//! blocks with a timeout, for tools and tests.
//!
//! [`Clipboard::watch`] returns the facts the thread keeps (reads, overwrites, foreign
//! changes) plus the current sequence number. It copies them under a mutex and never calls
//! into another process, so the UI thread can poll it every frame.
//!
//! # Read detection
//!
//! The staged formats are written with delayed rendering (`SetClipboardData(format, NULL)`).
//! When another process asks for one, Windows sends `WM_RENDERFORMAT` to the owner window;
//! that is a read (the Windows form of the lazy data provider in `PasteTarget.swift`). Windows
//! keeps rendered data, so each format is counted at most once per staging. Formats that
//! Windows synthesizes (`CF_DIB` and `CF_BITMAP` from `CF_DIBV5`; `CF_TEXT` from
//! `CF_UNICODETEXT`) still render their source format first, so they count too. Markers are
//! written at once and never count. If the window is destroyed while it still owns delayed
//! formats, `WM_RENDERALLFORMATS` renders them all, so the clipboard stays valid after exit.
//!
//! # Snapshot allow-list and the "do not stage" rule (rule 6)
//!
//! Only formats that are plain `HGLOBAL` bytes are copied: `CF_UNICODETEXT`, `CF_TEXT`,
//! `CF_OEMTEXT`, `CF_LOCALE`, `CF_DIB`, `CF_DIBV5`, `CF_TIFF`, `CF_HDROP`, `CF_SYLK`, `CF_DIF`
//! and the registered names in [`NAMED_KEEP`]. Windows synthesizes some formats from others, so
//! only one of each family is kept and Windows re-creates the rest after a restore:
//! - text: the first present of `CF_UNICODETEXT`, `CF_TEXT`, `CF_OEMTEXT` (Unicode first, since
//!   converting from it loses nothing; `CF_LOCALE` is kept for the code page);
//! - images: `CF_DIBV5` before `CF_DIB` (V5 keeps alpha); `CF_BITMAP` and `CF_PALETTE` are GDI
//!   handles and are re-created from the DIB. A `CF_BITMAP` source is saved through the DIB
//!   Windows synthesizes from it.
//!
//! Never read: GDI handles (`CF_BITMAP`, `CF_PALETTE`, the metafiles, `0x300..=0x3FF`),
//! private formats (`0x200..=0x2FF`, display formats) and every registered name not on the list,
//! which includes OLE's private formats ("DataObject", "Ole Private Data", "Object Descriptor",
//! "Embed Source", ...). Not reading a format also means not forcing its owner to render it.
//! Reads run in priority order (text, files and links, rich text, tables, images) within a
//! [`SNAPSHOT_TIME`] budget and a [`SNAPSHOT_CAP`] byte cap.
//!
//! [`Snapshot::lost`] says whether the user would notice the difference after a restore:
//! - the cap or the time budget cut the snapshot short;
//! - a kind of content that was there (text, rich text, image, files, link) has no saved
//!   format left;
//! - the clipboard held content and nothing at all could be saved.
//!
//! App-private formats (Office's own formats, Chromium's source URL, OLE data objects) are
//! skipped and listed, but do not count by themselves: a copy from Word comes back as text,
//! HTML and RTF, which pastes the same everywhere except "Paste Special" in Word.
//! The rule for the caller: when `lost()` is `Some`, do not stage; keep the card in hand. With
//! no snapshot at all (the clipboard stayed locked) do not stage either.
//!
//! # Rule 5
//!
//! Staged and restored data both carry "ExcludeClipboardContentFromMonitorProcessing",
//! "CanIncludeInClipboardHistory" = 0 and "CanUploadToCloudClipboard" = 0, so neither the
//! screenshot nor a second copy of the user's data enters `Win+V` history or the cloud
//! clipboard, and well-behaved clipboard managers do not read (and so do not "paste") it.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use igorio_kit::Image;
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM,
    LRESULT, SetLastError, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, GetClipboardFormatNameW, GetClipboardOwner, GetClipboardSequenceNumber,
    GetOpenClipboardWindow, OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener,
    SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_BITMAP, CF_DIB, CF_DIBV5, CF_DIF, CF_HDROP, CF_LOCALE, CF_OEMTEXT, CF_PALETTE, CF_SYLK,
    CF_TEXT, CF_TIFF, CF_UNICODETEXT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowThreadProcessId, HWND_MESSAGE, MSG, MWMO_NONE, MsgWaitForMultipleObjectsEx,
    PM_NOREMOVE, PM_QS_SENDMESSAGE, PeekMessageW, PostMessageW, QS_SENDMESSAGE, RegisterClassW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_DESTROYCLIPBOARD,
    WM_RENDERALLFORMATS, WM_RENDERFORMAT, WNDCLASSW,
};
use windows::core::{HRESULT, PCWSTR, w};

/// `OpenClipboard` tries this many times, [`OPEN_RETRY_WAIT`] apart (about 100 ms in all).
pub const OPEN_TRIES: u32 = 10;
pub const OPEN_RETRY_WAIT: Duration = Duration::from_millis(10);
/// A request that waited this long in the queue is dropped unanswered by the clipboard.
pub const STALE_AFTER: Duration = Duration::from_secs(2);
/// At most this many requests wait for the clipboard thread.
pub const QUEUE_CAP: usize = 4;
/// A snapshot saves at most this many bytes in all.
pub const SNAPSHOT_CAP: usize = 64 * 1024 * 1024;
/// A snapshot starts no new read after this much time.
pub const SNAPSHOT_TIME: Duration = Duration::from_millis(1500);

const CLASS: PCWSTR = w!("IgorioClipboard");
const WM_COMMAND_QUEUED: u32 = WM_APP + 1;
const WM_QUIT_THREAD: u32 = WM_APP + 2;
/// Sequence numbers of our own writes kept to recognise their `WM_CLIPBOARDUPDATE`.
const OWN_SEQUENCES: usize = 32;

const UNICODETEXT: u32 = CF_UNICODETEXT.0 as u32;
const TEXT: u32 = CF_TEXT.0 as u32;
const OEMTEXT: u32 = CF_OEMTEXT.0 as u32;
const LOCALE: u32 = CF_LOCALE.0 as u32;
const DIB: u32 = CF_DIB.0 as u32;
const DIBV5: u32 = CF_DIBV5.0 as u32;
const BITMAP: u32 = CF_BITMAP.0 as u32;
const PALETTE: u32 = CF_PALETTE.0 as u32;
const HDROP: u32 = CF_HDROP.0 as u32;
const TIFF: u32 = CF_TIFF.0 as u32;

/// Kinds of content the user can tell apart after a restore (see [`Snapshot::lost`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    RichText,
    Image,
    Files,
    Link,
}

/// Standard formats that are plain `HGLOBAL` bytes: (id, read priority, kind).
const STANDARD_KEEP: &[(u32, u8, Option<Kind>)] = &[
    (UNICODETEXT, 0, Some(Kind::Text)),
    (TEXT, 0, Some(Kind::Text)),
    (OEMTEXT, 0, Some(Kind::Text)),
    (LOCALE, 0, None),
    (HDROP, 1, Some(Kind::Files)),
    (CF_SYLK.0 as u32, 3, None),
    (CF_DIF.0 as u32, 3, None),
    (DIBV5, 4, Some(Kind::Image)),
    (DIB, 4, Some(Kind::Image)),
    (TIFF, 4, Some(Kind::Image)),
];

/// Registered formats that are plain `HGLOBAL` bytes: (name, read priority, kind).
/// Names compare without case, as `RegisterClipboardFormatW` does.
pub const NAMED_KEEP: &[(&str, u8, Option<Kind>)] = &[
    ("UniformResourceLocatorW", 1, Some(Kind::Link)),
    ("UniformResourceLocator", 1, Some(Kind::Link)),
    ("Shell IDList Array", 1, Some(Kind::Files)),
    ("Preferred DropEffect", 1, None),
    ("FileNameW", 1, Some(Kind::Files)),
    ("FileName", 1, Some(Kind::Files)),
    ("HTML Format", 2, Some(Kind::RichText)),
    ("Rich Text Format", 2, Some(Kind::RichText)),
    ("Rich Text Format Without Objects", 2, Some(Kind::RichText)),
    ("Csv", 3, None),
    ("PNG", 4, Some(Kind::Image)),
    ("JFIF", 4, Some(Kind::Image)),
    ("GIF", 4, Some(Kind::Image)),
];

/// Formats that only say how to treat the rest. Not content; we write our own on restore.
const MARKERS: &[&str] = &[
    "ExcludeClipboardContentFromMonitorProcessing",
    "CanIncludeInClipboardHistory",
    "CanUploadToCloudClipboard",
    "Clipboard Viewer Ignore",
];

/// Names whose kind matters for [`Snapshot::lost`] although they are not saved: virtual
/// files (Outlook attachments, files inside a zip) whose contents only exist through OLE.
const NAMED_KIND_ONLY: &[(&str, Kind)] = &[
    ("FileGroupDescriptorW", Kind::Files),
    ("FileGroupDescriptor", Kind::Files),
];

// MARK: Public types

/// Names one request; its [`Reply`] carries the same ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ticket(pub u64);

/// What to put on the clipboard.
#[derive(Debug, Clone)]
pub enum Payload {
    /// As "PNG" and `CF_DIBV5`, both with alpha.
    Image(Arc<Image>),
    /// As `CF_UNICODETEXT` (a shell-quoted path for terminals).
    Text(String),
    /// As `CF_HDROP` plus "Preferred DropEffect" = copy, like a file copied in Explorer.
    /// The path must be absolute.
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipError {
    /// [`QUEUE_CAP`] requests are already waiting.
    Busy,
    /// The request waited longer than [`STALE_AFTER`]; the clipboard was not touched.
    Stale,
    /// Another window held the clipboard open through every retry. `holder` is its process.
    Locked { holder: Option<u32> },
    /// `restore`: the sequence number is not the one the caller gave; nothing was written.
    Changed,
    /// `restage`: nothing is staged.
    NothingStaged,
    /// `snapshot`: the clipboard holds our own staged data; use the snapshot taken before.
    OwnData,
    /// The payload cannot go on the clipboard (empty text, relative path, encoder error).
    Payload(String),
    /// A Win32 call failed.
    Win32 { call: &'static str, code: i32 },
    /// The clipboard thread is gone.
    Stopped,
    /// `stage` or `restage` failed after the clipboard was emptied: it is left empty (none
    /// of the user's data is on it any more). `sequence` is read after `CloseClipboard`.
    /// Every other `stage` error leaves the clipboard as it was.
    Emptied {
        cause: Box<ClipError>,
        sequence: u32,
    },
}

impl fmt::Display for ClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClipError::Busy => write!(f, "too many clipboard requests waiting"),
            ClipError::Stale => write!(f, "clipboard request dropped as stale"),
            ClipError::Locked { holder: Some(pid) } => {
                write!(f, "clipboard held open by process {pid}")
            }
            ClipError::Locked { holder: None } => write!(f, "clipboard held open"),
            ClipError::Changed => write!(f, "clipboard changed since the given sequence"),
            ClipError::NothingStaged => write!(f, "nothing staged"),
            ClipError::OwnData => write!(f, "clipboard holds our own staged data"),
            ClipError::Payload(why) => write!(f, "bad payload: {why}"),
            ClipError::Win32 { call, code } => write!(f, "{call} failed: {code:#010x}"),
            ClipError::Stopped => write!(f, "clipboard thread stopped"),
            ClipError::Emptied { cause, sequence } => {
                write!(f, "clipboard left empty (seq {sequence}) after: {cause}")
            }
        }
    }
}

impl std::error::Error for ClipError {}

#[derive(Debug, Clone)]
pub enum Done {
    Snapshot(Snapshot),
    /// `stage` or `restage` wrote the clipboard; the sequence number after closing it.
    Staged {
        sequence: u32,
    },
    /// `restore` wrote the clipboard; the sequence number after closing it.
    Restored {
        sequence: u32,
    },
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub ticket: Ticket,
    pub result: Result<Done, ClipError>,
}

/// The facts `paste_policy` needs, as of now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    /// `GetClipboardSequenceNumber` now. Our own writes change it too.
    pub sequence: u32,
    /// The last clipboard change made by anyone but this module (`WM_CLIPBOARDUPDATE`).
    pub last_change: Option<Instant>,
    /// The current staging, until `restore` replaces it.
    pub staged: Option<Staged>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Staged {
    /// The `stage` request that wrote it.
    pub ticket: Ticket,
    /// First written.
    pub at: Instant,
    /// Sequence number after the last (re)write.
    pub sequence: u32,
    /// `restage` calls that succeeded.
    pub rewrites: u32,
    /// Formats another process rendered (read), over all rewrites.
    pub reads: u32,
    pub first_read_at: Option<Instant>,
    pub last_read_at: Option<Instant>,
    /// Process of the window that had the clipboard open for the last read, when known
    /// (readers that open the clipboard without a window are not known).
    pub last_reader: Option<u32>,
    /// Someone else wrote the clipboard after our last (re)write (`WM_DESTROYCLIPBOARD`, or a
    /// foreign `WM_CLIPBOARDUPDATE`). Cleared by `restage`.
    pub overwritten_at: Option<Instant>,
}

/// Why a format found on the clipboard is not in the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Windows re-creates it from a saved format.
    Synthesized,
    /// A GDI or metafile handle, not bytes.
    Handle,
    /// `0x200..=0x2FF` or a display format: meaningful only to its owner.
    Private,
    /// Not on the allow-list (app-private and OLE formats).
    NotListed,
    /// Listed, but `GetClipboardData` or `GlobalLock` failed.
    Unreadable,
    /// Would pass [`SNAPSHOT_CAP`].
    OverCap,
    /// Not read: [`SNAPSHOT_TIME`] ran out.
    OutOfTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub format: u32,
    pub name: String,
    pub reason: SkipReason,
}

/// What a restore of this snapshot would lose that the user would notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lost {
    OverCap,
    OutOfTime,
    Kind(Kind),
    /// The clipboard held content and none of it could be saved.
    Everything,
}

/// The user's clipboard as saved bytes. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Snapshot(Arc<SnapshotData>);

#[derive(Debug)]
struct SnapshotData {
    formats: Vec<Saved>,
    skipped: Vec<Skipped>,
    lost: Option<Lost>,
    total: usize,
    sequence: u32,
    taken_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Saved {
    format: u32,
    name: String,
    bytes: Vec<u8>,
}

impl Snapshot {
    /// Sequence number read after `CloseClipboard`.
    pub fn sequence(&self) -> u32 {
        self.0.sequence
    }

    pub fn taken_at(&self) -> Instant {
        self.0.taken_at
    }

    /// Saved formats in clipboard order: (id, name, bytes).
    pub fn formats(&self) -> impl Iterator<Item = (u32, &str, &[u8])> {
        self.0
            .formats
            .iter()
            .map(|s| (s.format, s.name.as_str(), s.bytes.as_slice()))
    }

    pub fn skipped(&self) -> &[Skipped] {
        &self.0.skipped
    }

    pub fn total_bytes(&self) -> usize {
        self.0.total
    }

    /// `Some` when a restore would visibly differ from what the user had: do not stage.
    pub fn lost(&self) -> Option<Lost> {
        self.0.lost
    }

    pub fn safe_to_replace(&self) -> bool {
        self.0.lost.is_none()
    }
}

/// `GetClipboardSequenceNumber`. Needs no open clipboard; callable from any thread.
pub fn sequence() -> u32 {
    // SAFETY: no arguments.
    unsafe { GetClipboardSequenceNumber() }
}

/// A readable name for a format id: the registered name, or the `CF_` name.
pub fn format_name(id: u32) -> String {
    if id >= 0xC000
        && let Some(name) = registered_name(id)
    {
        return name;
    }
    let name = match id {
        1 => "CF_TEXT",
        2 => "CF_BITMAP",
        3 => "CF_METAFILEPICT",
        4 => "CF_SYLK",
        5 => "CF_DIF",
        6 => "CF_TIFF",
        7 => "CF_OEMTEXT",
        8 => "CF_DIB",
        9 => "CF_PALETTE",
        10 => "CF_PENDATA",
        11 => "CF_RIFF",
        12 => "CF_WAVE",
        13 => "CF_UNICODETEXT",
        14 => "CF_ENHMETAFILE",
        15 => "CF_HDROP",
        16 => "CF_LOCALE",
        17 => "CF_DIBV5",
        0x80 => "CF_OWNERDISPLAY",
        0x81 => "CF_DSPTEXT",
        0x82 => "CF_DSPBITMAP",
        0x83 => "CF_DSPMETAFILEPICT",
        0x8E => "CF_DSPENHMETAFILE",
        _ => return format!("{id:#06x}"),
    };
    name.into()
}

// MARK: Clipboard handle

/// The clipboard thread. Dropping it stops the thread; delayed formats it still owns are
/// rendered first (`WM_RENDERALLFORMATS`), so a staged screenshot stays pasteable.
pub struct Clipboard {
    client: CommandClient,
    replies: Receiver<Reply>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct CommandClient {
    commands: SyncSender<Command>,
    shared: Arc<Shared>,
    /// The owner window as a number, only to post to it. Never leaves the module.
    window: isize,
    next_ticket: Arc<AtomicU64>,
}

impl Clipboard {
    /// Starts the clipboard thread and its owner window. `wake` runs on that thread after
    /// each reply and each change in [`Watch`]; keep it short (post a message).
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> Result<Clipboard, ClipError> {
        let shared = Arc::new(Shared {
            facts: Mutex::new(Facts::default()),
            wake: Box::new(wake),
        });
        let (commands, command_rx) = mpsc::sync_channel(QUEUE_CAP);
        let (reply_tx, replies) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || run(thread_shared, command_rx, reply_tx, ready_tx))
            .map_err(|_| ClipError::Stopped)?;
        let window = ready_rx.recv().map_err(|_| ClipError::Stopped)??;
        Ok(Clipboard {
            client: CommandClient {
                commands,
                shared,
                window,
                next_ticket: Arc::new(AtomicU64::new(1)),
            },
            replies,
            thread: Some(thread),
        })
    }

    /// Saves the user's clipboard. Reply: [`Done::Snapshot`].
    pub fn snapshot(&self) -> Result<Ticket, ClipError> {
        self.send(Request::Snapshot)
    }

    /// Empties the clipboard and promises the payload (delayed rendering). Reply:
    /// [`Done::Staged`]. On failure the clipboard may be left empty: restore the snapshot.
    pub fn stage(&self, payload: Payload) -> Result<Ticket, ClipError> {
        self.send(Request::Stage(payload))
    }

    /// Writes the staged payload again (paste_policy's `OverwriteStep::Rewrite`). Read counts
    /// carry over; `overwritten_at` is cleared. Reply: [`Done::Staged`].
    pub fn restage(&self) -> Result<Ticket, ClipError> {
        self.send(Request::Restage)
    }

    /// Writes the snapshot back. With `if_sequence`, writes only if the clipboard's sequence
    /// number is still that one (checked while the clipboard is open), else
    /// [`ClipError::Changed`]. Ends the staging. Reply: [`Done::Restored`].
    pub fn restore(
        &self,
        snapshot: &Snapshot,
        if_sequence: Option<u32>,
    ) -> Result<Ticket, ClipError> {
        self.send(Request::Restore(
            snapshot.clone(),
            RestoreGuard::Sequence(if_sequence),
        ))
    }

    /// Writes the snapshot back only if the clipboard still holds our current staging
    /// (we own it and nobody emptied it since; checked while it is open), else
    /// [`ClipError::Changed`]. Renders of the staging do not count as changes, unlike a
    /// sequence-number guard. Reply: [`Done::Restored`].
    pub fn restore_over_staging(&self, snapshot: &Snapshot) -> Result<Ticket, ClipError> {
        self.send(Request::Restore(snapshot.clone(), RestoreGuard::OwnStaging))
    }

    pub fn try_reply(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }

    /// Blocks up to `timeout` for the next reply. For tools and tests, not the UI thread.
    pub fn wait_reply(&self, timeout: Duration) -> Option<Reply> {
        self.replies.recv_timeout(timeout).ok()
    }

    /// The facts as of now. Never waits on another process.
    pub fn watch(&self) -> Watch {
        let facts = *self.client.shared.lock();
        Watch {
            sequence: sequence(),
            last_change: facts.last_change,
            staged: facts.staged,
        }
    }

    pub(crate) fn screenshot_reader(&self) -> ScreenshotClipboard {
        ScreenshotClipboard(self.client.clone())
    }

    fn send(&self, request: Request) -> Result<Ticket, ClipError> {
        self.client.send(request)
    }
}

impl CommandClient {
    fn send(&self, request: Request) -> Result<Ticket, ClipError> {
        let ticket = Ticket(self.next_ticket.fetch_add(1, Ordering::Relaxed));
        let command = Command {
            ticket,
            queued: Instant::now(),
            request,
        };
        match self.commands.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(ClipError::Busy),
            Err(TrySendError::Disconnected(_)) => return Err(ClipError::Stopped),
        }
        // SAFETY: posting to a window by handle is allowed from any thread; if the window is
        // gone the call fails and nothing else happens.
        unsafe {
            PostMessageW(
                Some(hwnd(self.window)),
                WM_COMMAND_QUEUED,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .map_err(|_| ClipError::Stopped)?;
        Ok(ticket)
    }
}

/// Latest foreign clipboard write. Windows can coalesce updates if the clipboard changes
/// again before its message is processed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ClipboardChange {
    pub sequence: u32,
    pub time: f64,
    pub captured_at: SystemTime,
}

pub(crate) struct ImageMetadata {
    pub formats: Vec<(u32, Option<String>)>,
    pub owner_pid: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ImageFormat {
    Png,
    DibV5,
    Dib,
}

impl ImageFormat {
    pub(crate) fn preferred(formats: &[(u32, Option<String>)]) -> Option<Self> {
        if formats.iter().any(|(_, name)| {
            name.as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case("PNG"))
        }) {
            Some(Self::Png)
        } else if formats.iter().any(|(id, _)| *id == DIBV5) {
            Some(Self::DibV5)
        } else if formats.iter().any(|(id, _)| *id == DIB || *id == BITMAP) {
            Some(Self::Dib)
        } else {
            None
        }
    }
}

/// Read-only client. Its per-request receivers leave the paste transaction reply queue
/// to the app. The app keeps ownership of the clipboard thread.
#[derive(Clone)]
pub(crate) struct ScreenshotClipboard(CommandClient);

impl ScreenshotClipboard {
    pub(crate) fn latest(&self) -> Option<ClipboardChange> {
        self.0.shared.lock().screenshot_change
    }

    pub(crate) fn inspect(
        &self,
        change: ClipboardChange,
        deadline: Instant,
    ) -> Result<Receiver<Result<ImageMetadata, ClipError>>, ClipError> {
        let (tx, rx) = mpsc::channel();
        self.0.send(Request::InspectImage(change, deadline, tx))?;
        Ok(rx)
    }

    pub(crate) fn read(
        &self,
        change: ClipboardChange,
        format: ImageFormat,
        deadline: Instant,
    ) -> Result<Receiver<Result<Vec<u8>, ClipError>>, ClipError> {
        let (tx, rx) = mpsc::channel();
        self.0
            .send(Request::ReadImage(change, format, deadline, tx))?;
        Ok(rx)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: as in `send`.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd(self.client.window)),
                WM_QUIT_THREAD,
                WPARAM(0),
                LPARAM(0),
            )
        };
        // Wait for `WM_RENDERALLFORMATS` to finish, but not forever: the thread can be inside
        // a snapshot read that waits on another app.
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !thread.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

// MARK: Shared facts

struct Shared {
    facts: Mutex<Facts>,
    wake: Box<dyn Fn() + Send + Sync>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Facts {
    last_change: Option<Instant>,
    screenshot_change: Option<ClipboardChange>,
    staged: Option<Staged>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Facts> {
        self.facts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn update(&self, change: impl FnOnce(&mut Facts)) {
        change(&mut self.lock());
        (self.wake)();
    }
}

// MARK: Clipboard thread

enum Request {
    Snapshot,
    Stage(Payload),
    Restage,
    Restore(Snapshot, RestoreGuard),
    InspectImage(
        ClipboardChange,
        Instant,
        Sender<Result<ImageMetadata, ClipError>>,
    ),
    ReadImage(
        ClipboardChange,
        ImageFormat,
        Instant,
        Sender<Result<Vec<u8>, ClipError>>,
    ),
}

/// When a restore may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreGuard {
    /// Always, or only at this sequence number.
    Sequence(Option<u32>),
    /// Only while our staging is still on the clipboard.
    OwnStaging,
}

impl RestoreGuard {
    /// `current`: the sequence number now; `own_staging`: we own the clipboard and our
    /// staging was not emptied by someone else.
    fn allows(self, current: u32, own_staging: bool) -> bool {
        match self {
            RestoreGuard::Sequence(expected) => expected.is_none_or(|e| e == current),
            RestoreGuard::OwnStaging => own_staging,
        }
    }
}

/// A stage error, marked when the clipboard was already emptied.
fn after_empty(err: ClipError, emptied: bool, sequence: u32) -> ClipError {
    if emptied {
        ClipError::Emptied {
            cause: Box::new(err),
            sequence,
        }
    } else {
        err
    }
}

struct Command {
    ticket: Ticket,
    queued: Instant,
    request: Request,
}

/// State of the owner window, on the clipboard thread only. Borrowed only for short
/// stretches that make no clipboard call, because `EmptyClipboard` and `GetClipboardData`
/// can re-enter the window procedure.
struct Owner {
    shared: Arc<Shared>,
    stage: Option<Stage>,
    /// Sequence numbers our own writes produced.
    own: VecDeque<u32>,
    /// Last sequence number a `WM_CLIPBOARDUPDATE` was handled for.
    last_seen: u32,
}

struct Stage {
    payload: Payload,
    /// Formats promised with delayed rendering.
    promised: Vec<u32>,
    /// Promised formats already rendered since the last (re)write.
    rendered: Vec<u32>,
    /// Encoded bytes, kept across rewrites so a rewrite does not encode again.
    cache: Vec<(u32, Arc<Vec<u8>>)>,
    /// False once someone else emptied the clipboard: render nothing more.
    owned: bool,
}

impl Owner {
    fn observe(&mut self, sequence: u32) -> bool {
        if sequence == self.last_seen {
            return false;
        }
        self.last_seen = sequence;
        !self.own.contains(&sequence)
    }

    fn remember_own(&mut self, sequence: u32) {
        if self.own.len() == OWN_SEQUENCES {
            self.own.pop_front();
        }
        self.own.push_back(sequence);
    }
}

thread_local! {
    static OWNER: RefCell<Option<Owner>> = const { RefCell::new(None) };
    /// Set around our own `EmptyClipboard`, whose `WM_DESTROYCLIPBOARD` is not an overwrite.
    static EMPTYING: Cell<bool> = const { Cell::new(false) };
}

fn with_owner<R>(f: impl FnOnce(&mut Owner) -> R) -> Option<R> {
    OWNER.with(|cell| {
        let mut borrow = cell.try_borrow_mut().ok()?;
        borrow.as_mut().map(f)
    })
}

fn run(
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    replies: Sender<Reply>,
    ready: Sender<Result<isize, ClipError>>,
) {
    let window = match create_window() {
        Ok(window) => window,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    OWNER.with(|cell| {
        *cell.borrow_mut() = Some(Owner {
            shared: shared.clone(),
            stage: None,
            own: VecDeque::new(),
            last_seen: sequence(),
        })
    });
    // SAFETY: our own window, on its thread.
    if let Err(err) = unsafe { AddClipboardFormatListener(window) } {
        let _ = ready.send(Err(win32("AddClipboardFormatListener", &err)));
        // SAFETY: our own window, on its thread.
        let _ = unsafe { DestroyWindow(window) };
        return;
    }
    let _ = ready.send(Ok(window.0 as isize));

    let mut msg = MSG::default();
    // SAFETY: `msg` is a local; the loop ends on WM_QUIT (0) or an error (-1).
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        match msg.message {
            WM_COMMAND_QUEUED => {
                while let Ok(command) = commands.try_recv() {
                    if let Some(result) = execute(window, command) {
                        let _ = replies.send(result);
                        (shared.wake)();
                    }
                }
            }
            WM_QUIT_THREAD => break,
            // SAFETY: a message this thread received.
            _ => unsafe {
                DispatchMessageW(&msg);
            },
        }
    }
    // Requests still queued are answered so no caller waits for them.
    while let Ok(command) = commands.try_recv() {
        match command.request {
            Request::InspectImage(_, _, reply) => {
                let _ = reply.send(Err(ClipError::Stopped));
            }
            Request::ReadImage(_, _, _, reply) => {
                let _ = reply.send(Err(ClipError::Stopped));
            }
            _ => {
                let _ = replies.send(Reply {
                    ticket: command.ticket,
                    result: Err(ClipError::Stopped),
                });
            }
        }
    }
    // SAFETY: our own window, on its thread. `DestroyWindow` sends `WM_RENDERALLFORMATS`
    // while we still own delayed formats; `OWNER` is still set for it.
    unsafe {
        let _ = RemoveClipboardFormatListener(window);
        let _ = DestroyWindow(window);
    }
    OWNER.with(|cell| cell.borrow_mut().take());
}

fn execute(window: HWND, command: Command) -> Option<Reply> {
    let Command {
        ticket,
        queued,
        request,
    } = command;
    let result = match request {
        Request::InspectImage(change, deadline, reply) => {
            let result = inspect_image(window, change, deadline);
            let _ = reply.send(result);
            return None;
        }
        Request::ReadImage(change, format, deadline, reply) => {
            let result = read_image(window, change, format, deadline);
            let _ = reply.send(result);
            return None;
        }
        _ if queued.elapsed() > STALE_AFTER => Err(ClipError::Stale),
        Request::Snapshot => take_snapshot(window).map(Done::Snapshot),
        Request::Stage(payload) => {
            write_stage(window, ticket, Some(payload)).map(|sequence| Done::Staged { sequence })
        }
        Request::Restage => {
            write_stage(window, ticket, None).map(|sequence| Done::Staged { sequence })
        }
        Request::Restore(snapshot, guard) => {
            write_restore(window, &snapshot, guard).map(|sequence| Done::Restored { sequence })
        }
    };
    Some(Reply { ticket, result })
}

fn create_window() -> Result<HWND, ClipError> {
    // SAFETY: a null module name returns the exe's own module.
    let instance = HINSTANCE(
        unsafe { GetModuleHandleW(None) }
            .map_err(|e| win32("GetModuleHandleW", &e))?
            .0,
    );
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: CLASS,
        ..Default::default()
    };
    // SAFETY: `class` points only to static data and a `'static` function.
    if unsafe { RegisterClassW(&class) } == 0 {
        let error = windows::core::Error::from_thread();
        // A second `Clipboard` in the same process finds the class registered.
        if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(win32("RegisterClassW", &error));
        }
    }
    // SAFETY: the class is registered above; `HWND_MESSAGE` makes a message-only window
    // (never shown, no z-order, still a clipboard owner and listener); no creation parameter.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Igorio clipboard"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|e| win32("CreateWindowExW", &e))
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_RENDERFORMAT => {
            on_render(w.0 as u32);
            LRESULT(0)
        }
        WM_RENDERALLFORMATS => {
            on_render_all(hwnd);
            LRESULT(0)
        }
        WM_DESTROYCLIPBOARD => {
            if !EMPTYING.get() {
                on_overwritten();
            }
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            on_update();
            LRESULT(0)
        }
        // SAFETY: the arguments come unchanged from the system.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// Bytes for a promised format, encoded once.
fn render_bytes(format: u32) -> Option<Arc<Vec<u8>>> {
    with_owner(|owner| {
        let stage = owner
            .stage
            .as_mut()
            .filter(|s| s.owned && s.promised.contains(&format))?;
        if let Some((_, bytes)) = stage.cache.iter().find(|(f, _)| *f == format) {
            return Some(bytes.clone());
        }
        let bytes = Arc::new(encode(&stage.payload, format, ids())?);
        stage.cache.push((format, bytes.clone()));
        Some(bytes)
    })
    .flatten()
}

/// `WM_RENDERFORMAT`: another process asked for a promised format. The clipboard is open
/// (by the reader), so `SetClipboardData` is called without opening it.
fn on_render(format: u32) {
    let Some(bytes) = render_bytes(format) else {
        return;
    };
    let reader = open_clipboard_process();
    let Ok(global) = Global::new(&bytes) else {
        return;
    };
    if global.set(format).is_err() {
        return;
    }
    // A render may change the sequence number; that change is ours, not a foreign write.
    let sequence = sequence();
    let now = Instant::now();
    let shared = with_owner(|owner| {
        owner.remember_own(sequence);
        if let Some(stage) = owner.stage.as_mut() {
            stage.rendered.push(format);
        }
        owner.shared.clone()
    });
    if let Some(shared) = shared {
        shared.update(|facts| {
            if let Some(staged) = facts.staged.as_mut() {
                staged.reads += 1;
                staged.first_read_at.get_or_insert(now);
                staged.last_read_at = Some(now);
                staged.last_reader = reader;
            }
        });
    }
}

/// `WM_RENDERALLFORMATS`: the window is being destroyed while it owns delayed formats.
/// Render all of them so they stay on the clipboard. Not counted as reads.
fn on_render_all(window: HWND) {
    let pending = with_owner(|owner| {
        owner
            .stage
            .as_ref()
            .filter(|s| s.owned)
            .map(|s| {
                s.promised
                    .iter()
                    .copied()
                    .filter(|f| !s.rendered.contains(f))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    })
    .unwrap_or_default();
    if pending.is_empty() {
        return;
    }
    let rendered: Vec<(u32, Arc<Vec<u8>>)> = pending
        .into_iter()
        .filter_map(|format| render_bytes(format).map(|bytes| (format, bytes)))
        .collect();
    let Ok(_open) = Open::new(window) else { return };
    // SAFETY: the clipboard is open by this thread.
    let still_ours = unsafe { GetClipboardOwner() }.is_ok_and(|owner| owner == window);
    if !still_ours {
        return;
    }
    for (format, bytes) in rendered {
        if let Ok(global) = Global::new(&bytes) {
            let _ = global.set(format);
        }
    }
    with_owner(|owner| {
        if let Some(stage) = owner.stage.as_mut() {
            stage.owned = false;
        }
    });
}

/// `WM_DESTROYCLIPBOARD` from someone else's `EmptyClipboard`.
fn on_overwritten() {
    let now = Instant::now();
    let shared = with_owner(|owner| {
        if let Some(stage) = owner.stage.as_mut() {
            stage.owned = false;
        }
        owner.shared.clone()
    });
    if let Some(shared) = shared {
        shared.update(|facts| {
            if let Some(staged) = facts.staged.as_mut() {
                staged.overwritten_at.get_or_insert(now);
            }
        });
    }
}

/// `WM_CLIPBOARDUPDATE` (posted after any change). Ignored when the current sequence number
/// is one of our own writes or was already handled.
fn on_update() {
    let sequence = sequence();
    let now = Instant::now();
    let foreign =
        with_owner(|owner| owner.observe(sequence).then(|| owner.shared.clone())).flatten();
    if let Some(shared) = foreign {
        let change = ClipboardChange {
            sequence,
            time: crate::hook::now(),
            captured_at: SystemTime::now(),
        };
        shared.update(|facts| {
            facts.last_change = Some(now);
            facts.screenshot_change = Some(change);
            if let Some(staged) = facts.staged.as_mut() {
                staged.overwritten_at.get_or_insert(now);
            }
        });
    }
}

/// Like write_restore, verify the sequence under the clipboard lock before touching data.
/// The deadline is also checked after Open's bounded retry.
fn open_for_screenshot(
    window: HWND,
    change: ClipboardChange,
    deadline: Instant,
) -> Result<(Open, Option<HWND>), ClipError> {
    if Instant::now() >= deadline {
        return Err(ClipError::Stale);
    }
    if sequence() != change.sequence {
        return Err(ClipError::Changed);
    }
    let open = Open::new(window)?;
    if Instant::now() >= deadline {
        return Err(ClipError::Stale);
    }
    if sequence() != change.sequence {
        return Err(ClipError::Changed);
    }
    // SAFETY: no arguments. The lock keeps the current data and owner together.
    let owner = unsafe { GetClipboardOwner() }.ok();
    if owner == Some(window) {
        return Err(ClipError::OwnData);
    }
    Ok((open, owner))
}

fn inspect_image(
    window: HWND,
    change: ClipboardChange,
    deadline: Instant,
) -> Result<ImageMetadata, ClipError> {
    let (open, owner) = open_for_screenshot(window, change, deadline)?;
    let formats = enumerate_formats()?;
    let mut owner_pid = 0;
    if let Some(owner) = owner {
        // SAFETY: PID is written to a local. A vanished window gives no PID.
        unsafe { GetWindowThreadProcessId(owner, Some(&mut owner_pid)) };
    }
    drop(open);
    if sequence() != change.sequence {
        return Err(ClipError::Changed);
    }
    Ok(ImageMetadata { formats, owner_pid })
}

fn read_image(
    window: HWND,
    change: ClipboardChange,
    format: ImageFormat,
    deadline: Instant,
) -> Result<Vec<u8>, ClipError> {
    let id = match format {
        ImageFormat::Png => ids().png,
        ImageFormat::DibV5 => DIBV5,
        ImageFormat::Dib => DIB,
    };
    let (open, _) = open_for_screenshot(window, change, deadline)?;
    let data = read_format(id, SNAPSHOT_CAP);
    drop(open);
    // Delayed rendering may advance the sequence, so the pre-read check is authoritative.
    match data {
        Read::Bytes(bytes) if Instant::now() < deadline => Ok(bytes),
        Read::Bytes(_) => Err(ClipError::Stale),
        Read::TooBig => Err(ClipError::Payload(
            "clipboard image exceeds byte cap".into(),
        )),
        Read::Failed => Err(ClipError::Payload("could not read clipboard image".into())),
    }
}

fn take_snapshot(window: HWND) -> Result<Snapshot, ClipError> {
    let started = Instant::now();
    let open = Open::new(window)?;
    // SAFETY: the clipboard is open by this thread.
    let owner_is_us = unsafe { GetClipboardOwner() }.is_ok_and(|owner| owner == window);
    let staged = with_owner(|o| o.stage.as_ref().is_some_and(|s| s.owned)).unwrap_or(false);
    if owner_is_us && staged {
        // Reading our own promise would render it and count a read.
        return Err(ClipError::OwnData);
    }
    let present = enumerate_formats()?;
    let built = build_snapshot(&present, SNAPSHOT_CAP, read_format, || {
        started.elapsed() > SNAPSHOT_TIME
    });
    drop(open);
    Ok(Snapshot(Arc::new(SnapshotData {
        formats: built.formats,
        skipped: built.skipped,
        lost: built.lost,
        total: built.total,
        // Rule 4: after `CloseClipboard`.
        sequence: sequence(),
        taken_at: started,
    })))
}

/// `stage` (with a payload) or `restage` (without).
fn write_stage(window: HWND, ticket: Ticket, payload: Option<Payload>) -> Result<u32, ClipError> {
    let ids = ids();
    let restage = payload.is_none();
    let payload = match payload {
        Some(payload) => payload,
        None => with_owner(|o| o.stage.as_ref().map(|s| s.payload.clone()))
            .flatten()
            .ok_or(ClipError::NothingStaged)?,
    };
    let (promised, immediate) = staged_formats(&payload, ids)?;
    // Allocate before opening, so the clipboard stays open only for the writes.
    let immediate = immediate
        .into_iter()
        .map(|(format, bytes)| Ok((format, Global::new(&bytes)?)))
        .collect::<Result<Vec<_>, ClipError>>()?;
    let open = Open::new(window)?;
    let mut result = empty();
    let emptied = result.is_ok();
    if result.is_ok() {
        with_owner(|owner| {
            let cache = match owner.stage.take() {
                Some(old) if restage => old.cache,
                _ => Vec::new(),
            };
            owner.stage = Some(Stage {
                payload,
                promised: promised.clone(),
                rendered: Vec::new(),
                cache,
                owned: true,
            });
        });
        result = promised.iter().try_for_each(|&format| set_delayed(format));
    }
    if result.is_ok() {
        // On an error the remaining blocks drop here and are freed.
        result = immediate
            .into_iter()
            .try_for_each(|(format, global)| global.set(format));
    }
    if result.is_err() {
        // Leave no half-written promise behind; the caller restores its snapshot.
        with_owner(|owner| owner.stage = None);
        if emptied {
            let _ = empty();
        }
    }
    drop(open);
    // Rule 4: after `CloseClipboard`.
    let sequence = sequence();
    let shared = with_owner(|owner| {
        owner.remember_own(sequence);
        owner.shared.clone()
    })
    .ok_or(ClipError::Stopped)?;
    if let Err(err) = result {
        shared.update(|facts| facts.staged = None);
        return Err(after_empty(err, emptied, sequence));
    }
    let now = Instant::now();
    shared.update(|facts| match facts.staged.as_mut() {
        Some(staged) if restage => {
            staged.sequence = sequence;
            staged.rewrites += 1;
            staged.overwritten_at = None;
        }
        _ => {
            facts.staged = Some(Staged {
                ticket,
                at: now,
                sequence,
                rewrites: 0,
                reads: 0,
                first_read_at: None,
                last_read_at: None,
                last_reader: None,
                overwritten_at: None,
            })
        }
    });
    Ok(sequence)
}

fn write_restore(window: HWND, snapshot: &Snapshot, guard: RestoreGuard) -> Result<u32, ClipError> {
    let mut blocks = Vec::new();
    for saved in &snapshot.0.formats {
        blocks.push((saved.format, Global::new(&saved.bytes)?));
    }
    for (format, bytes) in markers(ids()) {
        blocks.push((format, Global::new(&bytes)?));
    }
    let open = Open::new(window)?;
    // Comparing while open is exact: nobody else can change the clipboard now.
    // SAFETY: the clipboard is open by this thread.
    let owner_is_us = unsafe { GetClipboardOwner() }.is_ok_and(|owner| owner == window);
    let staged = with_owner(|o| o.stage.as_ref().is_some_and(|s| s.owned)).unwrap_or(false);
    if !guard.allows(sequence(), owner_is_us && staged) {
        return Err(ClipError::Changed);
    }
    with_owner(|owner| owner.stage = None);
    empty()?;
    let mut first_error = None;
    for (format, global) in blocks {
        if let Err(err) = global.set(format) {
            first_error.get_or_insert(err);
        }
    }
    drop(open);
    let sequence = sequence();
    if let Some(shared) = with_owner(|owner| {
        owner.remember_own(sequence);
        owner.shared.clone()
    }) {
        shared.update(|facts| facts.staged = None);
    }
    first_error.map_or(Ok(sequence), Err)
}

// MARK: Win32 helpers

fn win32(call: &'static str, error: &windows::core::Error) -> ClipError {
    ClipError::Win32 {
        call,
        code: error.code().0,
    }
}

/// An open clipboard; `Drop` closes it, so every return path closes it.
struct Open;

impl Open {
    /// Rule 4: up to [`OPEN_TRIES`] tries, [`OPEN_RETRY_WAIT`] apart. Between tries, sent
    /// messages are handled: the holder may be a reader waiting for our `WM_RENDERFORMAT`.
    fn new(window: HWND) -> Result<Open, ClipError> {
        for attempt in 0..OPEN_TRIES {
            // SAFETY: our own window, on its thread.
            if unsafe { OpenClipboard(Some(window)) }.is_ok() {
                return Ok(Open);
            }
            if attempt + 1 < OPEN_TRIES {
                wait_handling_sent_messages(OPEN_RETRY_WAIT);
            }
        }
        Err(ClipError::Locked {
            holder: open_clipboard_process(),
        })
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard in `Open::new`.
        let _ = unsafe { CloseClipboard() };
    }
}

fn wait_handling_sent_messages(duration: Duration) {
    let deadline = Instant::now() + duration;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        // SAFETY: no handles; wakes early when a message is sent to this thread.
        unsafe {
            MsgWaitForMultipleObjectsEx(
                None,
                left.as_millis() as u32 + 1,
                QS_SENDMESSAGE,
                MWMO_NONE,
            );
        }
        let mut msg = MSG::default();
        // SAFETY: `msg` is a local. `PM_QS_SENDMESSAGE` dispatches only sent messages and
        // removes nothing from the posted queue.
        let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE) };
    }
}

/// The process of the window that has the clipboard open, if it opened it with a window.
fn open_clipboard_process() -> Option<u32> {
    // SAFETY: no arguments.
    let window = unsafe { GetOpenClipboardWindow() }.ok()?;
    let mut pid = 0u32;
    // SAFETY: `pid` is a local.
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

/// Our own `EmptyClipboard`. The clipboard must be open by this thread.
fn empty() -> Result<(), ClipError> {
    EMPTYING.set(true);
    // SAFETY: the clipboard is open by this thread. It sends `WM_DESTROYCLIPBOARD` to the
    // previous owner; if that is us, the flag marks it as our own.
    let result = unsafe { EmptyClipboard() };
    EMPTYING.set(false);
    result.map_err(|e| win32("EmptyClipboard", &e))
}

/// Promises a format. The clipboard must be open by this thread and emptied by it.
fn set_delayed(format: u32) -> Result<(), ClipError> {
    // SAFETY: plain value arguments.
    unsafe { SetLastError(WIN32_ERROR(0)) };
    // SAFETY: a null handle asks for delayed rendering; no memory changes hands.
    match unsafe { SetClipboardData(format, None) } {
        Ok(_) => Ok(()),
        // Success returns a null handle, which the wrapper reports as an error with no code.
        Err(err) if err.code() == HRESULT(0) => Ok(()),
        Err(err) => Err(win32("SetClipboardData", &err)),
    }
}

/// A movable global memory block. Freed on drop unless `set` gave it to the system.
struct Global(HGLOBAL);

impl Global {
    fn new(bytes: &[u8]) -> Result<Global, ClipError> {
        // A zero-byte block would be a discarded handle; allocate at least one byte.
        // SAFETY: plain value arguments; the block is freed in `Drop`.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }
            .map_err(|e| win32("GlobalAlloc", &e))?;
        let block = Global(handle);
        // SAFETY: a valid movable block of at least `bytes.len()` bytes.
        let data = unsafe { GlobalLock(block.0) };
        if data.is_null() {
            return Err(win32("GlobalLock", &windows::core::Error::from_thread()));
        }
        // SAFETY: `data` points to at least `bytes.len()` writable bytes, locked above.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(block.0);
        }
        Ok(block)
    }

    /// `SetClipboardData` with this block. The clipboard must be open by this thread, or this
    /// is a `WM_RENDERFORMAT` reply.
    fn set(self, format: u32) -> Result<(), ClipError> {
        // SAFETY: an unlocked movable block we own. On success the system owns it and frees
        // it when the clipboard is emptied, so we must not free it: `forget` skips `Drop`. On
        // failure we still own it and `Drop` frees it.
        match unsafe { SetClipboardData(format, Some(HANDLE(self.0.0))) } {
            Ok(_) => {
                std::mem::forget(self);
                Ok(())
            }
            Err(err) => Err(win32("SetClipboardData", &err)),
        }
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        // SAFETY: a block we allocated and still own (see `set`).
        let _ = unsafe { GlobalFree(Some(self.0)) };
    }
}

fn registered_name(id: u32) -> Option<String> {
    let mut buffer = [0u16; 256];
    // SAFETY: writes into the local buffer.
    let len = unsafe { GetClipboardFormatNameW(id, &mut buffer) };
    (len > 0).then(|| String::from_utf16_lossy(&buffer[..len as usize]))
}

/// The formats on the clipboard in clipboard order, with registered names.
/// The clipboard must be open by this thread.
fn enumerate_formats() -> Result<Vec<(u32, Option<String>)>, ClipError> {
    let mut formats = Vec::new();
    let mut previous = 0;
    loop {
        // SAFETY: plain value arguments; the clipboard is open by this thread.
        let id = unsafe {
            SetLastError(WIN32_ERROR(0));
            EnumClipboardFormats(previous)
        };
        if id == 0 {
            // SAFETY: no arguments.
            let error = unsafe { GetLastError() };
            if error.0 != 0 {
                return Err(ClipError::Win32 {
                    call: "EnumClipboardFormats",
                    code: error.to_hresult().0,
                });
            }
            return Ok(formats);
        }
        let name = if id >= 0xC000 {
            registered_name(id)
        } else {
            None
        };
        formats.push((id, name));
        previous = id;
    }
}

enum Read {
    Bytes(Vec<u8>),
    /// Larger than the room left under the cap.
    TooBig,
    Failed,
}

/// Copies one listed format. The clipboard must be open by this thread. Called only for
/// allow-listed formats, which are `HGLOBAL`s.
fn read_format(format: u32, room: usize) -> Read {
    // SAFETY: the clipboard is open by this thread. This can wait for the owner to render.
    let Ok(handle) = (unsafe { GetClipboardData(format) }) else {
        return Read::Failed;
    };
    let global = HGLOBAL(handle.0);
    // SAFETY: an allow-listed format is an HGLOBAL owned by the clipboard; we only read it.
    let size = unsafe { GlobalSize(global) };
    if size == 0 {
        return Read::Failed;
    }
    if size > room {
        return Read::TooBig;
    }
    // SAFETY: as above.
    let data = unsafe { GlobalLock(global) };
    if data.is_null() {
        return Read::Failed;
    }
    // SAFETY: `data` points to `size` readable bytes while locked.
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec();
    // SAFETY: locked above.
    let _ = unsafe { GlobalUnlock(global) };
    Read::Bytes(bytes)
}

// MARK: Registered ids

struct Ids {
    png: u32,
    drop_effect: u32,
    exclude: u32,
    history: u32,
    cloud: u32,
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| {
        // SAFETY: static strings; 0 on failure, and 0 formats are skipped.
        let register = |name: PCWSTR| unsafe { RegisterClipboardFormatW(name) };
        Ids {
            png: register(w!("PNG")),
            drop_effect: register(w!("Preferred DropEffect")),
            exclude: register(w!("ExcludeClipboardContentFromMonitorProcessing")),
            history: register(w!("CanIncludeInClipboardHistory")),
            cloud: register(w!("CanUploadToCloudClipboard")),
        }
    })
}

/// Rule 5 markers, written with staged and restored data.
fn markers(ids: &Ids) -> Vec<(u32, Vec<u8>)> {
    [ids.exclude, ids.history, ids.cloud]
        .into_iter()
        .filter(|&id| id != 0)
        .map(|id| (id, 0u32.to_le_bytes().to_vec()))
        .collect()
}

/// (promised formats in order of preference, formats written at once with their bytes).
type StagedFormats = (Vec<u32>, Vec<(u32, Vec<u8>)>);

fn staged_formats(payload: &Payload, ids: &Ids) -> Result<StagedFormats, ClipError> {
    let mut immediate = markers(ids);
    let promised = match payload {
        Payload::Image(image) => {
            let expected = image.width as usize * image.height as usize * 4;
            if image.width == 0 || image.height == 0 || image.rgba.len() != expected {
                return Err(ClipError::Payload(
                    "image buffer does not match its size".into(),
                ));
            }
            // PNG first: lossless with alpha; apps that read the first format they know
            // get the best one.
            vec![ids.png, DIBV5]
        }
        Payload::Text(text) => {
            if text.is_empty() {
                return Err(ClipError::Payload("empty text".into()));
            }
            vec![UNICODETEXT]
        }
        Payload::File(path) => {
            if !path.is_absolute() {
                return Err(ClipError::Payload(format!(
                    "not absolute: {}",
                    path.display()
                )));
            }
            immediate.push((ids.drop_effect, DROPEFFECT_COPY.to_le_bytes().to_vec()));
            vec![HDROP]
        }
    };
    Ok((
        promised.into_iter().filter(|&f| f != 0).collect(),
        immediate,
    ))
}

const DROPEFFECT_COPY: u32 = 1;

fn encode(payload: &Payload, format: u32, ids: &Ids) -> Option<Vec<u8>> {
    match payload {
        Payload::Image(image) if format == ids.png => encode_png(image).ok(),
        Payload::Image(image) if format == DIBV5 => Some(dibv5(image)),
        Payload::Text(text) if format == UNICODETEXT => Some(utf16z(text)),
        Payload::File(path) if format == HDROP => Some(dropfiles(&[path.as_path()])),
        _ => None,
    }
}

// MARK: Pure encoders

/// RGBA (straight alpha) as an 8-bit PNG. Fast compression: this runs while the target app
/// waits for the data.
fn encode_png(image: &Image) -> Result<Vec<u8>, png::EncodingError> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.rgba)?;
    writer.finish()?;
    Ok(out)
}

const BITMAPV5HEADER_SIZE: u32 = 124;
const BI_BITFIELDS: u32 = 3;
const LCS_SRGB: u32 = 0x7352_4742;
const LCS_GM_IMAGES: u32 = 4;
/// 96 DPI, so apps that size pictures by resolution show one pixel per 1/96 inch.
const PELS_PER_METER_96DPI: i32 = 3780;

/// `CF_DIBV5`: a `BITMAPV5HEADER` (32 bpp, `BI_BITFIELDS`, masks in the header, sRGB) and
/// bottom-up BGRA rows.
///
/// Straight (not premultiplied) alpha: `BITMAPV5HEADER` only defines an alpha mask and has no
/// premultiplied flag, and the premultiplied rule belongs to `AlphaBlend` on DIB sections, not
/// to clipboard DIBs. The writers and readers apps meet on the clipboard (Qt's
/// `QWindowsMimeImage`, Firefox, LibreOffice) treat `CF_DIBV5` alpha as straight. Straight also
/// keeps the colour of semi-transparent pixels exact, which the self-test checks byte for byte.
/// Bottom-up because some readers mishandle a negative (top-down) height.
fn dibv5(image: &Image) -> Vec<u8> {
    let row = image.width as usize * 4;
    let pixels = row * image.height as usize;
    let mut out = Vec::with_capacity(BITMAPV5HEADER_SIZE as usize + pixels);
    out.extend(dibv5_header(image.width, image.height));
    for line in image.rgba.chunks_exact(row).rev() {
        for px in line.as_chunks::<4>().0 {
            out.extend([px[2], px[1], px[0], px[3]]);
        }
    }
    out
}

fn dibv5_header(width: u32, height: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(BITMAPV5HEADER_SIZE as usize);
    h.extend(BITMAPV5HEADER_SIZE.to_le_bytes()); // bV5Size
    h.extend((width as i32).to_le_bytes()); // bV5Width
    h.extend((height as i32).to_le_bytes()); // bV5Height, positive: bottom-up
    h.extend(1u16.to_le_bytes()); // bV5Planes
    h.extend(32u16.to_le_bytes()); // bV5BitCount
    h.extend(BI_BITFIELDS.to_le_bytes()); // bV5Compression
    h.extend((width * height * 4).to_le_bytes()); // bV5SizeImage
    h.extend(PELS_PER_METER_96DPI.to_le_bytes()); // bV5XPelsPerMeter
    h.extend(PELS_PER_METER_96DPI.to_le_bytes()); // bV5YPelsPerMeter
    h.extend(0u32.to_le_bytes()); // bV5ClrUsed
    h.extend(0u32.to_le_bytes()); // bV5ClrImportant
    h.extend(0x00FF_0000u32.to_le_bytes()); // bV5RedMask
    h.extend(0x0000_FF00u32.to_le_bytes()); // bV5GreenMask
    h.extend(0x0000_00FFu32.to_le_bytes()); // bV5BlueMask
    h.extend(0xFF00_0000u32.to_le_bytes()); // bV5AlphaMask
    h.extend(LCS_SRGB.to_le_bytes()); // bV5CSType
    h.extend([0u8; 36]); // bV5Endpoints (unused for sRGB)
    h.extend([0u8; 12]); // bV5GammaRed, Green, Blue
    h.extend(LCS_GM_IMAGES.to_le_bytes()); // bV5Intent
    h.extend(0u32.to_le_bytes()); // bV5ProfileData
    h.extend(0u32.to_le_bytes()); // bV5ProfileSize
    h.extend(0u32.to_le_bytes()); // bV5Reserved
    h
}

/// UTF-16LE with a terminating NUL, for `CF_UNICODETEXT`.
fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// `CF_HDROP`: a `DROPFILES` header (pFiles = 20, pt = 0,0, fNC = 0, fWide = 1), then each
/// path as UTF-16 with a NUL, then one more NUL.
fn dropfiles(paths: &[&Path]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(20u32.to_le_bytes());
    out.extend(0i32.to_le_bytes());
    out.extend(0i32.to_le_bytes());
    out.extend(0i32.to_le_bytes());
    out.extend(1i32.to_le_bytes());
    for path in paths {
        for unit in path.as_os_str().encode_wide().chain([0]) {
            out.extend(unit.to_le_bytes());
        }
    }
    out.extend(0u16.to_le_bytes());
    out
}

// MARK: Pure snapshot rules

/// What to do with one format found on the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    Read {
        priority: u8,
    },
    Skip(SkipReason),
    /// Says how to treat the rest; not content.
    Marker,
}

/// The allow-list decision for one format, without looking at the others.
fn classify(id: u32, name: Option<&str>) -> Plan {
    if let Some(&(_, priority, _)) = STANDARD_KEEP.iter().find(|entry| entry.0 == id) {
        return Plan::Read { priority };
    }
    match id {
        2 | 3 | 9 | 14 | 0x82 | 0x83 | 0x8E | 0x300..=0x3FF => Plan::Skip(SkipReason::Handle),
        0x80 | 0x81 | 0x200..=0x2FF => Plan::Skip(SkipReason::Private),
        0xC000..=0xFFFF => {
            let Some(name) = name else {
                return Plan::Skip(SkipReason::NotListed);
            };
            if MARKERS.iter().any(|m| m.eq_ignore_ascii_case(name)) {
                return Plan::Marker;
            }
            match NAMED_KEEP
                .iter()
                .find(|entry| entry.0.eq_ignore_ascii_case(name))
            {
                Some(&(_, priority, _)) => Plan::Read { priority },
                None => Plan::Skip(SkipReason::NotListed),
            }
        }
        _ => Plan::Skip(SkipReason::NotListed),
    }
}

/// The plan for every present format, keeping one of each synthesized family.
fn plan_formats(present: &[(u32, Option<String>)]) -> Vec<Plan> {
    let has = |id: u32| present.iter().any(|(f, _)| *f == id);
    let text = [UNICODETEXT, TEXT, OEMTEXT].into_iter().find(|&f| has(f));
    let dib = [DIBV5, DIB].into_iter().find(|&f| has(f));
    present
        .iter()
        .map(|(id, name)| match *id {
            UNICODETEXT | TEXT | OEMTEXT if Some(*id) != text => {
                Plan::Skip(SkipReason::Synthesized)
            }
            DIBV5 | DIB if Some(*id) != dib => Plan::Skip(SkipReason::Synthesized),
            BITMAP | PALETTE if dib.is_some() => Plan::Skip(SkipReason::Synthesized),
            _ => classify(*id, name.as_deref()),
        })
        .collect()
}

/// The kind of content a format carries, saved or not.
fn kind_of(id: u32, name: Option<&str>) -> Option<Kind> {
    if let Some(&(_, _, kind)) = STANDARD_KEEP.iter().find(|entry| entry.0 == id) {
        return kind;
    }
    if id == BITMAP {
        return Some(Kind::Image);
    }
    let name = name?;
    NAMED_KEEP
        .iter()
        .find(|entry| entry.0.eq_ignore_ascii_case(name))
        .and_then(|entry| entry.2)
        .or_else(|| {
            NAMED_KIND_ONLY
                .iter()
                .find(|entry| entry.0.eq_ignore_ascii_case(name))
                .map(|entry| entry.1)
        })
}

struct Built {
    formats: Vec<Saved>,
    skipped: Vec<Skipped>,
    lost: Option<Lost>,
    total: usize,
}

/// Reads the listed formats in priority order under `cap` bytes until `out_of_time`, and
/// decides what a restore would lose. `read(id, room)` copies one format.
fn build_snapshot(
    present: &[(u32, Option<String>)],
    cap: usize,
    mut read: impl FnMut(u32, usize) -> Read,
    mut out_of_time: impl FnMut() -> bool,
) -> Built {
    let plans = plan_formats(present);
    let name_of = |i: usize| {
        let (id, name) = &present[i];
        name.clone().unwrap_or_else(|| format_name(*id))
    };
    let mut order: Vec<(u8, usize)> = plans
        .iter()
        .enumerate()
        .filter_map(|(i, plan)| match plan {
            Plan::Read { priority } => Some((*priority, i)),
            _ => None,
        })
        .collect();
    order.sort();

    let mut kept: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut skipped: Vec<(usize, SkipReason)> = Vec::new();
    let mut total = 0usize;
    let mut over_cap = false;
    let mut timed_out = false;
    for (_, i) in order {
        if timed_out || out_of_time() {
            timed_out = true;
            skipped.push((i, SkipReason::OutOfTime));
            continue;
        }
        match read(present[i].0, cap - total) {
            Read::Bytes(bytes) if bytes.len() <= cap - total => {
                total += bytes.len();
                kept.push((i, bytes));
            }
            Read::Bytes(_) | Read::TooBig => {
                over_cap = true;
                skipped.push((i, SkipReason::OverCap));
            }
            Read::Failed => skipped.push((i, SkipReason::Unreadable)),
        }
    }
    for (i, plan) in plans.iter().enumerate() {
        if let Plan::Skip(reason) = plan {
            skipped.push((i, *reason));
        }
    }
    kept.sort_by_key(|(i, _)| *i);
    skipped.sort_by_key(|(i, _)| *i);

    let kind = |i: usize| kind_of(present[i].0, present[i].1.as_deref());
    let content = plans.iter().any(|plan| *plan != Plan::Marker);
    let lost = if over_cap {
        Some(Lost::OverCap)
    } else if timed_out {
        Some(Lost::OutOfTime)
    } else if content && kept.is_empty() {
        Some(Lost::Everything)
    } else {
        [
            Kind::Text,
            Kind::RichText,
            Kind::Image,
            Kind::Files,
            Kind::Link,
        ]
        .into_iter()
        .find(|&k| {
            (0..present.len()).any(|i| kind(i) == Some(k))
                && !kept.iter().any(|(i, _)| kind(*i) == Some(k))
        })
        .map(Lost::Kind)
    };
    Built {
        formats: kept
            .into_iter()
            .map(|(i, bytes)| Saved {
                format: present[i].0,
                name: name_of(i),
                bytes,
            })
            .collect(),
        skipped: skipped
            .into_iter()
            .map(|(i, reason)| Skipped {
                format: present[i].0,
                name: name_of(i),
                reason,
            })
            .collect(),
        lost,
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP as Hdrop};

    #[test]
    fn restore_guards() {
        assert!(RestoreGuard::Sequence(None).allows(7, false));
        assert!(RestoreGuard::Sequence(Some(7)).allows(7, false));
        assert!(!RestoreGuard::Sequence(Some(7)).allows(8, true));
        assert!(RestoreGuard::OwnStaging.allows(8, true));
        assert!(!RestoreGuard::OwnStaging.allows(7, false));
    }

    #[test]
    fn stage_errors_say_whether_the_clipboard_was_emptied() {
        let locked = ClipError::Locked { holder: Some(4) };
        assert_eq!(after_empty(locked.clone(), false, 9), locked);
        let emptied = after_empty(locked.clone(), true, 9);
        assert_eq!(
            emptied,
            ClipError::Emptied {
                cause: Box::new(locked),
                sequence: 9
            }
        );
        assert!(emptied.to_string().contains("left empty"));
    }

    fn named(id: u32, name: &str) -> (u32, Option<String>) {
        (id, Some(name.into()))
    }

    fn plain(id: u32) -> (u32, Option<String>) {
        (id, None)
    }

    #[test]
    fn screenshot_reader_only_observes_new_foreign_sequences() {
        let mut owner = Owner {
            shared: Arc::new(Shared {
                facts: Mutex::new(Facts::default()),
                wake: Box::new(|| {}),
            }),
            stage: None,
            own: VecDeque::new(),
            last_seen: 1,
        };
        assert!(!owner.observe(1));
        assert!(owner.observe(2));
        assert!(!owner.observe(2));
        for own in [3, 4, 5] {
            // stage, delayed render, restore
            owner.remember_own(own);
            assert!(!owner.observe(own));
        }
        assert!(owner.observe(6));
    }

    #[test]
    fn only_selects_supported_hglobal_image_formats() {
        assert!(
            ImageFormat::preferred(&[plain(TEXT), plain(TIFF), named(0xc001, "GIF")]).is_none()
        );
        assert!(matches!(
            ImageFormat::preferred(&[plain(BITMAP)]),
            Some(ImageFormat::Dib)
        ));
        assert!(matches!(
            ImageFormat::preferred(&[plain(DIB), plain(DIBV5)]),
            Some(ImageFormat::DibV5)
        ));
        assert!(matches!(
            ImageFormat::preferred(&[plain(DIBV5), named(0xc001, "png")]),
            Some(ImageFormat::Png)
        ));
    }

    fn test_image() -> Image {
        // 3 x 2, with transparent, semi-transparent and opaque pixels.
        let rgba = vec![
            255, 0, 0, 255, /**/ 0, 255, 0, 128, /**/ 0, 0, 255, 0, //
            10, 20, 30, 40, /**/ 200, 100, 50, 255, /**/ 1, 2, 3, 4,
        ];
        Image::new(3, 2, rgba).unwrap()
    }

    #[test]
    fn dibv5_header_fields() {
        let h = dibv5_header(3, 2);
        assert_eq!(h.len(), 124);
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let i32_at = |o: usize| i32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(h[o..o + 2].try_into().unwrap());
        assert_eq!(u32_at(0), 124);
        assert_eq!(i32_at(4), 3);
        assert_eq!(i32_at(8), 2, "positive height: bottom-up");
        assert_eq!(u16_at(12), 1);
        assert_eq!(u16_at(14), 32);
        assert_eq!(u32_at(16), BI_BITFIELDS);
        assert_eq!(u32_at(20), 24);
        assert_eq!(u32_at(40), 0x00FF_0000);
        assert_eq!(u32_at(44), 0x0000_FF00);
        assert_eq!(u32_at(48), 0x0000_00FF);
        assert_eq!(u32_at(52), 0xFF00_0000);
        assert_eq!(u32_at(56), LCS_SRGB);
        assert_eq!(u32_at(108), LCS_GM_IMAGES);
        assert!(h[112..124].iter().all(|&b| b == 0));
    }

    #[test]
    fn dibv5_pixels_are_straight_bgra_bottom_up() {
        let dib = dibv5(&test_image());
        assert_eq!(dib.len(), 124 + 3 * 2 * 4);
        let pixels = &dib[124..];
        // First stored row is the bottom image row.
        assert_eq!(&pixels[0..4], &[30, 20, 10, 40], "colour not premultiplied");
        assert_eq!(&pixels[4..8], &[50, 100, 200, 255]);
        assert_eq!(&pixels[8..12], &[3, 2, 1, 4]);
        assert_eq!(&pixels[12..16], &[0, 0, 255, 255]);
        assert_eq!(&pixels[16..20], &[0, 255, 0, 128]);
        assert_eq!(
            &pixels[20..24],
            &[255, 0, 0, 0],
            "transparent pixel keeps its colour"
        );
    }

    #[test]
    fn png_round_trip() {
        let image = test_image();
        let bytes = encode_png(&image).unwrap();
        let decoder = png::Decoder::new(bytes.as_slice());
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&buf[..info.buffer_size()], image.rgba.as_slice());
    }

    #[test]
    fn allow_list_per_format() {
        assert_eq!(classify(UNICODETEXT, None), Plan::Read { priority: 0 });
        assert_eq!(classify(LOCALE, None), Plan::Read { priority: 0 });
        assert_eq!(classify(HDROP, None), Plan::Read { priority: 1 });
        assert_eq!(classify(DIBV5, None), Plan::Read { priority: 4 });
        for handle in [BITMAP, PALETTE, 3, 14, 0x82, 0x300, 0x3FF] {
            assert_eq!(
                classify(handle, None),
                Plan::Skip(SkipReason::Handle),
                "{handle:#x}"
            );
        }
        for private in [0x80, 0x200, 0x2FF] {
            assert_eq!(
                classify(private, None),
                Plan::Skip(SkipReason::Private),
                "{private:#x}"
            );
        }
        assert_eq!(
            classify(11, None),
            Plan::Skip(SkipReason::NotListed),
            "CF_RIFF"
        );
        assert_eq!(classify(0xC100, Some("png")), Plan::Read { priority: 4 });
        assert_eq!(
            classify(0xC101, Some("HTML Format")),
            Plan::Read { priority: 2 }
        );
        assert_eq!(
            classify(0xC102, Some("Shell IDList Array")),
            Plan::Read { priority: 1 }
        );
        for ole in [
            "DataObject",
            "Ole Private Data",
            "Object Descriptor",
            "Embed Source",
        ] {
            assert_eq!(
                classify(0xC103, Some(ole)),
                Plan::Skip(SkipReason::NotListed),
                "{ole}"
            );
        }
        assert_eq!(classify(0xC104, None), Plan::Skip(SkipReason::NotListed));
        assert_eq!(
            classify(0xC105, Some("ExcludeClipboardContentFromMonitorProcessing")),
            Plan::Marker
        );
    }

    #[test]
    fn synthesized_families_keep_one() {
        let present = [
            plain(UNICODETEXT),
            plain(LOCALE),
            plain(TEXT),
            plain(OEMTEXT),
        ];
        assert_eq!(
            plan_formats(&present),
            [
                Plan::Read { priority: 0 },
                Plan::Read { priority: 0 },
                Plan::Skip(SkipReason::Synthesized),
                Plan::Skip(SkipReason::Synthesized),
            ]
        );
        let present = [plain(BITMAP), plain(DIB), plain(PALETTE), plain(DIBV5)];
        assert_eq!(
            plan_formats(&present),
            [
                Plan::Skip(SkipReason::Synthesized),
                Plan::Skip(SkipReason::Synthesized),
                Plan::Skip(SkipReason::Synthesized),
                Plan::Read { priority: 4 },
            ]
        );
        // A lone CF_BITMAP has no DIB to come back from: it is a skipped handle.
        assert_eq!(
            plan_formats(&[plain(BITMAP)]),
            [Plan::Skip(SkipReason::Handle)]
        );
        // Only ANSI text: keep it, Windows re-creates Unicode.
        assert_eq!(plan_formats(&[plain(TEXT)]), [Plan::Read { priority: 0 }]);
    }

    fn fake_read(sizes: &[(u32, usize)]) -> impl FnMut(u32, usize) -> Read + '_ {
        |id, room| match sizes.iter().find(|(f, _)| *f == id) {
            Some(&(_, size)) if size > room => Read::TooBig,
            Some(&(_, size)) => Read::Bytes(vec![id as u8; size]),
            None => Read::Failed,
        }
    }

    #[test]
    fn office_copy_restores_without_private_formats() {
        let present = [
            named(0xC001, "Object Descriptor"),
            named(0xC002, "Rich Text Format"),
            named(0xC003, "HTML Format"),
            plain(UNICODETEXT),
            plain(14),
            plain(3),
            named(0xC004, "Ole Private Data"),
            plain(LOCALE),
            plain(OEMTEXT),
            plain(TEXT),
        ];
        let sizes = [(0xC002, 10), (0xC003, 20), (UNICODETEXT, 8), (LOCALE, 4)];
        let built = build_snapshot(&present, 1000, fake_read(&sizes), || false);
        let kept: Vec<u32> = built.formats.iter().map(|s| s.format).collect();
        assert_eq!(
            kept,
            [0xC002, 0xC003, UNICODETEXT, LOCALE],
            "clipboard order"
        );
        assert_eq!(built.total, 42);
        assert_eq!(built.lost, None);
        assert_eq!(built.skipped.len(), 6);
    }

    #[test]
    fn size_cap_makes_the_snapshot_partial() {
        let present = [plain(UNICODETEXT), named(0xC010, "PNG"), plain(DIBV5)];
        let sizes = [(UNICODETEXT, 10), (0xC010, 60), (DIBV5, 60)];
        let built = build_snapshot(&present, 100, fake_read(&sizes), || false);
        // Text first, then PNG fits (70), DIBV5 does not (130).
        assert_eq!(built.total, 70);
        assert_eq!(built.formats.len(), 2);
        assert_eq!(built.skipped[0].reason, SkipReason::OverCap);
        assert_eq!(built.lost, Some(Lost::OverCap));
        // Exactly at the cap is fine.
        let built = build_snapshot(&present, 130, fake_read(&sizes), || false);
        assert_eq!((built.total, built.lost), (130, None));
    }

    #[test]
    fn time_budget_makes_the_snapshot_partial() {
        let present = [plain(UNICODETEXT), named(0xC010, "PNG")];
        let sizes = [(UNICODETEXT, 10), (0xC010, 60)];
        let mut calls = 0;
        let built = build_snapshot(&present, 100, fake_read(&sizes), || {
            calls += 1;
            calls > 1
        });
        assert_eq!(built.formats.len(), 1);
        assert_eq!(built.skipped[0].reason, SkipReason::OutOfTime);
        assert_eq!(built.lost, Some(Lost::OutOfTime));
    }

    #[test]
    fn losses_the_user_would_notice() {
        // Only a metafile: content, nothing saved.
        let built = build_snapshot(&[plain(14), plain(3)], 100, fake_read(&[]), || false);
        assert_eq!(built.lost, Some(Lost::Everything));
        // Only app-private data.
        let built = build_snapshot(
            &[named(0xC020, "Art::GVML ClipFormat")],
            100,
            fake_read(&[]),
            || false,
        );
        assert_eq!(built.lost, Some(Lost::Everything));
        // Virtual files (an Outlook attachment): the text survives, the files do not.
        let present = [
            named(0xC021, "FileGroupDescriptorW"),
            named(0xC022, "FileContents"),
            plain(UNICODETEXT),
        ];
        let built = build_snapshot(&present, 100, fake_read(&[(UNICODETEXT, 4)]), || false);
        assert_eq!(built.lost, Some(Lost::Kind(Kind::Files)));
        // HTML that fails to render.
        let present = [named(0xC003, "HTML Format"), plain(UNICODETEXT)];
        let built = build_snapshot(&present, 100, fake_read(&[(UNICODETEXT, 4)]), || false);
        assert_eq!(built.lost, Some(Lost::Kind(Kind::RichText)));
        // An empty clipboard, or only markers, loses nothing.
        assert_eq!(
            build_snapshot(&[], 100, fake_read(&[]), || false).lost,
            None
        );
        let built = build_snapshot(
            &[named(0xC030, "CanIncludeInClipboardHistory")],
            100,
            fake_read(&[]),
            || false,
        );
        assert_eq!(built.lost, None);
    }

    fn read_dropfiles(bytes: &[u8]) -> Vec<String> {
        let block = Global::new(bytes).unwrap();
        let hdrop = Hdrop(block.0.0);
        // SAFETY: `block` holds a DROPFILES structure; DragQueryFileW only reads it.
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
        (0..count)
            .map(|i| {
                let mut buffer = [0u16; 260];
                // SAFETY: as above; writes into the local buffer.
                let len = unsafe { DragQueryFileW(hdrop, i, Some(&mut buffer)) } as usize;
                String::from_utf16(&buffer[..len]).unwrap()
            })
            .collect()
    }

    #[test]
    fn dropfiles_layout_one_path() {
        let path = Path::new(r"C:\Bilder\Größe 1.mp4");
        let bytes = dropfiles(&[path]);
        assert_eq!(&bytes[0..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1u32.to_le_bytes(), "fWide");
        let units = path.as_os_str().encode_wide().count();
        assert_eq!(bytes.len(), 20 + (units + 2) * 2);
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
        assert_eq!(read_dropfiles(&bytes), [r"C:\Bilder\Größe 1.mp4"]);
    }

    #[test]
    fn dropfiles_layout_two_paths() {
        let a = Path::new(r"C:\tmp\日本語.png");
        let b = Path::new(r"D:\Видео\clip 😀.mp4");
        let bytes = dropfiles(&[a, b]);
        let units = a.as_os_str().encode_wide().count() + b.as_os_str().encode_wide().count();
        assert_eq!(bytes.len(), 20 + (units + 3) * 2);
        assert_eq!(
            read_dropfiles(&bytes),
            [r"C:\tmp\日本語.png", r"D:\Видео\clip 😀.mp4"]
        );
    }

    #[test]
    fn unicode_text_is_nul_terminated() {
        assert_eq!(utf16z("é"), [0xE9, 0, 0, 0]);
    }

    #[test]
    fn payload_checks() {
        let ids = Ids {
            png: 0xC100,
            drop_effect: 0xC101,
            exclude: 0xC102,
            history: 0xC103,
            cloud: 0xC104,
        };
        let (promised, immediate) =
            staged_formats(&Payload::Image(Arc::new(test_image())), &ids).unwrap();
        assert_eq!(promised, [0xC100, DIBV5]);
        let marker_ids: Vec<u32> = immediate.iter().map(|(f, _)| *f).collect();
        assert_eq!(marker_ids, [0xC102, 0xC103, 0xC104]);
        assert!(immediate.iter().all(|(_, b)| b == &[0, 0, 0, 0]));
        let (promised, immediate) =
            staged_formats(&Payload::File(PathBuf::from(r"C:\a.mp4")), &ids).unwrap();
        assert_eq!(promised, [HDROP]);
        assert!(immediate.contains(&(0xC101, vec![1, 0, 0, 0])));
        assert!(staged_formats(&Payload::File(PathBuf::from("a.mp4")), &ids).is_err());
        assert!(staged_formats(&Payload::Text(String::new()), &ids).is_err());
        let bad = Image {
            width: 2,
            height: 2,
            rgba: vec![0; 4],
        };
        assert!(staged_formats(&Payload::Image(Arc::new(bad)), &ids).is_err());
    }
}
