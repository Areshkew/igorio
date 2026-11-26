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
//!    `resolveAfterMouseUp` does. `SaveToFinder`: [`Router::save_to_folder`] (not done yet).
//! 4. Delivery (`deliverPaste` and `pasteRest`): the whole pile, top card first, one paste
//!    per card, 0.45 s apart. Per card (`PasteJob`): payload ready (a terminal gets a PNG
//!    saved to `Settings::pasted_dir` and its quoted path) and the user's clipboard saved,
//!    wait for the mouse-up, check the foreground, press Right / Escape, stage the clipboard,
//!    check again and press Ctrl+V, then watch the reads until `OutcomeRules` decides.
//! 5. Restore: when the delivery is over, `RestoreRules` decides when to put the user's
//!    clipboard back; overwrites are handled with `overwrite_step` meanwhile.
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

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use igorio_core::drop_policy::{self, Action, DoubleClick};
use igorio_core::editable::{AccessibilityNode, editable_self_or_ancestor, editable_target};
use igorio_core::geom::{CardId, Point, Rect};
use igorio_core::paste_policy::{
    self, MouseUpStep, Outcome as PasteOutcome, OutcomeRules, OverwriteStep, RestoreRules,
    RestoreStep, TerminalSettle,
};
use igorio_kit::HeldCard;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};

use crate::clipboard::{self, ClipError, Clipboard, Done, Payload, Reply};
use crate::hook::{self, Button, MouseEvent};
use crate::input::{self, Foreground, Key, Modifier, Sent};
use crate::saver;
use crate::settings::{DefaultFolders, Settings};
use crate::state::FairyState;
use crate::uia::{self, Lookup, Outcome, Snapshot, Target, Uia, UiaNode};

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
/// A stage request whose reply did not come in time is waited for this long after it was
/// sent, so a late write can still be undone.
pub const STAGE_LATE_WAIT: f64 = 10.0;
/// At quit, how long to wait for the clipboard replies that put the user's clipboard back.
pub const QUIT_WAIT: Duration = Duration::from_secs(1);

/// Roles (as `uia::map_role` gives them) where a double-click means "open", "maximise" or
/// "activate", never "paste here": window chrome (title bar, toolbar, status bar and app
/// bar map to `AXToolbar`), menus, tabs (`AXRadioButton`, `AXTabGroup`), list, tree and
/// grid items (`AXRow`), buttons, links and value controls. Checked on the hit and its
/// ancestors unless the hit itself takes text (an address bar sits inside a toolbar).
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
pub const HINT_FOLDERS_NEXT: &str = "saving to folders comes next";
pub const HINT_RESTORE_FAILED: &str = "couldn't put your clipboard back";

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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Focus {
    pub pid: u32,
    pub editable: bool,
    pub frame: Option<Rect>,
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
        }
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

/// The keys that paste in the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteKeys {
    CtrlV,
    CtrlShiftV,
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
    fn lookup(&mut self, point: (i32, i32), want_focus: bool, deadline: f64) -> u64;
    fn clip_facts(&self, now: f64) -> ClipFacts;
    fn clip_snapshot(&mut self) -> Result<u64, String>;
    fn clip_stage(&mut self, payload: Payload) -> Result<u64, String>;
    fn clip_restage(&mut self) -> Result<u64, String>;
    fn clip_restore(&mut self, snap: &Self::Snap, guard: RestoreGuard) -> Result<u64, String>;
    /// Saves the card as a PNG into the pasted folder, off the UI thread; the result comes
    /// as [`Router::saved`] with the returned id.
    fn save_png(&mut self, card: &HeldCard) -> u64;
    /// Is the target (its top-level window, or its process when `window` is 0) in front?
    fn foreground_is(&self, window: isize, pid: u32) -> bool;
    /// The user's main mouse button is down right now (the real state, not the hook's).
    fn primary_down(&self) -> bool;
    /// The top-level window at `point` is a terminal (Win32 only, no UI Automation).
    fn terminal_at(&self, point: (i32, i32)) -> bool;
    fn press(&mut self, key: Key) -> Sent;
    fn paste_shortcut(&mut self, keys: PasteKeys) -> Sent;
    /// One debug line. Only written when `FAIRY_DEBUG=1`.
    fn log(&mut self, now: f64, line: &str);
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

/// xterm.js marks its elements with `xterm…` classes; Chromium reports the class attribute
/// as the UI Automation class name.
fn is_web_terminal(chain: &[UiaNode]) -> bool {
    for node in chain.iter().take(8) {
        let e = node.element();
        if e.role == "AXWebArea" {
            break;
        }
        if e.class_name
            .split_whitespace()
            .any(|c| c.starts_with("xterm"))
        {
            return true;
        }
    }
    false
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
    spot.focused = snapshot.focused.as_ref().map(|f| Focus {
        pid: f.element().pid,
        editable: editable_self_or_ancestor(f).is_some(),
        frame: f.element().frame,
    });
    if spot.is_shell() {
        let ancestors: Vec<String> = chain[1..].iter().filter_map(|n| n.role()).collect();
        spot.drop_spot = drop_policy::is_drop_spot(Some(e.role), &ancestors);
        return spot;
    }
    spot.chromium = chain.iter().any(|n| n.element().framework == "Chrome");
    spot.canvas = canvas_facts(&chain, snapshot.target.exe.as_deref());
    let in_page = chain.iter().any(|n| n.element().role == "AXWebArea");
    if in_page && is_web_terminal(&chain) {
        spot.web_terminal = true;
        spot.editable = true;
        return spot;
    }
    // A terminal's text area (Windows Terminal, conhost) has a Text pattern but no Value
    // pattern, so UI Automation does not call it editable; it still takes pasted text, as
    // iTerm's `AXTextArea` does on macOS. Its tabs and title bar have no Text pattern.
    let terminal_text = snapshot.target.is_terminal && e.text_pattern;
    spot.editable_hit = terminal_text || editable_self_or_ancestor(&hit).is_some();
    spot.activating = !spot.editable_hit
        && chain
            .iter()
            .any(|n| ACTIVATING_ROLES.contains(&n.element().role));
    let at = Point::new(point.0 as f64, point.1 as f64);
    spot.editable = spot.editable_hit || editable_target(at, &hit).is_some();
    spot
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
}

impl Aim {
    fn new(target: &Target, element_pid: u32) -> Aim {
        Aim {
            pid: target.pid,
            window: target.window,
            element_pid,
            keys: paste_keys_for(target),
        }
    }

    /// A read counts as the target taking the paste when its reader is unknown or is the
    /// target's window process or element process. Chromium and Electron read the clipboard
    /// in the browser process, which owns the top-level window, so they count.
    fn is_reader(&self, reader: Option<u32>) -> bool {
        reader.is_none_or(|pid| pid == self.pid || pid == self.element_pid)
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
    Front(f64),
    Keys { keys: Vec<Key>, since: f64 },
    Gap(f64),
    Staging { ticket: u64, since: f64 },
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
    /// Reads seen in the facts so far, and those made by the target.
    seen_reads: u32,
    reads: u32,
    /// `last_change` and sequence number of the foreign write taken for the click's own
    /// copy-on-select.
    adopted: Option<f64>,
    adopted_seq: Option<u32>,
    guarding: bool,
    restaging: Option<u64>,
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
            seen_reads: 0,
            reads: 0,
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
fn restore_guard(stage: &StageRec, facts: &ClipFacts) -> Option<RestoreGuard> {
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

/// Adds the reads that appeared since the last look; reads by another process (a clipboard
/// manager, the user pasting elsewhere) are not the target taking the paste.
fn count_reads<E: Env>(stage: &mut StageRec, facts: &ClipFacts, now: f64, env: &mut E) {
    let Some(staged) = facts.staged_for(stage.ticket) else {
        return;
    };
    if staged.reads <= stage.seen_reads {
        return;
    }
    let new = staged.reads - stage.seen_reads;
    stage.seen_reads = staged.reads;
    if stage.aim.is_reader(staged.last_reader) {
        stage.reads += new;
    } else {
        env.log(
            now,
            &format!(
                "{new} read(s) by pid {:?}, not the target → not counted",
                staged.last_reader
            ),
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
            } if button == self.primary => {
                if !state.is_holding() {
                    return;
                }
                match click_count {
                    1 => self.remember_clipboard((x, y), time, env),
                    2 => self.double_click((x, y), time, state, env),
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
        self.lookups.remove(index);
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

    // MARK: Quit

    /// The app quits: stop any delivery and put the user's clipboard back if ours is still
    /// there. Returns a clipboard ticket to wait for (a stage still in flight, or the
    /// restore); feed its reply to [`Router::quit_reply`], which returns the next one.
    pub fn quit<E: Env<Snap = S>>(&mut self, now: f64, env: &mut E) -> Option<u64> {
        if self.busy() {
            env.log(now, "quit: the delivery stops here");
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
            env.log(now, "quit: waiting for the stage in flight");
            return Some(stage.ticket);
        }
        let facts = env.clip_facts(now);
        let Some(guard) = restore_guard(&stage, &facts) else {
            env.log(
                now,
                "quit: the clipboard changed since the paste → leave it",
            );
            self.restore = None;
            return None;
        };
        match env.clip_restore(&restore.snap, guard) {
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

    fn request(
        &mut self,
        point: (i32, i32),
        want_focus: bool,
        deadline: f64,
        now: f64,
        env: &mut impl Env,
    ) -> u64 {
        let id = env.lookup(point, want_focus, deadline);
        self.lookups.push(Pending {
            id,
            point,
            give_up_at: now + deadline + LOOKUP_GRACE,
        });
        id
    }

    fn double_click<E: Env<Snap = S>>(
        &mut self,
        point: (i32, i32),
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
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
        let id = self.request(point, false, LOOKUP_DEADLINE, now, env);
        env.log(
            now,
            &format!("double-click at {point:?}: looking (lookup {id})"),
        );
        self.phase = Phase::Looking {
            id,
            click: Click { point, at: now },
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
        match std::mem::replace(&mut self.phase, Phase::Idle) {
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
            Seen::Spot(spot) => {
                if spot.activating {
                    env.log(
                        now,
                        &format!(
                            "double-click at {:?}: {} role {} is window chrome or a control → Ignore",
                            click.point,
                            spot.target.exe.as_deref().unwrap_or("?"),
                            spot.role
                        ),
                    );
                    return;
                }
                facts.is_finder = spot.is_shell();
                facts.finder_drop_spot = spot.drop_spot;
                facts.editable_under_cursor = spot.editable_hit;
                first = Some(First {
                    aim: Aim::new(&spot.target, spot.pid),
                    pid: Some(spot.pid),
                    before: spot.canvas,
                    chromium: spot.chromium,
                    slow: false,
                });
                if spot.target.is_terminal || spot.web_terminal {
                    surface = Surface::Terminal;
                }
                format!(
                    "{} pid {} window {:#x} role {} editable={} terminal={} shell={:?} drop-spot={} canvas={}",
                    spot.target.exe.as_deref().unwrap_or("?"),
                    spot.pid,
                    spot.target.window,
                    spot.role,
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
            Seen::TimedOut { target } => {
                // A terminal too busy to answer is still a terminal (`silentTerminal`).
                facts.silent_terminal = target.is_terminal;
                facts.is_finder = target.shell.is_some();
                first = Some(First {
                    aim: Aim::new(target, target.pid),
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
        env.log(
            now,
            &format!("double-click at {:?}: {what} → {action:?}", click.point),
        );
        match (action, first) {
            (Action::SaveToFinder, _) => {
                if let Seen::Spot(spot) = &seen {
                    self.save_to_folder(spot, now, state, env);
                }
            }
            (Action::Paste, Some(first)) => {
                self.start_delivery(first.aim, surface, false, click.at, now, state, env);
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

    /// SaveToFinder: save the pile into the Explorer folder (or the desktop) under the
    /// cursor. NOT DONE YET: the folder lookup is being written in `explorer.rs`. Until it
    /// lands this only says so and keeps the cards. When it does, follow
    /// `DropRouter.saveToFinder`: find the folder, `saver::save_screenshot` / `copy_movie`
    /// each card, `deliver` the saved ones with "saved to <folder>", reject with
    /// "couldn't find that folder" / "couldn't save there" otherwise.
    fn save_to_folder<E: Env<Snap = S>>(
        &mut self,
        spot: &Spot,
        now: f64,
        state: &mut FairyState,
        env: &mut E,
    ) {
        env.log(
            now,
            &format!(
                "save to folder (window {:#x}): not done yet",
                spot.target.child_window
            ),
        );
        state.flash_hint(HINT_FOLDERS_NEXT, 2.4, now);
        self.state_changed = true;
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
        let focused = spot.focused.is_some_and(|f| f.editable);
        env.log(
            now,
            &format!(
                "after mouse-up: {} role {} editable={} focused={} canvas={} (before: {}) → {confirmed:?}",
                spot.target.exe.as_deref().unwrap_or("?"),
                spot.role,
                yes(spot.editable),
                yes(focused),
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
            ..first.aim
        };
        self.start_delivery(aim, surface, dismiss_editor, now, now, state, env);
    }

    // MARK: Delivery

    /// `deliverPaste`: the whole pile, top card first.
    #[allow(clippy::too_many_arguments)]
    fn start_delivery<E: Env<Snap = S>>(
        &mut self,
        aim: Aim,
        surface: Surface,
        dismiss_editor: bool,
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
                    Some(Rem::Ready { snap, lost, .. }) => {
                        if let Some(lost) = lost {
                            env.log(
                                now,
                                &format!("clipboard: the saved copy lost {lost} → do not paste"),
                            );
                            return Err(());
                        }
                        self.restore = Some(Restore::new(snap));
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
                d.job.step = Step::Front(now);
                Flow::Again
            }
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
                    if !env.foreground_is(d.aim.window, d.aim.pid) {
                        return self.fail(
                            d,
                            Self::fail_hint(d),
                            "the target left the front",
                            now,
                            state,
                            env,
                        );
                    }
                    match env.press(key) {
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
                        sent => {
                            env.log(now, &format!("pressed {key:?}: {sent:?}"));
                            keys.remove(0);
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
                if self.restore.is_none() {
                    return self.fail(d, HINT_CLIPBOARD, "no saved clipboard", now, state, env);
                }
                // Write the clipboard only now: a terminal's copy-on-select on the mouse-up
                // would replace an earlier write.
                match env.clip_stage(payload.clone()) {
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
                    // The request may still run: the restore waits for its reply.
                    return self.fail(
                        d,
                        Self::fail_hint(d),
                        "no stage reply in time (the restore waits for it)",
                        now,
                        state,
                        env,
                    );
                }
                Flow::Wait(Some(give_up))
            }
            Step::Send(since) => {
                // Rule 7: the foreground and the modifiers are checked right before.
                if !env.foreground_is(d.aim.window, d.aim.pid) {
                    return self.fail(
                        d,
                        Self::fail_hint(d),
                        "the target left the front before the paste keys",
                        now,
                        state,
                        env,
                    );
                }
                match env.paste_shortcut(d.aim.keys) {
                    Sent::Blocked(modifier) => {
                        let give_up = since + MODIFIER_WAIT;
                        if now >= give_up {
                            let hint = modifier_hint(modifier);
                            return self.fail(d, &hint, "a modifier stayed down", now, state, env);
                        }
                        Flow::Wait(Some(give_up.min(now + MODIFIER_POLL)))
                    }
                    sent => {
                        env.log(now, &format!("sent {:?}: {sent:?}", d.aim.keys));
                        if let Some(stage) = self.restore.as_mut().and_then(|r| r.stage.as_mut()) {
                            stage.sent_at = Some(now);
                        }
                        d.job.step = Step::Watch(now);
                        Flow::Again
                    }
                }
            }
            Step::Watch(sent_at) => {
                let facts = env.clip_facts(now);
                self.reconcile(&facts, now, env);
                let reads = self
                    .restore
                    .as_ref()
                    .and_then(|r| r.stage)
                    .map_or(0, |s| s.reads);
                let canvas = matches!(d.surface, Surface::Canvas | Surface::Document);
                let rules = OutcomeRules::new(false, canvas);
                match rules.evaluate(now - sent_at, reads, false) {
                    PasteOutcome::Pending => Flow::Wait(Some(now + OUTCOME_POLL)),
                    PasteOutcome::Accepted => {
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
                        env.log(
                            now,
                            &format!(
                                "card {}: the target read {reads} format(s) → accepted",
                                d.job.id.0
                            ),
                        );
                        state.deliver(&[d.job.id], &hint, now);
                        self.state_changed = true;
                        if d.ids[d.index + 1..]
                            .iter()
                            .any(|id| state.card(*id).is_some())
                        {
                            d.job.step = Step::Next(now + PILE_STEP);
                            Flow::Again
                        } else {
                            Flow::Done
                        }
                    }
                    PasteOutcome::Rejected => {
                        let hint = if d.job.movie {
                            HINT_NO_VIDEOS
                        } else {
                            HINT_NO_IMAGES
                        };
                        self.fail(d, hint, "the field did not change", now, state, env)
                    }
                    PasteOutcome::Failed => {
                        if d.job.first
                            && let Some(restore) = self.restore.as_mut()
                        {
                            restore.late = Some((d.job.id, reads));
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
                            "the target read nothing (still watching for a late read)",
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
        let Some(stage) = self.restore.as_mut().and_then(|r| r.stage.as_mut()) else {
            return;
        };
        count_reads(stage, facts, now, env);
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
        match paste_policy::overwrite_step(now - sent, stage.reads, staged.rewrites, stage.terminal)
        {
            OverwriteStep::Rewrite => match env.clip_restage() {
                Ok(ticket) => {
                    stage.restaging = Some(ticket);
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
                    self.restore = Some(Restore::new(snap.clone()));
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
                other => {
                    // Nothing was written: an earlier paste's staging (if any) is still there.
                    restore.stage = restore.prev.take();
                    Some(format!(
                        "stage failed, clipboard untouched: {:?}",
                        other.as_ref().err()
                    ))
                }
            };
            if let Some(why) = failure {
                if staging_now {
                    let Phase::Delivering(d) = std::mem::replace(&mut self.phase, Phase::Idle)
                    else {
                        unreachable!("checked above");
                    };
                    let _ = self.fail(&d, Self::fail_hint(&d), &why, now, state, env);
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
            && stage.restaging == Some(ticket)
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
                    let id = self.request(click.point, true, deadline, now, env);
                    env.log(
                        now,
                        &format!(
                            "mouse is up: looking again with focus (lookup {id}, {deadline:.2}s)"
                        ),
                    );
                    self.phase = Phase::Recheck { id, click, first };
                    return;
                }
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
        if restore.stage.is_none() {
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
        // The app was busy and took the Ctrl+V after we had given up: the top card did land.
        if let Some((id, before)) = restore.late
            && stage.reads > before
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
            // A stage request still in flight: its reply decides.
            let give_up = stage.requested_at + STAGE_LATE_WAIT;
            if now >= give_up {
                env.log(now, "the stage never answered → nothing to restore");
                self.restore = None;
            } else {
                self.restore_wake = Some(give_up);
            }
            return;
        }
        if stage.restaging.is_some() {
            self.restore_wake = Some(now + RESTORE_POLL);
            return;
        }
        let guard = restore_guard(&stage, &facts);
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
            (RestoreStep::Restore, Some(guard)) => match env.clip_restore(&restore.snap, guard) {
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
        let allowed = self
            .hover
            .last_request
            .map_or(f64::NEG_INFINITY, |t| t + HOVER_MIN_GAP);
        let at = rested.max(allowed);
        if now < at {
            self.hover_wake = Some(at);
            return;
        }
        let id = self.request(point, false, LOOKUP_DEADLINE, now, env);
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
        eprintln!("{line}");
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
    wake: Arc<WakeEvent>,
    saves: Sender<(u64, Result<PathBuf, String>)>,
    saved: Receiver<(u64, Result<PathBuf, String>)>,
    next_save: u64,
    settings: Settings,
    log: DebugLog,
    primary: Button,
    /// Converts the clipboard's `Instant`s to the `hook::now` clock.
    origin: Instant,
    origin_secs: f64,
}

impl LiveEnv {
    /// Starts the UI Automation worker and the clipboard thread.
    pub fn start(settings: Settings, log: DebugLog) -> Result<LiveEnv, String> {
        let wake = Arc::new(WakeEvent::new().map_err(|e| format!("wake event: {e}"))?);
        let uia = Uia::start().map_err(|e| format!("UI Automation: {e}"))?;
        let clip_wake = wake.clone();
        // Runs on the clipboard thread: only signals the event.
        let clip =
            Clipboard::start(move || clip_wake.set()).map_err(|e| format!("clipboard: {e}"))?;
        let (saves, saved) = mpsc::channel();
        Ok(LiveEnv {
            uia: Some(uia),
            clip: Some(clip),
            wake,
            saves,
            saved,
            next_save: 1,
            settings,
            log,
            primary: LiveEnv::primary_button(),
            origin: Instant::now(),
            origin_secs: hook::now(),
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
    /// clipboard changes and saved PNGs.
    pub fn wake_handles(&self) -> Vec<HANDLE> {
        let mut handles = vec![self.wake.0];
        if let Some(uia) = &self.uia {
            handles.push(uia.wake_handle());
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
        while let Ok((job, result)) = self.saved.try_recv() {
            router.saved(job, result, now, state, self);
        }
        router.clipboard_changed(now, state, self);
    }

    /// At quit: let the router put the user's clipboard back, waiting up to `timeout` for
    /// the clipboard replies it needs, then stop the threads.
    fn finish(&mut self, router: &mut Router<clipboard::Snapshot>, timeout: Duration) {
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
        // Dropping the clipboard renders what we still own, so the clipboard stays valid.
        self.clip = None;
        if let Some(uia) = self.uia.take() {
            let _ = uia.shutdown();
        }
    }
}

fn convert_reply(reply: Reply) -> ClipReply<clipboard::Snapshot> {
    let result = match reply.result {
        Ok(Done::Snapshot(snap)) => Ok(ClipDone::Snapshot {
            sequence: snap.sequence(),
            lost: snap.lost().map(|lost| format!("{lost:?}")),
            snap,
        }),
        Ok(Done::Staged { sequence }) => Ok(ClipDone::Staged { sequence }),
        Ok(Done::Restored { sequence }) => Ok(ClipDone::Restored { sequence }),
        Err(ClipError::Changed) => Err(ClipFail::Changed),
        Err(ClipError::Emptied { sequence, .. }) => Err(ClipFail::Emptied { sequence }),
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

    fn lookup(&mut self, point: (i32, i32), want_focus: bool, deadline: f64) -> u64 {
        let lookup = Lookup {
            point,
            want_focus,
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
            }),
        }
    }

    fn clip_snapshot(&mut self) -> Result<u64, String> {
        let clip = self.clip.as_ref().ok_or("no clipboard thread")?;
        clip.snapshot().map(|t| t.0).map_err(|e| e.to_string())
    }

    fn clip_stage(&mut self, payload: Payload) -> Result<u64, String> {
        let clip = self.clip.as_ref().ok_or("no clipboard thread")?;
        clip.stage(payload).map(|t| t.0).map_err(|e| e.to_string())
    }

    fn clip_restage(&mut self) -> Result<u64, String> {
        let clip = self.clip.as_ref().ok_or("no clipboard thread")?;
        clip.restage().map(|t| t.0).map_err(|e| e.to_string())
    }

    fn clip_restore(
        &mut self,
        snap: &clipboard::Snapshot,
        guard: RestoreGuard,
    ) -> Result<u64, String> {
        let clip = self.clip.as_ref().ok_or("no clipboard thread")?;
        let ticket = match guard {
            RestoreGuard::OwnStaging => clip.restore_over_staging(snap),
            RestoreGuard::Sequence(sequence) => clip.restore(snap, Some(sequence)),
        };
        ticket.map(|t| t.0).map_err(|e| e.to_string())
    }

    fn save_png(&mut self, card: &HeldCard) -> u64 {
        let job = self.next_save;
        self.next_save += 1;
        let image = card.image.clone();
        let (scale, captured_at) = (card.scale, card.captured_at);
        let settings = self.settings.clone();
        let saves = self.saves.clone();
        let wake = self.wake.clone();
        // The folder lookup, the encode and the write all run off the UI thread.
        std::thread::spawn(move || {
            let folder = settings.pasted_dir(&DefaultFolders::from_system());
            let result = saver::save_screenshot(&image, scale, captured_at, &folder)
                .map_err(|e| e.to_string());
            let _ = saves.send((job, result));
            wake.set();
        });
        job
    }

    fn foreground_is(&self, window: isize, pid: u32) -> bool {
        if window != 0 {
            input::foreground_is(Foreground::Window(HWND(window as *mut _)))
        } else {
            input::foreground_is(Foreground::Process(pid))
        }
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

    fn press(&mut self, key: Key) -> Sent {
        input::press(key)
    }

    fn paste_shortcut(&mut self, keys: PasteKeys) -> Sent {
        match keys {
            PasteKeys::CtrlV => input::paste_shortcut(),
            PasteKeys::CtrlShiftV => input::paste_shortcut_with_shift(),
        }
    }

    fn log(&mut self, now: f64, line: &str) {
        self.log.line(now, line);
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
