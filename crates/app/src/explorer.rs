//! File Explorer and the desktop: which folder a window shows, answered off the UI thread
//! within a deadline, and selecting a saved file in that window afterwards.
//!
//! [`crate::uia`] already says whether a double-click hit an Explorer window or the
//! desktop, and `igorio_core::drop_policy` whether it hit empty background. This module
//! only turns the window into a folder path.
//!
//! # Threads
//!
//! Same shape as [`crate::uia`], so the main loop treats both alike:
//!
//! - The caller (the UI thread) only sends requests and reads answers: [`Explorer::request`],
//!   then [`Explorer::poll`] after [`Explorer::wake_handle`] is signalled. It never calls a
//!   Shell interface (rule 3).
//! - A controller thread owns the request slot, the deadlines and the reveal retries. Before
//!   it hands a request to the worker it checks the window with Win32 calls that do not wait
//!   on another process (`IsWindow`, `GetClassNameW`, `IsHungAppWindow`, `FindWindowW`):
//!   a window that is gone or not Explorer fails at once, a hung one times out at once.
//! - A worker thread makes every Shell call. It runs in a single-threaded apartment (STA):
//!   the objects it creates in-process (`IShellItem` from `SHCreateItemFromIDList`, the
//!   folder objects behind it) are apartment-threaded, and the Shell documents its
//!   interfaces as STA-only; in an MTA COM would host them on a separate STA thread and
//!   marshal every call to it. An STA must pump messages, so the idle worker waits in
//!   `MsgWaitForMultipleObjectsEx` and dispatches what arrives (COM's own hidden window is
//!   its only window). While it waits for Explorer inside a call, COM pumps for it. No COM
//!   pointer leaves the worker: answers are plain data ([`FolderOutcome`]).
//!
//! # Finding the folder of a window
//!
//! `IShellWindows` has one entry per folder view. Since Windows 11 22H2 a File Explorer
//! window (`CabinetWClass`) has tabs, and each tab is its own entry, while
//! `IWebBrowserApp::HWND` returns the same top-level window for all of them. Each tab is a
//! `ShellTabWindowClass` child of the top-level window; only the active tab's is visible
//! (the others are hidden), and the active one is also first among them in z-order. The
//! lookup:
//!
//! 1. Win32: list the `ShellTabWindowClass` children; the active tab is the only visible one
//!    (when that is ambiguous, the first in z-order). See [`tabs`].
//! 2. Enumerate `IShellWindows`; keep entries whose `HWND` is the top-level window; for
//!    those, `IServiceProvider::QueryService(SID_STopLevelBrowser)` gives the tab's
//!    `IShellBrowser`, whose `IOleWindow::GetWindow` is that tab's `ShellTabWindowClass`
//!    window. The entry whose window is the active tab wins. (A window without tab children,
//!    as before 22H2, has one view: the first match wins.)
//! 3. `IShellBrowser::QueryActiveShellView` -> `IFolderView::GetFolder(IPersistFolder2)` ->
//!    `GetCurFolder` gives the folder's PIDL; `SHCreateItemFromIDList` makes an `IShellItem`
//!    of it in this process.
//! 4. The item is a folder to save into when it has a `SIGDN_FILESYSPATH` path and the
//!    attributes `SFGAO_FILESYSTEM | SFGAO_FOLDER` without `SFGAO_STREAM` ([`folder_path`]).
//!    `SFGAO_STREAM` rules out a zip file opened as a folder and a library (both have a
//!    file path: the `.zip` or the `.library-ms` file). A server root (`\\server`) is not a
//!    folder either. Everything else (This PC, Home, search results, Recycle Bin, network
//!    roots) has no file path and answers [`FolderOutcome::NotAFolder`] with the display
//!    name. Whether the folder is writable is not checked: the save reports that error.
//!
//! The desktop view lives in `Progman` (or a `WorkerW` window on older builds) and is not
//! in the `IShellWindows` list; `FindWindowSW(CSIDL_DESKTOP, SWC_DESKTOP)` returns it. It
//! shows the user's and the public Desktop folders merged, so its own folder is not where a
//! new file should go. The answer is the user's Desktop known folder
//! (`SHGetKnownFolderPath(FOLDERID_Desktop)`, which follows a OneDrive redirection).
//!
//! # Deadline and hung Explorer
//!
//! Each request has a deadline (default [`DEFAULT_DEADLINE`]). The worker checks it before
//! every call and makes at most [`MAX_CALLS`] calls per request (rule 2). COM has no timeout
//! for a call into another process, so a call into a hung Explorer can block the worker for
//! a long time. The controller answers `TimedOut` at the deadline whatever the worker is
//! doing; if the worker is still inside a call [`STUCK_GRACE`] after the deadline, the
//! controller abandons that thread and starts a fresh one. At most [`MAX_WORKERS`] worker
//! threads exist at once, abandoned ones included; at the limit, requests time out until
//! one returns. An abandoned thread exits by itself when its call returns. A window that
//! `IsHungAppWindow` reports as hung is answered `TimedOut` without a Shell call.
//!
//! Requests replace each other: only the newest waiting request runs. A request that is
//! replaced before it starts gets no answer.
//!
//! While Explorer restarts, its windows and `Progman` are gone, so requests fail at the
//! Win32 check; a request that was already inside a call gets an RPC error (`Failed`).
//! `IShellWindows` is created per request, so nothing stale is kept.
//!
//! # Reveal
//!
//! [`Explorer::reveal`] selects a saved file in the window's active view, so the user sees
//! where it went. The view learns about a new file from a change notification a
//! moment after the write, so the controller schedules a few attempts
//! ([`REVEAL_DELAYS`]) instead of the worker sleeping; a folder request always goes first.
//! Each attempt checks that the view still shows the file's folder, parses the file name
//! in that folder, calls `IShellView::SelectItem` and reads back
//! `IFolderView2::GetSelectionState`. It stops when the file is selected, the view shows
//! another folder, or the attempts run out. Failures are silent.

use std::ffi::OsString;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, HWND};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, IDispatch, IServiceProvider,
};
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_FOLDER, SFGAO_STREAM};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, FOLDERID_Desktop, IFolderView, IFolderView2, IPersistFolder2, IShellBrowser,
    IShellFolder, IShellItem, IShellView, IShellWindows, IWebBrowserApp, KF_FLAG_DEFAULT,
    SHCreateItemFromIDList, SHCreateItemFromParsingName, SHGetKnownFolderPath,
    SID_STopLevelBrowser, SIGDN, SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY, SVSI_DESELECTOTHERS,
    SVSI_ENSUREVISIBLE, SVSI_FOCUSED, SVSI_SELECT, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowExW, FindWindowW, GetClassNameW, IsHungAppWindow, IsWindow,
    IsWindowVisible, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, QS_ALLINPUT,
};
use windows::core::{Error, HRESULT, Interface, PCWSTR, PWSTR, Result, w};

use crate::uia::{Shell, shell_kind};

/// How long a requester waits by default.
pub const DEFAULT_DEADLINE: Duration = Duration::from_millis(500);
/// Shell calls per request. Enumerating `IShellWindows` costs about 3 calls per open folder
/// view (every tab counts), so this covers some 60 open tabs.
pub const MAX_CALLS: u32 = 200;
/// A worker still inside a call this long after its request's deadline is abandoned.
pub const STUCK_GRACE: Duration = Duration::from_millis(1000);
/// Worker threads alive at once, abandoned ones included.
pub const MAX_WORKERS: usize = 3;
/// Wait before each reveal attempt, counted from the previous attempt's end. About 2.5 s
/// in all: the view usually shows a new file within a few hundred milliseconds.
pub const REVEAL_DELAYS: &[Duration] = &[
    Duration::from_millis(0),
    Duration::from_millis(100),
    Duration::from_millis(150),
    Duration::from_millis(250),
    Duration::from_millis(400),
    Duration::from_millis(600),
    Duration::from_millis(1000),
];
/// Deadline of one reveal attempt.
const REVEAL_DEADLINE: Duration = Duration::from_millis(500);
const START_WAIT: Duration = Duration::from_secs(5);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);
const NAME_LIMIT: usize = 120;

// ---------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------

/// Identifies a request in [`Explorer::poll`]'s answers. Increases with every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderRequest {
    /// The top-level `HWND` (as `isize`) of a File Explorer window (`CabinetWClass`).
    Window(isize),
    /// The desktop.
    Desktop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderOutcome {
    /// A real file-system folder.
    Folder(PathBuf),
    /// The window shows something that is not a file-system folder (This PC, Quick access /
    /// Home, a search result, Recycle Bin, a zip opened as a folder, a library, a network
    /// root).
    NotAFolder {
        display_name: String,
    },
    TimedOut,
    Failed(String),
}

/// The lookup service. Not `Sync`: use it from one thread (the UI thread).
pub struct Explorer {
    inbox: Sender<Msg>,
    answers: Receiver<(RequestId, FolderOutcome)>,
    answers_tx: Sender<(RequestId, FolderOutcome)>,
    wake: Arc<Event>,
    next_id: AtomicU64,
    controller: Option<JoinHandle<bool>>,
    #[cfg(test)]
    live: Arc<AtomicUsize>,
}

impl Explorer {
    /// Starts the controller and the first worker, and waits until the worker has entered
    /// its apartment.
    pub fn start() -> Result<Explorer> {
        Explorer::with_backend(Arc::new(Win32Backend), STUCK_GRACE)
    }

    /// Queues a lookup and returns at once. It replaces a lookup that is still waiting (that
    /// one gets no answer); a lookup that is already running finishes. `deadline` is counted
    /// from this call.
    pub fn request(&self, request: FolderRequest, deadline: Duration) -> RequestId {
        let id = RequestId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let msg = Msg::Request {
            id,
            request,
            deadline: Instant::now() + deadline,
        };
        if self.inbox.send(msg).is_err() {
            let _ = self.answers_tx.send((
                id,
                FolderOutcome::Failed("Explorer controller stopped".into()),
            ));
            self.wake.set();
        }
        id
    }

    /// Selects `file` in the active view of `window` (an Explorer window, or `Progman` /
    /// `WorkerW` for the desktop) once the view shows it. Best effort, no answer. Replaces
    /// a reveal that is still in progress.
    pub fn reveal(&self, window: isize, file: PathBuf) {
        let _ = self.inbox.send(Msg::Reveal { window, file });
    }

    /// The next answer, if any. Call until None after each wake.
    pub fn poll(&self) -> Option<(RequestId, FolderOutcome)> {
        self.answers.try_recv().ok()
    }

    /// An auto-reset event, signalled after answers are queued. Valid while `self` lives.
    pub fn wake_handle(&self) -> HANDLE {
        self.wake.0
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

    fn with_backend(backend: Arc<dyn Backend>, grace: Duration) -> Result<Explorer> {
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
        .map_err(|err| Error::new(E_FAIL, format!("Explorer worker: {err}")))?;
        match ready_rx.recv_timeout(START_WAIT) {
            Ok(Ok(())) => {}
            Ok(Err((code, message))) => return Err(Error::new(code, message)),
            Err(_) => return Err(Error::new(E_FAIL, "Explorer worker did not start")),
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
            reveal: None,
            reveal_seq: 0,
            grace,
        };
        let controller = thread::Builder::new()
            .name("explorer-controller".into())
            .spawn(move || controller.run())
            .map_err(|err| Error::new(E_FAIL, format!("Explorer controller: {err}")))?;
        Ok(Explorer {
            inbox,
            answers,
            answers_tx,
            wake,
            next_id: AtomicU64::new(1),
            controller: Some(controller),
            #[cfg(test)]
            live,
        })
    }
}

impl Drop for Explorer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The tabs of a File Explorer window, from Win32 alone (no Shell call, no message sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tabs {
    /// `ShellTabWindowClass` children, in z-order.
    pub windows: Vec<isize>,
    /// The active one (see [`active_tab`]).
    pub active: Option<isize>,
}

/// Lists the tab windows of a top-level Explorer window. Safe to call on any thread:
/// `FindWindowExW` without a title and `IsWindowVisible` read window state without sending
/// messages.
pub fn tabs(window: isize) -> Tabs {
    let parent = hwnd(window);
    let mut found = Vec::new();
    let mut after: Option<HWND> = None;
    // Bounded: a window has far fewer tabs than this.
    while found.len() < 256 {
        // SAFETY: plain window-handle queries; a stale handle makes them fail, not crash.
        let next = unsafe { FindWindowExW(Some(parent), after, w!("ShellTabWindowClass"), None) };
        let Ok(tab) = next else { break };
        // SAFETY: as above.
        let visible = unsafe { IsWindowVisible(tab) }.as_bool();
        found.push((tab.0 as isize, visible));
        after = Some(tab);
    }
    Tabs {
        windows: found.iter().map(|&(tab, _)| tab).collect(),
        active: active_tab(&found),
    }
}

// ---------------------------------------------------------------------------------------
// Pure rules
// ---------------------------------------------------------------------------------------

/// The active tab among `(window, visible)` pairs in z-order: the only visible one, else the
/// first (Explorer keeps the active tab on top of its siblings).
pub fn active_tab(tabs: &[(isize, bool)]) -> Option<isize> {
    let mut visible = tabs.iter().filter(|(_, v)| *v);
    match (visible.next(), visible.next()) {
        (Some(&(tab, _)), None) => Some(tab),
        _ => tabs.first().map(|&(tab, _)| tab),
    }
}

/// The folder to save into, from a Shell item's file-system path and attributes
/// (`SFGAO_*` bits). None for items without a path, files opened as folders (zip,
/// `.library-ms`: `SFGAO_STREAM`), and server roots (`\\server`).
pub fn folder_path(path: Option<PathBuf>, attributes: u32) -> Option<PathBuf> {
    let has = |flag: u32| attributes & flag == flag;
    let is_folder = has(SFGAO_FILESYSTEM.0 | SFGAO_FOLDER.0) && !has(SFGAO_STREAM.0);
    path.filter(|p| is_folder && !p.as_os_str().is_empty() && !is_server_root(p))
}

/// `\\server` or `\\server\`: a network computer, not a share.
fn is_server_root(path: &Path) -> bool {
    let text = path.to_string_lossy();
    let Some(rest) = text.strip_prefix(r"\\") else {
        return false;
    };
    !rest.trim_end_matches('\\').contains('\\')
}

/// The `NotAFolder` answer: the display name, else the path, else a placeholder.
fn not_a_folder(name: Option<String>, path: Option<&Path>) -> FolderOutcome {
    let name = name
        .filter(|n| !n.trim().is_empty())
        .or_else(|| path.map(|p| p.display().to_string()))
        .unwrap_or_else(|| "(unnamed)".into());
    FolderOutcome::NotAFolder {
        display_name: clip(&name, NAME_LIMIT),
    }
}

/// Same folder, ignoring ASCII case and trailing separators (as NTFS compares names for
/// the usual case).
fn same_folder(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

/// What the controller answers before a folder request reaches the worker. None: run it.
fn check_window(request: &FolderRequest, facts: Option<&WindowFacts>) -> Option<FolderOutcome> {
    let Some(facts) = facts else {
        return Some(FolderOutcome::Failed(match request {
            FolderRequest::Window(_) => "the window is gone".into(),
            FolderRequest::Desktop => "no desktop window (Explorer is not running)".into(),
        }));
    };
    let wanted = match request {
        FolderRequest::Window(_) => Shell::Explorer,
        FolderRequest::Desktop => Shell::Desktop,
    };
    if shell_kind(&facts.class) != Some(wanted) {
        return Some(FolderOutcome::Failed(format!(
            "not a File Explorer window (class {:?})",
            facts.class
        )));
    }
    facts.hung.then_some(FolderOutcome::TimedOut)
}

/// Can a reveal run against this window?
fn can_reveal(facts: Option<&WindowFacts>) -> bool {
    facts.is_some_and(|f| shell_kind(&f.class).is_some() && !f.hung)
}

fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Controller and workers
// ---------------------------------------------------------------------------------------

enum Msg {
    Request {
        id: RequestId,
        request: FolderRequest,
        deadline: Instant,
    },
    Reveal {
        window: isize,
        file: PathBuf,
    },
    Done {
        generation: u64,
        reply: Reply,
    },
    Exited {
        generation: u64,
    },
    Shutdown,
}

enum Work {
    Folder(FolderRequest),
    Reveal { window: isize, file: PathBuf },
}

struct Job {
    work: Work,
    deadline: Instant,
}

enum Reply {
    /// None when the deadline passed (the controller has answered already).
    Folder(Option<FolderOutcome>),
    Reveal(RevealStep),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevealStep {
    /// The file is selected.
    Done,
    /// The view does not show the file yet.
    Retry,
    /// The view shows another folder, or a call failed.
    GiveUp,
}

/// Win32 facts about a window.
struct WindowFacts {
    class: String,
    hung: bool,
}

/// What runs on the controller (Win32 checks) and on each worker (Shell calls).
/// The real one is [`Win32Backend`]; tests use a fake.
trait Backend: Send + Sync + 'static {
    /// Runs on the controller thread. Must not call the Shell or wait on another process.
    /// None when the window does not exist.
    fn facts(&self, request: &FolderRequest) -> Option<WindowFacts>;
    /// Same, for a reveal target.
    fn window_facts(&self, window: isize) -> Option<WindowFacts>;
    /// Runs once on each new worker thread; the session never leaves that thread.
    fn open(&self) -> Result<Box<dyn Session>>;
}

trait Session {
    /// None when the deadline passed.
    fn folder(&mut self, request: &FolderRequest, deadline: Instant) -> Option<FolderOutcome>;
    fn reveal(&mut self, window: isize, file: &Path, deadline: Instant) -> RevealStep;
}

struct Busy {
    /// None for a reveal attempt (nobody waits for it).
    id: Option<RequestId>,
    reveal_seq: Option<u64>,
    deadline: Instant,
    answered: bool,
}

struct RevealPlan {
    seq: u64,
    window: isize,
    file: PathBuf,
    /// Index into [`REVEAL_DELAYS`] of the next attempt.
    attempt: usize,
    /// None while an attempt runs.
    due: Option<Instant>,
}

struct Worker {
    generation: u64,
    jobs: Option<Sender<Job>>,
    /// Set after each job is sent and when the channel closes; the idle worker waits on it.
    signal: Arc<Event>,
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
        let signal = Arc::new(Event::new().map_err(std::io::Error::other)?);
        let wait_on = signal.clone();
        live.fetch_add(1, Ordering::SeqCst);
        let guard = LiveGuard(live);
        let thread = thread::Builder::new()
            .name(format!("explorer-worker-{generation}"))
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
                loop {
                    let job = match job_rx.try_recv() {
                        Ok(job) => job,
                        Err(TryRecvError::Empty) => {
                            wait_pumping(&wait_on);
                            continue;
                        }
                        Err(TryRecvError::Disconnected) => break,
                    };
                    let reply = match (&mut session, &job.work) {
                        (Ok(s), Work::Folder(request)) => {
                            Reply::Folder(s.folder(request, job.deadline))
                        }
                        (Ok(s), Work::Reveal { window, file }) => {
                            Reply::Reveal(s.reveal(*window, file, job.deadline))
                        }
                        (Err(err), Work::Folder(_)) => {
                            Reply::Folder(Some(FolderOutcome::Failed(format!("COM: {err}"))))
                        }
                        (Err(_), Work::Reveal { .. }) => Reply::Reveal(RevealStep::GiveUp),
                    };
                    if outbox.send(Msg::Done { generation, reply }).is_err() {
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
            signal,
            thread: Some(thread),
            busy: None,
        })
    }

    fn send(&mut self, job: Job) -> bool {
        let sent = self
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(job).is_ok());
        if sent {
            self.signal.set();
        } else {
            self.close();
        }
        sent
    }

    /// Closes the job channel; the worker exits when it next looks at it.
    fn close(&mut self) {
        self.jobs = None;
        self.signal.set();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Dropping a `Worker` abandons its thread: close the channel first, then wake it so
        // an idle worker sees the closed channel.
        self.close();
    }
}

/// Waits for the job signal while dispatching this thread's messages, as an STA must.
fn wait_pumping(signal: &Event) {
    // SAFETY: the handle is alive while `signal` is; no other pointers.
    unsafe {
        MsgWaitForMultipleObjectsEx(
            Some(&[signal.0]),
            INFINITE,
            QS_ALLINPUT,
            MWMO_INPUTAVAILABLE,
        )
    };
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid out-pointer; only this thread's messages (COM's window) are
    // read and dispatched.
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: `msg` was filled by `PeekMessageW` just above.
        unsafe { DispatchMessageW(&msg) };
    }
}

/// Counts live worker threads, abandoned ones included.
struct LiveGuard(Arc<AtomicUsize>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The newest folder request not yet given to the worker.
struct Pending {
    id: RequestId,
    request: FolderRequest,
    deadline: Instant,
}

struct Controller {
    backend: Arc<dyn Backend>,
    inbox: Receiver<Msg>,
    outbox: Sender<Msg>,
    answers: Sender<(RequestId, FolderOutcome)>,
    wake: Arc<Event>,
    live: Arc<AtomicUsize>,
    worker: Worker,
    next_gen: u64,
    pending: Option<Pending>,
    reveal: Option<RevealPlan>,
    reveal_seq: u64,
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
            Msg::Request {
                id,
                request,
                deadline,
            } => {
                let facts = self.backend.facts(&request);
                match check_window(&request, facts.as_ref()) {
                    Some(outcome) => self.answer(id, outcome),
                    None if Instant::now() >= deadline => self.answer(id, FolderOutcome::TimedOut),
                    // Replaces the waiting request, which gets no answer.
                    None => {
                        self.pending = Some(Pending {
                            id,
                            request,
                            deadline,
                        })
                    }
                }
            }
            Msg::Reveal { window, file } => {
                if can_reveal(self.backend.window_facts(window).as_ref()) {
                    self.reveal_seq += 1;
                    self.reveal = Some(RevealPlan {
                        seq: self.reveal_seq,
                        window,
                        file,
                        attempt: 0,
                        due: Some(Instant::now() + REVEAL_DELAYS[0]),
                    });
                }
            }
            Msg::Done { generation, reply } => {
                if generation == self.worker.generation
                    && let Some(busy) = self.worker.busy.take()
                {
                    match reply {
                        Reply::Folder(outcome) => {
                            if let Some(id) = busy.id
                                && !busy.answered
                            {
                                self.answer(id, outcome.unwrap_or(FolderOutcome::TimedOut));
                            }
                        }
                        Reply::Reveal(step) => self.reveal_done(busy.reveal_seq, step),
                    }
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

    fn reveal_done(&mut self, seq: Option<u64>, step: RevealStep) {
        let Some(plan) = self.reveal.as_mut().filter(|p| Some(p.seq) == seq) else {
            return;
        };
        plan.attempt += 1;
        match REVEAL_DELAYS.get(plan.attempt) {
            Some(delay) if step == RevealStep::Retry => plan.due = Some(Instant::now() + *delay),
            _ => self.reveal = None,
        }
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
        let pending = self.pending.as_ref().map(|p| p.deadline);
        let reveal = self.reveal.as_ref().and_then(|r| r.due);
        [busy, pending, reveal].into_iter().flatten().min()
    }

    fn tick(&mut self, now: Instant) {
        if let Some(busy) = &mut self.worker.busy
            && !busy.answered
            && now >= busy.deadline
        {
            busy.answered = true;
            if let Some(id) = busy.id {
                self.answer(id, FolderOutcome::TimedOut);
            }
        }
        if let Some(p) = self.pending.take_if(|p| now >= p.deadline) {
            self.answer(p.id, FolderOutcome::TimedOut);
        }
        let stuck = self
            .worker
            .busy
            .as_ref()
            .is_some_and(|b| now >= b.deadline + self.grace);
        if (stuck || self.worker.jobs.is_none()) && self.live.load(Ordering::SeqCst) < MAX_WORKERS {
            self.replace_worker();
        }
        if self.worker.busy.is_some() {
            return;
        }
        // A folder request goes before a reveal attempt.
        if let Some(p) = self.pending.take() {
            let busy = Busy {
                id: Some(p.id),
                reveal_seq: None,
                deadline: p.deadline,
                answered: false,
            };
            let job = Job {
                work: Work::Folder(p.request),
                deadline: p.deadline,
            };
            if self.worker.send(job) {
                self.worker.busy = Some(busy);
            } else {
                self.answer(
                    p.id,
                    FolderOutcome::Failed("Explorer worker stopped".into()),
                );
            }
        } else if let Some(plan) = &mut self.reveal
            && plan.due.is_some_and(|due| now >= due)
        {
            plan.due = None;
            let deadline = now + REVEAL_DEADLINE;
            let job = Job {
                work: Work::Reveal {
                    window: plan.window,
                    file: plan.file.clone(),
                },
                deadline,
            };
            let seq = plan.seq;
            if self.worker.send(job) {
                self.worker.busy = Some(Busy {
                    id: None,
                    reveal_seq: Some(seq),
                    deadline,
                    answered: true,
                });
            } else {
                self.reveal = None;
            }
        }
    }

    /// Abandons the current worker (it exits by itself when its call returns) and starts a
    /// fresh one.
    fn replace_worker(&mut self) {
        let generation = self.next_gen;
        self.next_gen += 1;
        if self
            .worker
            .busy
            .as_ref()
            .is_some_and(|b| b.reveal_seq.is_some())
        {
            // A reveal that hung once is not tried again.
            self.reveal = None;
        }
        match Worker::spawn(
            self.backend.clone(),
            generation,
            self.outbox.clone(),
            self.live.clone(),
            None,
        ) {
            // Dropping the old `Worker` closes its job channel and detaches its thread.
            Ok(worker) => self.worker = worker,
            Err(_) => self.worker.close(),
        }
    }

    /// Closes the worker's job channel and joins it if it exits within `SHUTDOWN_WAIT`.
    fn finish(mut self) -> bool {
        self.worker.close();
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

    fn answer(&self, id: RequestId, outcome: FolderOutcome) {
        if self.answers.send((id, outcome)).is_ok() {
            self.wake.set();
        }
    }
}

// ---------------------------------------------------------------------------------------
// Win32 backend
// ---------------------------------------------------------------------------------------

struct Win32Backend;

impl Backend for Win32Backend {
    fn facts(&self, request: &FolderRequest) -> Option<WindowFacts> {
        match request {
            FolderRequest::Window(window) => window_facts(*window),
            FolderRequest::Desktop => {
                // SAFETY: constant class name; no title, so no message is sent.
                let progman = unsafe { FindWindowW(w!("Progman"), None) }.ok()?;
                window_facts(progman.0 as isize)
            }
        }
    }

    fn window_facts(&self, window: isize) -> Option<WindowFacts> {
        window_facts(window)
    }

    fn open(&self) -> Result<Box<dyn Session>> {
        Ok(Box::new(ShellSession {
            _apartment: Apartment::enter()?,
        }))
    }
}

fn hwnd(window: isize) -> HWND {
    HWND(window as *mut _)
}

/// Class and hung state; None when the window does not exist. Sends no message.
fn window_facts(window: isize) -> Option<WindowFacts> {
    let hwnd = hwnd(window);
    // SAFETY: plain handle checks; a stale handle makes them fail, not crash.
    if window == 0 || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return None;
    }
    let mut name = [0u16; 64];
    // SAFETY: the buffer is valid for its length.
    let len = unsafe { GetClassNameW(hwnd, &mut name) } as usize;
    // SAFETY: as above.
    let hung = unsafe { IsHungAppWindow(hwnd) }.as_bool();
    Some(WindowFacts {
        class: String::from_utf16_lossy(&name[..len]),
        hung,
    })
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
// Shell session (worker thread only)
// ---------------------------------------------------------------------------------------

/// COM initialised as a single-threaded apartment for the life of the value.
struct Apartment;

impl Apartment {
    fn enter() -> Result<Apartment> {
        // SAFETY: no reserved pointer; balanced by `CoUninitialize` in `drop`.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()? };
        Ok(Apartment)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` in `enter`, on the same thread.
        unsafe { CoUninitialize() };
    }
}

/// Lives on one worker thread. Holds no COM object between requests, so an Explorer
/// restart leaves nothing stale behind.
struct ShellSession {
    _apartment: Apartment,
}

impl Session for ShellSession {
    fn folder(&mut self, request: &FolderRequest, deadline: Instant) -> Option<FolderOutcome> {
        let mut budget = Budget::new(deadline);
        let result = match request {
            FolderRequest::Window(window) => window_folder(&mut budget, *window),
            FolderRequest::Desktop => desktop_folder(&mut budget),
        };
        match result {
            Ok(outcome) => Some(outcome),
            Err(Stop::Expired) => None,
            Err(Stop::Capped) => Some(FolderOutcome::Failed(format!(
                "more than {MAX_CALLS} Shell calls"
            ))),
            Err(Stop::Failed(err)) => Some(FolderOutcome::Failed(err)),
        }
    }

    fn reveal(&mut self, window: isize, file: &Path, deadline: Instant) -> RevealStep {
        try_reveal(&mut Budget::new(deadline), window, file).unwrap_or(RevealStep::GiveUp)
    }
}

/// Why a lookup stopped early.
#[derive(Debug)]
enum Stop {
    Expired,
    /// More than [`MAX_CALLS`] calls.
    Capped,
    Failed(String),
}

/// Deadline and call cap of one request (rule 2).
struct Budget {
    deadline: Instant,
    calls: u32,
}

impl Budget {
    fn new(deadline: Instant) -> Budget {
        Budget { deadline, calls: 0 }
    }

    fn call<T>(
        &mut self,
        what: &str,
        f: impl FnOnce() -> Result<T>,
    ) -> std::result::Result<T, Stop> {
        if Instant::now() >= self.deadline {
            return Err(Stop::Expired);
        }
        if self.calls >= MAX_CALLS {
            return Err(Stop::Capped);
        }
        self.calls += 1;
        f().map_err(|err| Stop::Failed(format!("{what}: {}", err.message())))
    }
}

/// A failed call is skipped; a passed deadline or the call cap still stops the lookup.
fn soft<T>(result: std::result::Result<T, Stop>) -> std::result::Result<Option<T>, Stop> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Stop::Failed(_)) => Ok(None),
        Err(stop) => Err(stop),
    }
}

/// A PIDL allocated by the Shell for us; freed on drop.
struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        // SAFETY: the PIDL was allocated with the COM task allocator for this process and is
        // freed once; `CoTaskMemFree` accepts null.
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

/// A string allocated by the Shell with the COM task allocator; freed on drop.
struct CoString(PWSTR);

impl CoString {
    fn to_os(&self) -> OsString {
        // SAFETY: the Shell returned a valid NUL-terminated string that lives until drop.
        OsString::from_wide(unsafe { self.0.as_wide() })
    }
}

impl Drop for CoString {
    fn drop(&mut self) {
        // SAFETY: allocated with the COM task allocator, freed once; null is accepted.
        unsafe { CoTaskMemFree(Some(self.0.0 as *const _)) };
    }
}

fn shell_windows(budget: &mut Budget) -> std::result::Result<IShellWindows, Stop> {
    // `ShellWindows` is served by Explorer; the window checks before this made sure it runs.
    budget.call("ShellWindows", || {
        // SAFETY: plain object creation; the class id is a constant.
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }
    })
}

/// The browser of a Shell window entry (a tab, or the desktop).
fn browser_of(
    budget: &mut Budget,
    dispatch: &IDispatch,
) -> std::result::Result<IShellBrowser, Stop> {
    let services: IServiceProvider = budget.call("IServiceProvider", || dispatch.cast())?;
    budget.call("QueryService", || {
        // SAFETY: `services` is a live interface on this thread; the service id is a constant.
        unsafe { services.QueryService(&SID_STopLevelBrowser) }
    })
}

/// The browser of the active tab of a top-level Explorer window.
fn active_browser(budget: &mut Budget, window: isize) -> std::result::Result<IShellBrowser, Stop> {
    let active = tabs(window).active;
    let windows = shell_windows(budget)?;
    // SAFETY (closures below): every interface used is alive and owned by this thread.
    let count = budget.call("IShellWindows::Count", || unsafe { windows.Count() })?;
    for index in 0..count {
        let item = soft(budget.call("IShellWindows::Item", || unsafe {
            windows.Item(&i32_variant(index))
        }))?;
        let Some(dispatch) = item else { continue };
        let Some(app) = soft(budget.call("IWebBrowserApp", || dispatch.cast::<IWebBrowserApp>()))?
        else {
            continue;
        };
        let Some(top) = soft(budget.call("IWebBrowserApp::HWND", || unsafe { app.HWND() }))? else {
            continue;
        };
        if top.0 != window {
            continue;
        }
        let Some(browser) = soft(browser_of(budget, &dispatch))? else {
            continue;
        };
        let Some(active) = active else {
            // No tab windows (before Windows 11 22H2): one view per window.
            return Ok(browser);
        };
        let tab = soft(budget.call("IShellBrowser::GetWindow", || unsafe {
            browser.GetWindow()
        }))?;
        if tab.is_some_and(|tab| tab.0 as isize == active) {
            return Ok(browser);
        }
    }
    Err(Stop::Failed(
        "no Shell view for the active tab of this window".into(),
    ))
}

/// The desktop's browser (`Progman`).
fn desktop_browser(budget: &mut Budget) -> std::result::Result<IShellBrowser, Stop> {
    let windows = shell_windows(budget)?;
    let dispatch = budget.call("FindWindowSW", || {
        let mut found = 0;
        // SAFETY: the VARIANTs and the out-pointer outlive the call.
        unsafe {
            windows.FindWindowSW(
                &i32_variant(CSIDL_DESKTOP as i32),
                &VARIANT::default(),
                SWC_DESKTOP,
                &mut found,
                SWFO_NEEDDISPATCH,
            )
        }
    })?;
    browser_of(budget, &dispatch)
}

/// The active view of a browser and the item for the folder it shows.
fn view_folder(
    budget: &mut Budget,
    browser: &IShellBrowser,
) -> std::result::Result<(IShellView, IFolderView, IShellItem), Stop> {
    // SAFETY (closures below): every interface used is alive and owned by this thread.
    let view = budget.call("QueryActiveShellView", || unsafe {
        browser.QueryActiveShellView()
    })?;
    let folder_view: IFolderView = budget.call("IFolderView", || view.cast())?;
    let persist: IPersistFolder2 = budget.call("IFolderView::GetFolder", || unsafe {
        folder_view.GetFolder()
    })?;
    let pidl = Pidl(budget.call("GetCurFolder", || unsafe { persist.GetCurFolder() })?);
    // SAFETY: `pidl` is a valid absolute PIDL until it drops at the end of this function;
    // the item copies it.
    let item = budget.call("SHCreateItemFromIDList", || unsafe {
        SHCreateItemFromIDList(pidl.0)
    })?;
    Ok((view, folder_view, item))
}

fn display_name(
    budget: &mut Budget,
    item: &IShellItem,
    kind: SIGDN,
) -> std::result::Result<Option<OsString>, Stop> {
    let name = soft(budget.call("IShellItem::GetDisplayName", || {
        // SAFETY: `item` is alive; the returned string is owned by `CoString`.
        unsafe { item.GetDisplayName(kind) }.map(CoString)
    }))?;
    Ok(name.map(|name| name.to_os()))
}

/// `Folder` or `NotAFolder` for a Shell item.
fn item_outcome(
    budget: &mut Budget,
    item: &IShellItem,
) -> std::result::Result<FolderOutcome, Stop> {
    let path = display_name(budget, item, SIGDN_FILESYSPATH)?.map(PathBuf::from);
    let mask = SFGAO_FILESYSTEM | SFGAO_FOLDER | SFGAO_STREAM;
    // SAFETY: `item` is alive. S_FALSE (not all bits set) still returns the bits.
    let attributes = soft(budget.call("IShellItem::GetAttributes", || unsafe {
        item.GetAttributes(mask)
    }))?
    .map_or(0, |a| a.0);
    if let Some(folder) = folder_path(path.clone(), attributes) {
        return Ok(FolderOutcome::Folder(folder));
    }
    let name = display_name(budget, item, SIGDN_NORMALDISPLAY)?;
    Ok(not_a_folder(
        name.map(|n| n.to_string_lossy().into_owned()),
        path.as_deref(),
    ))
}

fn window_folder(budget: &mut Budget, window: isize) -> std::result::Result<FolderOutcome, Stop> {
    let browser = active_browser(budget, window)?;
    let (_, _, item) = view_folder(budget, &browser)?;
    item_outcome(budget, &item)
}

fn desktop_folder(budget: &mut Budget) -> std::result::Result<FolderOutcome, Stop> {
    // Proves the desktop view is there; the folder itself is the user's Desktop.
    desktop_browser(budget)?;
    let path = budget.call("SHGetKnownFolderPath", || {
        // SAFETY: constant folder id; the returned string is owned by `CoString`.
        unsafe { SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None) }.map(CoString)
    })?;
    Ok(FolderOutcome::Folder(path.to_os().into()))
}

fn try_reveal(
    budget: &mut Budget,
    window: isize,
    file: &Path,
) -> std::result::Result<RevealStep, Stop> {
    let (Some(parent), Some(name)) = (file.parent(), file.file_name()) else {
        return Ok(RevealStep::GiveUp);
    };
    let facts = window_facts(window);
    let browser = match facts.as_ref().and_then(|f| shell_kind(&f.class)) {
        Some(Shell::Desktop) => desktop_browser(budget)?,
        Some(Shell::Explorer) => active_browser(budget, window)?,
        None => return Ok(RevealStep::GiveUp),
    };
    let (view, folder_view, item) = view_folder(budget, &browser)?;
    // The desktop view's own folder is the merged desktop, whose path is the user's Desktop.
    let shown = display_name(budget, &item, SIGDN_FILESYSPATH)?;
    if !shown.is_some_and(|shown| same_folder(Path::new(&shown), parent)) {
        return Ok(RevealStep::GiveUp);
    }
    // SAFETY (closures below): every interface used is alive and owned by this thread.
    let folder: IShellFolder = budget.call("IFolderView::GetFolder", || unsafe {
        folder_view.GetFolder()
    })?;
    let wide: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
    let mut child = Pidl(std::ptr::null_mut());
    let parsed = soft(budget.call("IShellFolder::ParseDisplayName", || {
        // SAFETY: `wide` is NUL-terminated and outlives the call; `child.0` receives a PIDL
        // that `child` frees; the attribute pointer may be null.
        unsafe {
            folder.ParseDisplayName(
                HWND::default(),
                None,
                PCWSTR(wide.as_ptr()),
                None,
                &mut child.0,
                std::ptr::null_mut(),
            )
        }
    }))?;
    if parsed.is_none() || child.0.is_null() {
        return Ok(RevealStep::Retry);
    }
    let flags =
        (SVSI_SELECT.0 | SVSI_ENSUREVISIBLE.0 | SVSI_DESELECTOTHERS.0 | SVSI_FOCUSED.0) as u32;
    // SAFETY: `child` is a valid child PIDL of the view's folder for this call.
    if soft(budget.call("IShellView::SelectItem", || unsafe {
        view.SelectItem(child.0, flags)
    }))?
    .is_none()
    {
        return Ok(RevealStep::Retry);
    }
    // The view ignores the selection of an item it does not list yet: check it took.
    let view2: IFolderView2 = budget.call("IFolderView2", || view.cast())?;
    // SAFETY: as above.
    let state = soft(budget.call("GetSelectionState", || unsafe {
        view2.GetSelectionState(child.0)
    }))?;
    Ok(if state.is_some_and(|s| s & SVSI_SELECT.0 as u32 != 0) {
        RevealStep::Done
    } else {
        RevealStep::Retry
    })
}

fn i32_variant(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

/// For the self-test only: the outcome for a Shell parsing name (a path, `::{CLSID}`,
/// `search-ms:`), by the same rules as a window lookup, but without an Explorer window.
/// Makes Shell calls on the calling thread, which must be in a single-threaded apartment;
/// never call it from the UI thread.
#[doc(hidden)]
pub fn outcome_for_parsing_name(name: &str) -> FolderOutcome {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let item =
        unsafe { SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None) };
    let item = match item {
        Ok(item) => item,
        Err(err) => {
            return FolderOutcome::Failed(format!(
                "SHCreateItemFromParsingName: {}",
                err.message()
            ));
        }
    };
    let mut budget = Budget::new(Instant::now() + Duration::from_secs(5));
    match item_outcome(&mut budget, &item) {
        Ok(outcome) => outcome,
        Err(Stop::Expired | Stop::Capped) => FolderOutcome::TimedOut,
        Err(Stop::Failed(err)) => FolderOutcome::Failed(err),
    }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use windows::Win32::System::Threading::WaitForSingleObject;

    const FS_FOLDER: u32 = SFGAO_FILESYSTEM.0 | SFGAO_FOLDER.0;

    // --- Pure rules ---

    #[test]
    fn folder_paths() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            folder_path(p(r"C:\Users\a\Pictures"), FS_FOLDER),
            p(r"C:\Users\a\Pictures")
        );
        assert_eq!(
            folder_path(p(r"\\server\share\dir"), FS_FOLDER),
            p(r"\\server\share\dir")
        );
        assert_eq!(
            folder_path(p(r"\\server\share"), FS_FOLDER),
            p(r"\\server\share")
        );
        // A zip opened as a folder, and a library: a file path, but a stream.
        assert_eq!(
            folder_path(p(r"C:\a\b.zip"), FS_FOLDER | SFGAO_STREAM.0),
            None
        );
        assert_eq!(
            folder_path(
                p(r"C:\x\Libraries\Documents.library-ms"),
                FS_FOLDER | SFGAO_STREAM.0
            ),
            None
        );
        // No path (This PC, Home, Recycle Bin, search results).
        assert_eq!(folder_path(None, FS_FOLDER), None);
        assert_eq!(folder_path(None, 0), None);
        // A path but not a file-system folder.
        assert_eq!(folder_path(p(r"C:\a\b.txt"), SFGAO_FILESYSTEM.0), None);
        assert_eq!(folder_path(p(r"C:\a"), SFGAO_FOLDER.0), None);
        assert_eq!(folder_path(p(""), FS_FOLDER), None);
        // A network computer, not a share.
        assert_eq!(folder_path(p(r"\\server"), FS_FOLDER), None);
        assert_eq!(folder_path(p(r"\\server\"), FS_FOLDER), None);
    }

    #[test]
    fn not_a_folder_names() {
        let name = |n: Option<&str>, p: Option<&str>| match not_a_folder(
            n.map(Into::into),
            p.map(Path::new),
        ) {
            FolderOutcome::NotAFolder { display_name } => display_name,
            other => panic!("{other:?}"),
        };
        assert_eq!(name(Some("This PC"), None), "This PC");
        assert_eq!(name(Some(" "), Some(r"C:\a\b.zip")), r"C:\a\b.zip");
        assert_eq!(name(None, None), "(unnamed)");
        let long = "x".repeat(300);
        assert_eq!(name(Some(&long), None).chars().count(), NAME_LIMIT + 1);
    }

    #[test]
    fn same_folders() {
        assert!(same_folder(
            Path::new(r"C:\Users\A\Desktop"),
            Path::new(r"c:\users\a\desktop\")
        ));
        assert!(same_folder(Path::new("C:/x/y"), Path::new(r"C:\x\y")));
        assert!(!same_folder(Path::new(r"C:\x\y"), Path::new(r"C:\x\yz")));
    }

    #[test]
    fn active_tab_rules() {
        assert_eq!(active_tab(&[]), None);
        assert_eq!(active_tab(&[(5, true)]), Some(5));
        // The visible one wins over z-order.
        assert_eq!(active_tab(&[(5, false), (6, true), (7, false)]), Some(6));
        // Ambiguous (minimised window, or a moment during a switch): first in z-order.
        assert_eq!(active_tab(&[(5, false), (6, false)]), Some(5));
        assert_eq!(active_tab(&[(5, true), (6, true)]), Some(5));
    }

    fn facts(class: &str, hung: bool) -> WindowFacts {
        WindowFacts {
            class: class.into(),
            hung,
        }
    }

    #[test]
    fn window_checks() {
        let win = FolderRequest::Window(1);
        assert_eq!(
            check_window(&win, Some(&facts("CabinetWClass", false))),
            None
        );
        assert_eq!(
            check_window(&win, Some(&facts("CabinetWClass", true))),
            Some(FolderOutcome::TimedOut)
        );
        assert!(matches!(
            check_window(&win, None),
            Some(FolderOutcome::Failed(_))
        ));
        assert!(matches!(
            check_window(&win, Some(&facts("Notepad", false))),
            Some(FolderOutcome::Failed(_))
        ));
        assert!(matches!(
            check_window(&win, Some(&facts("Progman", false))),
            Some(FolderOutcome::Failed(_))
        ));
        let desk = FolderRequest::Desktop;
        assert_eq!(check_window(&desk, Some(&facts("Progman", false))), None);
        assert_eq!(
            check_window(&desk, Some(&facts("Progman", true))),
            Some(FolderOutcome::TimedOut)
        );
        assert!(matches!(
            check_window(&desk, None),
            Some(FolderOutcome::Failed(_))
        ));
        assert!(can_reveal(Some(&facts("CabinetWClass", false))));
        assert!(can_reveal(Some(&facts("WorkerW", false))));
        assert!(!can_reveal(Some(&facts("CabinetWClass", true))));
        assert!(!can_reveal(Some(&facts("Notepad", false))));
        assert!(!can_reveal(None));
    }

    // --- Controller, with a fake backend (no COM) ---

    /// `Window(n)`: n < 0 is not an Explorer window; otherwise the lookup takes n ms.
    /// `Desktop`: a hung desktop. Reveal on window w: `Retry` until attempt w, then `Done`;
    /// w = 99 hangs for 1.5 s.
    struct Fake {
        log: Arc<Mutex<Vec<String>>>,
    }

    struct FakeSession {
        log: Arc<Mutex<Vec<String>>>,
        reveals: usize,
    }

    impl Backend for Fake {
        fn facts(&self, request: &FolderRequest) -> Option<WindowFacts> {
            Some(match request {
                FolderRequest::Window(n) if *n < 0 => facts("Notepad", false),
                FolderRequest::Window(_) => facts("CabinetWClass", false),
                FolderRequest::Desktop => facts("Progman", true),
            })
        }
        fn window_facts(&self, window: isize) -> Option<WindowFacts> {
            (window >= 0).then(|| facts("CabinetWClass", false))
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            Ok(Box::new(FakeSession {
                log: self.log.clone(),
                reveals: 0,
            }))
        }
    }

    impl Session for FakeSession {
        fn folder(&mut self, request: &FolderRequest, deadline: Instant) -> Option<FolderOutcome> {
            let FolderRequest::Window(n) = *request else {
                unreachable!()
            };
            self.log.lock().unwrap().push(format!("folder {n}"));
            thread::sleep(Duration::from_millis(n as u64));
            (Instant::now() < deadline)
                .then(|| FolderOutcome::Folder(PathBuf::from(format!("ran {n}"))))
        }
        fn reveal(&mut self, window: isize, _: &Path, _: Instant) -> RevealStep {
            self.log.lock().unwrap().push(format!("reveal {window}"));
            if window == 99 {
                thread::sleep(Duration::from_millis(1500));
            }
            self.reveals += 1;
            if self.reveals > window as usize {
                RevealStep::Done
            } else {
                RevealStep::Retry
            }
        }
    }

    fn fake(grace: Duration) -> (Explorer, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let explorer = Explorer::with_backend(Arc::new(Fake { log: log.clone() }), grace).unwrap();
        (explorer, log)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Waits on the wake handle until `id` is answered; returns every answer seen.
    fn wait_for(
        explorer: &Explorer,
        id: RequestId,
        limit: Duration,
    ) -> Vec<(RequestId, FolderOutcome)> {
        let until = Instant::now() + limit;
        let mut seen = Vec::new();
        while Instant::now() < until {
            // SAFETY: the handle is alive while `explorer` is.
            unsafe { WaitForSingleObject(explorer.wake_handle(), 20) };
            while let Some(answer) = explorer.poll() {
                let done = answer.0 == id;
                seen.push(answer);
                if done {
                    return seen;
                }
            }
        }
        seen
    }

    fn ran(answers: &[(RequestId, FolderOutcome)], id: RequestId, n: isize) -> bool {
        matches!(answers.last(), Some((i, FolderOutcome::Folder(p))) if *i == id && *p == Path::new(&format!("ran {n}")))
    }

    fn timed_out(answers: &[(RequestId, FolderOutcome)]) -> bool {
        matches!(answers.last(), Some((_, FolderOutcome::TimedOut)))
    }

    #[test]
    fn newest_request_replaces_waiting_ones() {
        let (explorer, log) = fake(STUCK_GRACE);
        let mut last = RequestId(0);
        for n in 30..60 {
            last = explorer.request(FolderRequest::Window(n), ms(3000));
        }
        let answers = wait_for(&explorer, last, Duration::from_secs(3));
        assert!(ran(&answers, last, 59), "{answers:?}");
        let log = log.lock().unwrap().clone();
        assert!(log.len() <= 3, "ran {log:?}");
        assert_eq!(answers.len(), log.len());
        assert!(explorer.shutdown());
    }

    #[test]
    fn deadline_answers_at_once_and_worker_continues() {
        let (explorer, _) = fake(STUCK_GRACE);
        let started = Instant::now();
        let slow = explorer.request(FolderRequest::Window(200), ms(50));
        assert!(timed_out(&wait_for(
            &explorer,
            slow,
            Duration::from_secs(1)
        )));
        assert!(started.elapsed() < ms(150), "{:?}", started.elapsed());
        let next = explorer.request(FolderRequest::Window(1), ms(1000));
        let answers = wait_for(&explorer, next, Duration::from_secs(1));
        assert!(ran(&answers, next, 1), "{answers:?}");
        assert!(explorer.shutdown());
    }

    #[test]
    fn zero_deadline_times_out_without_running() {
        let (explorer, log) = fake(STUCK_GRACE);
        let id = explorer.request(FolderRequest::Window(1), ms(0));
        assert!(timed_out(&wait_for(&explorer, id, Duration::from_secs(1))));
        assert!(log.lock().unwrap().is_empty());
        let next = explorer.request(FolderRequest::Window(2), ms(500));
        assert!(ran(
            &wait_for(&explorer, next, Duration::from_secs(1)),
            next,
            2
        ));
        assert!(explorer.shutdown());
    }

    #[test]
    fn wrong_or_hung_window_skips_the_worker() {
        let (explorer, log) = fake(STUCK_GRACE);
        let wrong = explorer.request(FolderRequest::Window(-1), ms(500));
        assert!(matches!(
            wait_for(&explorer, wrong, Duration::from_secs(1)).last(),
            Some((_, FolderOutcome::Failed(_)))
        ));
        let hung = explorer.request(FolderRequest::Desktop, ms(500));
        assert!(timed_out(&wait_for(
            &explorer,
            hung,
            Duration::from_secs(1)
        )));
        assert!(log.lock().unwrap().is_empty());
        assert!(explorer.shutdown());
    }

    #[test]
    fn stuck_worker_is_replaced_and_threads_are_capped() {
        let grace = ms(50);
        let (explorer, _) = fake(grace);
        // Three lookups that hang for 1.5 s each: every one strands a worker.
        for _ in 0..3 {
            let id = explorer.request(FolderRequest::Window(1500), ms(30));
            assert!(timed_out(&wait_for(&explorer, id, Duration::from_secs(1))));
            thread::sleep(grace * 3);
            assert!(explorer.live.load(Ordering::SeqCst) <= MAX_WORKERS);
        }
        // At the cap: a quick lookup times out instead of piling up threads.
        let id = explorer.request(FolderRequest::Window(0), ms(50));
        let answers = wait_for(&explorer, id, Duration::from_secs(1));
        assert!(timed_out(&answers), "{answers:?}");
        assert!(explorer.live.load(Ordering::SeqCst) <= MAX_WORKERS);
        // Once the stuck calls return, a fresh worker answers again.
        thread::sleep(ms(1700));
        let id = explorer.request(FolderRequest::Window(3), ms(500));
        let answers = wait_for(&explorer, id, Duration::from_secs(1));
        assert!(ran(&answers, id, 3), "{answers:?}");
        explorer.shutdown();
    }

    fn reveals(log: &Mutex<Vec<String>>) -> usize {
        log.lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("reveal"))
            .count()
    }

    #[test]
    fn reveal_retries_until_selected() {
        let (explorer, log) = fake(STUCK_GRACE);
        // Done on the third attempt (after 0, 100 and 150 ms).
        explorer.reveal(2, PathBuf::from(r"C:\x\a.png"));
        thread::sleep(ms(1500));
        assert_eq!(reveals(&log), 3, "{:?}", log.lock().unwrap());
        assert!(explorer.shutdown());
    }

    #[test]
    fn reveal_gives_up_after_the_last_attempt() {
        let (explorer, log) = fake(STUCK_GRACE);
        explorer.reveal(1000, PathBuf::from(r"C:\x\a.png"));
        let total: Duration = REVEAL_DELAYS.iter().sum();
        thread::sleep(total + ms(800));
        assert_eq!(reveals(&log), REVEAL_DELAYS.len());
        // An unknown window is dropped before the worker.
        explorer.reveal(-1, PathBuf::from(r"C:\x\a.png"));
        thread::sleep(ms(100));
        assert_eq!(reveals(&log), REVEAL_DELAYS.len());
        assert!(explorer.shutdown());
    }

    #[test]
    fn folder_request_goes_before_a_reveal_and_stuck_reveal_is_dropped() {
        let (explorer, log) = fake(ms(50));
        explorer.reveal(1000, PathBuf::from(r"C:\x\a.png"));
        let id = explorer.request(FolderRequest::Window(5), ms(500));
        assert!(ran(&wait_for(&explorer, id, Duration::from_secs(1)), id, 5));
        assert_eq!(
            log.lock().unwrap().first().map(String::as_str),
            Some("folder 5")
        );
        // A reveal that hangs strands its worker and is not tried again; lookups go on.
        explorer.reveal(99, PathBuf::from(r"C:\x\a.png"));
        thread::sleep(ms(700));
        let id = explorer.request(FolderRequest::Window(1), ms(500));
        assert!(ran(&wait_for(&explorer, id, Duration::from_secs(1)), id, 1));
        thread::sleep(ms(1500));
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|l| *l == "reveal 99")
                .count(),
            1
        );
        explorer.shutdown();
    }
}
