//! UI Automation: what is at a screen point, answered off the UI thread within a deadline.
//! From `AXNode.swift` and the lookup half of `DropSpotInfo.resolve` in `DropRouter.swift`.
//!
//! # Threads
//!
//! - The caller (the UI thread) only sends requests and reads answers: [`Uia::request`],
//!   then [`Uia::poll`] after [`Uia::wake_handle`] is signalled. It never calls UI
//!   Automation (rule 3). The wake is an auto-reset event rather than a callback, because
//!   the answer must be handled on the UI thread anyway and the UI loop already waits with
//!   `MsgWaitForMultipleObjectsEx`, which takes the handle as is. After a wake, call `poll`
//!   until it returns `None`.
//! - A controller thread owns the request slot and the deadlines. It answers "own process",
//!   "elevated" and "hung" from cheap Win32 calls (`WindowFromPoint`, `GetClassNameW`,
//!   process and token queries; none of them waits on another process, and
//!   `WindowFromPoint` only sends `WM_NCHITTEST` to windows of the calling thread, which
//!   owns none). It never calls UI Automation, so it can always post `TimedOut` on time.
//! - A worker thread owns the `IUIAutomation` object and every element it reads. It runs
//!   in the multithreaded apartment: Microsoft's UI Automation threading rules say a
//!   client must not call UI Automation from a thread that owns windows (our UI thread),
//!   and recommend MTA for client threads because an STA thread would have to pump
//!   messages while it waits for a provider, and calls that arrive on a non-pumping STA
//!   can deadlock. The worker owns no windows. No COM pointer leaves the worker thread:
//!   it turns everything into plain data ([`Snapshot`]) before sending it.
//!
//! # One request, one snapshot
//!
//! One cache request (`TreeScope_Element`, control view, no patterns, only the properties
//! below) is used for every call, so each element arrives with all its properties in one
//! cross-process round trip and the reads after that are local. Pattern objects are not
//! needed: the `Is*PatternAvailable` properties and `ValueIsReadOnly` answer every question
//! the rules ask. `TreeScope_Ancestors` is not supported in cache requests, so ancestors
//! are fetched one call each. A lookup makes, in order:
//!
//! 1. `ElementFromPointBuildCache`: 1 call.
//! 2. `GetFocusedElementBuildCache` (when `want_focus` and the foreground window is not
//!    ours): 1 call.
//! 3. `GetParentElementBuildCache` up to the top-level window, or in web content up to the
//!    page (`Document`): 1 call per level, at most [`MAX_ANCESTORS`].
//! 4. Only in a browser page: the page address (`ValueValue` of the `Document`): 1 call.
//! 5. Only when nothing editable was found and the target is not a terminal or the shell:
//!    one `FindAllBuildCache` under the nearest ancestors for editable descendants (the
//!    composer hidden under its placeholder label): 1 call.
//!
//! The total is capped at [`MAX_CALLS`] (rule 2); a typical lookup makes 6 to 14.
//!
//! # Deadline and hung apps
//!
//! Each request has a deadline (default [`DEFAULT_DEADLINE`], 250 ms as in `AXNode.swift`).
//! The worker checks it before every call and sets the `IUIAutomation2` connection and
//! transaction timeouts to the time left, so a call into a slow app returns
//! `UIA_E_TIMEOUT` around the deadline. The controller posts `TimedOut` at the deadline
//! whatever the worker is doing. If the worker is still inside a call [`STUCK_GRACE`] after
//! the deadline (an app that ignores the timeouts), the controller abandons that thread
//! and starts a fresh worker. At most [`MAX_WORKERS`] worker threads exist at once,
//! abandoned ones included; at the limit, requests time out until one returns. An abandoned
//! thread exits by itself when its call returns. Windows that `IsHungAppWindow` reports
//! as hung are answered `TimedOut` without calling UI Automation.
//!
//! Requests replace each other: only the newest waiting request runs. A request that is
//! replaced before it starts gets no answer.
//!
//! # Coordinates
//!
//! Points and frames are physical pixels with a top-left origin. UI Automation reports
//! physical pixels only to a Per-Monitor V2 aware process (the manifest sets it; dev builds
//! call `overlay::ensure_dpi_awareness`).

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use igorio_core::editable::{
    AccessibilityNode, DEFAULT_ANCESTOR_LEVELS, editable_self_or_ancestor,
};
use igorio_core::geom::Rect;
use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, HANDLE, HWND, POINT, VARIANT_BOOL, VARIANT_FALSE, VARIANT_TRUE,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, TokenUIAccess,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, SetEvent,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_BSTR, VT_I4, VariantClear,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetClassNameW, GetDesktopWindow, GetForegroundWindow,
    GetWindowThreadProcessId, IsHungAppWindow, WindowFromPoint,
};
use windows::core::{BOOL, BSTR, Error, HRESULT, Interface, PWSTR, Result};

/// How long a requester waits by default, as in `AXNode.swift`.
pub const DEFAULT_DEADLINE: Duration = Duration::from_millis(250);
/// Parent calls per lookup. `AXNode.chain` walks 12 levels too.
pub const MAX_ANCESTORS: usize = 12;
/// Cross-process calls per lookup: hit, focus, ancestors, page address, nearby search.
pub const MAX_CALLS: u32 = 4 + MAX_ANCESTORS as u32;
/// A worker still inside a call this long after its request's deadline is abandoned.
pub const STUCK_GRACE: Duration = Duration::from_millis(750);
/// Worker threads alive at once, abandoned ones included.
pub const MAX_WORKERS: usize = 3;
/// Terminals, matched by exe name (ASCII case-insensitive). A paste there types the path.
pub const TERMINAL_EXES: &[&str] = &[
    "WindowsTerminal.exe",
    "OpenConsole.exe",
    "conhost.exe",
    "wezterm-gui.exe",
    "alacritty.exe",
];
/// A classic console window is drawn by conhost.exe, but `GetWindowThreadProcessId` reports
/// the shell inside it (cmd.exe, pwsh.exe), so it is matched by class as well.
const TERMINAL_CLASSES: &[&str] = &["ConsoleWindowClass"];
/// `FrameworkId` of web content: Chromium (Chrome, Edge, Electron, WebView2), Firefox,
/// and the old EdgeHTML.
const WEB_FRAMEWORKS: &[&str] = &["Chrome", "Gecko", "MicrosoftEdge"];
/// Editable descendants kept from the nearby search.
const NEARBY_LIMIT: usize = 40;
const NAME_LIMIT: usize = 120;
const URL_LIMIT: usize = 2048;
const START_WAIT: Duration = Duration::from_secs(5);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);
const UIA_TIMEOUT: HRESULT = HRESULT(UIA_E_TIMEOUT as i32);

/// Every property the lookup reads, all fetched with the element in one round trip.
const CACHED_PROPERTIES: &[UIA_PROPERTY_ID] = &[
    UIA_ControlTypePropertyId,
    UIA_ClassNamePropertyId,
    UIA_NamePropertyId,
    UIA_FrameworkIdPropertyId,
    UIA_AriaRolePropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_ProcessIdPropertyId,
    UIA_NativeWindowHandlePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_IsKeyboardFocusablePropertyId,
    UIA_IsPasswordPropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_ValueIsReadOnlyPropertyId,
    UIA_IsTextPatternAvailablePropertyId,
    UIA_IsTextEditPatternAvailablePropertyId,
];

// ---------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------

/// Identifies a request in [`Uia::poll`]'s answers. Increases with every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lookup {
    /// Physical pixels, top-left origin.
    pub point: (i32, i32),
    /// Also read the focused element (the re-check after the mouse-up needs it).
    pub want_focus: bool,
    /// Counted from the `request` call.
    pub deadline: Duration,
}

impl Lookup {
    /// A lookup with focus and the default deadline.
    pub fn at(point: (i32, i32)) -> Lookup {
        Lookup {
            point,
            want_focus: true,
            deadline: DEFAULT_DEADLINE,
        }
    }
}

#[derive(Debug)]
pub enum Outcome {
    Found(Snapshot),
    /// The window (or element) at the point belongs to this process. UI Automation was not
    /// called for the window case.
    OwnProcess,
    /// The window belongs to a process with a higher integrity level and this process has
    /// no `uiAccess`: UI Automation and input cannot reach it.
    Elevated {
        pid: u32,
    },
    /// No answer in time. `target` is still known (from Win32), so a terminal that is too
    /// busy to answer can still be pasted into (`DoubleClick::silent_terminal`).
    TimedOut {
        target: Target,
    },
    Failed(String),
}

/// The window at the point, from Win32 (no UI Automation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Process of the top-level window.
    pub pid: u32,
    /// File name only, for example `WindowsTerminal.exe`. None when the process cannot be
    /// opened.
    pub exe: Option<String>,
    /// Top-level window (`HWND` value).
    pub window: isize,
    /// The window `WindowFromPoint` returned (for the Explorer folder lookup).
    pub child_window: isize,
    /// Class of the top-level window.
    pub class_name: String,
    pub is_terminal: bool,
    pub shell: Option<Shell>,
}

impl Target {
    pub fn hwnd(&self) -> HWND {
        HWND(self.window as *mut _)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// A File Explorer window (`CabinetWClass`).
    Explorer,
    /// The desktop (`Progman` or `WorkerW`).
    Desktop,
}

/// What UI Automation saw at the point: plain data that outlives the COM objects.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub target: Target,
    /// The element at the point. Its parents run up to the top-level window (`AXWindow`),
    /// or in web content up to the page (`AXWebArea` with `url`), unless `truncated`.
    pub hit: UiaNode,
    /// The focused element (system-wide; compare `element().pid`). It has no parents in the
    /// snapshot.
    pub focused: Option<UiaNode>,
    /// Cross-process calls this lookup made.
    pub calls: u32,
    /// Time spent on the worker.
    pub elapsed: Duration,
    /// The parent walk stopped before the top-level window (call cap or an error).
    pub truncated: bool,
}

/// The properties of one element. The last three fields are derived by the role mapping.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ElementInfo {
    /// `UIA_CONTROLTYPE_ID` value.
    pub control_type: i32,
    pub class_name: String,
    /// Clipped to 120 characters.
    pub name: String,
    pub framework: String,
    pub aria_role: String,
    /// Physical pixels. None when the element has no rectangle.
    pub frame: Option<Rect>,
    pub pid: u32,
    /// `NativeWindowHandle`, 0 for elements without their own window.
    pub hwnd: isize,
    pub enabled: bool,
    pub focusable: bool,
    pub password: bool,
    pub value_pattern: bool,
    /// None without a Value pattern.
    pub value_read_only: Option<bool>,
    pub text_pattern: bool,
    pub text_edit_pattern: bool,
    /// The element is the target's top-level window.
    pub top_level: bool,
    /// Address of a browser page (`Document` in a web framework).
    pub url: Option<String>,
    /// Derived: [`is_editable_text_root`].
    pub editable_root: bool,
    /// Derived: the macOS role ([`map_role`]).
    pub role: &'static str,
    /// Derived: the macOS subrole ([`map_role`]).
    pub subrole: Option<&'static str>,
}

impl ElementInfo {
    /// Fills the derived fields from the others.
    pub fn classify(&mut self) {
        self.editable_root = is_editable_text_root(self);
        let (role, subrole) = map_role(self);
        self.role = role;
        self.subrole = subrole;
    }
}

/// An element of a [`Snapshot`]: a cheap handle (clone keeps identity) that implements the
/// core trait, so `editable::editable_target` runs on it unchanged.
#[derive(Clone)]
pub struct UiaNode {
    tree: Arc<Tree>,
    index: usize,
}

impl UiaNode {
    pub fn element(&self) -> &ElementInfo {
        &self.tree.nodes[self.index].info
    }

    /// The same element of the same snapshot.
    pub fn same(&self, other: &UiaNode) -> bool {
        Arc::ptr_eq(&self.tree, &other.tree) && self.index == other.index
    }

    /// An element built from plain data, for tests of code that reads snapshots (the drop
    /// router). `elements` is the hit first, then its ancestors; each is classified. None
    /// when `elements` is empty.
    pub fn from_chain(elements: Vec<ElementInfo>) -> Option<UiaNode> {
        let mut tree = Tree::default();
        let mut previous = None;
        for mut info in elements {
            info.classify();
            previous = Some(match previous {
                None => tree.push_root(info),
                Some(child) => tree.push_parent(child, info),
            });
        }
        previous?;
        Some(UiaNode {
            tree: Arc::new(tree),
            index: 0,
        })
    }
}

impl std::fmt::Debug for UiaNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let e = self.element();
        write!(
            f,
            "UiaNode({} {:?} {:?})",
            e.role,
            control_type_name(e.control_type),
            e.name
        )
    }
}

impl AccessibilityNode for UiaNode {
    fn role(&self) -> Option<String> {
        Some(self.element().role.to_string())
    }

    fn subrole(&self) -> Option<String> {
        self.element().subrole.map(str::to_string)
    }

    fn frame(&self) -> Option<Rect> {
        self.element().frame
    }

    fn parent(&self) -> Option<Self> {
        self.tree.nodes[self.index].parent.map(|index| UiaNode {
            tree: self.tree.clone(),
            index,
        })
    }

    fn children(&self) -> Vec<Self> {
        self.tree.nodes[self.index]
            .children
            .iter()
            .map(|&index| UiaNode {
                tree: self.tree.clone(),
                index,
            })
            .collect()
    }

    /// Always false. UI Automation has no `AXEditableAncestor`: Chromium marks only the
    /// editable root (an `Edit`, or a node with a writable Value or a TextEdit pattern), not
    /// the text runs inside it. The resolver walks up from the hit and finds that root
    /// within its 12 levels (our snapshot has the same 12), and returns the root rather than
    /// an inner run, which is also the element a paste should focus.
    fn has_editable_ancestor_attribute(&self) -> bool {
        false
    }

    /// The element itself takes typed text ([`is_editable_text_root`]): how a contenteditable
    /// region without a textbox role, or a custom text view, looks.
    fn is_settable_text_value(&self) -> bool {
        self.element().editable_root
    }
}

/// The lookup service. Not `Sync`: use it from one thread (the UI thread).
pub struct Uia {
    inbox: Sender<Msg>,
    answers: Receiver<(RequestId, Outcome)>,
    answers_tx: Sender<(RequestId, Outcome)>,
    wake: Arc<Event>,
    next_id: AtomicU64,
    calls: Arc<AtomicU64>,
    controller: Option<JoinHandle<bool>>,
    #[cfg(test)]
    live: Arc<AtomicUsize>,
}

impl Uia {
    /// Starts the controller and the first worker, and waits until the worker has created
    /// its `IUIAutomation` object.
    pub fn start() -> Result<Uia> {
        let calls = Arc::new(AtomicU64::new(0));
        let backend = Win32Backend::new(calls.clone())?;
        Uia::with_backend(Arc::new(backend), calls, STUCK_GRACE)
    }

    /// Queues a lookup and returns at once. It replaces a lookup that is still waiting (that
    /// one gets no answer); a lookup that is already running finishes.
    pub fn request(&self, lookup: Lookup) -> RequestId {
        let id = RequestId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let msg = Msg::Request {
            id,
            lookup,
            at: Instant::now(),
        };
        if self.inbox.send(msg).is_err() {
            let _ = self.answers_tx.send((
                id,
                Outcome::Failed("UI Automation controller stopped".into()),
            ));
            self.wake.set();
        }
        id
    }

    /// The next answer, if any. Call until None after each wake.
    pub fn poll(&self) -> Option<(RequestId, Outcome)> {
        self.answers.try_recv().ok()
    }

    /// An auto-reset event, signalled after answers are queued. Valid while `self` lives.
    pub fn wake_handle(&self) -> HANDLE {
        self.wake.0
    }

    /// Cross-process UI Automation calls made since start (for tests and logs).
    pub fn calls_made(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// Stops the controller and the worker. True when the worker thread was joined; false
    /// when it was stuck inside a call (it is then left to exit on its own).
    pub fn shutdown(mut self) -> bool {
        self.stop()
    }

    fn stop(&mut self) -> bool {
        let Some(controller) = self.controller.take() else {
            return true;
        };
        let _ = self.inbox.send(Msg::Shutdown);
        controller.join().unwrap_or(false)
    }

    fn with_backend(
        backend: Arc<dyn Backend>,
        calls: Arc<AtomicU64>,
        grace: Duration,
    ) -> Result<Uia> {
        let wake = Arc::new(Event::new()?);
        let (inbox, inbox_rx) = mpsc::channel();
        let (answers_tx, answers) = mpsc::channel();
        let live = Arc::new(AtomicUsize::new(0));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = Worker::spawn(
            backend.clone(),
            1,
            inbox.clone(),
            live.clone(),
            Some(ready_tx),
        )
        .map_err(|err| Error::new(E_FAIL, format!("UI Automation worker: {err}")))?;
        match ready_rx.recv_timeout(START_WAIT) {
            Ok(Ok(())) => {}
            Ok(Err((code, message))) => return Err(Error::new(code, message)),
            Err(_) => return Err(Error::new(E_FAIL, "UI Automation worker did not start")),
        }
        let controller = Controller {
            backend,
            inbox: inbox_rx,
            outbox: inbox.clone(),
            answers: answers_tx.clone(),
            wake: wake.clone(),
            live: live.clone(),
            worker,
            next_gen: 2,
            pending: None,
            grace,
        };
        let controller = thread::Builder::new()
            .name("uia-controller".into())
            .spawn(move || controller.run())
            .map_err(|err| Error::new(E_FAIL, format!("UI Automation controller: {err}")))?;
        Ok(Uia {
            inbox,
            answers,
            answers_tx,
            wake,
            next_id: AtomicU64::new(1),
            calls,
            controller: Some(controller),
            #[cfg(test)]
            live,
        })
    }
}

impl Drop for Uia {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------
// Pure rules
// ---------------------------------------------------------------------------------------

/// Is `exe` (file name) or the window class a terminal?
pub fn is_terminal(exe: Option<&str>, class_name: &str) -> bool {
    exe.is_some_and(|exe| TERMINAL_EXES.iter().any(|t| t.eq_ignore_ascii_case(exe)))
        || TERMINAL_CLASSES.contains(&class_name)
}

/// File Explorer or the desktop, by the class of the top-level window.
pub fn shell_kind(class_name: &str) -> Option<Shell> {
    match class_name {
        "CabinetWClass" => Some(Shell::Explorer),
        "Progman" | "WorkerW" => Some(Shell::Desktop),
        _ => None,
    }
}

/// Can UIPI block us from this process? `target` is its integrity RID, None when its token
/// cannot be read: a same-user process at our level always lets us read it, so an
/// unreadable token means a higher level or another user's process, both out of reach.
fn is_elevated(own: u32, ui_access: bool, target: Option<u32>) -> bool {
    !ui_access && target.is_none_or(|level| level > own)
}

fn is_web(e: &ElementInfo) -> bool {
    WEB_FRAMEWORKS.contains(&e.framework.as_str())
}

/// A web page (or frame) that is not itself editable: the `AXWebArea` boundary.
fn is_page(e: &ElementInfo) -> bool {
    e.control_type == UIA_DocumentControlTypeId.0 && is_web(e) && !e.editable_root
}

/// Does this element itself take typed text? The Windows stand-in for macOS's settable
/// `AXValue` with a selection. In order:
/// - Password fields, disabled elements and a read-only Value pattern: no.
/// - A TextEdit pattern (WinUI, Chromium editables): yes.
/// - A writable Value pattern on a text-like element (Win32 Edit, WPF and WinUI TextBox,
///   Chrome `<input>`, a contenteditable root): yes. Non-text values (sliders, spinners)
///   are excluded by their roles in the core resolver.
/// - A native `Edit` or `Document` with a Text pattern that takes focus but has no Value
///   pattern (Word, Notepad's RichEdit): yes. Not for web content, where every node has a
///   Text pattern.
/// - An `Edit` without Value pattern that takes focus (old common controls): yes.
///
/// The text-range `IsReadOnly` attribute is not read: it costs three uncached calls over
/// the whole document (slow in Word and Chrome).
pub fn is_editable_text_root(e: &ElementInfo) -> bool {
    if e.password || !e.enabled {
        return false;
    }
    if e.value_pattern && e.value_read_only == Some(true) {
        return false;
    }
    if e.text_edit_pattern {
        return true;
    }
    let ct = UIA_CONTROLTYPE_ID(e.control_type);
    let text_like = [
        UIA_EditControlTypeId,
        UIA_ComboBoxControlTypeId,
        UIA_DocumentControlTypeId,
        UIA_TextControlTypeId,
        UIA_GroupControlTypeId,
        UIA_CustomControlTypeId,
        UIA_PaneControlTypeId,
    ]
    .contains(&ct);
    if e.value_pattern && e.value_read_only == Some(false) && (e.text_pattern || text_like) {
        return true;
    }
    let edit_or_document = ct == UIA_EditControlTypeId || ct == UIA_DocumentControlTypeId;
    if edit_or_document && !is_web(e) && e.text_pattern && e.focusable {
        return true;
    }
    ct == UIA_EditControlTypeId && e.focusable && !e.value_pattern
}

/// The macOS role (and subrole) the core rules expect for a UI Automation element. Uses the
/// input fields of `e`, not its derived ones.
#[allow(non_upper_case_globals)] // The SDK's control type constants, matched by name.
pub fn map_role(e: &ElementInfo) -> (&'static str, Option<&'static str>) {
    // macOS's window element; Chrome's top-level window is a Pane, so the handle decides.
    if e.top_level {
        return ("AXWindow", None);
    }
    let editable = is_editable_text_root(e);
    let aria = e.aria_role.as_str();
    let role = match UIA_CONTROLTYPE_ID(e.control_type) {
        UIA_EditControlTypeId => {
            if e.password {
                return ("AXTextField", Some("AXSecureTextField"));
            }
            if !editable {
                // A read-only or disabled field: macOS shows such text as a label, and an
                // editable role would make the core treat it as a paste target.
                "AXStaticText"
            } else if aria == "searchbox" {
                "AXSearchField"
            } else {
                // UI Automation has no multiline flag; both roles are editable to the core.
                "AXTextField"
            }
        }
        // An editable document (Word, Notepad, a designMode page) is a text area. A page
        // or a read-only document is the web-area boundary of the editable walk.
        UIA_DocumentControlTypeId if editable => "AXTextArea",
        UIA_DocumentControlTypeId => "AXWebArea",
        UIA_TextControlTypeId if editable => "AXTextArea",
        UIA_TextControlTypeId if aria == "heading" => "AXHeading",
        UIA_TextControlTypeId => "AXStaticText",
        UIA_ButtonControlTypeId if aria == "switch" => "AXCheckBox",
        UIA_ButtonControlTypeId => "AXButton",
        UIA_CheckBoxControlTypeId => "AXCheckBox",
        // An editable combo box takes text (AXComboBox); a drop-down list (Chrome <select>,
        // CBS_DROPDOWNLIST) has a read-only value and is macOS's pop-up button.
        UIA_ComboBoxControlTypeId if editable => "AXComboBox",
        UIA_ComboBoxControlTypeId => "AXPopUpButton",
        UIA_HyperlinkControlTypeId => "AXLink",
        // Chromium exposes <canvas> as an image; the canvas rule treats both alike.
        UIA_ImageControlTypeId => "AXImage",
        UIA_ListControlTypeId => "AXList",
        // Items are rows to the core: a double-click opens them, never a drop spot.
        UIA_ListItemControlTypeId | UIA_TreeItemControlTypeId | UIA_DataItemControlTypeId => {
            "AXRow"
        }
        UIA_TreeControlTypeId => "AXOutline",
        UIA_DataGridControlTypeId | UIA_TableControlTypeId => "AXTable",
        UIA_MenuControlTypeId => "AXMenu",
        UIA_MenuBarControlTypeId => "AXMenuBar",
        UIA_MenuItemControlTypeId => "AXMenuItem",
        UIA_SplitButtonControlTypeId => "AXMenuButton",
        UIA_ProgressBarControlTypeId => "AXProgressIndicator",
        UIA_RadioButtonControlTypeId => "AXRadioButton",
        UIA_ScrollBarControlTypeId => "AXScrollBar",
        UIA_ThumbControlTypeId => "AXValueIndicator",
        UIA_SliderControlTypeId => "AXSlider",
        UIA_SpinnerControlTypeId => "AXIncrementor",
        UIA_TabControlTypeId => "AXTabGroup",
        // macOS draws tabs as radio buttons.
        UIA_TabItemControlTypeId => "AXRadioButton",
        // Window chrome without a macOS element (title bar, status bar, app bar): a
        // toolbar, so it is neither a drop spot nor a canvas.
        UIA_ToolBarControlTypeId
        | UIA_TitleBarControlTypeId
        | UIA_StatusBarControlTypeId
        | UIA_AppBarControlTypeId => "AXToolbar",
        UIA_ToolTipControlTypeId => "AXHelpTag",
        UIA_HeaderItemControlTypeId => "AXButton",
        UIA_SeparatorControlTypeId => "AXSplitter",
        // A dialog or an MDI child: a window boundary.
        UIA_WindowControlTypeId => "AXWindow",
        // Generic containers. Editable ones are found by `is_settable_text_value`.
        UIA_GroupControlTypeId
        | UIA_PaneControlTypeId
        | UIA_HeaderControlTypeId
        | UIA_CalendarControlTypeId
        | UIA_SemanticZoomControlTypeId => "AXGroup",
        // Custom controls (and anything unknown) are macOS's unknown role.
        _ => "AXUnknown",
    };
    (role, None)
}

/// The SDK name of a control type, for logs.
pub fn control_type_name(control_type: i32) -> &'static str {
    const NAMES: [&str; 41] = [
        "Button",
        "Calendar",
        "CheckBox",
        "ComboBox",
        "Edit",
        "Hyperlink",
        "Image",
        "ListItem",
        "List",
        "Menu",
        "MenuBar",
        "MenuItem",
        "ProgressBar",
        "RadioButton",
        "ScrollBar",
        "Slider",
        "Spinner",
        "StatusBar",
        "Tab",
        "TabItem",
        "Text",
        "ToolBar",
        "ToolTip",
        "Tree",
        "TreeItem",
        "Custom",
        "Group",
        "Thumb",
        "DataGrid",
        "DataItem",
        "Document",
        "SplitButton",
        "Window",
        "Pane",
        "Header",
        "HeaderItem",
        "Table",
        "TitleBar",
        "Separator",
        "SemanticZoom",
        "AppBar",
    ];
    usize::try_from(control_type - UIA_ButtonControlTypeId.0)
        .ok()
        .and_then(|i| NAMES.get(i).copied())
        .unwrap_or("?")
}

enum Triage {
    Answer(Outcome),
    Run(Target),
}

/// What the controller does with a request, from the Win32 facts. Own process wins over
/// everything; a passed deadline wins over the rest.
fn triage(pre: Pre, expired: bool) -> Triage {
    if pre.own {
        return Triage::Answer(Outcome::OwnProcess);
    }
    if expired || pre.hung {
        return Triage::Answer(Outcome::TimedOut { target: pre.target });
    }
    if pre.elevated {
        return Triage::Answer(Outcome::Elevated {
            pid: pre.target.pid,
        });
    }
    Triage::Run(pre.target)
}

fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Snapshot tree
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Tree {
    nodes: Vec<Slot>,
}

#[derive(Debug)]
struct Slot {
    info: ElementInfo,
    parent: Option<usize>,
    children: Vec<usize>,
}

impl Tree {
    fn push_root(&mut self, info: ElementInfo) -> usize {
        self.nodes.push(Slot {
            info,
            parent: None,
            children: Vec::new(),
        });
        self.nodes.len() - 1
    }

    /// Adds `info` as the parent of `child`.
    fn push_parent(&mut self, child: usize, info: ElementInfo) -> usize {
        let index = self.push_root(info);
        self.nodes[index].children.push(child);
        self.nodes[child].parent = Some(index);
        index
    }

    fn push_child(&mut self, parent: usize, info: ElementInfo) -> usize {
        let index = self.push_root(info);
        self.nodes[index].parent = Some(parent);
        self.nodes[parent].children.push(index);
        index
    }
}

// ---------------------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------------------

enum Msg {
    Request {
        id: RequestId,
        lookup: Lookup,
        at: Instant,
    },
    Done {
        generation: u64,
        outcome: Option<Outcome>,
    },
    Exited {
        generation: u64,
    },
    Shutdown,
}

/// A request that passed the Win32 checks.
struct Job {
    id: RequestId,
    point: (i32, i32),
    want_focus: bool,
    deadline: Instant,
    target: Target,
}

/// Win32 facts about the window at a point.
struct Pre {
    target: Target,
    own: bool,
    elevated: bool,
    hung: bool,
}

/// What runs on the controller (Win32 checks) and on each worker (UI Automation).
/// The real one is [`Win32Backend`]; tests use a fake.
trait Backend: Send + Sync + 'static {
    /// Runs on the controller thread. Must not call UI Automation or wait on another process.
    fn precheck(&self, point: (i32, i32)) -> std::result::Result<Pre, String>;
    /// Runs once on each new worker thread; the session never leaves that thread.
    fn open(&self) -> Result<Box<dyn Session>>;
}

trait Session {
    /// None when the deadline passed (the controller has answered already).
    fn lookup(&mut self, job: &Job) -> Option<Outcome>;
}

struct Busy {
    id: RequestId,
    deadline: Instant,
    target: Target,
    answered: bool,
}

struct Worker {
    generation: u64,
    jobs: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    busy: Option<Busy>,
}

type Ready = SyncSender<std::result::Result<(), (HRESULT, String)>>;

impl Worker {
    fn spawn(
        backend: Arc<dyn Backend>,
        generation: u64,
        outbox: Sender<Msg>,
        live: Arc<AtomicUsize>,
        ready: Option<Ready>,
    ) -> std::io::Result<Worker> {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        live.fetch_add(1, Ordering::SeqCst);
        let guard = LiveGuard(live);
        let thread = thread::Builder::new()
            .name(format!("uia-worker-{generation}"))
            .spawn(move || {
                let _guard = guard;
                let mut session = backend.open();
                if let Some(ready) = ready {
                    let result = session
                        .as_ref()
                        .map(|_| ())
                        .map_err(|err| (err.code(), err.message()));
                    let _ = ready.send(result);
                }
                while let Ok(job) = job_rx.recv() {
                    let outcome = match &mut session {
                        Ok(session) => session.lookup(&job),
                        Err(err) => Some(Outcome::Failed(format!("UI Automation: {err}"))),
                    };
                    if outbox
                        .send(Msg::Done {
                            generation,
                            outcome,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                // Release the COM objects (and leave the apartment) before saying goodbye.
                drop(session);
                let _ = outbox.send(Msg::Exited { generation });
            })?;
        Ok(Worker {
            generation,
            jobs: Some(jobs),
            thread: Some(thread),
            busy: None,
        })
    }
}

/// Counts live worker threads, abandoned ones included.
struct LiveGuard(Arc<AtomicUsize>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Controller {
    backend: Arc<dyn Backend>,
    inbox: Receiver<Msg>,
    outbox: Sender<Msg>,
    answers: Sender<(RequestId, Outcome)>,
    wake: Arc<Event>,
    live: Arc<AtomicUsize>,
    worker: Worker,
    next_gen: u64,
    /// The newest request not yet given to the worker.
    pending: Option<Job>,
    grace: Duration,
}

impl Controller {
    /// Returns whether the last worker was joined.
    fn run(mut self) -> bool {
        loop {
            let msg = match self.next_wake() {
                Some(at) => self
                    .inbox
                    .recv_timeout(at.saturating_duration_since(Instant::now())),
                None => self
                    .inbox
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
            };
            let mut stop = match msg {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => true,
            };
            // Take everything already queued before dispatching, so only the newest runs.
            while !stop && let Ok(msg) = self.inbox.try_recv() {
                stop = self.handle(msg);
            }
            if stop {
                break;
            }
            self.tick(Instant::now());
        }
        self.finish()
    }

    /// Returns true on shutdown.
    fn handle(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Request { id, lookup, at } => {
                let deadline = at + lookup.deadline;
                let pre = match self.backend.precheck(lookup.point) {
                    Ok(pre) => pre,
                    Err(err) => {
                        self.answer(id, Outcome::Failed(err));
                        return false;
                    }
                };
                match triage(pre, Instant::now() >= deadline) {
                    Triage::Answer(outcome) => self.answer(id, outcome),
                    // Replaces the waiting request, which gets no answer.
                    Triage::Run(target) => {
                        self.pending = Some(Job {
                            id,
                            point: lookup.point,
                            want_focus: lookup.want_focus,
                            deadline,
                            target,
                        })
                    }
                }
            }
            Msg::Done {
                generation,
                outcome,
            } => {
                if generation == self.worker.generation
                    && let Some(busy) = self.worker.busy.take()
                    && !busy.answered
                {
                    let outcome = outcome.unwrap_or(Outcome::TimedOut {
                        target: busy.target,
                    });
                    self.answer(busy.id, outcome);
                }
            }
            Msg::Exited { generation } => {
                // Only happens when the worker's channel broke; start a fresh one on demand.
                if generation == self.worker.generation {
                    self.worker.jobs = None;
                }
            }
            Msg::Shutdown => return true,
        }
        false
    }

    fn next_wake(&self) -> Option<Instant> {
        // Past the grace time the worker is replaced, or (at the thread cap) the next
        // message (an abandoned worker exiting) wakes the loop: no timer then.
        let now = Instant::now();
        let busy = self.worker.busy.as_ref().and_then(|b| {
            if b.answered {
                Some(b.deadline + self.grace).filter(|&at| at > now)
            } else {
                Some(b.deadline)
            }
        });
        let pending = self.pending.as_ref().map(|job| job.deadline);
        match (busy, pending) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn tick(&mut self, now: Instant) {
        if let Some(busy) = &mut self.worker.busy
            && !busy.answered
            && now >= busy.deadline
        {
            busy.answered = true;
            let (id, target) = (busy.id, busy.target.clone());
            self.answer(id, Outcome::TimedOut { target });
        }
        if let Some(job) = self.pending.take_if(|job| now >= job.deadline) {
            self.answer(job.id, Outcome::TimedOut { target: job.target });
        }
        let stuck = self
            .worker
            .busy
            .as_ref()
            .is_some_and(|b| now >= b.deadline + self.grace);
        if (stuck || self.worker.jobs.is_none()) && self.live.load(Ordering::SeqCst) < MAX_WORKERS {
            self.replace_worker();
        }
        if self.worker.busy.is_none()
            && let Some(job) = self.pending.take()
        {
            let busy = Busy {
                id: job.id,
                deadline: job.deadline,
                target: job.target.clone(),
                answered: false,
            };
            match self.worker.jobs.as_ref().map(|jobs| jobs.send(job)) {
                Some(Ok(())) => self.worker.busy = Some(busy),
                _ => {
                    self.worker.jobs = None;
                    self.answer(
                        busy.id,
                        Outcome::Failed("UI Automation worker stopped".into()),
                    );
                }
            }
        }
    }

    /// Abandons the current worker (it exits by itself when its call returns) and starts a
    /// fresh one.
    fn replace_worker(&mut self) {
        let generation = self.next_gen;
        self.next_gen += 1;
        match Worker::spawn(
            self.backend.clone(),
            generation,
            self.outbox.clone(),
            self.live.clone(),
            None,
        ) {
            // Dropping the old `Worker` closes its job channel and detaches its thread.
            Ok(worker) => self.worker = worker,
            Err(_) => self.worker.jobs = None,
        }
    }

    /// Closes the worker's job channel and joins it if it exits within `SHUTDOWN_WAIT`.
    fn finish(mut self) -> bool {
        self.worker.jobs = None;
        let generation = self.worker.generation;
        let until = Instant::now() + SHUTDOWN_WAIT;
        loop {
            match self
                .inbox
                .recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                Ok(Msg::Exited { generation: g }) if g == generation => {
                    return self.worker.thread.take().is_some_and(|t| t.join().is_ok());
                }
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }

    fn answer(&self, id: RequestId, outcome: Outcome) {
        if self.answers.send((id, outcome)).is_ok() {
            self.wake.set();
        }
    }
}

// ---------------------------------------------------------------------------------------
// Win32 backend
// ---------------------------------------------------------------------------------------

struct Win32Backend {
    own_pid: u32,
    own_integrity: u32,
    ui_access: bool,
    calls: Arc<AtomicU64>,
}

impl Win32Backend {
    fn new(calls: Arc<AtomicU64>) -> Result<Win32Backend> {
        // SAFETY: the pseudo handle of the current process needs no closing.
        let process = unsafe { GetCurrentProcess() };
        let token = Handle::token(process)?;
        let mut ui_access = 0u32;
        let mut size = 0;
        // SAFETY: the buffer is a u32 of the size given, as `TokenUIAccess` returns.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUIAccess,
                Some((&raw mut ui_access).cast()),
                size_of::<u32>() as u32,
                &mut size,
            )?
        };
        Ok(Win32Backend {
            // SAFETY: no arguments.
            own_pid: unsafe { GetCurrentProcessId() },
            own_integrity: token.integrity()?,
            ui_access: ui_access != 0,
            calls,
        })
    }
}

impl Backend for Win32Backend {
    fn precheck(&self, (x, y): (i32, i32)) -> std::result::Result<Pre, String> {
        // SAFETY: value arguments only. It sends WM_NCHITTEST only to windows of the calling
        // thread, and this thread owns none, so it never waits on another process.
        let child = unsafe { WindowFromPoint(POINT { x, y }) };
        if child.is_invalid() {
            return Err(format!("no window at ({x}, {y})"));
        }
        // SAFETY: value arguments only; reads window manager state.
        let root = unsafe { GetAncestor(child, GA_ROOT) };
        let root = if root.is_invalid() { child } else { root };
        let pid = window_pid(root);
        let child_pid = window_pid(child);
        let class_name = class_name(root);
        let own = pid == self.own_pid || child_pid == self.own_pid;
        let mut target = Target {
            pid,
            exe: None,
            window: root.0 as isize,
            child_window: child.0 as isize,
            shell: shell_kind(&class_name),
            is_terminal: false,
            class_name,
        };
        if own {
            return Ok(Pre {
                target,
                own,
                elevated: false,
                hung: false,
            });
        }
        let process = Handle::process(pid);
        target.exe = process.as_ref().and_then(Handle::exe_name);
        target.is_terminal = is_terminal(target.exe.as_deref(), &target.class_name);
        let level =
            |handle: Option<Handle>| handle.and_then(|h| Handle::token(h.0).ok()?.integrity().ok());
        let mut elevated = is_elevated(self.own_integrity, self.ui_access, level(process));
        if child_pid != pid && child_pid != 0 {
            elevated |= is_elevated(
                self.own_integrity,
                self.ui_access,
                level(Handle::process(child_pid)),
            );
        }
        // SAFETY: value argument only; reads window manager state, sends no message.
        let hung = unsafe { IsHungAppWindow(root) }.as_bool();
        Ok(Pre {
            target,
            own,
            elevated,
            hung,
        })
    }

    fn open(&self) -> Result<Box<dyn Session>> {
        UiaSession::open(self.own_pid, self.calls.clone()).map(|s| Box::new(s) as Box<dyn Session>)
    }
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0;
    // SAFETY: `pid` outlives the call; an invalid window leaves it 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn class_name(hwnd: HWND) -> String {
    let mut name = [0u16; 256];
    // SAFETY: the buffer outlives the call and its length is passed with it. Reads the class
    // from the window manager; no message is sent.
    let len = unsafe { GetClassNameW(hwnd, &mut name) };
    String::from_utf16_lossy(&name[..len.max(0) as usize])
}

/// A kernel handle closed on drop.
struct Handle(HANDLE);

impl Handle {
    fn process(pid: u32) -> Option<Handle> {
        // SAFETY: value arguments; the handle is owned by the returned `Handle`.
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .ok()
            .map(Handle)
    }

    fn token(process: HANDLE) -> Result<Handle> {
        let mut token = HANDLE::default();
        // SAFETY: `token` outlives the call; the handle is owned by the returned `Handle`.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token)? };
        Ok(Handle(token))
    }

    fn exe_name(&self) -> Option<String> {
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        // SAFETY: the buffer outlives the call and `len` holds its size in characters.
        unsafe {
            QueryFullProcessImageNameW(
                self.0,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut len,
            )
        }
        .ok()?;
        let path = String::from_utf16_lossy(&path[..len as usize]);
        path.rsplit('\\').next().map(str::to_string)
    }

    /// The mandatory integrity RID of a token (0x2000 medium, 0x3000 high).
    fn integrity(&self) -> Result<u32> {
        // Aligned room for TOKEN_MANDATORY_LABEL and the SID it points into.
        let mut buffer = [0u64; 16];
        let mut size = 0;
        let class: TOKEN_INFORMATION_CLASS = TokenIntegrityLevel;
        // SAFETY: the buffer outlives the call and its size is passed with it.
        unsafe {
            GetTokenInformation(
                self.0,
                class,
                Some(buffer.as_mut_ptr().cast()),
                size_of_val(&buffer) as u32,
                &mut size,
            )?
        };
        // SAFETY: on success the buffer starts with a TOKEN_MANDATORY_LABEL whose SID points
        // into the same buffer, which is still alive; the SID has at least one sub-authority.
        unsafe {
            let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
            let count = *GetSidSubAuthorityCount(label.Label.Sid);
            Ok(*GetSidSubAuthority(
                label.Label.Sid,
                u32::from(count.saturating_sub(1)),
            ))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by `self` and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The wake event.
struct Event(HANDLE);

// SAFETY: an event handle is a process-wide kernel object reference; SetEvent, waits and
// CloseHandle may be called from any thread.
unsafe impl Send for Event {}
// SAFETY: as above; `set` takes `&self` and the handle never changes.
unsafe impl Sync for Event {}

impl Event {
    fn new() -> Result<Event> {
        // SAFETY: no pointers; auto-reset, initially not signalled, unnamed.
        unsafe { CreateEventW(None, false, false, None) }.map(Event)
    }

    fn set(&self) {
        // SAFETY: the handle is alive while `self` is.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by `self` and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// ---------------------------------------------------------------------------------------
// UI Automation session (worker thread only)
// ---------------------------------------------------------------------------------------

/// COM initialised as MTA for the life of the value.
struct Apartment;

impl Apartment {
    fn enter() -> Result<Apartment> {
        // SAFETY: no reserved pointer; balanced by `CoUninitialize` in `drop`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Apartment)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` in `enter`, on the same thread.
        unsafe { CoUninitialize() };
    }
}

/// Lives on one worker thread. Fields drop in order, so the COM objects are released
/// before the apartment is left.
struct UiaSession {
    automation: IUIAutomation,
    timeouts: Option<IUIAutomation2>,
    walker: IUIAutomationTreeWalker,
    cache: IUIAutomationCacheRequest,
    nearby: IUIAutomationCondition,
    own_pid: u32,
    calls: Arc<AtomicU64>,
    _apartment: Apartment,
}

impl UiaSession {
    fn open(own_pid: u32, calls: Arc<AtomicU64>) -> Result<UiaSession> {
        let apartment = Apartment::enter()?;
        // SAFETY: COM is initialised on this thread; the class and interface match.
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)? };
        let timeouts = automation.cast::<IUIAutomation2>().ok();
        // SAFETY: in-process calls on the object created above.
        let (walker, cache) = unsafe {
            (
                automation.ControlViewWalker()?,
                automation.CreateCacheRequest()?,
            )
        };
        for &property in CACHED_PROPERTIES {
            // SAFETY: in-process call on the cache request created above.
            unsafe { cache.AddProperty(property)? };
        }
        let nearby = nearby_condition(&automation)?;
        Ok(UiaSession {
            automation,
            timeouts,
            walker,
            cache,
            nearby,
            own_pid,
            calls,
            _apartment: apartment,
        })
    }

    fn element_info(&self, element: &IUIAutomationElement, window: isize) -> ElementInfo {
        let mut info = read_cached(element);
        info.top_level = info.hwnd != 0 && info.hwnd == window;
        info.classify();
        info
    }
}

impl Session for UiaSession {
    fn lookup(&mut self, job: &Job) -> Option<Outcome> {
        let started = Instant::now();
        let mut budget = Budget {
            deadline: job.deadline,
            calls: 0,
            total: &self.calls,
            timeouts: self.timeouts.as_ref(),
        };
        let window = job.target.window;
        let point = POINT {
            x: job.point.0,
            y: job.point.1,
        };

        // 1. The element at the point.
        // SAFETY: COM objects of this thread; the cache request lives across the call.
        let hit = match budget.call(|| unsafe {
            self.automation
                .ElementFromPointBuildCache(point, &self.cache)
        }) {
            Err(_) => return None,
            Ok(Err(err)) => return budget.failed("ElementFromPoint", &err),
            Ok(Ok(hit)) => hit,
        };
        let mut tree = Tree::default();
        let info = self.element_info(&hit, window);
        if info.pid == self.own_pid {
            return Some(Outcome::OwnProcess);
        }
        let hit_index = tree.push_root(info);

        // 2. The focused element, unless focus is in our own process (rule 3).
        let mut focused = None;
        if job.want_focus && !foreground_is(self.own_pid) {
            // SAFETY: as above.
            match budget
                .call(|| unsafe { self.automation.GetFocusedElementBuildCache(&self.cache) })
            {
                Err(Stop::Expired) => return None,
                Err(Stop::Cap) | Ok(Err(_)) => {}
                Ok(Ok(element)) => {
                    let info = self.element_info(&element, window);
                    if info.pid != self.own_pid {
                        focused = Some(tree.push_root(info));
                    }
                }
            }
        }

        // 3. Ancestors up to the top-level window, or up to the page in web content: above
        // the page is browser chrome (6 to 12 more levels in Chrome and Vivaldi) that no
        // rule looks at, since the editable walk stops at the page.
        // SAFETY: no arguments.
        let desktop = unsafe { GetDesktopWindow() }.0 as isize;
        let mut chain = vec![(hit_index, hit)];
        let mut truncated = false;
        loop {
            let (index, element) = chain.last().map(|(i, e)| (*i, e.clone())).expect("the hit");
            let info = &tree.nodes[index].info;
            if info.top_level || is_page(info) {
                break;
            }
            if chain.len() > MAX_ANCESTORS {
                truncated = true;
                break;
            }
            // SAFETY: as above; `element` is alive in `chain`.
            match budget.call(|| unsafe {
                self.walker
                    .GetParentElementBuildCache(&element, &self.cache)
            }) {
                Err(Stop::Expired) => return None,
                Err(Stop::Cap) => {
                    truncated = true;
                    break;
                }
                // windows-rs reports the root's "no parent" (S_OK with null) as an error
                // with a success code.
                Ok(Err(err)) if err.code().is_ok() => break,
                Ok(Err(_)) => {
                    if budget.expired() {
                        return None;
                    }
                    truncated = true;
                    break;
                }
                Ok(Ok(parent)) => {
                    let info = self.element_info(&parent, window);
                    if info.hwnd != 0 && info.hwnd == desktop {
                        break;
                    }
                    let index = tree.push_parent(index, info);
                    chain.push((index, parent));
                }
            }
        }

        // 4. The address of a browser page.
        let page = chain.iter().find(|(i, _)| is_page(&tree.nodes[*i].info));
        if let Some((index, element)) = page {
            // SAFETY: as above.
            match budget.call(|| unsafe {
                element.GetCurrentPropertyValueEx(UIA_ValueValuePropertyId, true)
            }) {
                Err(Stop::Expired) => return None,
                Err(Stop::Cap) | Ok(Err(_)) => {}
                Ok(Ok(value)) => {
                    tree.nodes[*index].info.url = take_string(value).map(|u| clip(&u, URL_LIMIT))
                }
            }
        }

        // 5. An editable element near the hit (a composer under its placeholder label).
        let tree = Arc::new(tree);
        let hit_node = UiaNode {
            tree: tree.clone(),
            index: hit_index,
        };
        let mut tree = tree;
        let search_under = nearby_ancestor(&chain, &tree);
        if job.target.shell.is_none()
            && !job.target.is_terminal
            && editable_self_or_ancestor(&hit_node).is_none()
            && let Some((index, element)) = search_under
        {
            drop(hit_node);
            // SAFETY: as above.
            let found = budget.call(|| unsafe {
                element.FindAllBuildCache(TreeScope_Descendants, &self.nearby, &self.cache)
            });
            match found {
                Err(Stop::Expired) => return None,
                Err(Stop::Cap) | Ok(Err(_)) => {}
                Ok(Ok(array)) => {
                    let tree = Arc::get_mut(&mut tree).expect("no other handle to the tree yet");
                    // SAFETY: in-process reads of the array UI Automation returned.
                    let count = unsafe { array.Length() }.unwrap_or(0).max(0) as usize;
                    for i in 0..count.min(NEARBY_LIMIT) {
                        // SAFETY: `i` is below the array length; the element is cached locally.
                        if let Ok(element) = unsafe { array.GetElement(i as i32) } {
                            let info = self.element_info(&element, window);
                            tree.push_child(index, info);
                        }
                    }
                }
            }
        }
        if budget.expired() {
            return None;
        }
        Some(Outcome::Found(Snapshot {
            target: job.target.clone(),
            hit: UiaNode {
                tree: tree.clone(),
                index: hit_index,
            },
            focused: focused.map(|index| UiaNode { tree, index }),
            calls: budget.calls,
            elapsed: started.elapsed(),
            truncated,
        }))
    }
}

/// The highest of the first `DEFAULT_ANCESTOR_LEVELS` ancestors below a boundary role: the
/// resolver searches under these, and one search under the highest covers the others.
fn nearby_ancestor<'a>(
    chain: &'a [(usize, IUIAutomationElement)],
    tree: &Tree,
) -> Option<(usize, &'a IUIAutomationElement)> {
    let mut found = None;
    for (index, element) in chain.iter().skip(1).take(DEFAULT_ANCESTOR_LEVELS) {
        let role = tree.nodes[*index].info.role;
        if ["AXWindow", "AXApplication", "AXWebArea", "AXSheet"].contains(&role) {
            break;
        }
        found = Some((*index, element));
    }
    found
}

/// Elements that may take text: a TextEdit pattern, a writable Value pattern, or an Edit.
fn nearby_condition(automation: &IUIAutomation) -> Result<IUIAutomationCondition> {
    // SAFETY: in-process calls; the variants hold no pointers and outlive the calls.
    unsafe {
        let text_edit = automation.CreatePropertyCondition(
            UIA_IsTextEditPatternAvailablePropertyId,
            &bool_variant(true),
        )?;
        let value = automation
            .CreatePropertyCondition(UIA_IsValuePatternAvailablePropertyId, &bool_variant(true))?;
        let writable = automation
            .CreatePropertyCondition(UIA_ValueIsReadOnlyPropertyId, &bool_variant(false))?;
        let edit = automation.CreatePropertyCondition(
            UIA_ControlTypePropertyId,
            &i32_variant(UIA_EditControlTypeId.0),
        )?;
        let value_writable = automation.CreateAndCondition(&value, &writable)?;
        automation.CreateOrConditionFromNativeArray(&[
            Some(text_edit),
            Some(value_writable),
            Some(edit),
        ])
    }
}

fn foreground_is(pid: u32) -> bool {
    // SAFETY: no arguments; reads window manager state.
    let foreground = unsafe { GetForegroundWindow() };
    !foreground.is_invalid() && window_pid(foreground) == pid
}

enum Stop {
    Expired,
    Cap,
}

/// Counts cross-process calls and enforces the deadline and the cap.
struct Budget<'a> {
    deadline: Instant,
    calls: u32,
    total: &'a AtomicU64,
    timeouts: Option<&'a IUIAutomation2>,
}

impl Budget<'_> {
    fn call<T>(&mut self, f: impl FnOnce() -> Result<T>) -> std::result::Result<Result<T>, Stop> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(1) {
            return Err(Stop::Expired);
        }
        if self.calls >= MAX_CALLS {
            return Err(Stop::Cap);
        }
        if let Some(timeouts) = self.timeouts {
            let ms = left.as_millis().clamp(1, u32::MAX as u128) as u32;
            // SAFETY: in-process setters on this thread's automation object.
            unsafe {
                let _ = timeouts.SetConnectionTimeout(ms);
                let _ = timeouts.SetTransactionTimeout(ms);
            }
        }
        self.calls += 1;
        self.total.fetch_add(1, Ordering::Relaxed);
        Ok(f())
    }

    fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// None (timed out) for a UI Automation timeout or a passed deadline.
    fn failed(&self, what: &str, err: &Error) -> Option<Outcome> {
        if err.code() == UIA_TIMEOUT || self.expired() {
            None
        } else {
            Some(Outcome::Failed(format!("{what}: {err}")))
        }
    }
}

fn read_cached(element: &IUIAutomationElement) -> ElementInfo {
    let text = |value: Result<BSTR>| {
        value
            .map(|s| clip(&s.to_string(), NAME_LIMIT))
            .unwrap_or_default()
    };
    let flag = |value: Result<BOOL>, default: bool| value.map_or(default, |b| b.as_bool());
    let cached = |property| cached_bool(element, property);
    // SAFETY: `element` is a live element of this thread, and the Cached* getters only read
    // the cache UI Automation filled when the element arrived: no cross-process call.
    unsafe {
        let frame = element.CachedBoundingRectangle().ok().and_then(|r| {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            (w > 0 && h > 0).then(|| Rect::new(r.left as f64, r.top as f64, w as f64, h as f64))
        });
        let value_pattern = cached(UIA_IsValuePatternAvailablePropertyId).unwrap_or(false);
        ElementInfo {
            control_type: element.CachedControlType().map_or(0, |c| c.0),
            class_name: text(element.CachedClassName()),
            name: text(element.CachedName()),
            framework: text(element.CachedFrameworkId()),
            aria_role: text(element.CachedAriaRole()),
            frame,
            pid: element.CachedProcessId().map_or(0, |p| p as u32),
            hwnd: element
                .CachedNativeWindowHandle()
                .map_or(0, |h| h.0 as isize),
            enabled: flag(element.CachedIsEnabled(), true),
            focusable: flag(element.CachedIsKeyboardFocusable(), false),
            password: flag(element.CachedIsPassword(), false),
            value_pattern,
            value_read_only: if value_pattern {
                cached(UIA_ValueIsReadOnlyPropertyId)
            } else {
                None
            },
            text_pattern: cached(UIA_IsTextPatternAvailablePropertyId).unwrap_or(false),
            text_edit_pattern: cached(UIA_IsTextEditPatternAvailablePropertyId).unwrap_or(false),
            ..ElementInfo::default()
        }
    }
}

fn cached_bool(element: &IUIAutomationElement, property: UIA_PROPERTY_ID) -> Option<bool> {
    // SAFETY: reads the local cache of a live element of this thread.
    let value = unsafe { element.GetCachedPropertyValueEx(property, true) }.ok()?;
    take_bool(value)
}

/// Reads a VT_BOOL and frees the variant.
fn take_bool(mut value: VARIANT) -> Option<bool> {
    // SAFETY: the tag is read first and the union field read is the one it names; the
    // variant is freed once and not used after.
    unsafe {
        let inner = &value.Anonymous.Anonymous;
        let result = (inner.vt == VT_BOOL).then(|| inner.Anonymous.boolVal.0 != 0);
        let _ = VariantClear(&mut value);
        result
    }
}

/// Reads a VT_BSTR and frees the variant.
fn take_string(mut value: VARIANT) -> Option<String> {
    // SAFETY: as in `take_bool`; the BSTR is copied before the variant frees it.
    unsafe {
        let inner = &value.Anonymous.Anonymous;
        let result = (inner.vt == VT_BSTR).then(|| inner.Anonymous.bstrVal.to_string());
        let _ = VariantClear(&mut value);
        result.filter(|s| !s.is_empty())
    }
}

fn bool_variant(value: bool) -> VARIANT {
    let flag: VARIANT_BOOL = if value { VARIANT_TRUE } else { VARIANT_FALSE };
    scalar_variant(VT_BOOL, VARIANT_0_0_0 { boolVal: flag })
}

fn i32_variant(value: i32) -> VARIANT {
    scalar_variant(VT_I4, VARIANT_0_0_0 { lVal: value })
}

fn scalar_variant(vt: windows::Win32::System::Variant::VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use igorio_core::editable::{editable_target, is_text_input};
    use igorio_core::geom::Point;
    use std::sync::Mutex;
    use windows::Win32::System::Threading::WaitForSingleObject;

    // --- Fixtures: in-memory snapshots that imitate real apps. ---

    fn el(control: UIA_CONTROLTYPE_ID) -> ElementInfo {
        ElementInfo {
            control_type: control.0,
            enabled: true,
            framework: "Win32".into(),
            ..ElementInfo::default()
        }
    }

    trait Fixture {
        fn web(self) -> Self;
        fn framework(self, f: &str) -> Self;
        fn value(self, read_only: bool) -> Self;
        fn text(self) -> Self;
        fn text_edit(self) -> Self;
        fn focusable(self) -> Self;
        fn aria(self, role: &str) -> Self;
        fn frame(self, x: f64, y: f64, w: f64, h: f64) -> Self;
        fn top(self) -> Self;
    }

    impl Fixture for ElementInfo {
        fn web(self) -> Self {
            self.framework("Chrome")
        }
        fn framework(mut self, f: &str) -> Self {
            self.framework = f.into();
            self
        }
        fn value(mut self, read_only: bool) -> Self {
            self.value_pattern = true;
            self.value_read_only = Some(read_only);
            self
        }
        fn text(mut self) -> Self {
            self.text_pattern = true;
            self
        }
        fn text_edit(mut self) -> Self {
            self.text_edit_pattern = true;
            self
        }
        fn focusable(mut self) -> Self {
            self.focusable = true;
            self
        }
        fn aria(mut self, role: &str) -> Self {
            self.aria_role = role.into();
            self
        }
        fn frame(mut self, x: f64, y: f64, w: f64, h: f64) -> Self {
            self.frame = Some(Rect::new(x, y, w, h));
            self
        }
        fn top(mut self) -> Self {
            self.top_level = true;
            self
        }
    }

    /// Hit first, then its ancestors.
    fn chain(elements: Vec<ElementInfo>) -> UiaNode {
        let mut tree = Tree::default();
        let mut previous = None;
        for mut info in elements {
            info.classify();
            previous = Some(match previous {
                None => tree.push_root(info),
                Some(child) => tree.push_parent(child, info),
            });
        }
        UiaNode {
            tree: Arc::new(tree),
            index: 0,
        }
    }

    fn window() -> ElementInfo {
        el(UIA_WindowControlTypeId).top()
    }

    fn role(info: ElementInfo) -> (&'static str, Option<&'static str>) {
        map_role(&info)
    }

    fn editable(hit: &UiaNode) -> bool {
        editable_self_or_ancestor(hit).is_some()
    }

    // --- Role mapping ---

    #[test]
    fn from_chain_links_the_hit_to_its_ancestors() {
        assert!(UiaNode::from_chain(Vec::new()).is_none());
        let hit = UiaNode::from_chain(vec![
            el(UIA_EditControlTypeId).value(false).text(),
            el(UIA_PaneControlTypeId),
            window(),
        ])
        .unwrap();
        assert_eq!(hit.element().role, "AXTextField");
        let parent = hit.parent().unwrap();
        assert_eq!(parent.element().role, "AXGroup");
        assert!(parent.children()[0].same(&hit));
        assert_eq!(parent.parent().unwrap().element().role, "AXWindow");
        assert!(parent.parent().unwrap().parent().is_none());
    }

    #[test]
    fn maps_every_role_the_core_tests() {
        let cases: &[(ElementInfo, &str)] = &[
            (el(UIA_EditControlTypeId).value(false).text(), "AXTextField"),
            (
                el(UIA_EditControlTypeId).value(false).aria("searchbox"),
                "AXSearchField",
            ),
            (
                el(UIA_DocumentControlTypeId).text().focusable(),
                "AXTextArea",
            ),
            (el(UIA_ComboBoxControlTypeId).value(false), "AXComboBox"),
            (el(UIA_ComboBoxControlTypeId).value(true), "AXPopUpButton"),
            (el(UIA_PaneControlTypeId).top(), "AXWindow"),
            (el(UIA_WindowControlTypeId), "AXWindow"),
            (
                el(UIA_DocumentControlTypeId).web().value(true).text(),
                "AXWebArea",
            ),
            (el(UIA_SliderControlTypeId), "AXSlider"),
            (el(UIA_CheckBoxControlTypeId), "AXCheckBox"),
            (el(UIA_ButtonControlTypeId).aria("switch"), "AXCheckBox"),
            (el(UIA_RadioButtonControlTypeId), "AXRadioButton"),
            (el(UIA_TabItemControlTypeId), "AXRadioButton"),
            (el(UIA_SpinnerControlTypeId), "AXIncrementor"),
            (el(UIA_ScrollBarControlTypeId), "AXScrollBar"),
            (el(UIA_ThumbControlTypeId), "AXValueIndicator"),
            (el(UIA_ButtonControlTypeId), "AXButton"),
            (el(UIA_HeaderItemControlTypeId), "AXButton"),
            (el(UIA_HyperlinkControlTypeId), "AXLink"),
            (el(UIA_MenuItemControlTypeId), "AXMenuItem"),
            (el(UIA_SplitButtonControlTypeId), "AXMenuButton"),
            (el(UIA_TabControlTypeId), "AXTabGroup"),
            (el(UIA_ToolBarControlTypeId), "AXToolbar"),
            (el(UIA_TitleBarControlTypeId), "AXToolbar"),
            (el(UIA_StatusBarControlTypeId), "AXToolbar"),
            (el(UIA_ListItemControlTypeId), "AXRow"),
            (el(UIA_DataItemControlTypeId), "AXRow"),
            (el(UIA_TreeItemControlTypeId), "AXRow"),
            (el(UIA_TextControlTypeId).aria("heading"), "AXHeading"),
            (el(UIA_TextControlTypeId), "AXStaticText"),
            (el(UIA_MenuBarControlTypeId), "AXMenuBar"),
            (el(UIA_MenuControlTypeId), "AXMenu"),
            (el(UIA_TreeControlTypeId), "AXOutline"),
            (el(UIA_TableControlTypeId), "AXTable"),
            (el(UIA_DataGridControlTypeId), "AXTable"),
            (el(UIA_ListControlTypeId), "AXList"),
            (el(UIA_GroupControlTypeId), "AXGroup"),
            (el(UIA_PaneControlTypeId), "AXGroup"),
            (el(UIA_ImageControlTypeId), "AXImage"),
            (el(UIA_CustomControlTypeId), "AXUnknown"),
        ];
        for (info, expected) in cases {
            assert_eq!(map_role(info).0, *expected, "{info:?}");
        }
        assert_eq!(
            role(el(UIA_EditControlTypeId).value(false).text().focusable()),
            ("AXTextField", None)
        );
    }

    #[test]
    fn password_field_is_a_secure_text_field() {
        let mut info = el(UIA_EditControlTypeId).value(false).text();
        info.password = true;
        assert_eq!(role(info), ("AXTextField", Some("AXSecureTextField")));
    }

    #[test]
    fn control_type_names() {
        assert_eq!(control_type_name(UIA_ButtonControlTypeId.0), "Button");
        assert_eq!(control_type_name(UIA_EditControlTypeId.0), "Edit");
        assert_eq!(control_type_name(UIA_AppBarControlTypeId.0), "AppBar");
        assert_eq!(control_type_name(7), "?");
    }

    // --- Editable rule, on imitations of real apps ---

    #[test]
    fn win32_edit_control() {
        let hit = chain(vec![
            el(UIA_EditControlTypeId).value(false).text().focusable(),
            window(),
        ]);
        assert_eq!(hit.role().as_deref(), Some("AXTextField"));
        assert!(is_text_input(&hit));
    }

    #[test]
    fn old_edit_without_value_pattern() {
        let hit = chain(vec![el(UIA_EditControlTypeId).focusable(), window()]);
        assert!(is_text_input(&hit));
    }

    #[test]
    fn wpf_and_winui_text_boxes() {
        let wpf = chain(vec![
            el(UIA_EditControlTypeId)
                .framework("WPF")
                .value(false)
                .text()
                .focusable(),
            el(UIA_PaneControlTypeId).framework("WPF"),
            window(),
        ]);
        assert!(is_text_input(&wpf));
        let winui = chain(vec![
            el(UIA_EditControlTypeId)
                .framework("XAML")
                .value(false)
                .text()
                .text_edit()
                .focusable(),
            el(UIA_GroupControlTypeId).framework("XAML"),
            window(),
        ]);
        assert!(is_text_input(&winui));
    }

    #[test]
    fn chrome_input() {
        let hit = chain(vec![
            el(UIA_EditControlTypeId)
                .web()
                .value(false)
                .text()
                .focusable(),
            el(UIA_GroupControlTypeId).web(),
            el(UIA_DocumentControlTypeId).web().value(true).text(),
            el(UIA_PaneControlTypeId).top(),
        ]);
        assert_eq!(hit.role().as_deref(), Some("AXTextField"));
        assert!(is_text_input(&hit));
    }

    #[test]
    fn chrome_contenteditable_composer() {
        // Slack and Gmail: a div with role=textbox (an Edit in UI Automation); the hit is a
        // text run inside it.
        let hit = chain(vec![
            el(UIA_TextControlTypeId).web().text(),
            el(UIA_GroupControlTypeId).web().text(),
            el(UIA_EditControlTypeId)
                .web()
                .aria("textbox")
                .value(false)
                .text()
                .focusable(),
            el(UIA_GroupControlTypeId).web(),
            el(UIA_DocumentControlTypeId).web().value(true).text(),
            el(UIA_PaneControlTypeId).top(),
        ]);
        assert!(!is_text_input(&hit));
        let found = editable_self_or_ancestor(&hit).expect("the composer root");
        assert_eq!(found.role().as_deref(), Some("AXTextField"));
        // Without a role the root is a Group with a writable value.
        let plain = chain(vec![
            el(UIA_TextControlTypeId).web().text(),
            el(UIA_GroupControlTypeId)
                .web()
                .value(false)
                .text()
                .focusable(),
            el(UIA_DocumentControlTypeId).web().value(true).text(),
            el(UIA_PaneControlTypeId).top(),
        ]);
        let found = editable_self_or_ancestor(&plain).expect("the editable group");
        assert_eq!(found.role().as_deref(), Some("AXGroup"));
        assert!(found.is_settable_text_value());
    }

    #[test]
    fn composer_under_its_placeholder_is_found_nearby() {
        let mut tree = Tree::default();
        let mut label = el(UIA_TextControlTypeId)
            .web()
            .text()
            .frame(10.0, 10.0, 100.0, 20.0);
        label.classify();
        let hit = tree.push_root(label);
        let mut wrapper = el(UIA_GroupControlTypeId).web();
        wrapper.classify();
        let wrapper = tree.push_parent(hit, wrapper);
        let mut document = el(UIA_DocumentControlTypeId).web().value(true).text();
        document.classify();
        tree.push_parent(wrapper, document);
        let mut composer = el(UIA_EditControlTypeId)
            .web()
            .value(false)
            .text()
            .focusable()
            .frame(0.0, 0.0, 400.0, 40.0);
        composer.classify();
        let composer = tree.push_child(wrapper, composer);
        let tree = Arc::new(tree);
        let hit = UiaNode {
            tree: tree.clone(),
            index: hit,
        };
        assert!(!editable(&hit));
        let found = editable_target(Point::new(20.0, 20.0), &hit).expect("the composer");
        assert!(found.same(&UiaNode {
            tree,
            index: composer
        }));
        assert!(editable_target(Point::new(500.0, 500.0), &hit).is_none());
    }

    #[test]
    fn word_and_notepad_documents() {
        let word = chain(vec![
            el(UIA_DocumentControlTypeId).text().focusable(),
            el(UIA_PaneControlTypeId),
            window(),
        ]);
        assert_eq!(word.role().as_deref(), Some("AXTextArea"));
        assert!(is_text_input(&word));
        let notepad = chain(vec![
            el(UIA_DocumentControlTypeId)
                .value(false)
                .text()
                .text_edit()
                .focusable(),
            window(),
        ]);
        assert!(is_text_input(&notepad));
    }

    #[test]
    fn windows_terminal_is_not_a_text_field_but_a_terminal() {
        let hit = chain(vec![
            el(UIA_TextControlTypeId)
                .framework("XAML")
                .text()
                .focusable(),
            el(UIA_PaneControlTypeId),
            el(UIA_PaneControlTypeId).top(),
        ]);
        assert_eq!(hit.role().as_deref(), Some("AXStaticText"));
        assert!(!editable(&hit));
        assert!(is_terminal(
            Some("WindowsTerminal.exe"),
            "CASCADIA_HOSTING_WINDOW_CLASS"
        ));
    }

    #[test]
    fn read_only_and_disabled_text_controls() {
        let read_only = chain(vec![
            el(UIA_EditControlTypeId).value(true).text().focusable(),
            window(),
        ]);
        assert_eq!(read_only.role().as_deref(), Some("AXStaticText"));
        assert!(!editable(&read_only));
        let mut disabled = el(UIA_EditControlTypeId).value(false).text();
        disabled.enabled = false;
        assert!(!editable(&chain(vec![disabled, window()])));
        let read_only_document = chain(vec![
            el(UIA_DocumentControlTypeId).value(true).text().focusable(),
            window(),
        ]);
        assert_eq!(read_only_document.role().as_deref(), Some("AXWebArea"));
        assert!(!editable(&read_only_document));
    }

    #[test]
    fn password_field_is_not_a_paste_target() {
        let mut info = el(UIA_EditControlTypeId)
            .web()
            .value(false)
            .text()
            .focusable();
        info.password = true;
        let hit = chain(vec![
            info,
            el(UIA_DocumentControlTypeId).web().value(true),
            window(),
        ]);
        assert!(!editable(&hit));
    }

    #[test]
    fn button_and_slider_are_not_text() {
        let button = chain(vec![el(UIA_ButtonControlTypeId).focusable(), window()]);
        assert!(!editable(&button));
        let slider = chain(vec![
            el(UIA_SliderControlTypeId).value(false).focusable(),
            window(),
        ]);
        assert!(!editable(&slider));
    }

    #[test]
    fn static_web_page_stops_at_the_web_area() {
        let hit = chain(vec![
            el(UIA_TextControlTypeId).web().text(),
            el(UIA_GroupControlTypeId).web().text().focusable(),
            el(UIA_DocumentControlTypeId)
                .web()
                .value(true)
                .text()
                .focusable(),
            // An editable thing above the page must not count.
            el(UIA_EditControlTypeId).value(false),
            el(UIA_PaneControlTypeId).top(),
        ]);
        assert!(!editable(&hit));
        // Focusable web text with a Text pattern is not enough on its own.
        let span = chain(vec![
            el(UIA_EditControlTypeId)
                .web()
                .text()
                .focusable()
                .value(true),
            window(),
        ]);
        assert!(!editable(&span));
    }

    // --- Terminal, shell, elevation ---

    #[test]
    fn terminals_by_exe_and_class() {
        for exe in TERMINAL_EXES {
            assert!(is_terminal(Some(exe), ""));
        }
        assert!(is_terminal(Some("windowsterminal.EXE"), ""));
        assert!(is_terminal(Some("cmd.exe"), "ConsoleWindowClass"));
        assert!(!is_terminal(Some("notepad.exe"), "Notepad"));
        assert!(!is_terminal(None, ""));
    }

    #[test]
    fn explorer_and_desktop_by_class() {
        assert_eq!(shell_kind("CabinetWClass"), Some(Shell::Explorer));
        assert_eq!(shell_kind("Progman"), Some(Shell::Desktop));
        assert_eq!(shell_kind("WorkerW"), Some(Shell::Desktop));
        assert_eq!(shell_kind("Chrome_WidgetWin_1"), None);
    }

    #[test]
    fn elevation() {
        const MEDIUM: u32 = 0x2000;
        const HIGH: u32 = 0x3000;
        assert!(is_elevated(MEDIUM, false, Some(HIGH)));
        assert!(is_elevated(MEDIUM, false, None));
        assert!(!is_elevated(MEDIUM, false, Some(MEDIUM)));
        assert!(!is_elevated(MEDIUM, false, Some(0x1000)));
        assert!(!is_elevated(MEDIUM, true, Some(HIGH)));
        assert!(!is_elevated(HIGH, false, Some(HIGH)));
    }

    #[test]
    fn triage_order() {
        let pre = |own, elevated, hung| Pre {
            target: target(),
            own,
            elevated,
            hung,
        };
        assert!(matches!(
            triage(pre(true, true, true), true),
            Triage::Answer(Outcome::OwnProcess)
        ));
        assert!(matches!(
            triage(pre(false, true, false), true),
            Triage::Answer(Outcome::TimedOut { .. })
        ));
        assert!(matches!(
            triage(pre(false, false, true), false),
            Triage::Answer(Outcome::TimedOut { .. })
        ));
        assert!(matches!(
            triage(pre(false, true, false), false),
            Triage::Answer(Outcome::Elevated { pid: 7 })
        ));
        assert!(matches!(
            triage(pre(false, false, false), false),
            Triage::Run(_)
        ));
    }

    #[test]
    fn clip_names() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 3), "abc…");
    }

    // --- Controller, with a fake backend (no COM) ---

    fn target() -> Target {
        Target {
            pid: 7,
            exe: Some("fake.exe".into()),
            window: 1,
            child_window: 1,
            class_name: "Fake".into(),
            is_terminal: false,
            shell: None,
        }
    }

    /// x: tag (-1 own process, -2 elevated); y: how long the lookup takes, in ms.
    struct Fake {
        ran: Arc<Mutex<Vec<i32>>>,
    }

    struct FakeSession(Arc<Mutex<Vec<i32>>>);

    impl Backend for Fake {
        fn precheck(&self, (x, _): (i32, i32)) -> std::result::Result<Pre, String> {
            Ok(Pre {
                target: target(),
                own: x == -1,
                elevated: x == -2,
                hung: false,
            })
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            Ok(Box::new(FakeSession(self.ran.clone())))
        }
    }

    impl Session for FakeSession {
        fn lookup(&mut self, job: &Job) -> Option<Outcome> {
            self.0.lock().unwrap().push(job.point.0);
            thread::sleep(Duration::from_millis(job.point.1 as u64));
            (Instant::now() < job.deadline).then(|| Outcome::Failed(format!("ran {}", job.point.0)))
        }
    }

    fn fake(grace: Duration) -> (Uia, Arc<Mutex<Vec<i32>>>) {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(Fake { ran: ran.clone() });
        let uia = Uia::with_backend(backend, Arc::new(AtomicU64::new(0)), grace).unwrap();
        (uia, ran)
    }

    fn lookup(tag: i32, ms: i32, deadline_ms: u64) -> Lookup {
        Lookup {
            point: (tag, ms),
            want_focus: false,
            deadline: Duration::from_millis(deadline_ms),
        }
    }

    /// Waits on the wake handle until `id` is answered; returns every answer seen.
    fn wait_for(uia: &Uia, id: RequestId, limit: Duration) -> Vec<(RequestId, Outcome)> {
        let until = Instant::now() + limit;
        let mut seen = Vec::new();
        while Instant::now() < until {
            // SAFETY: the handle is alive while `uia` is.
            unsafe { WaitForSingleObject(uia.wake_handle(), 20) };
            while let Some(answer) = uia.poll() {
                let done = answer.0 == id;
                seen.push(answer);
                if done {
                    return seen;
                }
            }
        }
        seen
    }

    fn last_is(answers: &[(RequestId, Outcome)], id: RequestId, text: &str) -> bool {
        matches!(answers.last(), Some((i, Outcome::Failed(t))) if *i == id && t == text)
    }

    #[test]
    fn newest_request_replaces_waiting_ones() {
        let (uia, ran) = fake(STUCK_GRACE);
        let mut last = RequestId(0);
        for tag in 0..50 {
            last = uia.request(lookup(tag, 30, 2000));
        }
        let answers = wait_for(&uia, last, Duration::from_secs(3));
        assert!(last_is(&answers, last, "ran 49"), "{answers:?}");
        let ran = ran.lock().unwrap().clone();
        assert!(ran.len() <= 3, "ran {ran:?}");
        assert_eq!(answers.len(), ran.len());
        assert!(uia.shutdown());
    }

    #[test]
    fn deadline_answers_at_once_and_worker_continues() {
        let (uia, _) = fake(STUCK_GRACE);
        let started = Instant::now();
        let slow = uia.request(lookup(1, 200, 50));
        let answers = wait_for(&uia, slow, Duration::from_secs(1));
        assert!(matches!(
            answers.last(),
            Some((_, Outcome::TimedOut { .. }))
        ));
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "{:?}",
            started.elapsed()
        );
        let next = uia.request(lookup(2, 0, 1000));
        let answers = wait_for(&uia, next, Duration::from_secs(1));
        assert!(last_is(&answers, next, "ran 2"), "{answers:?}");
        assert!(uia.shutdown());
    }

    #[test]
    fn zero_deadline_times_out_without_running() {
        let (uia, ran) = fake(STUCK_GRACE);
        let id = uia.request(lookup(1, 0, 0));
        let answers = wait_for(&uia, id, Duration::from_secs(1));
        assert!(matches!(
            answers.last(),
            Some((_, Outcome::TimedOut { .. }))
        ));
        assert!(ran.lock().unwrap().is_empty());
        let next = uia.request(lookup(2, 0, 500));
        assert!(last_is(
            &wait_for(&uia, next, Duration::from_secs(1)),
            next,
            "ran 2"
        ));
        assert!(uia.shutdown());
    }

    #[test]
    fn own_process_and_elevated_skip_the_worker() {
        let (uia, ran) = fake(STUCK_GRACE);
        let own = uia.request(lookup(-1, 0, 500));
        assert!(matches!(
            wait_for(&uia, own, Duration::from_secs(1)).last(),
            Some((_, Outcome::OwnProcess))
        ));
        let elevated = uia.request(lookup(-2, 0, 500));
        assert!(matches!(
            wait_for(&uia, elevated, Duration::from_secs(1)).last(),
            Some((_, Outcome::Elevated { pid: 7 }))
        ));
        assert!(ran.lock().unwrap().is_empty());
        assert!(uia.shutdown());
    }

    #[test]
    fn stuck_worker_is_replaced_and_threads_are_capped() {
        let grace = Duration::from_millis(50);
        let (uia, _) = fake(grace);
        // Three lookups that hang for 1.5 s each: every one strands a worker.
        for tag in 0..3 {
            let id = uia.request(lookup(tag, 1500, 30));
            let answers = wait_for(&uia, id, Duration::from_secs(1));
            assert!(matches!(
                answers.last(),
                Some((_, Outcome::TimedOut { .. }))
            ));
            thread::sleep(grace * 3);
            assert!(uia.live.load(Ordering::SeqCst) <= MAX_WORKERS);
        }
        // At the cap: a quick lookup times out instead of piling up threads.
        let id = uia.request(lookup(9, 0, 50));
        let answers = wait_for(&uia, id, Duration::from_secs(1));
        assert!(
            matches!(answers.last(), Some((_, Outcome::TimedOut { .. }))),
            "{answers:?}"
        );
        assert!(uia.live.load(Ordering::SeqCst) <= MAX_WORKERS);
        // Once the stuck calls return, a fresh worker answers again.
        thread::sleep(Duration::from_millis(1700));
        let id = uia.request(lookup(10, 0, 500));
        let answers = wait_for(&uia, id, Duration::from_secs(1));
        assert!(last_is(&answers, id, "ran 10"), "{answers:?}");
        uia.shutdown();
    }
}
