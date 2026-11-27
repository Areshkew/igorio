//! Turns a double-click into one of the fairy's deliveries: paste the pile into a text field,
//! a terminal (as a file path), a canvas or a page-drawn document. From `DropRouter.swift`
//! (the routing), `PasteTarget.swift` (one paste: clipboard, keys, outcome, restore) and the
//! hover hint in `AppDelegate.swift`. The decisions are the core's (`drop_policy`,
//! `paste_policy`, `editable`); this module sequences them.
//!
//! # Shape
//!
//! [`Router`] is a state machine owned by the UI thread. It never blocks and never sleeps:
//! every input (a hook event, a UI Automation answer, a clipboard reply, a saved PNG) and
//! every [`Router::tick`] either finishes a step at once or leaves the router waiting for
//! the next input or deadline. [`Router::next_deadline`] says when it must run again, like
//! `FairyState::next_deadline`.
//!
//! It reaches the outside only through the [`Env`] trait: UI Automation requests, clipboard
//! requests and facts, the PNG saver, the foreground check and the keystrokes. [`LiveEnv`] is
//! the thin Windows implementation; the tests drive the router with a scripted fake.
//!
//! # One double-click, step by step
//!
//! 1. First mouse-down of a possible double-click while holding: remember the clipboard
//!    (`PasteTarget.rememberClipboard`): its sequence number and time, and a snapshot when
//!    none is kept for that sequence yet.
//! 2. Second mouse-down (`click_count == 2`): ask UI Automation what is at the point, then
//!    gather the facts and call `drop_policy::decide`.
//! 3. `Paste`: deliver now. `CheckFocusAfterMouseUp`: wait for the release and a settle
//!    delay, look again with focus, and confirm a text input or a canvas as
//!    `resolveAfterMouseUp` does. `SaveToFinder`: save into the folder, see below.
//! 4. Delivery (`deliverPaste` and `pasteRest`): the whole pile, top card first, one paste
//!    per card, 0.45 s apart. Per card (`PasteJob`): payload ready (a terminal gets a PNG
//!    saved to `Settings::pasted_dir` and its quoted path) and the user's clipboard saved,
//!    wait for the mouse-up, check the foreground, press Right / Escape, stage the clipboard,
//!    check again and press Ctrl+V, then watch the reads until `OutcomeRules` decides.
//! 5. Restore: when the delivery is over, `RestoreRules` decides when to put the user's
//!    clipboard back; overwrites are handled with `overwrite_step` meanwhile.
//!
//! # Saving into a folder (`saveToFinder`)
//!
//! A double-click on the empty background of a File Explorer folder view or of the desktop
//! ([`shell_drop_spot`]) saves the pile there. The steps of the saving phase:
//!
//! 1. Asking: [`Env::folder_lookup`] asks `explorer::Explorer` for the folder of the
//!    double-clicked top-level window (or the desktop) with [`FOLDER_DEADLINE`]. A later
//!    press or a typed key drops the save.
//! 2. Ready: the folder is known; wait for the button to come up (still down after
//!    `MOUSE_UP_TIMEOUT` is a drag: no save). A press or a key still drops the save.
//! 3. Writing: one card at a time on the saver thread ([`Env::save_to_folder`]): a PNG, or
//!    the movie file copied for a recording (not copied when it already lives in that
//!    folder). From here a press or a key no longer stops it: the file being written is
//!    finished (the saver never leaves half a file). A card shaken off before its own write
//!    is skipped; a capture that lands meanwhile is not taken (the pile is fixed at the
//!    double-click, top card first, as `deliverable` gives it).
//! 4. Done: the saved cards are delivered at once with "saved to <folder>" (Swift: one
//!    `deliver` with every saved id), and [`Env::reveal`] selects the last file written.
//!    A write error stops the rest. With nothing saved the card wobbles with a hint naming
//!    the problem; otherwise that hint follows the "saved" one after [`SAVE_FAIL_STEP`] per
//!    saved card (Swift's "couldn't save N").
//!
//! # Windows differences
//!
//! - `KeyRoute::Process` has no Windows form: `SendInput` reaches only the foreground window.
//!   So the keys are sent only while the target is in front; when it does not come to the
//!   front within `ACTIVATION_TIMEOUT` the paste fails (the Swift code activates the app; a
//!   background process cannot reliably do that on Windows, and pasting into whatever else
//!   is in front would be wrong).
//! - No acceptance probe: the uia module does not read element values, so the outcome comes
//!   from clipboard reads only (`OutcomeRules::new(false, ..)`). Windows tells us every
//!   render of a delayed format, which macOS does not for single-line fields.
//! - Terminal paths are quoted for Windows shells ([`quoted_path`]): the core's
//!   `shell_quoted` uses POSIX single quotes, which cmd.exe does not understand.
//! - Canvas apps are matched by exe name ([`windows_app_id`]), since the core's list holds
//!   macOS bundle ids.
//! - UI Automation answers late or not at all in slow apps (Qt, games). A terminal that does
//!   not answer is still a terminal (`silent_terminal`). Any other app that does not answer
//!   gets the re-check after the mouse-up with a longer deadline ([`SLOW_DEADLINE`]); if
//!   that also times out, the fairy says so and keeps the card. It never pastes into a
//!   window it knows nothing about.
//!
//! # Notes
//!
//! A note window is one of the app's own windows, and it takes the pile itself (Swift: the
//! note's `DropZone`). A double-click over a note ([`Env::note_at`]) is never looked up and
//! never pasted: the carried cards, top first, go to the note ([`Env::give_to_note`]) and stay
//! on the pile until the notes thread says which it took ([`Router::note_took`]); with no
//! answer within [`NOTE_TIMEOUT`] the fairy keeps them and says so. While the cursor rests over
//! a note the hover hint is the note's own ([`DROP_HINT`]), with no lookup.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::{Duration, Instant};

use igorio_core::drop_policy::{self, Action, DoubleClick};
use igorio_core::editable::{AccessibilityNode, editable_self_or_ancestor, editable_target};
use igorio_core::geom::{CardId, Point, Rect};
use igorio_core::paste_policy::{
    self, MouseUpStep, Outcome as PasteOutcome, OutcomeRules, OverwriteStep, RestoreRules,
    RestoreStep, TerminalSettle,
};
use igorio_kit::HeldCard;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::Accessibility::{UIA_HeaderControlTypeId, UIA_HeaderItemControlTypeId};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetSystemMetrics, SM_SWAPBUTTON};

use crate::clipboard::{self, ClipError, Clipboard, Done, Expect, Payload, Reply, Ticket};
use crate::explorer::{Explorer, FolderOutcome, FolderRequest};
use crate::hook::{self, Button, KeyWatch, MouseEvent};
use crate::input::{self, Batch, Key, Modifier, Sent};
use crate::notes::window::{DROP_HINT, DropZones};
use crate::saver;
use crate::settings::{DefaultFolders, Settings};
use crate::state::FairyState;
use crate::uia::{
    self, ElementInfo, Lookup, Outcome, Probe, Requester, Shell, Snapshot, Target, Uia, UiaNode,
};

// MARK: Constants

/// The cursor must rest this long before the hover hint looks under it (`AppDelegate`).
pub const HOVER_REST: f64 = 0.22;
/// At most one hover lookup per this long: 4 a second.
pub const HOVER_MIN_GAP: f64 = 0.25;
/// Deadline of an ordinary lookup, as in `AXNode.swift`.
pub const LOOKUP_DEADLINE: f64 = 0.25;
/// Deadline of the re-check after the mouse-up when the double-click's lookup timed out.
pub const SLOW_DEADLINE: f64 = 1.0;
/// The router stops waiting for an answer this long after the lookup's deadline (a replaced
/// request gets no answer; the uia controller answers every other one at its deadline).
pub const LOOKUP_GRACE: f64 = 1.0;
/// Settle after the release before the re-check (`checkFocusAfterMouseUp`).
pub const SETTLE_AFTER_UP: f64 = 0.1;
pub const SETTLE_AFTER_UP_CHROMIUM: f64 = 0.2;
/// Between two cards of a pile (`pasteRest`).
pub const PILE_STEP: f64 = 0.45;
/// How often the foreground is checked while waiting for the target (Swift: 0.03).
pub const ACTIVATION_POLL: f64 = 0.03;
/// How often the outcome is evaluated (Swift: 0.05) and the restore (Swift: 0.1).
pub const OUTCOME_POLL: f64 = 0.05;
pub const RESTORE_POLL: f64 = 0.1;
/// While the user holds Shift, Alt or Win, keys are retried this often, for this long.
pub const MODIFIER_POLL: f64 = 0.05;
pub const MODIFIER_WAIT: f64 = 2.0;
/// The user's clipboard and the payload must be ready within this long.
pub const PREPARE_TIMEOUT: f64 = 4.0;
/// A stage or restore reply must come within this long.
pub const REPLY_TIMEOUT: f64 = 3.0;
/// A restore that could not run (clipboard locked, queue full) is tried again this often,
/// for this long.
pub const RESTORE_RETRY: f64 = 0.5;
pub const RESTORE_GIVE_UP: f64 = 10.0;
/// A clipboard request that never answers (a stage, a rewrite, a rollback) is polled for at
/// most this long; then the router stops and logs it. The clipboard thread still owns the
/// rollback and runs it in order, or when it stops.
pub const UNANSWERED_LIMIT: f64 = 10.0;
/// At quit, how long to wait for the clipboard replies that put the user's clipboard back.
pub const QUIT_WAIT: Duration = Duration::from_secs(1);

/// After the paste keys, the acceptance probe looks at the field this often
/// (`PasteJob.pollOutcome`: 0.2 s).
pub const PROBE_INTERVAL: f64 = 0.2;

/// Roles of a field that holds one line (`AXTextField` is also a web `<textarea>`: UI
/// Automation has no multi-line flag, and neither takes images). Pastes into these are
/// verified by the acceptance probe, as `PasteTarget.swift` verifies single-line fields.
pub const SINGLE_LINE_ROLES: &[&str] = &["AXTextField", "AXSearchField", "AXComboBox"];

/// Class tokens that mark a terminal in web content (Chromium reports the HTML class
/// attribute as the UI Automation class name):
/// - `xterm…`: xterm.js, the terminal of VS Code, Cursor, Hyper, Tabby and most Electron
///   apps. Its parts are `xterm`, `xterm-screen` (the canvas renderer's parent),
///   `xterm-viewport`, `xterm-rows` (the DOM renderer) and `xterm-helper-textarea` (the
///   hidden input that has the keyboard focus).
/// - `terminal-wrapper`: VS Code's element around each xterm.js instance.
///
/// Not a terminal: Monaco (`monaco-editor`, `inputarea`), VS Code's code editor. It takes
/// text only, and a file path typed into source code is not what a drop means.
pub const WEB_TERMINAL_CLASS_PREFIXES: &[&str] = &["xterm", "terminal-wrapper"];

/// ARIA roles (Chromium reports them as `AriaRole`) of things a double-click opens or
/// activates although their control type maps to a generic group: grid and table cells and
/// headers, list options, tree items, tabs, rows, links, buttons, menu items and switches.
/// Calendar days are buttons or grid cells and are covered by those.
pub const ACTIVATING_ARIA_ROLES: &[&str] = &[
    "gridcell",
    "cell",
    "columnheader",
    "rowheader",
    "row",
    "option",
    "treeitem",
    "tab",
    "link",
    "button",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "switch",
    "checkbox",
    "radio",
];

/// Terminals that may be pasted into when UI Automation did not answer, if the point is in
/// the client area of the window (`GetClientRect`): there their client area is only text.
/// The classic console (`ConsoleWindowClass`, title bar and scroll bars are non-client) and
/// Alacritty (no tabs; native title bar). Windows Terminal (and its `OpenConsole`) and
/// WezTerm draw their tab strip inside the client area, and nothing Win32 tells it from the
/// text without asking the app, so a silent one is not pasted into.
pub fn silent_terminal_pasteable(target: &Target) -> bool {
    target.class_name == "ConsoleWindowClass"
        || target
            .exe
            .as_deref()
            .is_some_and(|exe| exe.eq_ignore_ascii_case("alacritty.exe"))
}

/// Roles (as `uia::map_role` gives them) where a double-click means "open", "maximise" or
/// "activate", never "paste here": window chrome (title bar, toolbar, status bar and app
/// bar map to `AXToolbar`), menus, tabs (`AXRadioButton`, `AXTabGroup`), list, tree and
/// grid items (`AXRow`), buttons and header items (`AXButton`), links and value controls.
/// Checked on the hit and its ancestors unless the hit itself takes text (an address bar
/// sits inside a toolbar), on the double-click's lookup and again on the re-check after the
/// release; see also [`ACTIVATING_ARIA_ROLES`].
pub const ACTIVATING_ROLES: &[&str] = &[
    "AXToolbar",
    "AXMenuBar",
    "AXMenu",
    "AXMenuItem",
    "AXMenuButton",
    "AXButton",
    "AXLink",
    "AXRow",
    "AXRadioButton",
    "AXTabGroup",
    "AXCheckBox",
    "AXPopUpButton",
    "AXScrollBar",
    "AXValueIndicator",
    "AXSlider",
    "AXIncrementor",
];

// Hints. The Swift texts, plus the Windows-only refusals.
pub const HINT_PASTED: &str = "pasted";
pub const HINT_PASTED_PATH: &str = "pasted path";
pub const HINT_PASTE_FAILED: &str = "couldn't paste · still holding it";
pub const HINT_CANVAS_FAILED: &str = "nothing here took it · still holding it";
pub const HINT_REST_FAILED: &str = "couldn't paste the rest";
pub const HINT_NO_IMAGES: &str = "this field doesn't take images";
pub const HINT_NO_VIDEOS: &str = "this field doesn't take videos";
pub const HINT_HOVER_PASTE: &str = "double-click to paste";
pub const HINT_HOVER_SAVE: &str = "double-click to save here";
pub const HINT_ELEVATED: &str = "can't reach this window";
pub const HINT_CLIPBOARD: &str = "can't save your clipboard · still holding it";
pub const HINT_SLOW: &str = "this app didn't answer · still holding it";
/// Saving into a folder (`saveToFinder`). The success hints are [`saved_hint`] and
/// [`HINT_ALREADY_SAVED`]; [`not_a_folder_hint`] names a place that is not a folder.
pub const HINT_SAVE_FAILED: &str = "can't save here · still holding it";
pub const HINT_READ_ONLY: &str = "this folder is read-only · still holding it";
pub const HINT_DISK_FULL: &str = "disk is full · still holding it";
pub const HINT_EXPLORER_SLOW: &str = "Explorer didn't answer · still holding it";
/// A recording whose movie file already lives in that folder: nothing is copied.
pub const HINT_ALREADY_SAVED: &str = "already saved here";
pub const HINT_RESTORE_FAILED: &str = "couldn't put your clipboard back";
/// Something read the paste but nothing proves the target took it: the card stays (a lost
/// card is worse than a second paste) and the user is asked to look.
/// A note did not say whether it took the cards.
pub const HINT_NOTE_SLOW: &str = "the note didn't answer · still holding it";
/// The note could not add the cards.
pub const HINT_NOTE_FAILED: &str = "couldn't add to the note · still holding it";
/// How long a note may take to answer a double-click (it writes one PNG per card).
pub const NOTE_TIMEOUT: f64 = 10.0;
pub const HINT_UNCERTAIN: &str = "check the paste · still holding it";
/// How long after the paste keys the probe may still confirm a paste (`OutcomeRules`).
pub const PROBE_TIMEOUT: f64 = 3.0;
/// The keyboard focus is not in the target field (Swift's canvas text, used for "the paste
/// would go nowhere").
pub const HINT_NOT_FOCUSED: &str = HINT_CANVAS_FAILED;

/// Deadline of the folder lookup (`explorer::DEFAULT_DEADLINE`).
pub const FOLDER_DEADLINE: f64 = 0.5;
/// One file must be written within this long, or the save gives up (a stuck network
/// drive); the saver thread may still finish that file later.
pub const SAVE_TIMEOUT: f64 = 15.0;
/// After a partial save, the failure hint follows the "saved" one after this long per
/// saved card (Swift: `0.4 * saved.count`).
pub const SAVE_FAIL_STEP: f64 = 0.4;

/// "saved to Pictures", "saved 3 to Pictures" (`saveToFinder`: "saved" or "saved N", then
/// "to" and the folder's name). A drive root has no name, so it is shown whole.
pub fn saved_hint(count: usize, folder: &Path) -> String {
    let name = folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    if count == 1 {
        format!("saved to {name}")
    } else {
        format!("saved {count} to {name}")
    }
}

/// The window shows something that is not a file-system folder (This PC, Recycle Bin).
pub fn not_a_folder_hint(display_name: &str) -> String {
    format!(
        "can't save in {} · still holding it",
        clip_chars(display_name, 40)
    )
}

/// Why a file could not be written, in the words of the hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveProblem {
    ReadOnly,
    DiskFull,
    Other,
}

impl SaveProblem {
    pub fn hint(self) -> &'static str {
        match self {
            SaveProblem::ReadOnly => HINT_READ_ONLY,
            SaveProblem::DiskFull => HINT_DISK_FULL,
            SaveProblem::Other => HINT_SAVE_FAILED,
        }
    }
}

/// A failed folder save: the kind for the hint, the error for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFail {
    pub problem: SaveProblem,
    pub why: String,
}

impl SaveFail {
    pub fn other(why: impl Into<String>) -> SaveFail {
        SaveFail {
            problem: SaveProblem::Other,
            why: why.into(),
        }
    }

    /// Access denied (a read-only or protected folder) and a full disk
    /// (`ERROR_DISK_FULL`, `ERROR_HANDLE_DISK_FULL`) get their own hint.
    pub fn of(error: &saver::SaveError) -> SaveFail {
        let problem = match error {
            saver::SaveError::Io(io) => match (io.kind(), io.raw_os_error()) {
                (std::io::ErrorKind::PermissionDenied, _) | (_, Some(5)) => SaveProblem::ReadOnly,
                (std::io::ErrorKind::StorageFull, _) | (_, Some(39 | 112)) => SaveProblem::DiskFull,
                _ => SaveProblem::Other,
            },
            saver::SaveError::Encode(_) => SaveProblem::Other,
        };
        SaveFail {
            problem,
            why: error.to_string(),
        }
    }
}

/// The hint after the user held a modifier through every retry.
pub fn modifier_hint(modifier: Modifier) -> String {
    let name = match modifier {
        Modifier::Shift => "Shift",
        Modifier::Ctrl => "Ctrl",
        Modifier::Alt => "Alt",
        Modifier::Win => "the Windows key",
    };
    format!("let go of {name} to paste · still holding it")
}

// MARK: Facts

/// What a point on screen looks like to the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CanvasFacts {
    /// A surface that takes a pasted image anywhere (`drop_policy::is_canvas`).
    pub canvas: bool,
    pub known: bool,
    /// A text document the page draws itself (Google Docs).
    pub document: bool,
}

/// The focused element of the snapshot (it has no parents, so only itself is checked).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Focus {
    pub pid: u32,
    pub editable: bool,
    pub frame: Option<Rect>,
    /// An editable field of one line ([`SINGLE_LINE_ROLES`]).
    pub single_line: bool,
    /// xterm.js's hidden input (`xterm-helper-textarea`) in web content.
    pub xterm: bool,
}

/// What the log says about an element. Never its value.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Desc {
    /// UI Automation control type name.
    pub control: &'static str,
    /// Class name, at most 60 characters.
    pub class: String,
    /// UI Automation name (a label, not the contents), at most 40 characters.
    pub name: String,
    pub aria: String,
    /// The mapped role.
    pub role: String,
}

impl Desc {
    pub fn of(e: &ElementInfo) -> Desc {
        Desc {
            control: uia::control_type_name(e.control_type),
            class: clip_chars(&e.class_name, 60),
            name: clip_chars(&e.name, 40),
            aria: e.aria_role.clone(),
            role: e.role.to_string(),
        }
    }
}

impl std::fmt::Display for Desc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {:?} class={:?}", self.control, self.name, self.class)?;
        if !self.aria.is_empty() {
            write!(f, " aria={}", self.aria)?;
        }
        write!(f, " role {}", self.role)
    }
}

fn clip_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// "single-line", "multi-line" or "-" for the role of the field a paste goes into.
pub fn lines(field_role: Option<&str>) -> &'static str {
    match field_role {
        Some(role) if SINGLE_LINE_ROLES.contains(&role) => "single-line",
        Some(_) => "multi-line",
        None => "-",
    }
}

/// A UI Automation snapshot reduced to the facts the router decides from (`DropSpotInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct Spot {
    pub target: Target,
    /// Process of the hit element (for a UWP app, the app rather than its frame host).
    pub pid: u32,
    /// For the log.
    pub role: String,
    /// The hit or one of its ancestors takes text (`uiElementIsEditableText`).
    pub editable_hit: bool,
    /// The hit, an ancestor or an editable element under its placeholder takes text.
    pub editable: bool,
    /// File Explorer or the desktop: the empty background of a folder view.
    pub drop_spot: bool,
    /// xterm.js inside a web view (VS Code): text only, like a terminal.
    pub web_terminal: bool,
    /// Computed for every hit; counts as a canvas only when nothing is editable.
    pub canvas: CanvasFacts,
    /// Web content drawn by Chromium.
    pub chromium: bool,
    /// The hit is window chrome or a control ([`ACTIVATING_ROLES`]) and takes no text.
    pub activating: bool,
    /// Only with `want_focus`.
    pub focused: Option<Focus>,
    /// The point the answer is about (None for spots built by hand).
    pub point: Option<(i32, i32)>,
    /// The hit and the focused element, for the log.
    pub hit_desc: Desc,
    pub focus_desc: Option<Desc>,
    /// Role of the editable element found (the hit, an ancestor or a nearby element).
    pub field_role: Option<String>,
    /// A probe lookup's fingerprint of the focused element.
    pub probe: Option<Probe>,
}

impl Spot {
    /// A spot over `target` where nothing is recognised.
    pub fn new(target: Target) -> Spot {
        Spot {
            pid: target.pid,
            target,
            role: String::new(),
            editable_hit: false,
            editable: false,
            drop_spot: false,
            web_terminal: false,
            canvas: CanvasFacts::default(),
            chromium: false,
            activating: false,
            focused: None,
            point: None,
            hit_desc: Desc::default(),
            focus_desc: None,
            field_role: None,
            probe: None,
        }
    }

    /// The facts the debug log shows for this spot.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "{} pid {} window {:#x} hit {} {}",
            self.target.exe.as_deref().unwrap_or("?"),
            self.pid,
            self.target.window,
            self.hit_desc,
            lines(self.field_role.as_deref()),
        );
        if let (Some(desc), Some(focus)) = (&self.focus_desc, &self.focused) {
            text.push_str(&format!(
                "; focused {desc} pid {} editable={}",
                focus.pid,
                yes(focus.editable)
            ));
        }
        text
    }

    pub fn is_shell(&self) -> bool {
        self.target.shell.is_some()
    }

    /// Canvas facts that count (Swift fills them only when nothing editable was found).
    pub fn canvas_here(&self) -> CanvasFacts {
        if self.editable {
            CanvasFacts::default()
        } else {
            self.canvas
        }
    }

    /// `DropSpotInfo.surface`.
    pub fn surface(&self) -> Surface {
        if self.web_terminal || self.target.is_terminal {
            return Surface::Terminal;
        }
        let canvas = self.canvas_here();
        if !self.editable && canvas.canvas {
            if canvas.document {
                Surface::Document
            } else {
                Surface::Canvas
            }
        } else {
            Surface::Field
        }
    }
}

/// A lookup's answer as plain facts.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    Spot(Box<Spot>),
    OwnProcess,
    Elevated { pid: u32 },
    TimedOut { target: Target },
    Failed(String),
}

/// What the paste goes into (`PasteTarget.Surface`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Field,
    Terminal,
    Canvas,
    Document,
}

/// The clipboard facts of [`clipboard::Watch`], with times on the router's clock.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClipFacts {
    pub sequence: u32,
    /// The last change made by anyone but us.
    pub last_change: Option<f64>,
    pub staged: Option<StagedFacts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StagedFacts {
    /// The `stage` request that wrote it.
    pub ticket: u64,
    /// Formats another process rendered.
    pub reads: u32,
    pub rewrites: u32,
    pub last_read_at: Option<f64>,
    /// Someone else wrote the clipboard after our last (re)write.
    pub overwritten_at: Option<f64>,
    pub last_reader: Option<u32>,
    /// Every render: (reader process if known, time), oldest first.
    pub renders: [Option<(Option<u32>, f64)>; clipboard::RENDER_LOG],
}

impl ClipFacts {
    fn staged_for(&self, ticket: u64) -> Option<StagedFacts> {
        self.staged.filter(|s| s.ticket == ticket)
    }
}

/// A clipboard reply, with the snapshot type of the [`Env`].
#[derive(Debug, Clone)]
pub struct ClipReply<S> {
    pub ticket: u64,
    pub result: Result<ClipDone<S>, ClipFail>,
}

#[derive(Debug, Clone)]
pub enum ClipDone<S> {
    Snapshot {
        snap: S,
        /// Read after `CloseClipboard`.
        sequence: u32,
        /// `Snapshot::lost`, for the log; Some means "do not stage".
        lost: Option<String>,
    },
    Staged {
        sequence: u32,
    },
    Restored {
        sequence: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipFail {
    /// `restore`: the guard refused (the clipboard changed since).
    Changed,
    /// `stage` / `restage` failed after emptying the clipboard, leaving this sequence.
    Emptied {
        sequence: u32,
    },
    /// The clipboard was locked, the queue full or the request stale: nothing was written;
    /// worth another try.
    Retry(String),
    /// A restore wrote some formats only; the clipboard thread kept its copy, and our partial
    /// result counts as our own staging for the next try.
    Partial,
    Other(String),
}

/// What a restore may write over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreGuard {
    /// Only our staging (`Clipboard::restore_over_staging`).
    OwnStaging,
    /// Only the clipboard at this sequence number (after a failed stage emptied it, or after
    /// a terminal's copy-on-select was adopted).
    Sequence(u32),
}

/// What must still hold when a batch of keys goes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendGuard {
    pub window: isize,
    pub pid: u32,
    /// The focused control seen at the first batch of this delivery.
    pub focus: Option<isize>,
    /// The staging that must still own the clipboard (the paste keys only).
    pub stage: Option<u64>,
}

/// The keys that paste in the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteKeys {
    CtrlV,
    CtrlShiftV,
}

impl PasteKeys {
    fn batch(self) -> Batch {
        match self {
            PasteKeys::CtrlV => Batch::Paste,
            PasteKeys::CtrlShiftV => Batch::PasteWithShift,
        }
    }
}

/// What each terminal of `uia::TERMINAL_EXES` gets, and why:
///
/// | Terminal | Sent | Why |
/// |---|---|---|
/// | `WindowsTerminal.exe` | Ctrl+V | its default key bindings paste on Ctrl+V |
/// | `OpenConsole.exe` | Ctrl+V | Windows Terminal's console host, same bindings |
/// | `conhost.exe`, `ConsoleWindowClass` | Ctrl+V | "Ctrl key shortcuts" (on by default since Windows 10) paste on Ctrl+V |
/// | `wezterm-gui.exe` | Ctrl+Shift+V | by default Ctrl+V reaches the program as a literal ^V |
/// | `alacritty.exe` | Ctrl+Shift+V | by default Ctrl+V reaches the program as a literal ^V |
///
/// Fields, documents, canvases and web terminals (VS Code) get Ctrl+V.
pub const TERMINAL_PASTE_KEYS: &[(&str, PasteKeys)] = &[
    ("WindowsTerminal.exe", PasteKeys::CtrlV),
    ("OpenConsole.exe", PasteKeys::CtrlV),
    ("conhost.exe", PasteKeys::CtrlV),
    ("wezterm-gui.exe", PasteKeys::CtrlShiftV),
    ("alacritty.exe", PasteKeys::CtrlShiftV),
];

/// The paste keys for a target window ([`TERMINAL_PASTE_KEYS`]).
pub fn paste_keys_for(target: &Target) -> PasteKeys {
    if !target.is_terminal {
        return PasteKeys::CtrlV;
    }
    let exe = target.exe.as_deref().unwrap_or("");
    TERMINAL_PASTE_KEYS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(exe))
        .map_or(PasteKeys::CtrlV, |(_, keys)| *keys)
}

// MARK: Env

/// Everything the router does to the outside. Times are on the caller's clock
/// (`hook::now`). Each call returns at once.
pub trait Env {
    /// The user's clipboard as saved by the clipboard thread.
    type Snap: Clone;

    /// Starts a UI Automation lookup; the answer comes as [`Router::lookup_answered`] or
    /// [`Router::seen`] with the returned id.
    fn lookup(&mut self, point: (i32, i32), want_focus: bool, probe: bool, deadline: f64) -> u64;
    /// The exe name of a process, for the log.
    fn process_name(&self, pid: u32) -> Option<String>;
    fn clip_facts(&self, now: f64) -> ClipFacts;
    fn clip_snapshot(&mut self) -> Result<u64, String>;
    /// Stages only over the clipboard `expect` names (checked while it is open). With
    /// `rollback`, the clipboard thread keeps that copy of the user's clipboard and owns the
    /// rollback from then on.
    fn clip_stage(
        &mut self,
        payload: Payload,
        expect: Expect,
        rollback: Option<&Self::Snap>,
    ) -> Result<u64, String>;
    fn clip_restage(&mut self, expect: Expect) -> Result<u64, String>;
    /// Asks the clipboard thread to write its copy back if `guard` holds then.
    fn clip_rollback(&mut self, guard: RestoreGuard) -> Result<u64, String>;
    /// The user copied something: the clipboard thread forgets its copy.
    fn clip_abandon(&mut self);
    /// Saves the card as a PNG into the pasted folder, off the UI thread; the result comes
    /// as [`Router::saved`] with the returned id.
    fn save_png(&mut self, card: &HeldCard) -> u64;
    /// Asks File Explorer for the folder a window (or the desktop) shows; the answer comes
    /// as [`Router::folder_answered`] with the returned id.
    fn folder_lookup(&mut self, request: FolderRequest, deadline: f64) -> u64;
    /// Writes the card into `folder` on the saver thread: a PNG, or a copy of a
    /// recording's movie file. The result comes as [`Router::folder_saved`]. Err: not
    /// queued (no saver, queue full).
    fn save_to_folder(&mut self, card: &HeldCard, folder: &Path) -> Result<u64, String>;
    /// Selects `file` in `window`'s view once it shows it. Best effort, no answer.
    fn reveal(&mut self, window: isize, file: &Path);
    /// Is the target in front: its top-level window, and its process when both are known?
    fn foreground_is(&self, window: isize, pid: u32) -> bool;
    /// Is `point` inside the client area of `window` (Win32 only)?
    fn point_in_client(&self, window: isize, point: (i32, i32)) -> bool;
    /// The user's key presses so far (a counter; modifiers and injected keys excluded).
    fn key_presses(&self) -> u64;
    /// The user's main mouse button is down right now (the real state, not the hook's).
    fn primary_down(&self) -> bool;
    /// The top-level window at `point` is a terminal (Win32 only, no UI Automation).
    fn terminal_at(&self, point: (i32, i32)) -> bool;
    /// Sends one batch through `input::send_guarded`: the last checks (the staging still on
    /// the clipboard, the window, its process, its focused control, the modifiers) and the
    /// keys with nothing in between. Returns the focused control seen.
    fn send(&mut self, batch: Batch, guard: SendGuard) -> (Sent, Option<isize>);
    /// One debug line. Only written when `FAIRY_DEBUG=1`.
    fn log(&mut self, now: f64, line: &str);
    /// One of our note windows is at `point` (from the notes' drop zones, no lookup).
    fn note_at(&self, point: (i32, i32)) -> bool;
    /// Hands the carried cards, top first, to the note at `point`. The answer comes as
    /// [`Router::note_took`] or [`Router::note_failed`].
    fn give_to_note(&mut self, point: (i32, i32), cards: Vec<HeldCard>);
}

// MARK: Pure helpers

/// macOS bundle ids for Windows canvas apps, so the core's `CANVAS_BUNDLE_PREFIXES` match
/// them (the core rule names macOS apps; this is the Windows workaround).
pub fn windows_app_id(exe: Option<&str>) -> Option<&'static str> {
    const APPS: &[(&str, &str)] = &[
        ("figma.exe", "com.figma.Desktop"),
        ("powerpnt.exe", "com.microsoft.Powerpoint"),
        ("miro.exe", "com.miro.desktop"),
        ("canva.exe", "com.canva.desktop"),
        ("photoshop.exe", "com.adobe.Photoshop"),
        ("illustrator.exe", "com.adobe.illustrator"),
    ];
    let exe = exe?;
    APPS.iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(exe))
        .map(|(_, id)| *id)
}

/// A path for a Windows command line, followed by a space so the line can go on (as the
/// core's `shell_quoted`). Double quotes when needed: cmd.exe, PowerShell and bash all read
/// them, and a Windows path cannot contain `"`.
pub fn quoted_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let needs = text
        .chars()
        .any(|c| c.is_whitespace() || "&()[]{}^=;!'+,`~$%@#".contains(c));
    if needs {
        format!("\"{text}\" ")
    } else {
        format!("{text} ")
    }
}

/// `FinderTarget.copy`'s check: is `parent` the folder (Windows paths ignore case and a
/// trailing separator)?
pub fn same_folder(parent: Option<&Path>, folder: &Path) -> bool {
    let key = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_lowercase()
    };
    parent.is_some_and(|p| key(p) == key(folder))
}

/// xterm.js marks its elements with `xterm…` classes; Chromium reports the class attribute
/// as the UI Automation class name.
fn is_web_terminal(chain: &[UiaNode]) -> bool {
    for node in chain {
        let e = node.element();
        if e.role == "AXWebArea" {
            break;
        }
        if has_class_prefix(e, WEB_TERMINAL_CLASS_PREFIXES) {
            return true;
        }
    }
    false
}

fn has_class_prefix(e: &ElementInfo, prefixes: &[&str]) -> bool {
    e.class_name
        .split_whitespace()
        .any(|class| prefixes.iter().any(|p| class.starts_with(p)))
}

/// xterm.js's hidden input, which has the keyboard focus while the terminal does.
fn is_xterm_input(e: &ElementInfo) -> bool {
    e.class_name
        .split_whitespace()
        .any(|class| class == "xterm-helper-textarea")
}

fn canvas_facts(chain: &[UiaNode], exe: Option<&str>) -> CanvasFacts {
    let Some(node) = chain.first() else {
        return CanvasFacts::default();
    };
    let web = chain.iter().find(|n| n.element().role == "AXWebArea");
    let url = web.and_then(|w| w.element().url.clone());
    let bundle = windows_app_id(exe);
    let spot = drop_policy::Spot {
        role: node.role(),
        ancestor_roles: chain[1..].iter().filter_map(|n| n.role()).collect(),
        frame: node.frame(),
        web_area_frame: web.and_then(|w| w.frame()),
        url: url.clone(),
        bundle_id: bundle.map(str::to_string),
    };
    CanvasFacts {
        canvas: drop_policy::is_canvas(&spot),
        known: drop_policy::is_known_canvas_app(bundle, url.as_deref()),
        document: drop_policy::is_page_drawn_document(url.as_deref()),
    }
}

/// UI Automation class of File Explorer's items view (the `DirectUIHWND` window inside
/// `SHELLDLL_DefView`): the list of a folder's items and its empty background.
pub const ITEMS_VIEW_CLASS: &str = "UIItemsView";
/// UI Automation class of the desktop's folder view window. Its child is the icon list
/// (`SysListView32`); with desktop icons hidden the list is hidden and the view is the hit.
pub const DESKTOP_VIEW_CLASS: &str = "SHELLDLL_DefView";

/// `FinderTarget.isDropSpot` for Windows: is the hit the empty background of a folder view?
///
/// The core's role rule alone is not enough here. Windows 11 Explorer shows the navigation
/// pane as a `Pane` (`SysTreeView32`), and the title bar area, the tab strip's free space,
/// the breadcrumb bar and the details pane as panes and groups: all of them map to
/// `AXGroup`, a container role. So the hit must also be inside the folder view itself, told
/// by its class: Explorer's items view ([`ITEMS_VIEW_CLASS`]) or the desktop's view
/// ([`DESKTOP_VIEW_CLASS`]). Inside the view, an item (`AXRow`, with its name and cells)
/// and a column header (`Header`, `HeaderItem`) are not background. The one text that is
/// background is the view's own "This folder is empty." (a static text whose parent is the
/// view; an item's name has the item as parent).
pub fn shell_drop_spot(chain: &[UiaNode], shell: Shell) -> bool {
    let Some(hit) = chain.first() else {
        return false;
    };
    let view_class = match shell {
        Shell::Explorer => ITEMS_VIEW_CLASS,
        Shell::Desktop => DESKTOP_VIEW_CLASS,
    };
    let Some(view) = chain
        .iter()
        .position(|n| n.element().class_name == view_class)
    else {
        return false;
    };
    if chain.iter().any(|n| n.element().role == "AXRow") {
        return false;
    }
    let header = |n: &UiaNode| {
        let control = n.element().control_type;
        control == UIA_HeaderControlTypeId.0 || control == UIA_HeaderItemControlTypeId.0
    };
    if chain[..view].iter().any(header) {
        return false;
    }
    let e = hit.element();
    let ancestors: Vec<String> = chain[1..].iter().filter_map(|n| n.role()).collect();
    drop_policy::is_drop_spot(Some(e.role), &ancestors) || (view == 1 && e.role == "AXStaticText")
}

/// `DropSpotInfo.resolve`, on a snapshot.
pub fn spot_from_snapshot(snapshot: &Snapshot, point: (i32, i32)) -> Spot {
    let hit = snapshot.hit.clone();
    let chain: Vec<UiaNode> = std::iter::successors(Some(hit.clone()), |n| n.parent())
        .take(uia::MAX_ANCESTORS + 1)
        .collect();
    let e = hit.element();
    let mut spot = Spot::new(snapshot.target.clone());
    spot.pid = e.pid;
    spot.role = e.role.to_string();
    spot.point = Some(snapshot.point);
    spot.hit_desc = Desc::of(e);
    spot.probe = snapshot.probe;
    spot.focus_desc = snapshot.focused.as_ref().map(|f| Desc::of(f.element()));
    spot.focused = snapshot.focused.as_ref().map(|f| {
        let editable = editable_self_or_ancestor(f).is_some();
        let info = f.element();
        Focus {
            pid: info.pid,
            editable,
            frame: info.frame,
            single_line: editable && SINGLE_LINE_ROLES.contains(&info.role),
            xterm: is_xterm_input(info),
        }
    });
    if let Some(shell) = snapshot.target.shell {
        spot.drop_spot = shell_drop_spot(&chain, shell);
        return spot;
    }
    spot.chromium = chain.iter().any(|n| n.element().framework == "Chrome");
    spot.canvas = canvas_facts(&chain, snapshot.target.exe.as_deref());
    let in_page = chain.iter().any(|n| n.element().role == "AXWebArea");
    // The hit can be the helper input itself, a canvas or a row inside the terminal, or an
    // element whose terminal ancestors UI Automation does not show; then the focused
    // helper input (after the click) tells.
    let focused_xterm = spot.focused.is_some_and(|f| f.xterm && f.pid == e.pid);
    if (in_page && is_web_terminal(&chain)) || is_xterm_input(e) || focused_xterm {
        spot.web_terminal = true;
        spot.editable = true;
        return spot;
    }
    // A terminal's text area (Windows Terminal, conhost) has a Text pattern but no Value
    // pattern, so UI Automation does not call it editable; it still takes pasted text, as
    // iTerm's `AXTextArea` does on macOS. Its tabs and title bar have no Text pattern.
    let terminal_text = snapshot.target.is_terminal && e.text_pattern;
    let field = if terminal_text {
        Some(hit.clone())
    } else {
        editable_self_or_ancestor(&hit)
    };
    spot.editable_hit = field.is_some();
    spot.field_role = field.map(|f| f.element().role.to_string());
    spot.activating = !spot.editable_hit
        && chain.iter().any(|n| {
            let e = n.element();
            ACTIVATING_ROLES.contains(&e.role)
                || ACTIVATING_ARIA_ROLES.contains(&e.aria_role.as_str())
        });
    let at = Point::new(point.0 as f64, point.1 as f64);
    if spot.editable_hit {
        spot.editable = true;
    } else if let Some(nearby) = editable_target(at, &hit) {
        spot.editable = true;
        spot.field_role = Some(nearby.element().role.to_string());
    }
    spot
}

/// Before any key goes out to a field or a document: does the keyboard focus show that the
/// target field will get the paste? The focused element (from a focus lookup after the
/// release) must belong to the target window's process (or the hit's), take text, and for a
/// field be a visible field under the click; a page-drawn document catches typing in a
/// hidden input, so there any focused text input of the target counts. Without this a
/// double-click on text next to a field (or a hidden input a page keeps under the text)
/// sends Ctrl+V to a page that reads the clipboard and drops the image.
pub fn focus_evidence(
    spot: &Spot,
    pids: (u32, u32),
    window: isize,
    point: (i32, i32),
    surface: Surface,
) -> bool {
    if spot.target.window != window {
        return false;
    }
    let Some(focus) = spot.focused else {
        return false;
    };
    if !(focus.pid == pids.0 || focus.pid == pids.1) || !focus.editable {
        return false;
    }
    match surface {
        Surface::Document => true,
        _ => focus.frame.is_some_and(|frame| {
            let at = Point::new(point.0 as f64, point.1 as f64);
            drop_policy::is_visible_field(&frame) && frame.inset_by(-6.0, -6.0).contains(at)
        }),
    }
}

/// What the acceptance probe saw after the paste keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeView {
    /// The field is single-line: a read with no change is a refusal.
    pub verify: bool,
    /// At least one probe after the paste answered.
    pub answered: bool,
    /// The "before" fingerprint has something that can change (a value or an image count).
    pub can_tell: bool,
    pub changed: bool,
}

/// Renders of the staged formats after the paste keys went out, by who read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reads {
    /// By the identified target process.
    pub target: u32,
    /// By a reader with no window (pid unknown) that is probably the target: Chromium and
    /// Electron open the clipboard without a window. See [`verdict`].
    pub probable: u32,
    /// By another process, or by an unknown reader that cannot be the target's.
    pub other: u32,
}

/// What happens to the card of one paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pending,
    /// The card leaves the pile.
    Accepted,
    /// The target read it and showed no change: it refused it ("doesn't take images").
    Rejected,
    /// Nothing read it: the keys went nowhere ("couldn't paste").
    Failed,
    /// Something read it but nothing proves the target took it ("check the paste"); the
    /// card stays and the pile stops.
    Uncertain,
}

/// The outcome rule, in one place. Never lose a card on weak evidence: a card leaves only
/// on proof, and when the proof is missing but the paste may have happened the user is told
/// to check instead of being told it failed.
///
/// - The probe saw the field change (its text, or its image count): accepted.
/// - Reads: a render by the identified target counts. A render by a reader without a window
///   counts as "probably the target" when it came after our successful send, while the
///   target was in front, and the target is Chromium-based (it opens the clipboard without
///   a window) or no windowless reader was seen before the send. Any other reader (a
///   clipboard manager, the user pasting elsewhere) never counts.
/// - Single-line field whose probe can tell: the probe is the judge. Changed: accepted.
///   Read by the target (or probably the target) and unchanged by [`PROBE_TIMEOUT`]:
///   refused. Read only by others: uncertain. Not read at all: failed. If no probe answers
///   by then, or the probe cannot tell, the reads decide as below.
/// - Otherwise (multi-line composers are trusted, as in Swift): a read by the target or
///   probably the target is acceptance. Read only by others: the probe may still confirm
///   until [`PROBE_TIMEOUT`], then uncertain. Not read at all: failed.
/// - A delivery the user stopped (another press, a key) never ends as refused or failed
///   after its keys went out: it ends uncertain unless evidence arrives.
pub fn verdict(
    elapsed: f64,
    reads: Reads,
    canvas: bool,
    probe: Option<ProbeView>,
    cancelled: bool,
) -> Verdict {
    let found = judge(elapsed, reads, canvas, probe);
    match found {
        Verdict::Rejected | Verdict::Failed if cancelled => Verdict::Uncertain,
        other => other,
    }
}

fn judge(elapsed: f64, reads: Reads, canvas: bool, probe: Option<ProbeView>) -> Verdict {
    if probe.is_some_and(|p| p.changed) {
        return Verdict::Accepted;
    }
    let evidence = reads.target + reads.probable;
    if let Some(p) = probe
        && p.verify
        && p.can_tell
    {
        if p.answered {
            let any = evidence + reads.other;
            return match OutcomeRules::new(true, canvas).evaluate(elapsed, any, false) {
                PasteOutcome::Pending => Verdict::Pending,
                PasteOutcome::Accepted => Verdict::Accepted,
                PasteOutcome::Rejected if evidence > 0 => Verdict::Rejected,
                PasteOutcome::Rejected => Verdict::Uncertain,
                PasteOutcome::Failed => Verdict::Failed,
            };
        }
        if elapsed < PROBE_TIMEOUT {
            // Wait for the judge: Chromium reads a single-line field and may drop the image.
            return Verdict::Pending;
        }
    }
    match OutcomeRules::new(false, canvas).evaluate(elapsed, evidence, false) {
        PasteOutcome::Accepted => Verdict::Accepted,
        PasteOutcome::Pending => Verdict::Pending,
        _ if reads.other > 0 && probe.is_some() && elapsed < PROBE_TIMEOUT => Verdict::Pending,
        _ if reads.other > 0 => Verdict::Uncertain,
        _ => Verdict::Failed,
    }
}

pub fn seen_from_outcome(outcome: Outcome, point: (i32, i32)) -> Seen {
    match outcome {
        Outcome::Found(snapshot) => Seen::Spot(Box::new(spot_from_snapshot(&snapshot, point))),
        Outcome::OwnProcess => Seen::OwnProcess,
        Outcome::Elevated { pid } => Seen::Elevated { pid },
        Outcome::TimedOut { target } => Seen::TimedOut { target },
        Outcome::Failed(why) => Seen::Failed(why),
    }
}

/// The hover hint for a lookup's answer (`applyHover`).
pub fn hover_hint(seen: &Seen) -> Option<&'static str> {
    match seen {
        Seen::Elevated { .. } => Some(HINT_ELEVATED),
        Seen::Spot(spot) if spot.is_shell() => spot.drop_spot.then_some(HINT_HOVER_SAVE),
        Seen::Spot(spot) => (spot.editable || spot.canvas_here().canvas || spot.target.is_terminal)
            .then_some(HINT_HOVER_PASTE),
        _ => None,
    }
}

/// `resolveAfterMouseUp`: what to do once the button is up and the app was asked again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmed {
    Ignore,
    Paste {
        surface: Surface,
        /// Close a whiteboard's fresh text box (Escape) before pasting on the board.
        dismiss_editor: bool,
    },
}

/// The re-check after the mouse-up. `pid` and `before` come from the double-click's lookup.
pub fn confirm_after_up(
    spot: &Spot,
    point: (i32, i32),
    pid: Option<u32>,
    before: CanvasFacts,
) -> Confirmed {
    let at = Point::new(point.0 as f64, point.1 as f64);
    let same_pid = pid.is_none_or(|pid| pid == spot.pid);
    let focus = spot
        .focused
        .filter(|f| f.editable && pid.is_none_or(|pid| pid == f.pid));
    let focused_editable = focus.is_some();
    let focused_frame = focus.and_then(|f| f.frame);
    let ok = drop_policy::confirm_after_mouse_up(
        at,
        spot.editable && same_pid,
        focused_editable,
        focused_frame,
        false,
    );
    let here = spot.canvas_here();
    let document = here.document || before.document;
    let fresh_editor = ok && before.canvas && !spot.web_terminal && !document;
    let canvas = fresh_editor
        || (!ok
            && same_pid
            && drop_policy::confirm_canvas_drop(
                at,
                here.canvas || before.canvas,
                here.known || before.known,
                focused_editable,
                focused_frame,
            ));
    if !(ok || canvas) {
        return Confirmed::Ignore;
    }
    let surface = if canvas {
        if document {
            Surface::Document
        } else {
            Surface::Canvas
        }
    } else if ok && same_pid {
        spot.surface()
    } else {
        Surface::Field
    };
    Confirmed::Paste {
        surface,
        dismiss_editor: fresh_editor,
    }
}

// MARK: Router

#[derive(Debug, Clone, Copy)]
struct Click {
    point: (i32, i32),
    at: f64,
    /// The top-level window under both presses (from the hook).
    window: isize,
    /// The first press of the double-click.
    first_at: f64,
    /// The user's key presses counted at the double-click.
    keys: u64,
}

/// Where the keys go: the top-level window that was double-clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Aim {
    /// Process of the top-level window.
    pid: u32,
    window: isize,
    /// Process of the hit element (a UWP app inside its frame host's window).
    element_pid: u32,
    keys: PasteKeys,
    /// The double-click point.
    point: (i32, i32),
    /// Chromium-based (it reads the clipboard without a window).
    chromium: bool,
}

impl Aim {
    fn new(target: &Target, element_pid: u32, point: (i32, i32)) -> Aim {
        Aim {
            pid: target.pid,
            window: target.window,
            element_pid,
            keys: paste_keys_for(target),
            point,
            chromium: false,
        }
    }

    /// A read is the target's when its reader is the target's window process or element
    /// process. Chromium and Electron read the clipboard in the browser process, which owns
    /// the top-level window, so they count. An unknown reader never counts on its own.
    fn is_target(&self, reader: Option<u32>) -> bool {
        reader.is_some_and(|pid| pid == self.pid || pid == self.element_pid)
    }
}

/// What the double-click's lookup said, kept for the re-check after the release.
#[derive(Debug, Clone, Copy)]
struct First {
    aim: Aim,
    /// Process of the first hit element; None when the lookup timed out.
    pid: Option<u32>,
    before: CanvasFacts,
    chromium: bool,
    /// The lookup timed out: the re-check gets [`SLOW_DEADLINE`] and must find an editable
    /// element at the click point itself.
    slow: bool,
}

enum Phase {
    Idle,
    Looking {
        id: u64,
        click: Click,
    },
    AwaitUp {
        click: Click,
        first: First,
        ready_at: Option<f64>,
    },
    Recheck {
        id: u64,
        click: Click,
        first: First,
    },
    Delivering(Box<Delivery>),
    Saving(Box<FolderSave>),
    /// The cards went to a note; waiting for it to say which it took.
    Note {
        until: f64,
    },
}

/// A save into a folder (`saveToFinder`); see the module comment.
struct FolderSave {
    click: Click,
    /// The window to reveal the file in: the Explorer window, or the desktop's.
    window: isize,
    /// The pile at the double-click, top first.
    ids: Vec<CardId>,
    /// The next card to write.
    index: usize,
    folder: Option<PathBuf>,
    /// Written (or already there), in order.
    saved: Vec<(CardId, PathBuf)>,
    /// Recordings whose movie already lived in the folder.
    already: usize,
    /// The first file went to the saver: the save finishes whatever the user does.
    started: bool,
    step: SaveStep,
}

#[derive(Debug, Clone, PartialEq)]
enum SaveStep {
    /// The folder lookup is out.
    Asking { id: u64, since: f64 },
    /// The folder is known: wait for the button to come up, then write.
    Ready,
    /// One card's file is being written by the saver.
    Writing { job: u64, since: f64 },
    /// A partial save: the failure hint after the "saved" one.
    LateHint { at: f64, hint: &'static str },
}

impl FolderSave {
    /// The first file is being written (or was): a press or a key no longer stops it.
    fn writing(&self) -> bool {
        self.started
    }
}

struct Delivery {
    /// The pile when the delivery started, top first.
    ids: Vec<CardId>,
    index: usize,
    aim: Aim,
    terminal: bool,
    surface: Surface,
    dismiss_editor: bool,
    /// The user's clipboard: a snapshot reply still to come, or ready in `Router::restore`.
    clip: Option<u64>,
    job: Job,
    probe: ProbeState,
    click: Click,
    /// The focused control of the target's thread at the first batch of keys.
    focus: Option<isize>,
    /// The user pressed again or typed: no more batches (the card being watched still
    /// finds its outcome).
    cancelled: Option<&'static str>,
}

/// The acceptance probe of the first card (`AcceptanceProbe`).
#[derive(Debug, Default)]
struct ProbeState {
    /// The fingerprint taken with the focus check, before the paste keys.
    before: Option<Probe>,
    /// Single-line field: every paste is verified.
    verify: bool,
    /// The "before" fingerprint has something that can change.
    can_tell: bool,
    after_req: Option<u64>,
    last_check: Option<f64>,
    answered: bool,
    changed: bool,
}

struct Job {
    id: CardId,
    first: bool,
    movie: bool,
    started: f64,
    payload: Pay,
    is_path: bool,
    step: Step,
}

enum Pay {
    Ready(Payload),
    Saving(u64),
    /// The PNG could not be saved.
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Prepare,
    AfterUp(f64),
    /// The focus lookup is out.
    Focus(u64),
    Front(f64),
    Keys {
        keys: Vec<Key>,
        since: f64,
    },
    Gap(f64),
    Staging {
        ticket: u64,
        since: f64,
    },
    Send(f64),
    Watch(f64),
    Next(f64),
}

enum Flow {
    Wait(Option<f64>),
    Again,
    Done,
}

/// One staging of the clipboard, from the `stage` request on.
#[derive(Debug, Clone, Copy)]
struct StageRec {
    ticket: u64,
    requested_at: f64,
    /// The `Staged` reply came: our data is on the clipboard.
    confirmed: bool,
    /// The stage failed after emptying the clipboard, leaving this sequence number.
    emptied: Option<u32>,
    sent_at: Option<f64>,
    terminal: bool,
    aim: Aim,
    /// Reads already logged, by kind (probably the target, others).
    logged_probable: u32,
    logged_foreign: u32,
    /// Windowless reads counted as probably the target while the target was in front.
    probable_latched: u32,
    /// `last_change` and sequence number of the foreign write taken for the click's own
    /// copy-on-select.
    adopted: Option<f64>,
    adopted_seq: Option<u32>,
    guarding: bool,
    restaging: Option<(u64, f64)>,
}

impl StageRec {
    fn new(ticket: u64, now: f64, terminal: bool, aim: Aim) -> StageRec {
        StageRec {
            ticket,
            requested_at: now,
            confirmed: false,
            emptied: None,
            sent_at: None,
            terminal,
            aim,
            logged_probable: 0,
            logged_foreign: 0,
            probable_latched: 0,
            adopted: None,
            adopted_seq: None,
            guarding: true,
            restaging: None,
        }
    }
}

/// The user's clipboard, from the moment a delivery saved it until it is back.
struct Restore<S> {
    snap: S,
    stage: Option<StageRec>,
    /// An earlier paste's staging, kept while a new stage request is in flight: if that
    /// request fails without touching the clipboard, the earlier one is still there.
    prev: Option<StageRec>,
    /// Restore at once (nothing was pasted).
    now_please: bool,
    restoring: Option<(u64, f64)>,
    /// First restore attempt, and when to try again after a refusal.
    first_try: Option<f64>,
    retry_at: Option<f64>,
    /// The top card failed; a target read after this count means the app took it late.
    late: Option<(CardId, u32)>,
    /// The sequence number of the clipboard the snapshot saved (read under the lock); the
    /// first stage expects it.
    expect: Option<u32>,
    /// The user copied something over our staging: never restore.
    user_copied: bool,
    /// The last restore wrote only some formats: the next try writes over that.
    partial: bool,
}

impl<S> Restore<S> {
    fn new(snap: S) -> Restore<S> {
        Restore {
            snap,
            stage: None,
            prev: None,
            now_please: false,
            restoring: None,
            first_try: None,
            retry_at: None,
            late: None,
            expect: None,
            user_copied: false,
            partial: false,
        }
    }
}

enum Rem<S> {
    Waiting(u64),
    Ready {
        snap: S,
        sequence: u32,
        lost: Option<String>,
    },
    Failed,
}

/// The clipboard at the first mouse-down (`rememberClipboard`): the sequence number and the
/// time, and a snapshot only when that click was in a terminal (see `remember_clipboard`).
struct Remembered<S> {
    sequence: u32,
    at: f64,
    rem: Option<Rem<S>>,
}

#[derive(Default)]
struct Hover {
    point: Option<(i32, i32)>,
    moved_at: f64,
    resolved: bool,
    in_flight: Option<u64>,
    last_request: Option<f64>,
}

struct Pending {
    id: u64,
    point: (i32, i32),
    give_up_at: f64,
}

/// See the module comment.
pub struct Router<S> {
    primary: Button,
    phase: Phase,
    restore: Option<Restore<S>>,
    remembered: Option<Remembered<S>>,
    hover: Hover,
    lookups: Vec<Pending>,
    last_up: Option<f64>,
    phase_wake: Option<f64>,
    restore_wake: Option<f64>,
    hover_wake: Option<f64>,
    state_changed: bool,
    /// The last first press: (time, top-level window).
    first_down: Option<(f64, isize)>,
}

fn earlier(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Nobody but us wrote the clipboard since our last write (or since the write we adopted
/// as the click's own copy-on-select).
fn untouched(stage: &StageRec, facts: &ClipFacts) -> bool {
    let Some(staged) = facts.staged_for(stage.ticket) else {
        return false;
    };
    match (staged.overwritten_at, stage.adopted) {
        (None, _) => true,
        (Some(_), Some(base)) => facts.last_change.is_none_or(|c| c <= base),
        (Some(_), None) => false,
    }
}

/// How to put the user's clipboard back over this staging, or None when nothing of ours is
/// there to replace: only what we replaced is restored.
fn restore_guard(stage: &StageRec, facts: &ClipFacts, partial: bool) -> Option<RestoreGuard> {
    if partial {
        // Our own partial restore is on the clipboard; the clipboard thread recognises it.
        return Some(RestoreGuard::OwnStaging);
    }
    if let Some(sequence) = stage.emptied {
        return Some(RestoreGuard::Sequence(sequence));
    }
    if !stage.confirmed || !untouched(stage, facts) {
        return None;
    }
    Some(match stage.adopted_seq {
        // The terminal's copy-on-select replaced our data; it is restored over only while
        // nothing newer came.
        Some(sequence) => RestoreGuard::Sequence(sequence),
        None => RestoreGuard::OwnStaging,
    })
}

/// Renders after the paste keys went out, by kind (see [`verdict`] for the rule). Renders
/// before a successful send never count. `front`: the target is in front now; a windowless
/// read counts as probably the target only while it is (latched in the stage record).
fn read_counts(stage: &mut StageRec, facts: &ClipFacts, front: bool) -> Reads {
    let (Some(staged), Some(sent)) = (facts.staged_for(stage.ticket), stage.sent_at) else {
        return Reads::default();
    };
    let renders = staged.renders.iter().flatten();
    let unknown_before = renders
        .clone()
        .any(|&(reader, at)| reader.is_none() && at < sent);
    let trusted = stage.aim.chromium || !unknown_before;
    let mut reads = Reads::default();
    let mut windowless = 0;
    for &(reader, at) in renders {
        if at < sent {
            continue;
        }
        if stage.aim.is_target(reader) {
            reads.target += 1;
        } else if reader.is_none() && trusted {
            windowless += 1;
        } else {
            reads.other += 1;
        }
    }
    if front {
        stage.probable_latched = stage.probable_latched.max(windowless);
    }
    reads.probable = stage.probable_latched.min(windowless);
    reads.other += windowless - reads.probable;
    reads
}

/// Logs reads not logged yet, by kind, with the reader's exe name.
fn log_reads<E: Env>(stage: &mut StageRec, reads: Reads, facts: &ClipFacts, now: f64, env: &mut E) {
    if reads.probable > stage.logged_probable {
        let new = reads.probable - stage.logged_probable;
        stage.logged_probable = reads.probable;
        env.log(
            now,
            &format!("{new} read(s) by a reader without a window → probably the target"),
        );
    }
    if reads.other > stage.logged_foreign {
        let new = reads.other - stage.logged_foreign;
        stage.logged_foreign = reads.other;
        let reader = facts.staged_for(stage.ticket).and_then(|s| s.last_reader);
        let name = reader
            .and_then(|pid| env.process_name(pid))
            .unwrap_or_else(|| "?".into());
        env.log(
            now,
            &format!("{new} read(s) by pid {reader:?} ({name}), not the target → not counted"),
        );
    }
}

impl<S: Clone> Router<S> {
    /// `primary` is the physical button of the user's main click (Right when the buttons
    /// are swapped).
    pub fn new(primary: Button) -> Router<S> {
        Router {
            primary,
            phase: Phase::Idle,
            restore: None,
            remembered: None,
            hover: Hover::default(),
            lookups: Vec::new(),
            last_up: None,
            phase_wake: None,
            restore_wake: None,
            hover_wake: None,
            state_changed: false,
            first_down: None,
        }
    }

    /// A double-click is being handled or a delivery runs (`DropRouter.busy`).
    pub fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// A screenshot of ours may still be on the clipboard, waiting to be replaced by the
    /// user's clipboard.
    pub fn restore_pending(&self) -> bool {
        self.restore.as_ref().is_some_and(|r| r.stage.is_some())
    }

    /// When the router must run again; None waits for the next input.
    pub fn next_deadline(&self) -> Option<f64> {
        let lookups = self.lookups.iter().map(|p| p.give_up_at).reduce(f64::min);
        earlier(
            earlier(self.phase_wake, self.restore_wake),
            earlier(self.hover_wake, lookups),
        )
    }

    /// True once after the router changed the `FairyState` (redraw).
    pub fn take_state_changed(&mut self) -> bool {
        std::mem::take(&mut self.state_changed)
    }

    // MARK: Inputs

    /// A hook event. Injected events (ours or another tool's) are not the user's clicks.
    pub fn mouse<E: Env<Snap = S>>(
        &mut self,
        event: MouseEvent,
        state: &mut FairyState,
        env: &mut E,
    ) {
        match event {
            MouseEvent::Move {
                x,
                y,
                time,
                injected: false,
            } => {
                if state.is_holding() && !state.hidden_for_capture() {
                    self.hover.point = Some((x, y));
                    self.hover.moved_at = time;
                    self.hover.resolved = false;
                }
                self.run(time, state, env);
            }
            MouseEvent::Down {
                button,
                x,
                y,
                time,
                click_count,
                injected: false,
                window,
                touch,
            } => {
                // Any later press stops what is in progress (A1): the user moved on.
                if matches!(button, Button::Left | Button::Right) && self.busy() {
                    self.cancel("another press", time, env);
                }
                if button != self.primary || touch {
                    self.run(time, state, env);
                    return;
                }
                if click_count == 1 {
                    self.first_down = Some((time, window));
                }
                if !state.is_holding() {
                    return;
                }
                match click_count {
                    1 => self.remember_clipboard((x, y), time, env),
                    2 => self.double_click((x, y), time, window, state, env),
                    _ => {}
                }
                self.run(time, state, env);
            }
            MouseEvent::Up {
                button,
                time,
                injected: false,
                ..
            } if button == self.primary => {
                self.last_up = Some(time);
                self.run(time, state, env);
            }
            _ => {}
        }
    }

    /// A UI Automation answer.
    pub fn lookup_answered<E: Env<Snap = S>>(
        &mut self,
        id: u64,
        outcome: Outcome,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let Some(point) = self.lookups.iter().find(|p| p.id == id).map(|p| p.point) else {
            return;
        };
        self.seen(id, seen_from_outcome(outcome, point), now, state, env);
    }

    /// A UI Automation answer as facts.
    pub fn seen<E: Env<Snap = S>>(
        &mut self,
        id: u64,
        seen: Seen,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let Some(index) = self.lookups.iter().position(|p| p.id == id) else {
            return;
        };
        let pending = self.lookups.remove(index);
        // An answer is only ever about the point its own request asked for.
        let seen = match seen {
            Seen::Spot(spot) if spot.point.is_some_and(|p| p != pending.point) => {
                Seen::Failed(format!(
                    "answer about {:?}, asked about {:?}",
                    spot.point, pending.point
                ))
            }
            seen => seen,
        };
        self.answer(id, seen, now, state, env);
        self.run(now, state, env);
    }

    pub fn clip_reply<E: Env<Snap = S>>(
        &mut self,
        reply: ClipReply<S>,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        self.on_clip_reply(reply, now, state, env);
        self.run(now, state, env);
    }

    /// The PNG for a terminal paste was written (or not).
    pub fn saved<E: Env<Snap = S>>(
        &mut self,
        job: u64,
        result: Result<PathBuf, String>,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        if let Phase::Delivering(d) = &mut self.phase
            && matches!(d.job.payload, Pay::Saving(j) if j == job)
        {
            match result {
                Ok(path) => {
                    env.log(now, &format!("saved {}", path.display()));
                    d.job.payload = Pay::Ready(Payload::Text(quoted_path(&path)));
                    d.job.is_path = true;
                }
                Err(why) => {
                    env.log(now, &format!("saving the PNG failed: {why}"));
                    d.job.payload = Pay::Failed;
                }
            }
        }
        self.run(now, state, env);
    }

    /// The clipboard facts changed (the clipboard thread's wake). Cheap when idle.
    pub fn clipboard_changed<E: Env<Snap = S>>(
        &mut self,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        if self.busy() || self.restore.is_some() {
            self.run(now, state, env);
        }
    }

    pub fn tick<E: Env<Snap = S>>(&mut self, now: f64, state: &mut FairyState, env: &mut E) {
        let expired: Vec<u64> = self
            .lookups
            .iter()
            .filter(|p| p.give_up_at <= now)
            .map(|p| p.id)
            .collect();
        self.lookups.retain(|p| p.give_up_at > now);
        for id in expired {
            self.answer(id, Seen::Failed("no answer".into()), now, state, env);
        }
        self.run(now, state, env);
    }

    /// The note took these cards: they leave with its hint. Also after the wait was given up
    /// or dropped: the cards are in the note by then, so the pile lets go of them.
    pub fn note_took<E: Env<Snap = S>>(
        &mut self,
        ids: &[CardId],
        hint: &str,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        if matches!(self.phase, Phase::Note { .. }) {
            self.phase = Phase::Idle;
            self.phase_wake = None;
        }
        env.log(now, &format!("the note took {} card(s) → {hint}", ids.len()));
        state.deliver(ids, hint, now);
        self.state_changed = true;
        self.run(now, state, env);
    }

    /// The notes thread reported an error. While a note is being given the cards, the wait
    /// ends and the fairy keeps them; returns true then.
    pub fn note_failed<E: Env<Snap = S>>(
        &mut self,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) -> bool {
        if !matches!(self.phase, Phase::Note { .. }) {
            return false;
        }
        self.phase = Phase::Idle;
        self.phase_wake = None;
        env.log(now, "the note could not add the cards → keep them");
        state.reject(HINT_NOTE_FAILED, now);
        self.state_changed = true;
        self.run(now, state, env);
        true
    }

    // MARK: Quit

    /// The app quits: stop any delivery and put the user's clipboard back if ours is still
    /// there. Returns a clipboard ticket to wait for (a stage still in flight, or the
    /// restore); feed its reply to [`Router::quit_reply`], which returns the next one.
    pub fn quit<E: Env<Snap = S>>(&mut self, now: f64, env: &mut E) -> Option<u64> {
        match &self.phase {
            Phase::Saving(f) if matches!(f.step, SaveStep::Writing { .. }) => env.log(
                now,
                "quit: the file being written is finished by the saver; no more cards are saved",
            ),
            Phase::Idle => {}
            _ => env.log(now, "quit: the delivery stops here"),
        }
        self.phase = Phase::Idle;
        if let Some(restore) = self.restore.as_mut() {
            restore.now_please = true;
            restore.retry_at = None;
        }
        self.quit_step(now, env)
    }

    /// A clipboard reply during quit. Returns the next ticket to wait for.
    pub fn quit_reply<E: Env<Snap = S>>(
        &mut self,
        reply: ClipReply<S>,
        now: f64,
        env: &mut E,
    ) -> Option<u64> {
        let restore = self.restore.as_mut()?;
        if let Some(stage) = restore.stage.as_mut()
            && stage.ticket == reply.ticket
            && !stage.confirmed
            && stage.emptied.is_none()
        {
            match reply.result {
                Ok(ClipDone::Staged { .. }) => stage.confirmed = true,
                Err(ClipFail::Emptied { sequence }) => stage.emptied = Some(sequence),
                _ => restore.stage = restore.prev.take(),
            }
        } else if restore.restoring.is_some_and(|(t, _)| t == reply.ticket) {
            restore.restoring = None;
            match reply.result {
                Ok(_) => {
                    env.log(now, "quit: clipboard restored");
                    self.restore = None;
                    return None;
                }
                Err(ClipFail::Retry(why)) => {
                    env.log(now, &format!("quit: restore refused ({why}), again"))
                }
                Err(ClipFail::Partial) => {
                    restore.partial = true;
                    env.log(now, "quit: partial restore, again");
                }
                Err(why) => {
                    env.log(now, &format!("quit: restore refused ({why:?}) → leave it"));
                    self.restore = None;
                    return None;
                }
            }
        }
        self.quit_step(now, env)
    }

    fn quit_step<E: Env<Snap = S>>(&mut self, now: f64, env: &mut E) -> Option<u64> {
        let restore = self.restore.as_mut()?;
        if let Some((ticket, _)) = restore.restoring {
            return Some(ticket);
        }
        let Some(stage) = restore.stage else {
            self.restore = None;
            return None;
        };
        if !stage.confirmed && stage.emptied.is_none() {
            // The rollback runs after the stage on the clipboard thread.
            env.log(
                now,
                "quit: a stage is in flight → rollback queued behind it",
            );
            return match env.clip_rollback(RestoreGuard::OwnStaging) {
                Ok(ticket) => {
                    restore.restoring = Some((ticket, now));
                    Some(ticket)
                }
                Err(_) => {
                    self.restore = None;
                    None
                }
            };
        }
        if restore.user_copied {
            self.restore = None;
            return None;
        }
        let facts = env.clip_facts(now);
        let Some(guard) = restore_guard(&stage, &facts, restore.partial) else {
            env.log(
                now,
                "quit: the clipboard changed since the paste → leave it",
            );
            self.restore = None;
            return None;
        };
        match env.clip_rollback(guard) {
            Ok(ticket) => {
                env.log(now, &format!("quit: restoring the clipboard ({guard:?})"));
                restore.restoring = Some((ticket, now));
                Some(ticket)
            }
            Err(why) => {
                env.log(now, &format!("quit: restore failed: {why}"));
                self.restore = None;
                None
            }
        }
    }

    // MARK: Double-click

    /// `rememberClipboard`, on the first mouse-down. Only the sequence number and the time
    /// are kept: a snapshot here would make apps render large formats on every plain click.
    ///
    /// The exception is a click in a terminal. With copy-on-select, the terminal writes the
    /// double-clicked word on the second mouse-up, after the router's snapshot (taken once
    /// the double-click is decided, at the second mouse-down), so that case needs nothing.
    /// But a selection made by an earlier click (and some terminals copy on the first
    /// mouse-up) can replace the clipboard before the double-click is decided; then the
    /// copy taken here is the user's clipboard.
    fn remember_clipboard<E: Env<Snap = S>>(&mut self, point: (i32, i32), now: f64, env: &mut E) {
        if self.restore.is_some() || self.busy() {
            return;
        }
        let sequence = env.clip_facts(now).sequence;
        if let Some(memory) = self.remembered.as_mut() {
            let same = match &memory.rem {
                Some(Rem::Ready { sequence: s, .. }) => *s == sequence,
                Some(Rem::Waiting(_)) => memory.sequence == sequence,
                _ => false,
            };
            if same {
                memory.at = now;
                return;
            }
        }
        let rem = if env.terminal_at(point) {
            match env.clip_snapshot() {
                Ok(ticket) => Some(Rem::Waiting(ticket)),
                Err(why) => {
                    env.log(now, &format!("remember clipboard: {why}"));
                    None
                }
            }
        } else {
            None
        };
        self.remembered = Some(Remembered {
            sequence,
            at: now,
            rem,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn request(
        &mut self,
        point: (i32, i32),
        want_focus: bool,
        probe: bool,
        deadline: f64,
        now: f64,
        env: &mut impl Env,
    ) -> u64 {
        let id = env.lookup(point, want_focus, probe, deadline);
        self.lookups.push(Pending {
            id,
            point,
            give_up_at: now + deadline + LOOKUP_GRACE,
        });
        id
    }

    /// The user pressed again (or typed): drop a pending gesture; stop a delivery before its
    /// next batch, quietly, keeping the cards.
    fn cancel<E: Env<Snap = S>>(&mut self, why: &'static str, now: f64, env: &mut E) {
        match &mut self.phase {
            Phase::Idle => {}
            Phase::Saving(f) if f.writing() => {
                env.log(
                    now,
                    &format!("{why} → the save goes on (a file is being written)"),
                );
            }
            Phase::Saving(_) => {
                env.log(now, &format!("{why} → the save is dropped"));
                self.phase = Phase::Idle;
                self.phase_wake = None;
            }
            Phase::Delivering(d) => {
                if d.cancelled.is_none() {
                    env.log(now, &format!("{why} → the delivery stops"));
                    d.cancelled = Some(why);
                }
            }
            _ => {
                env.log(now, &format!("{why} → the gesture is dropped"));
                self.phase = Phase::Idle;
                self.phase_wake = None;
            }
        }
    }

    fn double_click<E: Env<Snap = S>>(
        &mut self,
        point: (i32, i32),
        now: f64,
        window: isize,
        state: &mut FairyState,
        env: &mut E,
    ) {
        // A2: both presses on the same top-level window, known.
        let first_at = match self.first_down {
            Some((at, first)) if first == window && window != 0 => at,
            other => {
                env.log(
                    now,
                    &format!(
                        "double-click at {point:?}: presses on windows {:#x} and {window:#x} → Ignore",
                        other.map_or(0, |(_, w)| w)
                    ),
                );
                return;
            }
        };
        // B5: no new gesture while an earlier stage has not answered.
        let unanswered = self
            .restore
            .as_ref()
            .and_then(|r| r.stage)
            .is_some_and(|s| !s.confirmed && s.emptied.is_none());
        if unanswered {
            env.log(
                now,
                &format!("double-click at {point:?}: an earlier stage has not answered → Ignore"),
            );
            return;
        }
        if self.busy() || state.hidden_for_capture() {
            let facts = DoubleClick {
                is_holding: true,
                hidden_for_capture: state.hidden_for_capture(),
                busy: self.busy(),
                ..DoubleClick::default()
            };
            let action = drop_policy::decide(&facts);
            env.log(
                now,
                &format!(
                    "double-click at {point:?}: busy={} hidden={} → {action:?}",
                    yes(self.busy()),
                    yes(state.hidden_for_capture())
                ),
            );
            return;
        }
        // The hover hint must not look meanwhile.
        self.hover.in_flight = None;
        // A note takes the pile itself: no lookup, nothing pasted into what is behind it.
        if env.note_at(point) {
            let cards = state.deliverable();
            env.log(
                now,
                &format!(
                    "double-click at {point:?}: over a note → give it {} card(s)",
                    cards.len()
                ),
            );
            env.give_to_note(point, cards);
            self.phase = Phase::Note {
                until: now + NOTE_TIMEOUT,
            };
            return;
        }
        let id = self.request(point, false, false, LOOKUP_DEADLINE, now, env);
        env.log(
            now,
            &format!("double-click at {point:?}: looking (lookup {id})"),
        );
        self.phase = Phase::Looking {
            id,
            click: Click {
                point,
                at: now,
                window,
                first_at,
                keys: env.key_presses(),
            },
        };
    }

    fn answer<E: Env<Snap = S>>(
        &mut self,
        id: u64,
        seen: Seen,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        if self.hover.in_flight == Some(id) {
            self.hover.in_flight = None;
            self.apply_hover(&seen, now, state);
            return;
        }
        if let Phase::Delivering(d) = &mut self.phase {
            if d.job.step == Step::Focus(id) {
                self.focus_answer(seen, now, state, env);
                return;
            }
            if d.probe.after_req == Some(id) {
                d.probe.after_req = None;
                if let Seen::Spot(spot) = &seen
                    && let (Some(after), Some(before)) = (spot.probe, d.probe.before)
                    && spot
                        .focused
                        .is_some_and(|f| f.pid == d.aim.pid || f.pid == d.aim.element_pid)
                {
                    d.probe.answered = true;
                    if after.changed_from(&before) {
                        d.probe.changed = true;
                        env.log(now, "probe: the field changed");
                    }
                }
                return;
            }
        }
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Looking { id: want, click }
            | Phase::Recheck {
                id: want, click, ..
            } if want == id && env.key_presses() != click.keys => {
                env.log(now, "the user typed → the gesture is dropped");
            }
            Phase::Looking { id: want, click } if want == id => {
                self.decide(click, seen, now, state, env);
            }
            Phase::Recheck {
                id: want,
                click,
                first,
            } if want == id => {
                self.after_up(click, first, seen, now, state, env);
            }
            other => self.phase = other,
        }
    }

    /// The double-click's facts → `drop_policy::decide` → act.
    fn decide<E: Env<Snap = S>>(
        &mut self,
        click: Click,
        seen: Seen,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let mut facts = DoubleClick {
            is_holding: state.is_holding(),
            hidden_for_capture: state.hidden_for_capture(),
            busy: false,
            click_count: 2,
            ..DoubleClick::default()
        };
        let mut first = None;
        let mut surface = Surface::Field;
        let what = match &seen {
            Seen::Spot(spot) if spot.target.window != click.window => {
                env.log(
                    now,
                    &format!(
                        "double-click at {:?}: the answer is about window {:#x}, the presses were on {:#x} → Ignore",
                        click.point, spot.target.window, click.window
                    ),
                );
                return;
            }
            Seen::Spot(spot) => {
                if spot.activating {
                    env.log(
                        now,
                        &format!(
                            "double-click at {:?}: {} is window chrome or a control → Ignore",
                            click.point,
                            spot.describe()
                        ),
                    );
                    return;
                }
                facts.is_finder = spot.is_shell();
                facts.finder_drop_spot = spot.drop_spot;
                facts.editable_under_cursor = spot.editable_hit;
                first = Some(First {
                    aim: Aim {
                        chromium: spot.chromium,
                        ..Aim::new(&spot.target, spot.pid, click.point)
                    },
                    pid: Some(spot.pid),
                    before: spot.canvas,
                    chromium: spot.chromium,
                    slow: false,
                });
                if spot.target.is_terminal || spot.web_terminal {
                    surface = Surface::Terminal;
                }
                format!(
                    "{} editable={} terminal={} shell={:?} drop-spot={} canvas={}",
                    spot.describe(),
                    yes(spot.editable_hit),
                    yes(spot.target.is_terminal || spot.web_terminal),
                    spot.target.shell,
                    yes(spot.drop_spot),
                    yes(spot.canvas.canvas),
                )
            }
            Seen::OwnProcess => {
                facts.target_is_own_process = true;
                "our own window".into()
            }
            Seen::Elevated { pid } => {
                env.log(
                    now,
                    &format!("double-click over an elevated window (pid {pid}) → refuse"),
                );
                state.reject(HINT_ELEVATED, now);
                self.state_changed = true;
                return;
            }
            Seen::TimedOut { target } if target.window != click.window => {
                env.log(
                    now,
                    &format!(
                        "double-click at {:?}: no answer, and window {:#x} is not the pressed {:#x} → Ignore",
                        click.point, target.window, click.window
                    ),
                );
                return;
            }
            Seen::TimedOut { target }
                if target.is_terminal
                    && !(silent_terminal_pasteable(target)
                        && env.point_in_client(target.window, click.point)) =>
            {
                // A4: without an answer the text area cannot be told from a tab or the
                // title bar: no path is typed blind.
                env.log(
                    now,
                    &format!(
                        "double-click at {:?}: {} did not answer and the point may be a tab or the title bar → refuse",
                        click.point,
                        target.exe.as_deref().unwrap_or("?")
                    ),
                );
                state.reject(HINT_SLOW, now);
                self.state_changed = true;
                return;
            }
            Seen::TimedOut { target } => {
                // A terminal too busy to answer is still a terminal (`silentTerminal`),
                // when the point is its text area.
                facts.silent_terminal = target.is_terminal;
                facts.is_finder = target.shell.is_some();
                first = Some(First {
                    aim: Aim::new(target, target.pid, click.point),
                    pid: None,
                    before: CanvasFacts::default(),
                    chromium: false,
                    slow: true,
                });
                surface = Surface::Terminal;
                format!(
                    "no answer in time from {} pid {} window {:#x} terminal={}",
                    target.exe.as_deref().unwrap_or("?"),
                    target.pid,
                    target.window,
                    yes(target.is_terminal)
                )
            }
            Seen::Failed(why) => {
                // Nothing is known about the place: never act on it.
                env.log(
                    now,
                    &format!(
                        "double-click at {:?}: lookup failed ({why}) → Ignore",
                        click.point
                    ),
                );
                return;
            }
        };
        let action = drop_policy::decide(&facts);
        let chosen = if action == Action::Paste {
            format!(" surface={surface:?}")
        } else {
            String::new()
        };
        env.log(
            now,
            &format!(
                "double-click at {:?}: {what} → {action:?}{chosen}",
                click.point
            ),
        );
        match (action, first) {
            (Action::SaveToFinder, _) => {
                if let Seen::Spot(spot) = &seen {
                    self.save_to_folder(spot, click, now, state, env);
                }
            }
            (Action::Paste, Some(first)) => {
                self.start_delivery(first.aim, surface, false, click, click.at, now, state, env);
            }
            (Action::CheckFocusAfterMouseUp, Some(first)) => {
                self.phase = Phase::AwaitUp {
                    click,
                    first,
                    ready_at: None,
                };
            }
            _ => {}
        }
    }

    /// SaveToFinder (`DropRouter.saveToFinder`): ask Explorer for the folder of the
    /// double-clicked window, or the desktop's. The rest runs in [`Router::save_step`].
    fn save_to_folder<E: Env<Snap = S>>(
        &mut self,
        spot: &Spot,
        click: Click,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let ids: Vec<CardId> = state.deliverable().iter().map(|c| c.id).collect();
        if ids.is_empty() {
            return;
        }
        let request = match spot.target.shell {
            Some(Shell::Desktop) => FolderRequest::Desktop,
            _ => FolderRequest::Window(spot.target.window),
        };
        let id = env.folder_lookup(request, FOLDER_DEADLINE);
        env.log(
            now,
            &format!(
                "save {} card(s): asking Explorer for the folder of {request:?} (request {id}, {FOLDER_DEADLINE:.2}s)",
                ids.len()
            ),
        );
        self.phase = Phase::Saving(Box::new(FolderSave {
            click,
            window: spot.target.window,
            ids,
            index: 0,
            folder: None,
            saved: Vec::new(),
            already: 0,
            started: false,
            step: SaveStep::Asking { id, since: now },
        }));
    }

    /// Explorer answered a folder lookup.
    pub fn folder_answered<E: Env<Snap = S>>(
        &mut self,
        id: u64,
        outcome: FolderOutcome,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let Phase::Saving(f) = &mut self.phase else {
            return;
        };
        let SaveStep::Asking { id: want, since } = f.step else {
            return;
        };
        if want != id {
            return;
        }
        let took = (now - since) * 1000.0;
        let refusal = match outcome {
            FolderOutcome::Folder(folder) => {
                env.log(
                    now,
                    &format!("folder (request {id}, {took:.0} ms): {}", folder.display()),
                );
                f.folder = Some(folder);
                f.step = SaveStep::Ready;
                None
            }
            FolderOutcome::NotAFolder { display_name } => {
                env.log(
                    now,
                    &format!("folder (request {id}, {took:.0} ms): {display_name:?} is not a folder → refuse"),
                );
                Some(not_a_folder_hint(&display_name))
            }
            FolderOutcome::TimedOut => {
                env.log(
                    now,
                    &format!("folder (request {id}, {took:.0} ms): Explorer did not answer in time → refuse"),
                );
                Some(HINT_EXPLORER_SLOW.to_string())
            }
            FolderOutcome::Failed(why) => {
                env.log(
                    now,
                    &format!("folder (request {id}, {took:.0} ms): failed ({why}) → refuse"),
                );
                Some(HINT_SAVE_FAILED.to_string())
            }
        };
        if let Some(hint) = refusal {
            self.phase = Phase::Idle;
            self.phase_wake = None;
            state.reject(&hint, now);
            self.state_changed = true;
        }
        self.run(now, state, env);
    }

    /// The saver wrote a card into the folder (or not).
    pub fn folder_saved<E: Env<Snap = S>>(
        &mut self,
        job: u64,
        result: Result<PathBuf, SaveFail>,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let Phase::Saving(mut f) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            return;
        };
        if !matches!(f.step, SaveStep::Writing { job: j, .. } if j == job) {
            self.phase = Phase::Saving(f);
            return;
        }
        let id = f.ids[f.index];
        match result {
            Ok(path) => {
                env.log(now, &format!("card {}: saved {}", id.0, path.display()));
                f.saved.push((id, path));
                f.index += 1;
                f.step = SaveStep::Ready;
                self.phase = Phase::Saving(f);
            }
            Err(fail) => {
                env.log(
                    now,
                    &format!(
                        "card {}: save failed ({:?}): {}",
                        id.0, fail.problem, fail.why
                    ),
                );
                if let Flow::Wait(at) = self.end_save(&mut f, Some(fail.problem), now, state, env) {
                    self.phase = Phase::Saving(f);
                    self.phase_wake = at;
                }
            }
        }
        self.run(now, state, env);
    }

    /// One step of a folder save; see the module comment.
    fn save_step<E: Env<Snap = S>>(
        &mut self,
        f: &mut FolderSave,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) -> Flow {
        if !f.writing() && env.key_presses() != f.click.keys {
            env.log(now, "the user typed → the save is dropped");
            return Flow::Done;
        }
        match f.step.clone() {
            SaveStep::Asking { since, .. } => {
                // The explorer controller answers at the deadline; this is the backstop.
                let give_up = since + FOLDER_DEADLINE + LOOKUP_GRACE;
                if now >= give_up {
                    env.log(now, "folder: no answer at all → refuse");
                    state.reject(HINT_EXPLORER_SLOW, now);
                    self.state_changed = true;
                    return Flow::Done;
                }
                Flow::Wait(Some(give_up))
            }
            SaveStep::Ready => {
                if f.index == 0 && env.primary_down() {
                    let up_deadline = f.click.at + paste_policy::MOUSE_UP_TIMEOUT;
                    if now >= up_deadline {
                        env.log(now, "the button is still down (a drag) → no save");
                        return Flow::Done;
                    }
                    return Flow::Wait(Some(
                        up_deadline.min(now + paste_policy::MOUSE_UP_POLL_INTERVAL),
                    ));
                }
                let Some(folder) = f.folder.clone() else {
                    return Flow::Done;
                };
                while let Some(&id) = f.ids.get(f.index) {
                    let Some(card) = state.card(id).cloned() else {
                        env.log(
                            now,
                            &format!("card {} left the pile meanwhile → skipped", id.0),
                        );
                        f.index += 1;
                        continue;
                    };
                    let position = format!("card {} ({}/{})", id.0, f.index + 1, f.ids.len());
                    if let Some(movie) = &card.movie
                        && same_folder(movie.parent(), &folder)
                    {
                        env.log(
                            now,
                            &format!("{position}: the recording already lives there → not copied"),
                        );
                        f.saved.push((id, movie.clone()));
                        f.already += 1;
                        f.index += 1;
                        continue;
                    }
                    let name = match &card.movie {
                        Some(movie) => format!(
                            "a copy of {}",
                            movie.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        None => saver::screenshot_file_name(card.captured_at),
                    };
                    return match env.save_to_folder(&card, &folder) {
                        Ok(job) => {
                            env.log(
                                now,
                                &format!(
                                    "{position}: writing {name} into {} (job {job})",
                                    folder.display()
                                ),
                            );
                            f.started = true;
                            f.step = SaveStep::Writing { job, since: now };
                            Flow::Wait(Some(now + SAVE_TIMEOUT))
                        }
                        Err(why) => {
                            env.log(
                                now,
                                &format!("{position}: the saver did not take it ({why})"),
                            );
                            self.end_save(f, Some(SaveProblem::Other), now, state, env)
                        }
                    };
                }
                self.end_save(f, None, now, state, env)
            }
            SaveStep::Writing { since, .. } => {
                let give_up = since + SAVE_TIMEOUT;
                if now >= give_up {
                    env.log(now, "the file was not written in time → stop");
                    return self.end_save(f, Some(SaveProblem::Other), now, state, env);
                }
                Flow::Wait(Some(give_up))
            }
            SaveStep::LateHint { at, hint } => {
                if now < at {
                    return Flow::Wait(Some(at));
                }
                state.reject(hint, now);
                self.state_changed = true;
                Flow::Done
            }
        }
    }

    /// The writes are over (all, or up to a failure): deliver what was saved, reveal the
    /// last file, and say what failed.
    fn end_save<E: Env<Snap = S>>(
        &mut self,
        f: &mut FolderSave,
        failed: Option<SaveProblem>,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) -> Flow {
        let hint = failed.map(SaveProblem::hint);
        let Some(folder) = f.folder.clone() else {
            return Flow::Done;
        };
        if f.saved.is_empty() {
            match hint {
                Some(hint) => {
                    env.log(now, &format!("nothing saved → {hint}"));
                    state.reject(hint, now);
                    self.state_changed = true;
                }
                None => env.log(now, "nothing left to save"),
            }
            return Flow::Done;
        }
        let ids: Vec<CardId> = f.saved.iter().map(|(id, _)| *id).collect();
        let text = if f.already == f.saved.len() {
            HINT_ALREADY_SAVED.to_string()
        } else {
            saved_hint(f.saved.len(), &folder)
        };
        env.log(now, &format!("delivered {} card(s) → {text}", ids.len()));
        state.deliver(&ids, &text, now);
        self.state_changed = true;
        if let Some((_, last)) = f.saved.last() {
            env.log(
                now,
                &format!("reveal {} in window {:#x}", last.display(), f.window),
            );
            env.reveal(f.window, last);
        }
        match hint {
            Some(hint) => {
                let at = now + SAVE_FAIL_STEP * f.saved.len() as f64;
                env.log(now, &format!("the rest stays → {hint} at {at:.3}"));
                f.step = SaveStep::LateHint { at, hint };
                Flow::Wait(Some(at))
            }
            None => Flow::Done,
        }
    }

    /// `resolveAfterMouseUp`.
    fn after_up<E: Env<Snap = S>>(
        &mut self,
        click: Click,
        first: First,
        seen: Seen,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let spot = match seen {
            Seen::Spot(spot) => spot,
            Seen::OwnProcess => {
                env.log(now, "after mouse-up: our own window → ignore");
                return;
            }
            Seen::Elevated { pid } => {
                env.log(
                    now,
                    &format!("after mouse-up: elevated window (pid {pid}) → refuse"),
                );
                state.reject(HINT_ELEVATED, now);
                self.state_changed = true;
                return;
            }
            Seen::TimedOut { target } => {
                // Never paste blind into a window we know nothing about.
                env.log(
                    now,
                    &format!(
                        "after mouse-up: {} did not answer again → refuse",
                        target.exe.as_deref().unwrap_or("?")
                    ),
                );
                state.reject(HINT_SLOW, now);
                self.state_changed = true;
                return;
            }
            Seen::Failed(why) => {
                env.log(
                    now,
                    &format!("after mouse-up: lookup failed ({why}) → ignore"),
                );
                return;
            }
        };
        // A3: the re-check found window chrome or a control (a canvas first, a button now).
        if spot.activating {
            env.log(
                now,
                &format!(
                    "after mouse-up: {} is window chrome or a control → ignore",
                    spot.describe()
                ),
            );
            return;
        }
        // The double-click opened or switched something (a file from an Open dialog, a
        // maximised window): what is under the point now is not what was clicked.
        if spot.target.window != first.aim.window {
            env.log(
                now,
                &format!(
                    "after mouse-up: window {:#x} is not the double-clicked {:#x} → ignore",
                    spot.target.window, first.aim.window
                ),
            );
            return;
        }
        let confirmed = if first.slow {
            // Nothing was known at the double-click: only an editable element at the click
            // point itself counts, never a focused field somewhere else.
            if spot.editable_hit && !spot.activating {
                Confirmed::Paste {
                    surface: spot.surface(),
                    dismiss_editor: false,
                }
            } else {
                Confirmed::Ignore
            }
        } else {
            confirm_after_up(&spot, click.point, first.pid, first.before)
        };
        env.log(
            now,
            &format!(
                "after mouse-up: {} editable={} canvas={} (before: {}) → {confirmed:?}",
                spot.describe(),
                yes(spot.editable),
                yes(spot.canvas_here().canvas),
                yes(first.before.canvas),
            ),
        );
        let Confirmed::Paste {
            surface,
            dismiss_editor,
        } = confirmed
        else {
            return;
        };
        if !state.is_holding() || state.hidden_for_capture() {
            return;
        }
        let aim = Aim {
            element_pid: spot.pid,
            chromium: first.aim.chromium || spot.chromium,
            ..first.aim
        };
        // The focus check after the release compares against this element's process too.
        self.start_delivery(aim, surface, dismiss_editor, click, now, now, state, env);
    }

    /// The focus lookup after the release answered. No key goes out unless the target field
    /// has the keyboard focus ([`focus_evidence`]); a lookup that did not answer is no
    /// evidence.
    fn focus_answer<E: Env<Snap = S>>(
        &mut self,
        seen: Seen,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let Phase::Delivering(mut d) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            return;
        };
        let refusal: Option<(Option<&str>, String)> = match &seen {
            Seen::Spot(spot) => {
                let pids = (d.aim.pid, d.aim.element_pid);
                // A later card of a pile: the first card's paste moved the layout (the
                // composer grew), so the focus must still be in the target's text input, but
                // not necessarily under the double-click point.
                let surface = if d.job.first {
                    d.surface
                } else {
                    Surface::Document
                };
                if focus_evidence(spot, pids, d.aim.window, d.aim.point, surface) {
                    d.probe.verify =
                        d.surface == Surface::Field && spot.focused.is_some_and(|f| f.single_line);
                    d.probe.before = spot.probe;
                    d.probe.can_tell = spot
                        .probe
                        .is_some_and(|p| p.value.is_some() || p.images.is_some());
                    env.log(
                        now,
                        &format!(
                            "focus: {} → in the target{}",
                            spot.describe(),
                            match (d.probe.before.is_some(), d.probe.verify) {
                                (true, true) => ", probe taken (verify)",
                                (true, false) => ", probe taken (standby)",
                                _ => "",
                            }
                        ),
                    );
                    None
                } else {
                    Some((
                        Some(HINT_NOT_FOCUSED),
                        format!("focus is not in the target field ({})", spot.describe()),
                    ))
                }
            }
            Seen::OwnProcess => Some((None, "focus is in our own window".into())),
            Seen::Elevated { pid } => Some((
                Some(HINT_ELEVATED),
                format!("focus check: elevated window (pid {pid})"),
            )),
            Seen::TimedOut { .. } => Some((Some(HINT_SLOW), "focus check timed out".into())),
            Seen::Failed(why) => Some((Some(HINT_SLOW), format!("focus check failed: {why}"))),
        };
        match refusal {
            None => {
                d.job.step = Step::Front(now);
                self.phase = Phase::Delivering(d);
            }
            Some((hint, why)) => {
                let _ = self.end(&d, hint, &why, now, state, env);
                self.finish_delivery(now, env);
            }
        }
    }

    // MARK: Delivery

    /// `deliverPaste`: the whole pile, top card first.
    #[allow(clippy::too_many_arguments)]
    fn start_delivery<E: Env<Snap = S>>(
        &mut self,
        aim: Aim,
        surface: Surface,
        dismiss_editor: bool,
        click: Click,
        clicked_at: f64,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let ids: Vec<CardId> = state.deliverable().iter().map(|c| c.id).collect();
        let Some(&first) = ids.first() else {
            return;
        };
        let terminal = surface == Surface::Terminal;
        // The user's clipboard (`currentClipboard`).
        let clip = if let Some(restore) = self.restore.as_mut() {
            // An earlier paste still has a screenshot on the clipboard: its copy of the
            // user's clipboard is the real one. Its restore is taken over.
            restore.restoring = None;
            restore.now_please = false;
            restore.first_try = None;
            restore.retry_at = None;
            env.log(now, "clipboard: taking over the pending restore");
            None
        } else {
            match self.take_remembered(now, env) {
                Ok(clip) => clip,
                Err(()) => {
                    state.reject(HINT_CLIPBOARD, now);
                    self.state_changed = true;
                    return;
                }
            }
        };
        let mut delivery = Delivery {
            ids,
            index: 0,
            aim,
            terminal,
            surface,
            dismiss_editor,
            clip,
            job: Job {
                id: first,
                first: true,
                movie: false,
                started: clicked_at,
                payload: Pay::Failed,
                is_path: false,
                step: Step::Prepare,
            },
            probe: ProbeState::default(),
            click,
            focus: None,
            cancelled: None,
        };
        env.log(
            now,
            &format!(
                "deliver {} card(s) to pid {} window {:#x} as {surface:?} with {:?}{}",
                delivery.ids.len(),
                aim.pid,
                aim.window,
                aim.keys,
                if dismiss_editor {
                    " (close the text box first)"
                } else {
                    ""
                }
            ),
        );
        if !self.load_job(&mut delivery, first, true, clicked_at, state, env, now) {
            return;
        }
        self.phase = Phase::Delivering(Box::new(delivery));
    }

    /// The copy remembered at the first click if `use_remembered_clipboard` says so, else a
    /// fresh snapshot. Ok(None): ready in `self.restore`. Ok(Some(ticket)): a reply to wait
    /// for.
    fn take_remembered<E: Env<Snap = S>>(
        &mut self,
        now: f64,
        env: &mut E,
    ) -> Result<Option<u64>, ()> {
        let current = env.clip_facts(now).sequence;
        if let Some(memory) = self.remembered.take() {
            let sequence = match &memory.rem {
                Some(Rem::Ready { sequence, .. }) => Some(*sequence),
                Some(Rem::Waiting(_)) => Some(memory.sequence),
                _ => None,
            };
            let age = now - memory.at;
            if let Some(sequence) = sequence
                && paste_policy::use_remembered_clipboard(Some(sequence), current, Some(age))
            {
                env.log(
                    now,
                    &format!("clipboard: using the copy from the first click (seq {sequence}, now {current}, {age:.2}s old)"),
                );
                match memory.rem {
                    Some(Rem::Ready {
                        snap,
                        lost,
                        sequence,
                    }) => {
                        if let Some(lost) = lost {
                            env.log(
                                now,
                                &format!("clipboard: the saved copy lost {lost} → do not paste"),
                            );
                            return Err(());
                        }
                        let mut restore = Restore::new(snap);
                        restore.expect = Some(sequence);
                        self.restore = Some(restore);
                        return Ok(None);
                    }
                    Some(Rem::Waiting(ticket)) => return Ok(Some(ticket)),
                    _ => {}
                }
            }
        }
        match env.clip_snapshot() {
            Ok(ticket) => {
                env.log(now, "clipboard: saving it now");
                Ok(Some(ticket))
            }
            Err(why) => {
                env.log(
                    now,
                    &format!("clipboard: cannot save it ({why}) → do not paste"),
                );
                Err(())
            }
        }
    }

    /// Sets up the job for card `id`. False when the card is gone.
    #[allow(clippy::too_many_arguments)]
    fn load_job<E: Env<Snap = S>>(
        &mut self,
        d: &mut Delivery,
        id: CardId,
        first: bool,
        started: f64,
        state: &mut FairyState,
        env: &mut E,
        now: f64,
    ) -> bool {
        let Some(card) = state.card(id).cloned() else {
            return false;
        };
        let (payload, is_path) = match (&card.movie, d.terminal) {
            (Some(movie), true) => (Pay::Ready(Payload::Text(quoted_path(movie))), true),
            (Some(movie), false) => (Pay::Ready(Payload::File(movie.clone())), false),
            (None, true) => (Pay::Saving(env.save_png(&card)), true),
            (None, false) => (Pay::Ready(Payload::Image(card.image.clone())), false),
        };
        env.log(
            now,
            &format!(
                "card {} ({}/{}): {}",
                id.0,
                d.index + 1,
                d.ids.len(),
                match (&payload, card.movie.is_some()) {
                    (Pay::Saving(_), _) => "saving a PNG for its path",
                    (_, true) if is_path => "recording as a path",
                    (_, true) => "recording as a file",
                    _ => "image",
                }
            ),
        );
        // Every card gets its own "before" fingerprint, right before its keys.
        d.probe = ProbeState::default();
        d.job = Job {
            id,
            first,
            movie: card.movie.is_some(),
            started,
            payload,
            is_path,
            step: Step::Prepare,
        };
        true
    }

    /// Ends the delivery. With a hint the card wobbles; without, it stays quietly.
    fn end<E: Env<Snap = S>>(
        &mut self,
        d: &Delivery,
        hint: Option<&str>,
        why: &str,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) -> Flow {
        env.log(
            now,
            &format!(
                "card {}: {why} → {}",
                d.job.id.0,
                hint.unwrap_or("(no hint)")
            ),
        );
        if let Some(restore) = self.restore.as_mut()
            && restore.stage.is_some_and(|s| s.sent_at.is_none())
        {
            restore.now_please = true;
        }
        if let Some(hint) = hint {
            state.reject(hint, now);
            self.state_changed = true;
        }
        Flow::Done
    }

    fn fail<E: Env<Snap = S>>(
        &mut self,
        d: &Delivery,
        hint: &str,
        why: &str,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) -> Flow {
        self.end(d, Some(hint), why, now, state, env)
    }

    fn fail_hint(d: &Delivery) -> &'static str {
        if d.job.first {
            HINT_PASTE_FAILED
        } else {
            HINT_REST_FAILED
        }
    }

    fn step<E: Env<Snap = S>>(
        &mut self,
        d: &mut Delivery,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) -> Flow {
        if !matches!(d.job.step, Step::Watch(_) | Step::Next(_)) {
            if env.key_presses() != d.click.keys && d.cancelled.is_none() {
                d.cancelled = Some("the user typed");
            }
            if let Some(why) = d.cancelled {
                return self.end(d, None, &format!("{why} → stopped"), now, state, env);
            }
        }
        match d.job.step.clone() {
            Step::Prepare => {
                let deadline = d.job.started + PREPARE_TIMEOUT;
                if d.clip.is_some() || matches!(d.job.payload, Pay::Saving(_)) {
                    if now >= deadline {
                        let hint = if d.clip.is_some() {
                            HINT_CLIPBOARD
                        } else {
                            Self::fail_hint(d)
                        };
                        return self.fail(d, hint, "not ready in time", now, state, env);
                    }
                    return Flow::Wait(Some(deadline));
                }
                if matches!(d.job.payload, Pay::Failed) {
                    return self.fail(d, Self::fail_hint(d), "no payload", now, state, env);
                }
                // The button must be up (focus and the selection are final). Still down after
                // the timeout means a drag (select by words, a touchpad double-tap-drag): the
                // gesture is not a drop. The real button state is read, since the hook does
                // not see a release over an elevated window.
                let up_deadline = d.job.started + paste_policy::MOUSE_UP_TIMEOUT;
                if env.primary_down() {
                    if now >= up_deadline {
                        return self.end(
                            d,
                            None,
                            "the button is still down (a drag)",
                            now,
                            state,
                            env,
                        );
                    }
                    return Flow::Wait(Some(
                        up_deadline.min(now + paste_policy::MOUSE_UP_POLL_INTERVAL),
                    ));
                }
                if d.terminal {
                    // Wait until the clicking is over and the terminal's copy-on-select landed.
                    let facts = env.clip_facts(now);
                    let since_up = self.last_up.map_or(f64::INFINITY, |t| now - t);
                    let since_change = facts.last_change.map(|t| now - t);
                    match TerminalSettle::default().step(
                        now - d.job.started,
                        false,
                        since_up,
                        since_change,
                    ) {
                        MouseUpStep::Wait => {
                            Flow::Wait(Some(now + paste_policy::MOUSE_UP_POLL_INTERVAL))
                        }
                        MouseUpStep::Proceed => {
                            d.job.step = Step::Front(now);
                            Flow::Again
                        }
                    }
                } else {
                    d.job.step = Step::AfterUp(now + paste_policy::AFTER_MOUSE_UP_DELAY);
                    Flow::Again
                }
            }
            Step::AfterUp(until) => {
                if now < until {
                    return Flow::Wait(Some(until));
                }
                if matches!(d.surface, Surface::Field | Surface::Document) {
                    // Evidence that the target field has the keyboard focus, and the probe's
                    // fingerprint of it, in one lookup.
                    let probe = d.surface == Surface::Field;
                    let id = self.request(d.aim.point, true, probe, LOOKUP_DEADLINE, now, env);
                    env.log(now, &format!("checking the keyboard focus (lookup {id})"));
                    d.job.step = Step::Focus(id);
                    // The answer, or the lookup's give-up, runs the router again.
                    return Flow::Wait(None);
                }
                d.job.step = Step::Front(now);
                Flow::Again
            }
            Step::Focus(_) => Flow::Wait(None),
            Step::Front(since) => {
                if env.foreground_is(d.aim.window, d.aim.pid) {
                    let mut keys = Vec::new();
                    // In a shell, → can accept an autosuggestion; on a canvas it would nudge
                    // the selected shape. Later cards already have the caret after the image.
                    if d.job.first && !d.terminal && d.surface != Surface::Canvas {
                        keys.push(Key::Right);
                    }
                    if d.job.first && d.dismiss_editor {
                        keys.push(Key::Escape);
                    }
                    d.job.step = Step::Keys { keys, since: now };
                    return Flow::Again;
                }
                let give_up = since + paste_policy::ACTIVATION_TIMEOUT;
                if now >= give_up {
                    return self.fail(
                        d,
                        Self::fail_hint(d),
                        "the target is not in front",
                        now,
                        state,
                        env,
                    );
                }
                Flow::Wait(Some(give_up.min(now + ACTIVATION_POLL)))
            }
            Step::Keys { mut keys, since } => {
                while let Some(&key) = keys.first() {
                    let sent = match self.send_batch(d, Batch::Key(key), None, now, state, env) {
                        Ok(sent) => sent,
                        Err(flow) => return flow,
                    };
                    match sent {
                        Sent::Blocked(modifier) => {
                            let give_up = since + MODIFIER_WAIT;
                            if now >= give_up {
                                let hint = modifier_hint(modifier);
                                return self.fail(
                                    d,
                                    &hint,
                                    "a modifier stayed down",
                                    now,
                                    state,
                                    env,
                                );
                            }
                            d.job.step = Step::Keys { keys, since };
                            return Flow::Wait(Some(give_up.min(now + MODIFIER_POLL)));
                        }
                        Sent::Done => {
                            env.log(now, &format!("pressed {key:?}"));
                            keys.remove(0);
                        }
                        other => {
                            return self.fail(
                                d,
                                Self::fail_hint(d),
                                &format!("{key:?} not sent: {other:?}"),
                                now,
                                state,
                                env,
                            );
                        }
                    }
                }
                let delay = if d.job.first && d.dismiss_editor {
                    paste_policy::DISMISS_TO_PASTE_DELAY
                } else {
                    paste_policy::COLLAPSE_TO_PASTE_DELAY
                };
                d.job.step = Step::Gap(now + delay);
                Flow::Again
            }
            Step::Gap(until) => {
                if now < until {
                    return Flow::Wait(Some(until));
                }
                if state.card(d.job.id).is_none() {
                    env.log(now, &format!("card {} left the pile meanwhile", d.job.id.0));
                    return self.next_card(d, now, state, env);
                }
                let Pay::Ready(payload) = &d.job.payload else {
                    return self.fail(d, Self::fail_hint(d), "no payload", now, state, env);
                };
                let Some(restore) = self.restore.as_ref() else {
                    return self.fail(d, HINT_CLIPBOARD, "no saved clipboard", now, state, env);
                };
                // B1: write only over what we expect. The first stage of a delivery with a
                // fresh copy expects exactly the clipboard that copy saved, and hands the copy
                // to the clipboard thread; later cards (and a taken-over restore) expect our
                // own staging still there.
                let fresh = restore.stage.is_none() && restore.prev.is_none();
                let (expect, rollback) = if fresh {
                    let mut expected = restore.expect.unwrap_or_default();
                    let facts = env.clip_facts(now);
                    // A terminal's copy-on-select after the first press is the click's own
                    // (B4: terminals only).
                    if d.terminal
                        && facts.sequence != expected
                        && facts.last_change.is_some_and(|c| c >= d.click.first_at)
                    {
                        env.log(now, "the terminal copied its selection → staged over it");
                        expected = facts.sequence;
                    }
                    (Expect::Sequence(expected), Some(&restore.snap))
                } else {
                    (Expect::OwnStaging, None)
                };
                // Write the clipboard only now: a terminal's copy-on-select on the mouse-up
                // would replace an earlier write.
                match env.clip_stage(payload.clone(), expect, rollback) {
                    Ok(ticket) => {
                        if let Some(restore) = self.restore.as_mut() {
                            restore.prev = restore.stage.take();
                            restore.stage = Some(StageRec::new(ticket, now, d.terminal, d.aim));
                        }
                        d.job.step = Step::Staging { ticket, since: now };
                        Flow::Wait(Some(now + REPLY_TIMEOUT))
                    }
                    Err(why) => self.fail(
                        d,
                        Self::fail_hint(d),
                        &format!("stage: {why}"),
                        now,
                        state,
                        env,
                    ),
                }
            }
            Step::Staging { since, .. } => {
                let give_up = since + REPLY_TIMEOUT;
                if now >= give_up {
                    // B5: the request may still run. A rollback queued behind it runs after it,
                    // on the clipboard thread, whatever the router does meanwhile.
                    if let Some(restore) = self.restore.as_mut()
                        && let Ok(ticket) = env.clip_rollback(RestoreGuard::OwnStaging)
                    {
                        restore.restoring = Some((ticket, now));
                        restore.first_try.get_or_insert(now);
                    }
                    return self.fail(
                        d,
                        Self::fail_hint(d),
                        "no stage reply in time (a rollback is queued behind it)",
                        now,
                        state,
                        env,
                    );
                }
                Flow::Wait(Some(give_up))
            }
            Step::Send(since) => {
                let ticket = self
                    .restore
                    .as_ref()
                    .and_then(|r| r.stage)
                    .map(|s| s.ticket);
                let sent = match self.send_batch(d, d.aim.keys.batch(), ticket, now, state, env) {
                    Ok(sent) => sent,
                    Err(flow) => return flow,
                };
                match sent {
                    Sent::Blocked(modifier) => {
                        let give_up = since + MODIFIER_WAIT;
                        if now >= give_up {
                            let hint = modifier_hint(modifier);
                            return self.fail(d, &hint, "a modifier stayed down", now, state, env);
                        }
                        Flow::Wait(Some(give_up.min(now + MODIFIER_POLL)))
                    }
                    Sent::Done | Sent::Incomplete { .. } => {
                        env.log(now, &format!("sent {:?}: {sent:?}", d.aim.keys));
                        if let Some(stage) = self.restore.as_mut().and_then(|r| r.stage.as_mut()) {
                            stage.sent_at = Some(now);
                        }
                        d.job.step = Step::Watch(now);
                        Flow::Again
                    }
                    Sent::Refused => {
                        // B3: someone wrote the clipboard after our stage: the user's new
                        // data is there; never paste it, never write over it.
                        if let Some(restore) = self.restore.as_mut() {
                            restore.user_copied = true;
                        }
                        env.clip_abandon();
                        self.end(
                            d,
                            None,
                            "the clipboard changed after the stage → no paste keys",
                            now,
                            state,
                            env,
                        )
                    }
                    other => self.fail(
                        d,
                        Self::fail_hint(d),
                        &format!("paste keys not sent: {other:?}"),
                        now,
                        state,
                        env,
                    ),
                }
            }
            Step::Watch(sent_at) => {
                let facts = env.clip_facts(now);
                self.reconcile(&facts, now, env);
                let front = env.foreground_is(d.aim.window, d.aim.pid);
                let mut reads = Reads::default();
                if let Some(stage) = self.restore.as_mut().and_then(|r| r.stage.as_mut()) {
                    reads = read_counts(stage, &facts, front);
                    log_reads(stage, reads, &facts, now, env);
                }
                let canvas = matches!(d.surface, Surface::Canvas | Surface::Document);
                // The probe verifies single-line fields, and any field whose reads came only
                // from others (they may hide the target's).
                let masked = reads.target + reads.probable == 0 && reads.other > 0;
                let probing = d.probe.before.is_some() && (d.probe.verify || masked);
                if probing
                    && d.probe.after_req.is_none()
                    && d.probe.last_check.is_none_or(|t| now >= t + PROBE_INTERVAL)
                {
                    let id = self.request(d.aim.point, true, true, LOOKUP_DEADLINE, now, env);
                    d.probe.after_req = Some(id);
                    d.probe.last_check = Some(now);
                }
                let view = d.probe.before.is_some().then_some(ProbeView {
                    verify: d.probe.verify,
                    answered: d.probe.answered,
                    can_tell: d.probe.can_tell,
                    changed: d.probe.changed,
                });
                let cancelled = d.cancelled.is_some() || env.key_presses() != d.click.keys;
                let evidence = reads.target + reads.probable;
                let summary = format!(
                    "the target read {}, probably the target {}, others {}, probe {}",
                    reads.target,
                    reads.probable,
                    reads.other,
                    if d.probe.changed {
                        "changed"
                    } else if d.probe.answered {
                        "unchanged"
                    } else {
                        "-"
                    }
                );
                match verdict(now - sent_at, reads, canvas, view, cancelled) {
                    Verdict::Pending => Flow::Wait(Some(now + OUTCOME_POLL)),
                    Verdict::Accepted => {
                        let count = d.ids.len();
                        let hint = if count == 1 {
                            if d.job.is_path {
                                HINT_PASTED_PATH.to_string()
                            } else {
                                HINT_PASTED.to_string()
                            }
                        } else {
                            format!("pasted {count}")
                        };
                        env.log(now, &format!("card {}: {summary} → accepted", d.job.id.0));
                        state.deliver(&[d.job.id], &hint, now);
                        self.state_changed = true;
                        if d.cancelled.is_none()
                            && env.key_presses() == d.click.keys
                            && !self.restore.as_ref().is_some_and(|r| r.user_copied)
                            && d.ids[d.index + 1..]
                                .iter()
                                .any(|id| state.card(*id).is_some())
                        {
                            d.job.step = Step::Next(now + PILE_STEP);
                            Flow::Again
                        } else {
                            Flow::Done
                        }
                    }
                    Verdict::Rejected => {
                        let hint = if d.job.movie {
                            HINT_NO_VIDEOS
                        } else {
                            HINT_NO_IMAGES
                        };
                        self.fail(
                            d,
                            hint,
                            &format!("{summary}: read, but the field did not change"),
                            now,
                            state,
                            env,
                        )
                    }
                    Verdict::Uncertain => {
                        if let Some(restore) = self.restore.as_mut() {
                            restore.late = Some((d.job.id, evidence));
                        }
                        self.fail(
                            d,
                            HINT_UNCERTAIN,
                            &format!(
                                "{summary}: nothing proves the target took it{} (still watching for a read; the pile stops)",
                                if cancelled { ", and the user moved on" } else { "" }
                            ),
                            now,
                            state,
                            env,
                        )
                    }
                    Verdict::Failed => {
                        if let Some(restore) = self.restore.as_mut() {
                            restore.late = Some((d.job.id, evidence));
                        }
                        let hint = if !d.job.first {
                            HINT_REST_FAILED
                        } else if canvas {
                            HINT_CANVAS_FAILED
                        } else {
                            HINT_PASTE_FAILED
                        };
                        self.fail(
                            d,
                            hint,
                            &format!("{summary}: nothing read it (still watching for a late read)"),
                            now,
                            state,
                            env,
                        )
                    }
                }
            }
            Step::Next(until) => {
                if now < until {
                    return Flow::Wait(Some(until));
                }
                self.next_card(d, now, state, env)
            }
        }
    }

    /// C1 and the guarded send: the gesture and the card are checked, then the environment
    /// sends with its own last checks. `Err` ends the delivery quietly.
    #[allow(clippy::too_many_arguments)]
    fn send_batch<E: Env<Snap = S>>(
        &mut self,
        d: &mut Delivery,
        batch: Batch,
        stage: Option<u64>,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) -> Result<Sent, Flow> {
        let why = if let Some(why) = d.cancelled {
            Some(why)
        } else if env.key_presses() != d.click.keys {
            Some("the user typed")
        } else if state.card(d.job.id).is_none() {
            Some("the card left the pile")
        } else {
            None
        };
        if let Some(why) = why {
            return Err(self.end(
                d,
                None,
                &format!("{why} → stopped before {batch:?}"),
                now,
                state,
                env,
            ));
        }
        let guard = SendGuard {
            window: d.aim.window,
            pid: d.aim.pid,
            focus: d.focus,
            stage,
        };
        let (sent, focus) = env.send(batch, guard);
        if d.focus.is_none() && matches!(sent, Sent::Done | Sent::Incomplete { .. }) {
            d.focus = focus;
        }
        Ok(sent)
    }

    /// Cards shaken off (or otherwise gone) meanwhile are skipped; new captures are not taken.
    fn next_card<E: Env<Snap = S>>(
        &mut self,
        d: &mut Delivery,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) -> Flow {
        while d.index + 1 < d.ids.len() {
            d.index += 1;
            let id = d.ids[d.index];
            if self.load_job(d, id, false, now, state, env, now) {
                return Flow::Again;
            }
        }
        Flow::Done
    }

    /// Counts the target's reads, then `reconcileClipboard`: someone else wrote the
    /// clipboard after we did.
    fn reconcile<E: Env<Snap = S>>(&mut self, facts: &ClipFacts, now: f64, env: &mut E) {
        let Some(restore) = self.restore.as_mut() else {
            return;
        };
        let Some(stage) = restore.stage.as_mut() else {
            return;
        };
        if !stage.guarding || stage.restaging.is_some() || !stage.confirmed {
            return;
        }
        let Some(sent) = stage.sent_at else {
            return;
        };
        let Some(staged) = facts.staged_for(stage.ticket) else {
            return;
        };
        let Some(over) = staged.overwritten_at else {
            return;
        };
        // Act once the writer has closed the clipboard (its update arrived).
        let Some(change) = facts.last_change.filter(|c| *c >= over) else {
            return;
        };
        if stage.adopted.is_some_and(|base| change <= base) {
            return;
        }
        // B4: only a terminal's copy-on-select is the click's own. Anywhere else a write after
        // our stage is the user copying: stop, never rewrite, never restore.
        if !stage.terminal {
            stage.guarding = false;
            restore.user_copied = true;
            env.clip_abandon();
            env.log(now, "the user copied something → leave the clipboard alone");
            return;
        }
        let target_reads = read_counts(stage, facts, false).target;
        match paste_policy::overwrite_step(
            now - sent,
            target_reads,
            staged.rewrites,
            stage.terminal,
        ) {
            OverwriteStep::Rewrite => match env.clip_restage(Expect::Sequence(facts.sequence)) {
                Ok(ticket) => {
                    stage.restaging = Some((ticket, now));
                    env.log(now, "clipboard overwritten before the paste was read → writing the screenshot again");
                }
                Err(why) => {
                    stage.guarding = false;
                    env.log(now, &format!("restage failed: {why}"));
                }
            },
            OverwriteStep::Adopt => {
                stage.adopted = Some(change);
                stage.adopted_seq = Some(facts.sequence);
                env.log(
                    now,
                    "selection copied after the paste → still restoring the clipboard",
                );
            }
            OverwriteStep::Leave => {
                stage.guarding = false;
                restore.user_copied = true;
                env.clip_abandon();
                env.log(now, "the user copied something → leave the clipboard alone");
            }
        }
    }

    fn on_clip_reply<E: Env<Snap = S>>(
        &mut self,
        reply: ClipReply<S>,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let ticket = reply.ticket;
        // A snapshot: the remembered one, a delivery's, or both.
        if let Some(memory) = self.remembered.as_mut()
            && matches!(memory.rem, Some(Rem::Waiting(t)) if t == ticket)
        {
            memory.rem = Some(match &reply.result {
                Ok(ClipDone::Snapshot {
                    snap,
                    sequence,
                    lost,
                }) => Rem::Ready {
                    snap: snap.clone(),
                    sequence: *sequence,
                    lost: lost.clone(),
                },
                _ => Rem::Failed,
            });
        }
        if let Phase::Delivering(d) = &mut self.phase
            && d.clip == Some(ticket)
        {
            d.clip = None;
            let refuse = match &reply.result {
                Ok(ClipDone::Snapshot {
                    snap,
                    sequence,
                    lost: None,
                }) => {
                    env.log(now, &format!("clipboard: saved (seq {sequence})"));
                    let mut restore = Restore::new(snap.clone());
                    restore.expect = Some(*sequence);
                    self.restore = Some(restore);
                    None
                }
                Ok(ClipDone::Snapshot {
                    lost: Some(lost), ..
                }) => Some(format!("the saved copy lost {lost}")),
                other => Some(format!("cannot save it ({:?})", other.as_ref().err())),
            };
            if let Some(why) = refuse {
                env.log(now, &format!("clipboard: {why} → do not paste"));
                self.phase = Phase::Idle;
                self.phase_wake = None;
                state.reject(HINT_CLIPBOARD, now);
                self.state_changed = true;
            }
            return;
        }
        // A stage, in the delivery or answered after it gave up.
        let staging_now = matches!(&self.phase, Phase::Delivering(d)
            if matches!(d.job.step, Step::Staging { ticket: t, .. } if t == ticket));
        if let Some(restore) = self.restore.as_mut()
            && let Some(stage) = restore.stage.as_mut()
            && stage.ticket == ticket
            && !stage.confirmed
            && stage.emptied.is_none()
        {
            let failure = match &reply.result {
                Ok(ClipDone::Staged { sequence }) => {
                    env.log(now, &format!("staged (seq {sequence})"));
                    stage.confirmed = true;
                    restore.prev = None;
                    restore.late = None;
                    if staging_now {
                        if let Phase::Delivering(d) = &mut self.phase {
                            d.job.step = Step::Send(now);
                        }
                    } else {
                        env.log(
                            now,
                            "the stage landed after the delivery gave up → restore at once",
                        );
                        restore.now_please = true;
                    }
                    None
                }
                Err(ClipFail::Emptied { sequence }) => {
                    // The clipboard is empty now: put the user's back over exactly that.
                    stage.emptied = Some(*sequence);
                    restore.prev = None;
                    restore.now_please = true;
                    Some(format!(
                        "stage failed after emptying the clipboard (seq {sequence})"
                    ))
                }
                Err(ClipFail::Changed) => {
                    // B1: the clipboard is not what we saved (the user copied): nothing was
                    // written; stop quietly, keep the cards, restore nothing.
                    restore.stage = restore.prev.take();
                    if restore.stage.is_none() {
                        restore.user_copied = true;
                    }
                    Some("the clipboard changed before the stage → nothing written".to_string())
                }
                other => {
                    // Nothing was written: an earlier paste's staging (if any) is still there.
                    restore.stage = restore.prev.take();
                    Some(format!(
                        "stage failed, clipboard untouched: {:?}",
                        other.as_ref().err()
                    ))
                }
            };
            let quiet = matches!(reply.result, Err(ClipFail::Changed));
            if let Some(why) = failure {
                if staging_now {
                    let Phase::Delivering(d) = std::mem::replace(&mut self.phase, Phase::Idle)
                    else {
                        unreachable!("checked above");
                    };
                    let hint = (!quiet).then(|| Self::fail_hint(&d));
                    let _ = self.end(&d, hint, &why, now, state, env);
                    self.finish_delivery(now, env);
                } else {
                    env.log(now, &why);
                }
            }
            return;
        }
        let Some(restore) = self.restore.as_mut() else {
            return;
        };
        // A restage.
        if let Some(stage) = restore.stage.as_mut()
            && stage.restaging.is_some_and(|(t, _)| t == ticket)
        {
            stage.restaging = None;
            match &reply.result {
                Ok(_) => {}
                Err(ClipFail::Emptied { sequence }) => {
                    stage.emptied = Some(*sequence);
                    stage.guarding = false;
                    restore.now_please = true;
                    env.log(
                        now,
                        "restage failed after emptying the clipboard → restore now",
                    );
                }
                Err(why) => {
                    stage.guarding = false;
                    env.log(now, &format!("restage failed: {why:?}"));
                }
            }
            return;
        }
        // A restore.
        if restore.restoring.is_some_and(|(t, _)| t == ticket) {
            restore.restoring = None;
            match &reply.result {
                Ok(_) => {
                    env.log(now, "clipboard restored");
                    self.restore = None;
                    self.remembered = None;
                }
                Err(ClipFail::Partial) => {
                    // Our partial result is on the clipboard now: write it over at the next try.
                    restore.partial = true;
                    restore.now_please = true;
                    self.retry_restore("partial restore", now, state, env);
                }
                Err(ClipFail::Changed) => {
                    env.log(
                        now,
                        "restore refused: the clipboard changed since → leave it",
                    );
                    self.restore = None;
                }
                Err(why) => {
                    let why = format!("{why:?}");
                    self.retry_restore(&why, now, state, env);
                }
            }
        }
    }

    /// A restore that could not run (locked clipboard, full queue): try again every
    /// [`RESTORE_RETRY`] for [`RESTORE_GIVE_UP`], then say so.
    fn retry_restore<E: Env<Snap = S>>(
        &mut self,
        why: &str,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        let Some(restore) = self.restore.as_mut() else {
            return;
        };
        let first = *restore.first_try.get_or_insert(now);
        if now >= first + RESTORE_GIVE_UP {
            env.log(
                now,
                &format!("restore failed ({why}) for too long → abandoned"),
            );
            self.restore = None;
            state.flash_hint(HINT_RESTORE_FAILED, 3.0, now);
            self.state_changed = true;
            return;
        }
        env.log(now, &format!("restore failed ({why}) → again shortly"));
        restore.retry_at = Some(now + RESTORE_RETRY);
        self.restore_wake = earlier(self.restore_wake, restore.retry_at);
    }

    // MARK: Driving

    fn run<E: Env<Snap = S>>(&mut self, now: f64, state: &mut FairyState, env: &mut E) {
        self.advance(now, state, env);
        self.advance_restore(now, state, env);
        self.advance_hover(now, state, env);
        if !state.is_holding() && !self.busy() {
            // Free the remembered copy (it can be large) once nothing is held.
            self.remembered = None;
        }
    }

    fn advance<E: Env<Snap = S>>(&mut self, now: f64, state: &mut FairyState, env: &mut E) {
        self.phase_wake = None;
        loop {
            match std::mem::replace(&mut self.phase, Phase::Idle) {
                Phase::Idle => return,
                waiting @ (Phase::Looking { .. } | Phase::Recheck { .. }) => {
                    self.phase = waiting;
                    return;
                }
                Phase::Note { until } => {
                    if now >= until {
                        env.log(now, "the note did not answer → keep the cards");
                        state.reject(HINT_NOTE_SLOW, now);
                        self.state_changed = true;
                        return;
                    }
                    self.phase = Phase::Note { until };
                    self.phase_wake = Some(until);
                    return;
                }
                Phase::AwaitUp {
                    click,
                    first,
                    ready_at: None,
                } => {
                    let up_deadline = click.at + paste_policy::MOUSE_UP_TIMEOUT;
                    if env.primary_down() {
                        if now >= up_deadline {
                            env.log(now, "the button is still down (a drag) → ignore");
                            return;
                        }
                        self.phase = Phase::AwaitUp {
                            click,
                            first,
                            ready_at: None,
                        };
                        self.phase_wake =
                            Some(up_deadline.min(now + paste_policy::MOUSE_UP_POLL_INTERVAL));
                        return;
                    }
                    // Chromium needs a moment after the release; focus needs one too.
                    let settle = if first.chromium {
                        SETTLE_AFTER_UP_CHROMIUM
                    } else {
                        SETTLE_AFTER_UP
                    };
                    self.phase = Phase::AwaitUp {
                        click,
                        first,
                        ready_at: Some(now + settle),
                    };
                }
                Phase::AwaitUp {
                    click,
                    first,
                    ready_at: Some(at),
                } => {
                    if now < at {
                        self.phase = Phase::AwaitUp {
                            click,
                            first,
                            ready_at: Some(at),
                        };
                        self.phase_wake = Some(at);
                        return;
                    }
                    let deadline = if first.slow {
                        SLOW_DEADLINE
                    } else {
                        LOOKUP_DEADLINE
                    };
                    let id = self.request(click.point, true, false, deadline, now, env);
                    env.log(
                        now,
                        &format!(
                            "mouse is up: looking again with focus (lookup {id}, {deadline:.2}s)"
                        ),
                    );
                    self.phase = Phase::Recheck { id, click, first };
                    return;
                }
                Phase::Saving(mut f) => match self.save_step(&mut f, now, state, env) {
                    Flow::Wait(at) => {
                        if matches!(self.phase, Phase::Idle) {
                            self.phase = Phase::Saving(f);
                        }
                        self.phase_wake = at;
                        return;
                    }
                    Flow::Again => self.phase = Phase::Saving(f),
                    Flow::Done => {
                        self.phase = Phase::Idle;
                        self.phase_wake = None;
                        env.log(now, "save over");
                        return;
                    }
                },
                Phase::Delivering(mut d) => match self.step(&mut d, now, state, env) {
                    Flow::Wait(at) => {
                        if matches!(self.phase, Phase::Idle) {
                            self.phase = Phase::Delivering(d);
                        }
                        self.phase_wake = at;
                        return;
                    }
                    Flow::Again => {
                        if matches!(self.phase, Phase::Idle) {
                            self.phase = Phase::Delivering(d);
                        }
                    }
                    Flow::Done => {
                        self.finish_delivery(now, env);
                        return;
                    }
                },
            }
        }
    }

    fn finish_delivery<E: Env<Snap = S>>(&mut self, now: f64, env: &mut E) {
        self.phase = Phase::Idle;
        self.phase_wake = None;
        if self.restore.as_ref().is_some_and(|r| r.stage.is_none()) {
            // Nothing of ours went on the clipboard.
            self.restore = None;
        }
        env.log(now, "delivery over");
    }

    /// `pollRestore`, once the delivery is over.
    fn advance_restore<E: Env<Snap = S>>(&mut self, now: f64, state: &mut FairyState, env: &mut E) {
        self.restore_wake = None;
        if self.busy() {
            return;
        }
        let Some(restore) = self.restore.as_mut() else {
            return;
        };
        if let Some((_, since)) = restore.restoring {
            let give_up = since + REPLY_TIMEOUT;
            if now >= give_up {
                restore.restoring = None;
                self.retry_restore("no reply", now, state, env);
            } else {
                self.restore_wake = Some(give_up);
            }
            return;
        }
        if let Some(at) = restore.retry_at {
            if now < at {
                self.restore_wake = Some(at);
                return;
            }
            restore.retry_at = None;
        }
        if restore.stage.is_none() || restore.user_copied {
            self.restore = None;
            return;
        }
        let facts = env.clip_facts(now);
        self.reconcile(&facts, now, env);
        let Some(restore) = self.restore.as_mut() else {
            return;
        };
        let Some(stage) = restore.stage else {
            return;
        };
        if restore.user_copied {
            self.restore = None;
            return;
        }
        // The app was busy and took the Ctrl+V after we had given up: the top card did land.
        let mut probe_stage = stage;
        let late_reads = read_counts(&mut probe_stage, &facts, false);
        if let Some((id, before)) = restore.late
            && late_reads.target + late_reads.probable > before
        {
            restore.late = None;
            if state.card(id).is_some() {
                env.log(
                    now,
                    &format!("card {}: the target read it late → it landed", id.0),
                );
                state.deliver(&[id], HINT_PASTED, now);
                self.state_changed = true;
            }
        }
        if !stage.confirmed && stage.emptied.is_none() {
            // A stage request still in flight: its reply decides (a rollback may already be
            // queued behind it). Polled for a bounded time only.
            let give_up = stage.requested_at + UNANSWERED_LIMIT;
            if now >= give_up {
                env.log(
                    now,
                    "the stage never answered → the clipboard thread rolls back when it runs",
                );
                self.restore = None;
            } else {
                self.restore_wake = Some(give_up);
            }
            return;
        }
        if let Some((_, since)) = stage.restaging {
            if now >= since + REPLY_TIMEOUT {
                env.log(now, "the rewrite never answered → stop guarding");
                if let Some(stage) = restore.stage.as_mut() {
                    stage.restaging = None;
                    stage.guarding = false;
                }
            } else {
                self.restore_wake = Some(now + RESTORE_POLL);
                return;
            }
        }
        let guard = restore_guard(&stage, &facts, restore.partial);
        let step = match stage.sent_at {
            Some(sent) if !restore.now_please => RestoreRules::default().step(
                now - sent,
                facts
                    .staged_for(stage.ticket)
                    .and_then(|s| s.last_read_at)
                    .map(|t| now - t),
                guard.is_some(),
            ),
            _ if guard.is_some() => RestoreStep::Restore,
            _ => RestoreStep::Abandon,
        };
        match (step, guard) {
            (RestoreStep::Wait, _) => self.restore_wake = Some(now + RESTORE_POLL),
            (RestoreStep::Restore, Some(guard)) => match env.clip_rollback(guard) {
                Ok(ticket) => {
                    env.log(now, &format!("restoring the clipboard ({guard:?})"));
                    restore.first_try.get_or_insert(now);
                    restore.restoring = Some((ticket, now));
                    self.restore_wake = Some(now + REPLY_TIMEOUT);
                }
                Err(why) => self.retry_restore(&why, now, state, env),
            },
            _ => {
                env.log(now, "clipboard changed by someone else → no restore");
                self.restore = None;
            }
        }
    }

    /// `scheduleHoverHint` and `resolveHover`: one lookup at a time, only while holding,
    /// after the cursor rested, at most 4 a second.
    fn advance_hover<E: Env<Snap = S>>(&mut self, now: f64, state: &mut FairyState, env: &mut E) {
        self.hover_wake = None;
        if self.busy() || !state.is_holding() || state.hidden_for_capture() {
            return;
        }
        let Some(point) = self.hover.point else {
            return;
        };
        if self.hover.resolved || self.hover.in_flight.is_some() {
            return;
        }
        let rested = self.hover.moved_at + HOVER_REST;
        if now < rested {
            self.hover_wake = Some(rested);
            return;
        }
        // Over a note: its own hint and no lookup (the drop zones of `scheduleHoverHint`).
        if env.note_at(point) {
            self.hover.resolved = true;
            if state.hint() != Some(DROP_HINT) {
                state.set_hover_hint(Some(DROP_HINT), now);
                self.state_changed = true;
            }
            return;
        }
        let allowed = self
            .hover
            .last_request
            .map_or(f64::NEG_INFINITY, |t| t + HOVER_MIN_GAP);
        if now < allowed {
            self.hover_wake = Some(allowed);
            return;
        }
        let id = self.request(point, false, false, LOOKUP_DEADLINE, now, env);
        self.hover.in_flight = Some(id);
        self.hover.resolved = true;
        self.hover.last_request = Some(now);
    }

    fn apply_hover(&mut self, seen: &Seen, now: f64, state: &mut FairyState) {
        // The cursor moved on since: a newer lookup will follow.
        if !self.hover.resolved || self.busy() || !state.is_holding() {
            return;
        }
        let hint = hover_hint(seen);
        if state.hint() != hint {
            state.set_hover_hint(hint, now);
            self.state_changed = true;
        }
    }
}

// MARK: Debug log

/// `FAIRY_DEBUG=1`: one line per router step on stderr and in
/// `%LOCALAPPDATA%\Igorio\debug.log`. Nothing without the variable.
pub struct DebugLog {
    enabled: bool,
    file: Option<File>,
}

impl DebugLog {
    pub fn from_env() -> DebugLog {
        let enabled = std::env::var_os("FAIRY_DEBUG").is_some_and(|v| v == "1");
        let file = enabled
            .then(|| {
                let folder = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Igorio");
                fs::create_dir_all(&folder).ok()?;
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(folder.join("debug.log"))
                    .ok()
            })
            .flatten();
        DebugLog { enabled, file }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn line(&mut self, now: f64, text: &str) {
        if !self.enabled {
            return;
        }
        let line = format!("{now:10.3} {text}");
        // Never panics (D1): a missing stderr or a full disk only loses the line, also while
        // a panic unwinds through the quit path.
        let _ = writeln!(std::io::stderr().lock(), "{line}");
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }
}

// MARK: Live environment

/// An auto-reset event that other threads set to wake the UI loop.
struct WakeEvent(HANDLE);

// SAFETY: a kernel event handle may be set and waited on from any thread.
unsafe impl Send for WakeEvent {}
// SAFETY: as above; `SetEvent` from several threads at once is allowed.
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    fn new() -> windows::core::Result<WakeEvent> {
        // SAFETY: no pointers are passed; the handle is owned by `WakeEvent`.
        Ok(WakeEvent(unsafe {
            CreateEventW(None, false, false, None)?
        }))
    }

    fn set(&self) {
        // SAFETY: the handle is live while `self` is.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        // SAFETY: created in `new`, closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The Windows side of [`Env`]: the UI Automation service, the clipboard thread and the
/// PNG saver. Every call returns at once; answers come back through [`Drops::pump`].
pub struct LiveEnv {
    uia: Option<Uia>,
    clip: Option<Clipboard>,
    keys: Option<KeyWatch>,
    wake: Arc<WakeEvent>,
    /// The one PNG saver thread (D2) and its job queue.
    saver: Option<(SyncSender<SaveJob>, std::thread::JoinHandle<()>)>,
    settings: Settings,
    saved: Receiver<(u64, Result<PathBuf, SaveFail>)>,
    next_save: u64,
    /// Saver jobs that are folder saves (their answers go to `Router::folder_saved`).
    folder_jobs: HashSet<u64>,
    /// The File Explorer folder lookup; None when it could not start (saves then fail).
    explorer: Option<Explorer>,
    log: DebugLog,
    primary: Button,
    /// Converts the clipboard's `Instant`s to the `hook::now` clock.
    origin: Instant,
    origin_secs: f64,
    /// The open notes' rectangles while the notes capability runs.
    notes: Option<DropZones>,
    /// Cards for a note, for the UI loop to send (`Drops::take_note_gifts`).
    gifts: Vec<((i32, i32), Vec<HeldCard>)>,
}

impl LiveEnv {
    /// Starts the UI Automation worker, the clipboard thread, the saver and the Explorer
    /// folder lookup.
    pub fn start(settings: Settings, mut log: DebugLog) -> Result<LiveEnv, String> {
        let wake = Arc::new(WakeEvent::new().map_err(|e| format!("wake event: {e}"))?);
        let uia = Uia::start().map_err(|e| format!("UI Automation: {e}"))?;
        let clip_wake = wake.clone();
        // Runs on the clipboard thread: only signals the event.
        let clip =
            Clipboard::start(move || clip_wake.set()).map_err(|e| format!("clipboard: {e}"))?;
        let (saves, saved) = mpsc::channel();
        let saver = spawn_saver(saves, wake.clone());
        // Without the key watch typing cannot stop a delivery; the rest still works.
        let keys = KeyWatch::start().ok();
        // Without it the pastes still work; a folder save then says it can't save.
        let explorer = match Explorer::start() {
            Ok(explorer) => Some(explorer),
            Err(e) => {
                log.line(hook::now(), &format!("Explorer folder lookup is off: {e}"));
                None
            }
        };
        Ok(LiveEnv {
            folder_jobs: HashSet::new(),
            explorer,
            uia: Some(uia),
            clip: Some(clip),
            keys,
            wake,
            saver,
            settings,
            saved,
            next_save: 1,
            log,
            primary: LiveEnv::primary_button(),
            origin: Instant::now(),
            origin_secs: hook::now(),
            notes: None,
            gifts: Vec::new(),
        })
    }

    /// The physical button of the user's main click.
    pub fn primary_button() -> Button {
        // SAFETY: plain value argument.
        if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 {
            Button::Right
        } else {
            Button::Left
        }
    }

    /// Handles to add to the UI loop's wait: UI Automation answers, and clipboard replies,
    /// clipboard changes and saved PNGs, and Explorer's folder answers.
    pub fn wake_handles(&self) -> Vec<HANDLE> {
        let mut handles = vec![self.wake.0];
        if let Some(uia) = &self.uia {
            handles.push(uia.wake_handle());
        }
        if let Some(explorer) = &self.explorer {
            handles.push(explorer.wake_handle());
        }
        handles
    }

    fn secs(&self, at: Instant) -> f64 {
        match at.checked_duration_since(self.origin) {
            Some(after) => self.origin_secs + after.as_secs_f64(),
            None => self.origin_secs - self.origin.duration_since(at).as_secs_f64(),
        }
    }

    /// Feeds every answer, reply and saved file to the router.
    fn pump(&mut self, router: &mut Router<clipboard::Snapshot>, state: &mut FairyState, now: f64) {
        let mut answers = Vec::new();
        if let Some(uia) = &self.uia {
            while let Some((id, outcome)) = uia.poll() {
                answers.push((id.0, outcome));
            }
        }
        for (id, outcome) in answers {
            router.lookup_answered(id, outcome, now, state, self);
        }
        let mut replies = Vec::new();
        if let Some(clip) = &self.clip {
            while let Some(reply) = clip.try_reply() {
                replies.push(reply);
            }
        }
        for reply in replies {
            router.clip_reply(convert_reply(reply), now, state, self);
        }
        let mut folders = Vec::new();
        if let Some(explorer) = &self.explorer {
            while let Some((id, outcome)) = explorer.poll() {
                folders.push((id.0, outcome));
            }
        }
        for (id, outcome) in folders {
            router.folder_answered(id, outcome, now, state, self);
        }
        while let Ok((job, result)) = self.saved.try_recv() {
            if self.folder_jobs.remove(&job) {
                router.folder_saved(job, result, now, state, self);
            } else {
                router.saved(job, result.map_err(|f| f.why), now, state, self);
            }
        }
        router.clipboard_changed(now, state, self);
    }

    /// The router's quit: put the user's clipboard back if ours is still there, waiting up to
    /// `timeout` for the clipboard replies. A second call finds nothing left to do.
    fn restore(&mut self, router: &mut Router<clipboard::Snapshot>, timeout: Duration) {
        let until = Instant::now() + timeout;
        let mut waiting = router.quit(hook::now(), self);
        while let Some(ticket) = waiting {
            let Some(left) = until.checked_duration_since(Instant::now()) else {
                self.log.line(hook::now(), "quit: out of time");
                break;
            };
            let Some(reply) = self.clip.as_ref().and_then(|c| c.wait_reply(left)) else {
                break;
            };
            if reply.ticket.0 != ticket {
                continue;
            }
            waiting = router.quit_reply(convert_reply(reply), hook::now(), self);
        }
    }

    /// At quit: let the router put the user's clipboard back, waiting up to `timeout` for
    /// the clipboard replies it needs, then stop the threads.
    fn finish(&mut self, router: &mut Router<clipboard::Snapshot>, timeout: Duration) {
        self.restore(router, timeout);
        // Dropping the clipboard stops its thread: it rolls back if our staging is still
        // there, else renders what we still own, so the clipboard stays valid.
        self.clip = None;
        if let Some(uia) = self.uia.take() {
            let _ = uia.shutdown();
        }
        self.keys = None;
        // Bounded: its controller abandons a worker stuck inside a Shell call.
        if let Some(explorer) = self.explorer.take() {
            let _ = explorer.shutdown();
        }
        // The saver: no new jobs; the one running (a terminal's PNG or a folder save) may
        // finish within a second. It writes through a temp file, so a file it is cut off
        // in never appears under its final name.
        if let Some((jobs, thread)) = self.saver.take() {
            drop(jobs);
            let until = Instant::now() + Duration::from_secs(1);
            while !thread.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

/// One file for the saver: a PNG for a terminal paste, or a card saved into a folder.
struct SaveJob {
    id: u64,
    image: Arc<igorio_kit::Image>,
    scale: f64,
    captured_at: std::time::SystemTime,
    dest: Dest,
}

enum Dest {
    /// A terminal paste: a PNG in the pasted folder of these settings.
    Pasted(Settings),
    /// A folder save: a PNG, or a copy of the recording's movie file.
    Folder {
        folder: PathBuf,
        movie: Option<PathBuf>,
    },
}

/// The saver worker: one thread for every file, fed through a short queue.
fn spawn_saver(
    done: Sender<(u64, Result<PathBuf, SaveFail>)>,
    wake: Arc<WakeEvent>,
) -> Option<(SyncSender<SaveJob>, std::thread::JoinHandle<()>)> {
    let (jobs, queue) = mpsc::sync_channel::<SaveJob>(2);
    let thread = std::thread::Builder::new()
        .name("paste-saver".into())
        .spawn(move || {
            while let Ok(job) = queue.recv() {
                let result = match &job.dest {
                    Dest::Pasted(settings) => {
                        let folder = settings.pasted_dir(&DefaultFolders::from_system());
                        saver::save_screenshot(&job.image, job.scale, job.captured_at, &folder)
                    }
                    Dest::Folder {
                        folder,
                        movie: Some(movie),
                    } => saver::copy_movie(movie, folder),
                    Dest::Folder {
                        folder,
                        movie: None,
                    } => saver::save_screenshot(&job.image, job.scale, job.captured_at, folder),
                };
                let result = result.map_err(|e| SaveFail::of(&e));
                let _ = done.send((job.id, result));
                wake.set();
            }
        })
        .ok()?;
    Some((jobs, thread))
}

fn convert_reply(reply: Reply) -> ClipReply<clipboard::Snapshot> {
    let result = match reply.result {
        Ok(Done::Snapshot(snap)) => Ok(ClipDone::Snapshot {
            // The identity read under the lock: a stage expects exactly this state.
            sequence: snap.held_sequence(),
            lost: snap.lost().map(|lost| format!("{lost:?}")),
            snap,
        }),
        Ok(Done::Staged { sequence }) => Ok(ClipDone::Staged { sequence }),
        Ok(Done::Restored { sequence }) => Ok(ClipDone::Restored { sequence }),
        Err(ClipError::Changed) => Err(ClipFail::Changed),
        Err(ClipError::Emptied { sequence, .. }) => Err(ClipFail::Emptied { sequence }),
        Err(ClipError::Partial { .. }) => Err(ClipFail::Partial),
        Err(other @ (ClipError::Busy | ClipError::Stale | ClipError::Locked { .. })) => {
            Err(ClipFail::Retry(other.to_string()))
        }
        Err(other) => Err(ClipFail::Other(other.to_string())),
    };
    ClipReply {
        ticket: reply.ticket.0,
        result,
    }
}

impl Env for LiveEnv {
    type Snap = clipboard::Snapshot;

    fn lookup(&mut self, point: (i32, i32), want_focus: bool, probe: bool, deadline: f64) -> u64 {
        let lookup = Lookup {
            point,
            want_focus,
            probe,
            deadline: Duration::from_secs_f64(deadline),
        };
        match &self.uia {
            Some(uia) => uia.request(lookup).0,
            None => u64::MAX,
        }
    }

    fn clip_facts(&self, _now: f64) -> ClipFacts {
        let Some(clip) = &self.clip else {
            return ClipFacts::default();
        };
        let watch = clip.watch();
        ClipFacts {
            sequence: watch.sequence,
            last_change: watch.last_change.map(|t| self.secs(t)),
            staged: watch.staged.map(|s| StagedFacts {
                ticket: s.ticket.0,
                reads: s.reads,
                rewrites: s.rewrites,
                last_read_at: s.last_read_at.map(|t| self.secs(t)),
                overwritten_at: s.overwritten_at.map(|t| self.secs(t)),
                last_reader: s.last_reader,
                renders: s.renders.map(|r| r.map(|r| (r.reader, self.secs(r.at)))),
            }),
        }
    }

    fn clip_snapshot(&mut self) -> Result<u64, String> {
        let clip = self.clip.as_ref().ok_or("no clipboard thread")?;
        clip.snapshot().map(|t| t.0).map_err(|e| e.to_string())
    }

    fn clip_stage(
        &mut self,
        payload: Payload,
        expect: Expect,
        rollback: Option<&clipboard::Snapshot>,
    ) -> Result<u64, String> {
        let clip = self.clip.as_ref().ok_or("no clipboard thread")?;
        clip.stage_guarded(payload, expect, rollback)
            .map(|t| t.0)
            .map_err(|e| e.to_string())
    }

    fn clip_restage(&mut self, expect: Expect) -> Result<u64, String> {
        let clip = self.clip.as_ref().ok_or("no clipboard thread")?;
        clip.restage_guarded(expect)
            .map(|t| t.0)
            .map_err(|e| e.to_string())
    }

    fn clip_rollback(&mut self, guard: RestoreGuard) -> Result<u64, String> {
        let clip = self.clip.as_ref().ok_or("no clipboard thread")?;
        let when = match guard {
            RestoreGuard::OwnStaging => clipboard::Rollback::OwnStaging,
            RestoreGuard::Sequence(sequence) => clipboard::Rollback::Sequence(sequence),
        };
        clip.rollback(when).map(|t| t.0).map_err(|e| e.to_string())
    }

    fn clip_abandon(&mut self) {
        if let Some(clip) = &self.clip {
            let _ = clip.abandon();
        }
    }

    fn save_png(&mut self, card: &HeldCard) -> u64 {
        let id = self.next_save;
        self.next_save += 1;
        let job = SaveJob {
            id,
            image: card.image.clone(),
            scale: card.scale,
            captured_at: card.captured_at,
            dest: Dest::Pasted(self.settings.clone()),
        };
        // The folder lookup, the encode and the write run on the saver thread. With no
        // saver or a full queue the save fails at once and the paste says so.
        let queued = self
            .saver
            .as_ref()
            .is_some_and(|(jobs, _)| jobs.try_send(job).is_ok());
        if !queued {
            self.log.line(hook::now(), "PNG saver busy or gone");
        }
        id
    }

    fn folder_lookup(&mut self, request: FolderRequest, deadline: f64) -> u64 {
        match &self.explorer {
            Some(explorer) => {
                explorer
                    .request(request, Duration::from_secs_f64(deadline))
                    .0
            }
            None => {
                // Never answered: the router gives up after the deadline and its grace.
                self.log.line(hook::now(), "no Explorer folder lookup");
                u64::MAX
            }
        }
    }

    fn save_to_folder(&mut self, card: &HeldCard, folder: &Path) -> Result<u64, String> {
        let id = self.next_save;
        self.next_save += 1;
        let job = SaveJob {
            id,
            image: card.image.clone(),
            scale: card.scale,
            captured_at: card.captured_at,
            dest: Dest::Folder {
                folder: folder.to_path_buf(),
                movie: card.movie.clone(),
            },
        };
        let (jobs, _) = self.saver.as_ref().ok_or("no saver thread")?;
        jobs.try_send(job)
            .map_err(|_| "the saver is busy".to_string())?;
        self.folder_jobs.insert(id);
        Ok(id)
    }

    fn reveal(&mut self, window: isize, file: &Path) {
        if let Some(explorer) = &self.explorer {
            explorer.reveal(window, file.to_path_buf());
        }
    }

    fn foreground_is(&self, window: isize, pid: u32) -> bool {
        input::foreground_matches(HWND(window as *mut _), pid)
    }

    fn point_in_client(&self, window: isize, (x, y): (i32, i32)) -> bool {
        let hwnd = HWND(window as *mut _);
        let mut rect = RECT::default();
        let mut origin = POINT::default();
        // SAFETY: locals outlive the calls; window manager state only, no message is sent.
        let ok = unsafe {
            GetClientRect(hwnd, &mut rect).is_ok() && ClientToScreen(hwnd, &mut origin).as_bool()
        };
        ok && x >= origin.x
            && y >= origin.y
            && x < origin.x + (rect.right - rect.left)
            && y < origin.y + (rect.bottom - rect.top)
    }

    fn key_presses(&self) -> u64 {
        self.keys.as_ref().map_or(0, KeyWatch::presses)
    }

    fn primary_down(&self) -> bool {
        let vk = match self.primary {
            Button::Right => VK_RBUTTON,
            _ => VK_LBUTTON,
        };
        // SAFETY: plain value argument. The high bit is set while the button is down; the
        // physical button is read, so a swap needs the matching key code (`primary`).
        unsafe { GetAsyncKeyState(vk.0 as i32) < 0 }
    }

    fn terminal_at(&self, point: (i32, i32)) -> bool {
        uia::terminal_at(point)
    }

    fn process_name(&self, pid: u32) -> Option<String> {
        uia::exe_name_of(pid)
    }

    fn send(&mut self, batch: Batch, guard: SendGuard) -> (Sent, Option<isize>) {
        let clip = self.clip.as_ref();
        // B3: right before the window checks, the exact staging must still own the clipboard.
        let precheck = || {
            guard
                .stage
                .is_none_or(|t| clip.is_some_and(|c| c.owns_staging(Ticket(t))))
        };
        let target = input::Guard {
            window: HWND(guard.window as *mut _),
            pid: guard.pid,
            focus: guard.focus.map(|f| HWND(f as *mut _)),
        };
        let (sent, focus) = input::send_guarded(&target, batch, precheck);
        (sent, focus.map(|f| f.0 as isize))
    }

    fn log(&mut self, now: f64, line: &str) {
        self.log.line(now, line);
    }

    fn note_at(&self, (x, y): (i32, i32)) -> bool {
        self.notes
            .as_ref()
            .is_some_and(|zones| zones.note_at(Point::new(f64::from(x), f64::from(y))).is_some())
    }

    fn give_to_note(&mut self, point: (i32, i32), cards: Vec<HeldCard>) {
        self.gifts.push((point, cards));
    }
}

/// The router and its Windows environment, for the UI loop. Dropping it (at the end of
/// `main`, on an error return or while a panic unwinds) puts the user's clipboard back if
/// a paste left ours there, then stops the workers.
pub struct Drops {
    router: Router<clipboard::Snapshot>,
    env: Option<LiveEnv>,
}

impl Drops {
    pub fn start(settings: Settings, log: DebugLog) -> Result<Drops, String> {
        let env = LiveEnv::start(settings, log)?;
        Ok(Drops {
            router: Router::new(LiveEnv::primary_button()),
            env: Some(env),
        })
    }

    pub fn wake_handles(&self) -> Vec<HANDLE> {
        self.env
            .as_ref()
            .map(LiveEnv::wake_handles)
            .unwrap_or_default()
    }

    /// Share the existing clipboard thread with screenshot detection.
    pub fn clipboard(&self) -> Option<&Clipboard> {
        self.env.as_ref()?.clip.as_ref()
    }

    pub fn set_settings(&mut self, settings: &Settings) {
        if let Some(env) = &mut self.env {
            env.settings = settings.clone();
        }
    }

    /// One pass of the UI loop: the hook events, then every answer and reply, then the
    /// deadline. Returns true when the state changed (redraw).
    pub fn run(&mut self, events: &[MouseEvent], state: &mut FairyState, now: f64) -> bool {
        let Some(env) = self.env.as_mut() else {
            return false;
        };
        for event in events {
            self.router.mouse(*event, state, env);
        }
        env.pump(&mut self.router, state, now);
        if self.router.next_deadline().is_some_and(|at| at <= now) {
            self.router.tick(now, state, env);
        }
        self.router.take_state_changed()
    }

    pub fn next_deadline(&self) -> Option<f64> {
        self.router.next_deadline()
    }

    /// The first step of [`Drop`], for session end: put the user's clipboard back if a paste
    /// left ours there, waiting up to `within` for the clipboard replies. The workers keep
    /// running; dropping `Drops` later finds nothing left to restore.
    pub fn restore_clipboard(&mut self, within: Duration) {
        if let Some(env) = &mut self.env {
            env.restore(&mut self.router, within);
        }
    }

    /// The second step of [`Drop`], for session end: stop the clipboard thread (bounded by its
    /// own wait). It renders what we still own, so the clipboard stays valid after exit.
    pub fn stop_clipboard(&mut self) {
        if let Some(env) = &mut self.env {
            env.clip = None;
        }
    }

    /// A double-click is being handled or a delivery runs.
    pub fn busy(&self) -> bool {
        self.router.busy()
    }

    /// A lane of the router's UI Automation worker for the note gesture
    /// (`notes::summoner::Summoner::start`).
    pub fn requester(&self) -> Result<Requester, String> {
        let uia = self.env.as_ref().and_then(|env| env.uia.as_ref());
        let uia = uia.ok_or("UI Automation is not running")?;
        uia.requester().map_err(|e| e.to_string())
    }

    /// The open notes' rectangles (None while the notes capability is off).
    pub fn set_note_zones(&mut self, zones: Option<DropZones>) {
        if let Some(env) = &mut self.env {
            env.notes = zones;
        }
    }

    /// Cards the router handed to a note since the last call: (point, cards top first).
    pub fn take_note_gifts(&mut self) -> Vec<((i32, i32), Vec<HeldCard>)> {
        self.env
            .as_mut()
            .map(|env| std::mem::take(&mut env.gifts))
            .unwrap_or_default()
    }

    /// The notes thread's `CardsTaken`. Returns true when the state changed (redraw).
    pub fn note_took(&mut self, ids: &[CardId], hint: &str, state: &mut FairyState, now: f64) -> bool {
        let Some(env) = self.env.as_mut() else {
            return false;
        };
        self.router.note_took(ids, hint, now, state, env);
        self.router.take_state_changed()
    }

    /// The notes thread's `Error`. True when it ended a wait for a note (the router showed
    /// its hint).
    pub fn note_failed(&mut self, state: &mut FairyState, now: f64) -> bool {
        let Some(env) = self.env.as_mut() else {
            return false;
        };
        self.router.note_failed(now, state, env)
    }
}

impl Drop for Drops {
    fn drop(&mut self) {
        if let Some(mut env) = self.env.take() {
            env.finish(&mut self.router, QUIT_WAIT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_saver_uses_the_capture_folder_from_each_job() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("paste-saver-test-{}", std::process::id()));
        let (done, results) = mpsc::channel();
        let (jobs, worker) = spawn_saver(done, Arc::new(WakeEvent::new().unwrap())).unwrap();
        let image = Arc::new(igorio_kit::Image::new(1, 1, vec![0, 0, 0, 255]).unwrap());
        for (id, name) in [(1, "first"), (2, "updated")] {
            let folder = root.join(name);
            jobs.send(SaveJob {
                id,
                image: image.clone(),
                scale: 1.0,
                captured_at: std::time::SystemTime::now(),
                dest: Dest::Pasted(Settings {
                    captures_folder: Some(folder.clone()),
                    ..Settings::default()
                }),
            })
            .unwrap();
            let (actual_id, saved) = results.recv_timeout(Duration::from_secs(3)).unwrap();
            assert_eq!(actual_id, id);
            let saved = saved.unwrap();
            assert_eq!(saved.parent(), Some(folder.join("Pasted").as_path()));
            std::fs::remove_file(saved).unwrap();
        }
        drop(jobs);
        worker.join().unwrap();
    }

    #[test]
    fn folder_saves_write_a_png_or_copy_the_movie_and_leave_no_temp_file() {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("folder-saver-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let movie = folder.join("source clip.mp4");
        std::fs::write(&movie, b"movie").unwrap();
        let into = folder.join("into");
        let (done, results) = mpsc::channel();
        let (jobs, worker) = spawn_saver(done, Arc::new(WakeEvent::new().unwrap())).unwrap();
        let image = Arc::new(igorio_kit::Image::new(1, 1, vec![0, 0, 0, 255]).unwrap());
        for (id, movie) in [(1, None), (2, Some(movie.clone()))] {
            jobs.send(SaveJob {
                id,
                image: image.clone(),
                scale: 1.0,
                captured_at: std::time::SystemTime::now(),
                dest: Dest::Folder {
                    folder: into.clone(),
                    movie,
                },
            })
            .unwrap();
            let (actual, saved) = results.recv_timeout(Duration::from_secs(3)).unwrap();
            assert_eq!(actual, id);
            let saved = saved.unwrap();
            assert_eq!(saved.parent(), Some(into.as_path()));
            let expected = if id == 1 { "png" } else { "mp4" };
            assert_eq!(saved.extension().unwrap(), expected);
        }
        drop(jobs);
        worker.join().unwrap();
        let names: Vec<String> = std::fs::read_dir(&into)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        assert!(names.contains(&"source clip.mp4".to_string()));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn paths_are_quoted_for_windows_shells() {
        assert_eq!(quoted_path(Path::new(r"C:\x\a.png")), r"C:\x\a.png ");
        assert_eq!(
            quoted_path(Path::new(
                r"C:\Users\me\Pictures\Screenshots\Pasted\Screenshot 2026-01-01 101010.png"
            )),
            "\"C:\\Users\\me\\Pictures\\Screenshots\\Pasted\\Screenshot 2026-01-01 101010.png\" "
        );
        assert_eq!(
            quoted_path(Path::new(r"C:\a&b\c.png")),
            "\"C:\\a&b\\c.png\" "
        );
    }

    #[test]
    fn canvas_apps_are_matched_by_exe() {
        assert_eq!(windows_app_id(Some("Figma.exe")), Some("com.figma.Desktop"));
        assert_eq!(
            windows_app_id(Some("POWERPNT.EXE")),
            Some("com.microsoft.Powerpoint")
        );
        assert_eq!(windows_app_id(Some("notepad.exe")), None);
        assert!(drop_policy::is_known_canvas_app(
            windows_app_id(Some("Figma.exe")),
            None
        ));
        assert!(drop_policy::is_known_canvas_app(
            windows_app_id(Some("miro.exe")),
            None
        ));
        assert!(drop_policy::is_known_canvas_app(
            windows_app_id(Some("Canva.exe")),
            None
        ));
    }

    #[test]
    fn modifier_hints_name_the_key() {
        assert_eq!(
            modifier_hint(Modifier::Shift),
            "let go of Shift to paste · still holding it"
        );
    }
}
