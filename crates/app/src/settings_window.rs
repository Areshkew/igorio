//! Native settings UI. A fresh STA owns each window, its controls, and the settings store.
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use igorio_kit::theme;
use windows::Win32::Foundation::{
    COLORREF, CloseHandle, ERROR_CANCELLED, ERROR_CLASS_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND,
    HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemServices::{SS_NOTIFY, SS_OWNERDRAW};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, IsWindowEnabled, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_MENU, VK_RETURN, VK_TAB,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    IShellItem, RemoveWindowSubclass, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, HRESULT, PCWSTR, Result, w};

use crate::input::Held;
use crate::layout::CardSize;
use crate::overlay::{cursor_pos, monitor_at};
use crate::settings::{DefaultFolders, Settings};
use crate::shortcuts::{
    Action, Chord, Hotkeys, RecorderInput, Registration, RegistrationReport, Shortcuts,
    chord_from_key_event, display_for_layout, held_for_message,
};

const SCREENSHOTS: &str = "fairy.screenshots";
const NOTES: &str = "fairy.notes";
const RECORDING: &str = crate::recording::feature::CAPABILITY;
/// Parts of a capability's key line drawn as plain text, not as key chips.
const PLAIN_WORDS: &[&str] = &["or", "Hold", "and click on empty space"];
const CLASS: PCWSTR = w!("IgorioSettings");
const OPEN: u32 = WM_APP + 1;
const PICK: u32 = WM_APP + 2;
const SHORTCUT_STATUS: u32 = WM_APP + 6;
const TEST_DIE: u32 = WM_APP + 7;
/// A recording showed the zoom hint: count it (`Lifetime::hint_bumps`) and save.
const HINT_SHOWN: u32 = WM_APP + 8;
/// Tags the window's registration checks. Tag 0 is the untagged status after a save or resume.
static NEXT_TAG: AtomicUsize = AtomicUsize::new(1);
const WORKER_EXITED: isize = -1;
const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);
const EX_STYLE: WINDOW_EX_STYLE = WS_EX_APPWINDOW;

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

/// Everything the overlay and workers need from a saved settings snapshot.
pub struct LiveSettings {
    pub screenshot_detector: bool,
    /// The Notes capability: note windows and the note gesture.
    pub notes: bool,
    /// The Screen recording capability: the shortcut, the tray item and the tool menu entry.
    pub recording: bool,
    pub card_size: CardSize,
    pub snapshot: Settings,
}

impl From<Settings> for LiveSettings {
    fn from(snapshot: Settings) -> Self {
        Self {
            screenshot_detector: snapshot.is_enabled(SCREENSHOTS),
            notes: snapshot.is_enabled(NOTES),
            recording: snapshot.is_enabled(RECORDING),
            card_size: snapshot.card_size,
            snapshot,
        }
    }
}

struct Wake(HANDLE);

// SAFETY: the kernel event can be signalled and waited on by different threads.
unsafe impl Send for Wake {}
// SAFETY: Arc keeps the event open while either thread uses it.
unsafe impl Sync for Wake {}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: this owns the event handle, and closes it once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct Lifetime {
    hwnd: AtomicIsize,
    stop: AtomicBool,
    wake: Arc<Wake>,
    requests: Sender<Request>,
    reports: Mutex<Vec<RegistrationReport>>,
    /// Test only: the window stops sending requests, as if its thread had died.
    muted: AtomicBool,
    test_modifiers: Mutex<Held>,
    test_choice: Mutex<Option<(Folder, PathBuf)>>,
    /// Recordings that showed the zoom hint, not yet counted in the window's settings.
    hint_bumps: AtomicU32,
}

impl Lifetime {
    fn emit(&self, request: Request) {
        if !self.muted.load(SeqCst) && self.requests.send(request).is_ok() {
            self.signal();
        }
    }

    fn post(&self, message: u32) {
        let hwnd = self.hwnd.load(SeqCst);
        if hwnd != 0 && hwnd != WORKER_EXITED {
            // SAFETY: scalar message to this worker's window. If it is gone, posting fails.
            let _ = unsafe {
                PostMessageW(Some(HWND(hwnd as *mut c_void)), message, WPARAM(0), LPARAM(0))
            };
        }
    }

    fn signal(&self) {
        // SAFETY: the Arc keeps the auto-reset event live during this call.
        let _ = unsafe { SetEvent(self.wake.0) };
    }

    fn exited(&self) -> bool {
        self.hwnd.load(SeqCst) == WORKER_EXITED
    }
}

/// What the settings thread asks of the UI loop.
enum Request {
    Saved(Settings),
    /// A recorder opened: stop every hotkey, so the keys reach the window.
    Pause,
    /// Register `proposed` (not saved yet), resume, and report back with `tag`.
    Check { tag: usize, proposed: Shortcuts },
    /// The recorder closed: register the saved shortcuts again and resume.
    Resume,
    /// The window opened: report the registrations.
    Status,
}

/// The hotkey service as the settings window uses it. The UI loop owns the service and
/// passes it to [`SettingsWindow::saved`]; the self-test passes a stand-in.
pub trait ShortcutService {
    fn set_paused(&self, paused: bool);
    fn apply(&self, shortcuts: &Shortcuts);
    /// Queues a status report with this tag behind the earlier commands. False if it cannot.
    fn report_after_commands(&self, tag: usize) -> bool;
    fn reports(&self) -> Vec<RegistrationReport>;
}

impl ShortcutService for Hotkeys {
    fn set_paused(&self, paused: bool) {
        Hotkeys::set_paused(self, paused);
    }

    fn apply(&self, shortcuts: &Shortcuts) {
        Hotkeys::apply(self, shortcuts);
    }

    fn report_after_commands(&self, tag: usize) -> bool {
        Hotkeys::report_after_commands(self, tag)
    }

    fn reports(&self) -> Vec<RegistrationReport> {
        Hotkeys::reports(self)
    }
}

#[derive(Clone)]
struct Options {
    path: PathBuf,
    test: Option<TestMode>,
}

/// Isolated window configuration for the self-test. Test windows show without activation.
#[doc(hidden)]
#[derive(Clone)]
pub struct TestMode {
    pub notes: PathBuf,
    pub captures: PathBuf,
    pub dark: bool,
    pub dpi: u32,
    pub run_suffix: u32,
}

impl TestMode {
    fn registry(&self) -> Registry {
        Registry(wide(format!(
            "Igorio.SettingsSelftest.{}.{}",
            std::process::id(),
            self.run_suffix
        )))
    }

    /// Cleans up this test process's unique Run value.
    pub fn remove_run_value(&self) -> io::Result<()> {
        self.registry().write(None)
    }
}

/// Overlay-owned endpoint. Open only spawns or posts. Settings writes stay on the STA.
///
/// It also holds the guarantee that a shortcut recorder never leaves the hotkey service
/// paused: from the first `Pause` or `Check` until the matching `Resume`, `session` names the
/// window that asked, and [`saved`](Self::saved) resumes the service itself as soon as that
/// window's thread has ended (closed, failed or unwound) without asking. [`shutdown`](Self::shutdown)
/// does the same when the app quits.
pub struct SettingsWindow {
    options: Options,
    wake: Arc<Wake>,
    requests_tx: Sender<Request>,
    requests_rx: Receiver<Request>,
    worker: Option<(Arc<Lifetime>, JoinHandle<()>)>,
    session: Option<Arc<Lifetime>>,
    /// The last short-lived thread that counted a hint while the window was closed. The next
    /// one, and a window that opens, wait for it first: one writer at a time.
    counter: Option<JoinHandle<()>>,
}

impl SettingsWindow {
    pub fn new(path: PathBuf) -> Result<Self> {
        Self::with_options(Options { path, test: None })
    }

    #[doc(hidden)]
    pub fn for_test(path: PathBuf, test: TestMode) -> Result<Self> {
        Self::with_options(Options { path, test: Some(test) })
    }

    fn with_options(options: Options) -> Result<Self> {
        // SAFETY: unnamed auto-reset event; the result is owned by Wake.
        let wake = Arc::new(Wake(unsafe { CreateEventW(None, false, false, None)? }));
        let (requests_tx, requests_rx) = mpsc::channel();
        Ok(Self {
            options,
            wake,
            requests_tx,
            requests_rx,
            worker: None,
            session: None,
            counter: None,
        })
    }

    /// A recording showed the "Hold Alt and scroll to zoom" hint: count it in settings.json
    /// (`recording_hints_shown`). Never blocks. The new settings arrive through
    /// [`saved`](Self::saved) like any other save.
    ///
    /// The settings window is the one writer of the file while it is open, so an open window
    /// does the counting and saving itself. When it is closed, a short-lived thread loads,
    /// counts and saves (`Settings::note_recording_hint_shown`); a window opened meanwhile
    /// waits for that thread before it loads the file, and so does the next such thread.
    pub fn note_recording_hint_shown(&mut self) {
        if let Some((life, thread)) = &self.worker
            && !thread.is_finished()
            && !life.exited()
        {
            // Counted first: a window still starting picks it up once it has loaded.
            life.hint_bumps.fetch_add(1, SeqCst);
            life.post(HINT_SHOWN);
            return;
        }
        let before = self.counter.take();
        let path = self.options.path.clone();
        let requests = self.requests_tx.clone();
        let wake = Arc::clone(&self.wake);
        let spawned = thread::Builder::new().name("settings-hint".into()).spawn(move || {
            if let Some(before) = before {
                let _ = before.join();
            }
            match Settings::note_recording_hint_shown(&path) {
                Ok(settings) => {
                    if requests.send(Request::Saved(settings)).is_ok() {
                        // SAFETY: the Arc keeps the auto-reset event live during this call.
                        let _ = unsafe { SetEvent(wake.0) };
                    }
                }
                Err(error) => eprintln!("the recording hint count was not saved: {error}"),
            }
        });
        match spawned {
            Ok(thread) => self.counter = Some(thread),
            Err(error) => eprintln!("the recording hint count was not saved: {error}"),
        }
    }

    pub fn wake_handle(&self) -> HANDLE {
        self.wake.0
    }

    /// Runs on the UI loop: carries out the window's requests to the hotkey service, sends
    /// its reports back to the window, and returns the saved snapshots, oldest first.
    /// `current` is what the service has registered: the last saved shortcuts.
    pub fn saved(
        &mut self,
        service: Option<&dyn ShortcutService>,
        current: &Shortcuts,
    ) -> Vec<Settings> {
        let mut current = current.clone();
        let mut saved = Vec::new();
        while let Ok(request) = self.requests_rx.try_recv() {
            match request {
                Request::Saved(settings) => {
                    if settings.shortcuts != current {
                        current = settings.shortcuts.clone();
                        if let Some(service) = service {
                            service.apply(&current);
                        }
                        self.report(service, 0);
                    }
                    saved.push(settings);
                }
                Request::Pause => {
                    self.open_session();
                    if let Some(service) = service {
                        service.set_paused(true);
                    }
                }
                Request::Check { tag, proposed } => {
                    self.open_session();
                    if let Some(service) = service {
                        service.apply(&proposed);
                        service.set_paused(false);
                    }
                    self.report(service, tag);
                }
                Request::Resume => self.resume(service, &current),
                Request::Status => self.report(service, 0),
            }
        }
        if self.session.as_ref().is_some_and(|life| life.exited()) {
            self.resume(service, &current);
        }
        if let Some(service) = service {
            for report in service.reports() {
                self.publish_registration(report);
            }
        }
        saved
    }

    fn open_session(&mut self) {
        if self.session.is_none() {
            self.session = self.worker.as_ref().map(|(life, _)| Arc::clone(life));
        }
    }

    fn resume(&mut self, service: Option<&dyn ShortcutService>, current: &Shortcuts) {
        self.session = None;
        if let Some(service) = service {
            service.apply(current);
            service.set_paused(false);
        }
        self.report(service, 0);
    }

    fn report(&self, service: Option<&dyn ShortcutService>, tag: usize) {
        if service.is_some_and(|service| service.report_after_commands(tag)) {
            return;
        }
        let entries = Action::ALL
            .iter()
            .map(|&action| {
                let status = if action.available() {
                    Registration::Failed("Keyboard shortcuts are off.".into())
                } else {
                    Registration::Unavailable
                };
                (action, status)
            })
            .collect();
        self.publish_registration(RegistrationReport { tag, entries });
    }

    /// Closes the window on quit and leaves the hotkey service running, never paused.
    pub fn shutdown(mut self, service: Option<&dyn ShortcutService>, current: &Shortcuts) {
        if !self.close() {
            eprintln!("settings thread did not exit within the quit budget");
        }
        let _ = self.saved(service, current);
        if self.session.is_some() {
            self.resume(service, current);
        }
    }

    #[doc(hidden)]
    pub fn publish_registration(&self, report: RegistrationReport) {
        if let Some((worker, _)) = &self.worker {
            worker.reports.lock().unwrap_or_else(|e| e.into_inner()).push(report);
            worker.post(SHORTCUT_STATUS);
        }
    }

    /// Test only: the modifiers the recorder reads instead of `GetKeyState`, which a sent
    /// key message does not change.
    #[doc(hidden)]
    pub fn test_modifiers(&self, held: Held) {
        if self.options.test.is_some()
            && let Some((worker, _)) = &self.worker
        {
            *worker.test_modifiers.lock().unwrap_or_else(|e| e.into_inner()) = held;
        }
    }

    /// Test only: the window thread ends without a word to the UI loop, as if it had died.
    #[doc(hidden)]
    pub fn test_die(&self) {
        if let Some(hwnd) = self.test_hwnd() {
            // SAFETY: scalar test command to our own settings window.
            unsafe { SendMessageW(hwnd, TEST_DIE, None, None) };
        }
    }

    pub fn open(&mut self) -> io::Result<()> {
        if let Some((life, thread)) = &self.worker
            && !thread.is_finished()
            && !life.exited()
        {
            life.post(OPEN);
            return Ok(());
        }
        let life = Arc::new(Lifetime {
            hwnd: AtomicIsize::new(0),
            stop: AtomicBool::new(false),
            wake: Arc::clone(&self.wake),
            requests: self.requests_tx.clone(),
            reports: Mutex::new(Vec::new()),
            muted: AtomicBool::new(false),
            test_modifiers: Mutex::new(Held::default()),
            test_choice: Mutex::new(None),
            hint_bumps: AtomicU32::new(0),
        });
        let options = self.options.clone();
        let worker_life = Arc::clone(&life);
        let counter = self.counter.take();
        let thread = thread::Builder::new().name("settings".into()).spawn(move || {
            // A hint counted while the window was closed is saved before the window loads.
            if let Some(counter) = counter {
                let _ = counter.join();
            }
            /// Marks the worker gone and wakes the UI loop on every way out, unwinding included.
            struct Exit(Arc<Lifetime>);
            impl Drop for Exit {
                fn drop(&mut self) {
                    // run_window has destroyed the HWND and released the STA.
                    self.0.hwnd.store(WORKER_EXITED, SeqCst);
                    self.0.signal();
                }
            }
            let exit = Exit(worker_life);
            if let Err(error) = run_window(&options, &exit.0) {
                eprintln!("could not open settings: {error}");
            }
        })?;
        self.worker = Some((life, thread));
        Ok(())
    }

    /// Requests closure, including from within the picker's modal loop, and bounds the
    /// wait on quit even if a filesystem or Shell provider is unresponsive.
    pub fn close(&mut self) -> bool {
        if let Some(counter) = self.counter.take() {
            // SAFETY: JoinHandle keeps the borrowed thread handle live throughout the wait.
            let done = unsafe { WaitForSingleObject(HANDLE(counter.as_raw_handle()), 950) };
            if done == WAIT_OBJECT_0 {
                let _ = counter.join();
            }
        }
        let Some((life, thread)) = self.worker.take() else {
            return true;
        };
        life.stop.store(true, SeqCst);
        life.post(WM_CLOSE);
        // SAFETY: JoinHandle keeps the borrowed thread handle live throughout the wait.
        let done = unsafe { WaitForSingleObject(HANDLE(thread.as_raw_handle()), 950) };
        done == WAIT_OBJECT_0 && thread.join().is_ok()
    }

    #[doc(hidden)]
    pub fn test_hwnd(&self) -> Option<HWND> {
        self.options.test.as_ref()?;
        let hwnd = self.worker.as_ref()?.0.hwnd.load(SeqCst);
        (hwnd != 0 && hwnd != WORKER_EXITED).then_some(HWND(hwnd as *mut c_void))
    }

    /// Synchronously exercise the same application path as a completed folder picker.
    #[doc(hidden)]
    pub fn test_choose_folder(&self, notes: bool, path: &Path) {
        if let Some(hwnd) = self.test_hwnd() {
            let folder = if notes { Folder::Notes } else { Folder::Captures };
            let life = &self.worker.as_ref().expect("open test window").0;
            if let Ok(mut choice) = life.test_choice.lock() {
                *choice = Some((folder, path.to_owned()));
            }
            // SAFETY: scalar notification to the test window; the path is in the mailbox.
            unsafe { SendMessageW(hwnd, TEST_CHOOSE, None, None) };
        }
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        if !self.close() {
            eprintln!("settings thread did not exit within the quit budget");
        }
    }
}

const TEST_CHOOSE: u32 = WM_APP + 3;

/// Isolated compositor samples. A scene changes only the test clock and paint state.
#[doc(hidden)]
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum TestScene {
    Settled,
    SwitchSlide,
    CardSlide,
    ButtonHover,
    ButtonPress,
    SwitchFocus,
    CardFocus,
    /// The page switch an eighth of the way through its time: about half of its distance.
    PageSlide,
}

const TEST_SCENE: u32 = WM_APP + 4;
const TEST_IDLE: u32 = WM_APP + 5;

impl SettingsWindow {
    #[doc(hidden)]
    pub fn test_scene(&self, scene: TestScene) {
        if let Some(hwnd) = self.test_hwnd() {
            // SAFETY: scalar test command to our own settings window.
            unsafe { SendMessageW(hwnd, TEST_SCENE, Some(WPARAM(scene as usize)), None) };
        }
    }

    #[doc(hidden)]
    pub fn test_idle(&self) -> bool {
        self.test_hwnd().is_some_and(|hwnd| {
            // SAFETY: scalar query to our own settings window.
            unsafe { SendMessageW(hwnd, TEST_IDLE, None, None) }.0 == 1
        })
    }
}

impl Window<'_> {
    fn test_scene(&self, scene: usize) {
        let settings = self.model.borrow().settings.clone();
        let size = match settings.card_size {
            CardSize::Small => 0,
            CardSize::Medium => 1,
            CardSize::Large => 2,
        };
        let dimensions = if scene == TestScene::CardSlide as usize {
            CardSize::Large
        } else {
            settings.card_size
        }
        .thumb_max();
        self.text(CardDimensions, &format!("{} × {} px", dimensions.width, dimensions.height));
        let mut motion = self.motion.borrow_mut();
        motion.settle();
        motion.select_card(size, 0.0, 0.0);
        for button in &mut motion.buttons {
            button.focus = Transition::default();
            button.hover = Transition::default();
            button.press = Transition::default();
        }
        let from = 1000.0;
        if motion.pulse_since.is_some() {
            motion.pulse_since = Some(from);
        }
        let sample = match scene {
            n if n == TestScene::SwitchSlide as usize => {
                let track = &mut motion.buttons[Screenshots.index()].checked;
                track.retarget(0.0, from, 0.0);
                track.retarget(1.0, from, SWITCH_MS);
                from + SWITCH_MS / 8.0
            }
            n if n == TestScene::CardSlide as usize => {
                motion.select_card(0, from, 0.0);
                motion.select_card(2, from, CARD_MS);
                from + CARD_MS / 8.0
            }
            n if n == TestScene::ButtonHover as usize || n == TestScene::ButtonPress as usize => {
                let button = &mut motion.buttons[ChooseNotes.index()];
                button.hover.retarget(1.0, from, 80.0);
                if n == TestScene::ButtonPress as usize {
                    button.press.retarget(1.0, from, 80.0);
                }
                from + 80.0
            }
            n if n == TestScene::SwitchFocus as usize => {
                motion.buttons[Screenshots.index()].focus.retarget(1.0, from, FOCUS_MS);
                from + FOCUS_MS
            }
            n if n == TestScene::CardFocus as usize => {
                motion.select_card(0, from, 0.0);
                motion.buttons[Small.index()].focus.retarget(1.0, from, FOCUS_MS);
                from + FOCUS_MS
            }
            n if n == TestScene::PageSlide as usize => {
                let to = if self.page.get() == Page::Shortcuts { 1.0 } else { 0.0 };
                motion.page.retarget(1.0 - to, from, 0.0);
                motion.page.retarget(to, from, PAGE_MS);
                motion.content.retarget(0.0, from, 0.0);
                motion.content.retarget(1.0, from, PAGE_MS);
                from + PAGE_MS / 8.0
            }
            _ => from,
        };
        drop(motion);
        self.frozen.set(Some(sample));
        self.update_animation();
        // SAFETY: test-only flush of our own client before PrintWindow samples the frame.
        let _ =
            unsafe { RedrawWindow(Some(self.hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW) };
    }
}
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

trait RunValue {
    fn exists(&self) -> io::Result<bool>;
    fn write(&self, command: Option<&[u16]>) -> io::Result<()>;
}

struct Registry(Vec<u16>);

impl RunValue for Registry {
    fn exists(&self) -> io::Result<bool> {
        let mut size = 0;
        // SAFETY: static key, terminated value name, and writable size.
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                PCWSTR(self.0.as_ptr()),
                RRF_RT_ANY,
                None,
                None,
                Some(&mut size),
            )
        };
        match result {
            ERROR_FILE_NOT_FOUND => Ok(false),
            status => status.ok().map(|()| true).map_err(io::Error::from),
        }
    }

    fn write(&self, command: Option<&[u16]>) -> io::Result<()> {
        let mut key = HKEY::default();
        // SAFETY: static subkey and writable result. Close the obtained handle below.
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key) }.ok()?;
        let bytes =
            command.map(|text| text.iter().flat_map(|unit| unit.to_le_bytes()).collect::<Vec<_>>());
        let name = PCWSTR(self.0.as_ptr());
        // SAFETY: the key, terminated name, and optional byte buffer are live.
        let result = unsafe {
            match bytes.as_deref() {
                Some(bytes) => RegSetValueExW(key, name, None, REG_SZ, Some(bytes)),
                None => RegDeleteValueW(key, name),
            }
        };
        // SAFETY: closes only the handle obtained above.
        let _ = unsafe { RegCloseKey(key) };
        if command.is_none() && result == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        result.ok().map_err(io::Error::from)
    }
}

fn set_launch(run: &impl RunValue, exe: &Path, enabled: bool) -> io::Result<bool> {
    let mut quoted = OsString::from("\"");
    quoted.push(exe);
    quoted.push("\"");
    let command = enabled.then(|| wide(quoted));
    run.write(command.as_deref())?;
    run.exists()
}

fn system_dark() -> bool {
    let mut light: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: the registry value is restricted to a DWORD and both out buffers are valid.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut light as *mut u32).cast()),
            Some(&mut size),
        )
    };
    result.is_ok() && light == 0
}

#[derive(Clone, Copy)]
struct Palette {
    paper: COLORREF,
    patch: COLORREF,
    chip: COLORREF,
    text: COLORREF,
    muted: COLORREF,
    outline: COLORREF,
    accent: COLORREF,
    selected: COLORREF,
    on: COLORREF,
    off: COLORREF,
    thumb: COLORREF,
    disabled: COLORREF,
    error: COLORREF,
}

const fn rgb(hex: u32) -> COLORREF {
    COLORREF(((hex & 0xff) << 16) | (hex & 0xff00) | ((hex >> 16) & 0xff))
}

const fn token([r, g, b, _]: theme::Rgba) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

const LIGHT: Palette = Palette {
    paper: rgb(0xffffff),
    patch: rgb(0xf4f7fb),
    chip: rgb(0xe9eef6),
    text: token(theme::TEXT),
    muted: token(theme::MUTED),
    outline: rgb(0x768398),
    accent: rgb(0x09265b),
    selected: rgb(0xdce9fc),
    on: rgb(0x5976a6),
    off: rgb(0x768398),
    thumb: rgb(0xffffff),
    disabled: rgb(0xb2bac7),
    error: rgb(0x9a2926),
};
const DARK: Palette = Palette {
    paper: rgb(0x1a1d25),
    patch: rgb(0x222733),
    chip: rgb(0x313948),
    text: rgb(0xeaf0fc),
    muted: rgb(0xaab7cc),
    outline: rgb(0x8090a8),
    accent: rgb(0xd9e8ff),
    selected: rgb(0x344a6b),
    on: rgb(0x4775aa),
    off: rgb(0x536781),
    thumb: rgb(0xf1f5ff),
    disabled: rgb(0x525d70),
    error: rgb(0xffaaa4),
};

/// The recording actions in the order of their chips.
const RECORDING_ACTIONS: [Action; 3] = [Action::ToggleRecording, Action::Pen, Action::Highlighter];

/// The Screen recording row's chips: the user's chords for recording, pen and highlighter.
fn recording_chords(shortcuts: &Shortcuts) -> Vec<Chord> {
    RECORDING_ACTIONS.iter().filter_map(|&action| shortcuts.get(action)).collect()
}

/// The chips' accessible name, read by screen readers instead of the drawing.
fn recording_keys_name(shortcuts: &Shortcuts) -> String {
    let parts: Vec<String> = RECORDING_ACTIONS
        .iter()
        .filter_map(|&action| {
            let chord = shortcuts.get(action)?;
            Some(format!("{} {}", action.label(), display_for_layout(&chord)))
        })
        .collect();
    if parts.is_empty() { "No recording shortcuts".to_owned() } else { parts.join(", ") }
}

fn key_chips(shortcut: &str) -> impl Iterator<Item = &str> {
    shortcut.split('+').map(str::trim).filter(|key| !key.is_empty())
}

fn display_path(path: &Path) -> Cow<'_, str> {
    let full = path.to_string_lossy();
    if let Some(unc) = full.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}").into()
    } else if let Some(drive) = full.strip_prefix(r"\\?\")
        && drive.as_bytes().get(1) == Some(&b':')
    {
        drive.to_owned().into()
    } else {
        full
    }
}

fn shorten(path: &str, fits: impl Fn(&str) -> bool) -> String {
    if fits(path) {
        return path.to_owned();
    }
    let chars: Vec<_> = path.chars().collect();
    let mut best = String::new();
    let (mut first, mut limit) = (0, chars.len());
    while first < limit {
        let count = first + (limit - first) / 2;
        let left = count.div_ceil(2);
        let right = count / 2;
        let label: String =
            chars[..left].iter().chain(&['…']).chain(&chars[chars.len() - right..]).collect();
        if fits(&label) {
            best = label;
            first = count + 1;
        } else {
            limit = count;
        }
    }
    best
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Control {
    Capabilities = 100,
    ScreenshotsTitle,
    ScreenshotKeys,
    ScreenshotDetail,
    Screenshots,
    KeepTitle,
    Keep,
    NotesTitle,
    NotesKeys,
    NotesDetail,
    Notes,
    RecordingTitle,
    RecordingKeys,
    RecordingDetail,
    Recording,
    General,
    NotesFolder,
    ChooseNotes,
    OpenNotes,
    ResetNotes,
    NotesPath,
    NotesFallback,
    CapturesFolder,
    ChooseCaptures,
    OpenCaptures,
    ResetCaptures,
    CapturesPath,
    CapturesFallback,
    CardTitle,
    Small,
    Medium,
    Large,
    Preview,
    CardDimensions,
    LaunchTitle,
    Launch,
    Status,
    Retry,
    WindowsShortcutNote,
    GeneralTab,
    ShortcutsTab,
    RestoreLastLabel,
    RestoreLastRecorder,
    RestoreLastReset,
    RestoreLastStatus,
    DiscardTopLabel,
    DiscardTopRecorder,
    DiscardTopReset,
    DiscardTopStatus,
    DiscardAllLabel,
    DiscardAllRecorder,
    DiscardAllReset,
    DiscardAllStatus,
    OpenSettingsLabel,
    OpenSettingsRecorder,
    OpenSettingsReset,
    OpenSettingsStatus,
    NewNoteLabel,
    NewNoteRecorder,
    NewNoteReset,
    NewNoteStatus,
    ToggleRecordingLabel,
    ToggleRecordingRecorder,
    ToggleRecordingReset,
    ToggleRecordingStatus,
    PenLabel,
    PenRecorder,
    PenReset,
    PenStatus,
    HighlighterLabel,
    HighlighterRecorder,
    HighlighterReset,
    HighlighterStatus,
}

use Control::*;

#[derive(Clone, Copy)]
enum Kind {
    Title,
    Heading,
    Detail,
    Path,
    Keys,
    Switch,
    Push(&'static str),
    Radio,
    Preview,
    Status,
    Recorder,
}

#[derive(Clone, Copy)]
struct Item {
    id: Control,
    kind: Kind,
    text: &'static str,
    rect: [i32; 4],
}

const fn item(id: Control, kind: Kind, text: &'static str, rect: [i32; 4]) -> Item {
    Item { id, kind, text, rect }
}

const WIDTH: i32 = 544;
/// One client height for both pages. It holds the General page with both folder notes and
/// the status row showing, and fits the work area of 1366x768 at 100% and 1920x1080 at 150%.
const HEIGHT: i32 = 630;
const STATUS_HEIGHT: i32 = 34;
const PAGE_SWITCH: [i32; 4] = [24, 12, 214, 32];
/// Everything below the page switch, which moves when the page changes.
const CONTENT: [i32; 4] = [0, 48, WIDTH, HEIGHT - 48];
const KEEP_PATCH: [i32; 4] = [24, 150, 496, 36];
const CARD_PATCH: [i32; 4] = [24, 437, 496, 72];

const ITEMS: &[Item] = &[
    item(GeneralTab, Kind::Radio, "General", [24, 12, 107, 32]),
    item(ShortcutsTab, Kind::Radio, "Shortcuts", [131, 12, 107, 32]),
    item(Capabilities, Kind::Heading, "Capabilities", [24, 54, 496, 22]),
    item(ScreenshotsTitle, Kind::Title, "Screenshots", [24, 83, 92, 22]),
    item(Screenshots, Kind::Switch, "Screenshots", [470, 81, 50, 28]),
    item(ScreenshotKeys, Kind::Keys, "Win+Shift+S|or|PrtScn", [122, 84, 342, 20]),
    item(
        ScreenshotDetail,
        Kind::Detail,
        "Use Win+Shift+S or PrtScn and your cursor carries the shot.",
        [24, 110, 496, 17],
    ),
    item(
        WindowsShortcutNote,
        Kind::Detail,
        "Taking a screenshot uses the Windows shortcuts. Change them in Windows settings.",
        [24, 128, 496, 17],
    ),
    item(KeepTitle, Kind::Title, "Keep every screenshot", [36, 156, 412, 24]),
    item(Keep, Kind::Switch, "Keep every screenshot", [458, 154, 50, 28]),
    item(NotesTitle, Kind::Title, "Notes", [24, 195, 45, 22]),
    item(NotesKeys, Kind::Keys, "Hold|Alt|and click on empty space", [75, 196, 367, 20]),
    item(Notes, Kind::Switch, "Notes", [470, 192, 50, 28]),
    item(
        NotesDetail,
        Kind::Detail,
        "A sticky note opens where you click, saved as Markdown.",
        [24, 219, 400, 18],
    ),
    item(RecordingTitle, Kind::Title, "Screen recording", [24, 248, 119, 22]),
    // The chips are the user's chords (`recording_chords`), drawn from the settings.
    item(RecordingKeys, Kind::Keys, "", [149, 249, 315, 20]),
    item(Recording, Kind::Switch, "Screen recording", [470, 245, 50, 28]),
    item(
        RecordingDetail,
        Kind::Detail,
        "Records the screen under the cursor as MP4. Alt + wheel zooms.",
        [24, 272, 440, 18],
    ),
    item(General, Kind::Heading, "General", [24, 302, 496, 22]),
    item(NotesFolder, Kind::Title, "Notes folder", [24, 329, 496, 20]),
    item(ChooseNotes, Kind::Push("&Choose…"), "&Choose notes folder…", [24, 352, 77, 24]),
    item(OpenNotes, Kind::Push("Open"), "Open notes folder", [107, 352, 55, 24]),
    item(ResetNotes, Kind::Push("Reset"), "Reset notes folder", [168, 352, 58, 24]),
    item(NotesPath, Kind::Path, "", [236, 352, 284, 24]),
    item(NotesFallback, Kind::Detail, "", [24, 377, 496, 17]),
    item(CapturesFolder, Kind::Title, "Captures folder", [24, 382, 496, 20]),
    item(ChooseCaptures, Kind::Push("Ch&oose…"), "Ch&oose captures folder…", [24, 405, 77, 24]),
    item(OpenCaptures, Kind::Push("Open"), "Open captures folder", [107, 405, 55, 24]),
    item(ResetCaptures, Kind::Push("Reset"), "Reset captures folder", [168, 405, 58, 24]),
    item(CapturesPath, Kind::Path, "", [236, 405, 284, 24]),
    item(CapturesFallback, Kind::Detail, "", [24, 430, 496, 17]),
    item(CardTitle, Kind::Title, "Card size", [36, 446, 220, 20]),
    item(Small, Kind::Radio, "Small", [36, 471, 66, 26]),
    item(Medium, Kind::Radio, "Medium", [108, 471, 75, 26]),
    item(Large, Kind::Radio, "Large", [189, 471, 65, 26]),
    item(Preview, Kind::Preview, "Card size preview", [412, 443, 96, 60]),
    item(CardDimensions, Kind::Detail, "", [264, 474, 108, 20]),
    item(LaunchTitle, Kind::Title, "Launch at login", [24, 521, 436, 24]),
    item(Launch, Kind::Switch, "Launch at login", [470, 519, 50, 28]),
    item(Status, Kind::Status, "", [24, 559, 414, STATUS_HEIGHT]),
    item(Retry, Kind::Push("&Retry"), "&Retry saving settings", [450, 561, 70, 29]),
];

/// The parts of one shortcut row, in the order of their control ids.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutPart {
    Label,
    Recorder,
    Reset,
    Status,
}

const PARTS: usize = 4;
const ROWS: usize = Action::ALL.len();
const ROW_TOP: i32 = 60;
const ROW_PITCH: i32 = 64;

const fn row_y(index: usize) -> i32 {
    ROW_TOP + index as i32 * ROW_PITCH
}

/// A whole row, with room for the recorder's focus ring.
const fn row_rect(index: usize) -> [i32; 4] {
    [0, row_y(index) - 4, WIDTH, ROW_PITCH]
}

const SHORTCUT_IDS: [Control; ROWS * PARTS] = [
    RestoreLastLabel, RestoreLastRecorder, RestoreLastReset, RestoreLastStatus,
    DiscardTopLabel, DiscardTopRecorder, DiscardTopReset, DiscardTopStatus,
    DiscardAllLabel, DiscardAllRecorder, DiscardAllReset, DiscardAllStatus,
    OpenSettingsLabel, OpenSettingsRecorder, OpenSettingsReset, OpenSettingsStatus,
    NewNoteLabel, NewNoteRecorder, NewNoteReset, NewNoteStatus,
    ToggleRecordingLabel, ToggleRecordingRecorder, ToggleRecordingReset, ToggleRecordingStatus,
    PenLabel, PenRecorder, PenReset, PenStatus,
    HighlighterLabel, HighlighterRecorder, HighlighterReset, HighlighterStatus,
];

const SHORTCUT_ITEMS: [Item; ROWS * PARTS] = {
    let mut list = [item(RestoreLastLabel, Kind::Title, "", [0; 4]); ROWS * PARTS];
    let mut index = 0;
    while index < ROWS * PARTS {
        let y = row_y(index / PARTS);
        let id = SHORTCUT_IDS[index];
        list[index] = match index % PARTS {
            0 => item(id, Kind::Title, "", [24, y + 4, 200, 22]),
            1 => item(id, Kind::Recorder, "", [234, y - 2, 224, 34]),
            2 => item(id, Kind::Push("Reset"), "", [464, y, 56, 30]),
            _ => item(id, Kind::Status, "", [24, y + 34, 496, 18]),
        };
        index += 1;
    }
    list
};

fn items() -> impl Iterator<Item = &'static Item> {
    ITEMS.iter().chain(&SHORTCUT_ITEMS)
}

impl Control {
    /// The control of one part of an action's row.
    #[doc(hidden)]
    pub fn shortcut(action: Action, part: ShortcutPart) -> Self {
        SHORTCUT_ITEMS[action as usize * PARTS + part as usize].id
    }

    fn shortcut_row(self) -> Option<(Action, ShortcutPart)> {
        let index = (self as usize).checked_sub(RestoreLastLabel as usize)?;
        let action = *Action::ALL.get(index / PARTS)?;
        let part = [ShortcutPart::Label, ShortcutPart::Recorder, ShortcutPart::Reset, ShortcutPart::Status]
            [index % PARTS];
        Some((action, part))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Shortcuts,
}

impl Page {
    fn contains(self, id: Control) -> bool {
        matches!(id, GeneralTab | ShortcutsTab)
            || (id.shortcut_row().is_some() == (self == Self::Shortcuts))
    }
}

const RECORDING_PROMPT: &str = "Press the keys · Esc cancels · Backspace clears";
/// The note under Pen and Highlighter while no recording runs (`Registration::WhileRecording`).
const WHILE_RECORDING: &str = "Active while recording";

/// The note shown under a row after its shortcut was set, if any. Muted: these are hints,
/// not errors.
fn shortcut_warning(chord: &Chord) -> Option<&'static str> {
    let key = chord.key.vk();
    let letter = (u16::from(b'A')..=u16::from(b'Z')).contains(&key);
    let digit = (u16::from(b'0')..=u16::from(b'9')).contains(&key);
    if chord.win {
        return None;
    }
    match (chord.ctrl, chord.alt, chord.shift) {
        // AltGr is Ctrl+Alt: on many layouts it, with or without Shift, types a character.
        (true, true, _) if letter || digit => {
            Some("On some keyboard layouts this combination types a character.")
        }
        (false, true, false) if letter => Some("This may override menu shortcuts in other apps."),
        (true, false, false) if b"CXZYASFPNOWT".iter().any(|&byte| u16::from(byte) == key) => {
            Some("Other apps will no longer receive this shortcut.")
        }
        _ => None,
    }
}

/// Key chips for the row: the modifiers, then the key as the active layout labels it.
fn chord_chips(chord: &Chord) -> Vec<String> {
    let mut chips = chord.chips();
    let modifiers = chips[..chips.len() - 1].iter().map(|chip| chip.len() + 1).sum::<usize>();
    if let Some(key) = chips.last_mut() {
        *key = display_for_layout(chord)[modifiers..].to_owned();
    }
    chips
}

fn modifier_chips(held: Held) -> Vec<String> {
    [(held.ctrl, "Ctrl"), (held.alt, "Alt"), (held.shift, "Shift"), (held.win, "Win")]
        .into_iter()
        .filter(|(down, _)| *down)
        .map(|(_, key)| key.to_owned())
        .collect()
}

fn taken_message(chord: &Chord) -> String {
    format!("Another program already uses {}", display_for_layout(chord))
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tone {
    #[default]
    Muted,
    Error,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Note {
    text: String,
    tone: Tone,
}

impl Note {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self { text: text.into(), tone }
    }
}

/// A recorder waiting for keys. The hotkey service is paused.
struct Capture {
    action: Action,
    held: Held,
    /// Why the last chord was refused. Recording goes on.
    error: Option<String>,
}

/// A chord the UI loop is registering. Saved only once its tagged report is not `Taken`.
struct Proposal {
    action: Action,
    tag: usize,
    value: Shortcuts,
}

/// What a row draws: kept apart from the model so fading text can outlive its cause.
#[derive(Clone, Default)]
struct RowView {
    chips: Vec<String>,
    /// How many leading chips were already shown; the others animate in.
    kept: usize,
    note: Note,
}

#[derive(Default)]
struct Recorder {
    capture: Option<Capture>,
    proposal: Option<Proposal>,
    /// The note under each row after a set: a refusal by Windows, a failure or a hint.
    notes: [Option<Note>; ROWS],
    /// The latest untagged registration status.
    registration: Vec<(Action, Registration)>,
    rows: [RowView; ROWS],
    /// The key that ended a recording. Its auto-repeat and release are eaten, so it does
    /// not click the recorder again or reach the dialog keys (Esc closes the window).
    swallow: Option<u16>,
}

const CHIPS: usize = 0;
const RESET: usize = 1;
const NOTE: usize = 2;

const TIMER: usize = 1;
const FOCUS_MS: f64 = 80.0;
const SWITCH_MS: f64 = 150.0;
const CARD_MS: f64 = 180.0;
const ROW_MS: f64 = 150.0;
const OPEN_MS: f64 = 120.0;
const THEME_MS: f64 = 200.0;
const PAGE_MS: f64 = 180.0;
const PULSE_MS: f64 = 1200.0;

fn ease(progress: f64) -> f64 {
    // For cubic-bezier(0, 0, 0, 1), x = t³ and y = 3t² - 2t³.
    let t = progress.clamp(0.0, 1.0).cbrt();
    t * t * (3.0 - 2.0 * t)
}

#[derive(Clone, Copy, Default)]
struct Transition {
    from: f64,
    to: f64,
    start: f64,
    duration: f64,
}

impl Transition {
    fn value(self, now: f64) -> f64 {
        if self.active(now) {
            let progress = ease((now - self.start) / self.duration);
            self.from + (self.to - self.from) * progress
        } else {
            self.to
        }
    }

    fn active(self, now: f64) -> bool {
        self.from != self.to && self.duration > 0.0 && now < self.start + self.duration
    }

    fn retarget(&mut self, to: f64, now: f64, duration: f64) {
        if duration == 0.0 {
            *self = Self { from: to, to, ..Self::default() };
        } else if self.to != to {
            *self = Self { from: self.value(now), to, start: now, duration };
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ButtonMotion {
    checked: Transition,
    hover: Transition,
    press: Transition,
    focus: Transition,
}

impl ButtonMotion {
    fn tracks(&self) -> [Transition; 4] {
        [self.checked, self.hover, self.press, self.focus]
    }
}

const CONTROLS: usize = HighlighterStatus as usize - Capabilities as usize + 1;

impl Control {
    fn index(self) -> usize {
        self as usize - Capabilities as usize
    }
}

struct Motion {
    enabled: bool,
    intro: Transition,
    theme: Transition,
    card: Transition,
    selection: [Transition; 2],
    folders: [Transition; 2],
    status: Transition,
    retry: Transition,
    buttons: [ButtonMotion; CONTROLS],
    /// The page switch highlight: 0 on General, 1 on Shortcuts.
    page: Transition,
    /// The shown page's content fading and sliding in.
    content: Transition,
    /// Per shortcut row: the new chips, the Reset button and the note under the row.
    rows: [[Transition; 3]; ROWS],
    /// The recorder's outline pulses from this time while it waits for keys.
    pulse_since: Option<f64>,
}

impl Motion {
    fn new(enabled: bool) -> Self {
        let shown = Transition { from: 1.0, to: 1.0, ..Transition::default() };
        Self {
            enabled,
            intro: Transition::default(),
            theme: Transition::default(),
            card: Transition::default(),
            selection: [Transition::default(); 2],
            folders: [Transition::default(); 2],
            status: Transition::default(),
            retry: Transition::default(),
            buttons: [ButtonMotion::default(); CONTROLS],
            page: Transition::default(),
            content: shown,
            // Chips start shown; they fade in only when they change.
            rows: [[shown, Transition::default(), Transition::default()]; ROWS],
            pulse_since: None,
        }
    }

    fn select_card(&mut self, index: i32, now: f64, duration: f64) {
        let item = Item::lookup(Small as i32 + index).expect("size choice");
        self.card.retarget(f64::from(index), now, duration);
        self.selection[0].retarget(f64::from(item.rect[0]), now, duration);
        self.selection[1].retarget(f64::from(item.rect[0] + item.rect[2]), now, duration);
    }

    fn duration(&self, duration: f64) -> f64 {
        if self.enabled { duration } else { 0.0 }
    }

    fn active(&self, now: f64) -> bool {
        self.enabled && (self.pulse_since.is_some() || self.tracks().any(|track| track.active(now)))
    }

    fn tracks(&self) -> impl Iterator<Item = Transition> + '_ {
        [
            self.intro,
            self.theme,
            self.card,
            self.folders[0],
            self.folders[1],
            self.status,
            self.retry,
            self.page,
            self.content,
        ]
        .into_iter()
        .chain(self.selection)
        .chain(self.rows.iter().flatten().copied())
        .chain(self.buttons.iter().flat_map(ButtonMotion::tracks))
    }

    fn settle(&mut self) {
        let snap = |track: &mut Transition| track.retarget(track.to, 0.0, 0.0);
        for track in [
            &mut self.intro,
            &mut self.theme,
            &mut self.card,
            &mut self.folders[0],
            &mut self.status,
            &mut self.retry,
            &mut self.page,
            &mut self.content,
        ] {
            snap(track);
        }
        snap(&mut self.folders[1]);
        self.selection.iter_mut().for_each(snap);
        self.rows.iter_mut().flatten().for_each(snap);
        for button in &mut self.buttons {
            for track in
                [&mut button.checked, &mut button.hover, &mut button.press, &mut button.focus]
            {
                snap(track);
            }
        }
    }
}

fn animation_effects() -> bool {
    let mut enabled = windows::core::BOOL(0);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes exactly one BOOL.
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut windows::core::BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok()
        && enabled.as_bool()
}

/// How far the General page's lower rows are pushed down by the folder notes (0 to 15).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Layout {
    notes: i32,
    captures: i32,
}

impl Layout {
    fn rect(self, item: &Item, dpi: u32) -> RECT {
        let mut rect = item.rect;
        rect[1] += self.offset(item.id);
        scaled(rect, dpi)
    }

    fn offset(self, id: Control) -> i32 {
        let id = id as i32;
        let below = |first: Control| id >= first as i32 && id <= Retry as i32;
        let notes = if below(CapturesFolder) { self.notes } else { 0 };
        notes + if below(CardTitle) { self.captures } else { 0 }
    }
}

fn mix(a: COLORREF, b: COLORREF, amount: f64) -> COLORREF {
    COLORREF([0, 8, 16].into_iter().fold(0, |value, shift| {
        let from = f64::from((a.0 >> shift) & 255);
        let to = f64::from((b.0 >> shift) & 255);
        value | (((from + (to - from) * amount).round() as u32) << shift)
    }))
}

impl Palette {
    fn at(amount: f64) -> Self {
        let color = |a, b| mix(a, b, amount);
        Self {
            paper: color(LIGHT.paper, DARK.paper),
            patch: color(LIGHT.patch, DARK.patch),
            chip: color(LIGHT.chip, DARK.chip),
            text: color(LIGHT.text, DARK.text),
            muted: color(LIGHT.muted, DARK.muted),
            outline: color(LIGHT.outline, DARK.outline),
            accent: color(LIGHT.accent, DARK.accent),
            selected: color(LIGHT.selected, DARK.selected),
            on: color(LIGHT.on, DARK.on),
            off: color(LIGHT.off, DARK.off),
            thumb: color(LIGHT.thumb, DARK.thumb),
            disabled: color(LIGHT.disabled, DARK.disabled),
            error: color(LIGHT.error, DARK.error),
        }
    }
}

fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

#[doc(hidden)]
pub fn test_client_size(dpi: u32) -> (i32, i32) {
    (scale(WIDTH, dpi), scale(HEIGHT, dpi))
}

/// The outer size of the settings window at `dpi`, frame and caption included.
#[doc(hidden)]
pub fn test_window_size(dpi: u32) -> (i32, i32) {
    let mut bounds = scaled([0, 0, WIDTH, HEIGHT], dpi);
    // SAFETY: a writable RECT; the style values are constants.
    let _ = unsafe { AdjustWindowRectExForDpi(&mut bounds, STYLE, false, EX_STYLE, dpi) };
    (bounds.right - bounds.left, bounds.bottom - bounds.top)
}

fn scaled([x, y, w, h]: [i32; 4], dpi: u32) -> RECT {
    RECT {
        left: scale(x, dpi),
        top: scale(y, dpi),
        right: scale(x + w, dpi),
        bottom: scale(y + h, dpi),
    }
}

impl Item {
    fn button(&self) -> bool {
        matches!(self.kind, Kind::Switch | Kind::Push(_) | Kind::Radio | Kind::Recorder)
    }

    /// Hover and press feedback: everything but the radio segments.
    fn pointer_effects(&self) -> bool {
        matches!(self.kind, Kind::Switch | Kind::Push(_) | Kind::Recorder)
    }

    fn caption(&self) -> &'static str {
        if let Some((action, ShortcutPart::Label)) = self.id.shortcut_row() {
            return action.label();
        }
        match self.id {
            ScreenshotKeys => "Windows plus Shift plus S, or Print Screen",
            NotesKeys => "Hold Alt and click on empty space",
            // Replaced by `recording_keys_name` at the first refresh.
            RecordingKeys => "Recording shortcuts",
            _ => self.text,
        }
    }

    fn style(&self) -> WINDOW_STYLE {
        let style = match self.kind {
            Kind::Switch => BS_CHECKBOX as u32 | WS_TABSTOP.0,
            Kind::Radio if matches!(self.id, Small | GeneralTab) => {
                BS_AUTORADIOBUTTON as u32 | WS_TABSTOP.0 | WS_GROUP.0
            }
            Kind::Radio => BS_AUTORADIOBUTTON as u32,
            Kind::Push(_) | Kind::Recorder => BS_OWNERDRAW as u32 | WS_TABSTOP.0 | WS_GROUP.0,
            Kind::Path => SS_OWNERDRAW.0 | SS_NOTIFY.0 | WS_GROUP.0,
            _ => SS_OWNERDRAW.0 | WS_GROUP.0,
        };
        let visible = if matches!(self.id, Status | Retry) || self.id.shortcut_row().is_some() {
            0
        } else {
            WS_VISIBLE.0
        };
        WINDOW_STYLE(style | WS_CHILD.0 | visible)
    }

    fn lookup(id: i32) -> Option<&'static Self> {
        items().find(|item| item.id as i32 == id)
    }
}

#[derive(Clone, Copy)]
enum Folder {
    Notes = 0,
    Captures = 1,
}

impl Folder {
    fn chosen(self, settings: &mut Settings) -> &mut Option<PathBuf> {
        match self {
            Self::Notes => &mut settings.notes_folder,
            Self::Captures => &mut settings.captures_folder,
        }
    }

    fn resolve(self, settings: &Settings, defaults: &DefaultFolders) -> (PathBuf, bool) {
        match self {
            Self::Notes => settings.notes_dir(defaults),
            Self::Captures => settings.captures_dir(defaults),
        }
    }
}

struct Model {
    settings: Settings,
    folders: [(PathBuf, bool); 2],
    load_error: Option<String>,
    save_error: Option<String>,
    action_error: Option<String>,
    launch: io::Result<bool>,
}

impl Model {
    fn load(path: &Path, defaults: &DefaultFolders, registry: &Registry) -> Self {
        let (settings, load_error) = match Settings::load(path) {
            Ok(value) => (value, None),
            Err(error) => {
                (Settings::default(), Some(format!("Settings could not be loaded. {error}")))
            }
        };
        let folders = if load_error.is_some() {
            [(defaults.notes.clone(), false), (defaults.captures.clone(), false)]
        } else {
            [
                Folder::Notes.resolve(&settings, defaults),
                Folder::Captures.resolve(&settings, defaults),
            ]
        };
        Self {
            settings,
            folders,
            load_error,
            save_error: None,
            action_error: None,
            launch: registry.exists(),
        }
    }

    fn status(&self) -> String {
        if let Some(error) =
            self.load_error.as_ref().or(self.save_error.as_ref()).or(self.action_error.as_ref())
        {
            error.clone()
        } else if let Err(error) = &self.launch {
            format!("Could not read Launch at login. {error}")
        } else {
            String::new()
        }
    }
}

struct Fonts([HFONT; 3]);

impl Fonts {
    fn new(dpi: u32) -> Self {
        Self([(14, 600), (13, 400), (12, 600)].map(|(size, weight)| {
            // SAFETY: font family is a static string; CreateFontW returns an owned font.
            unsafe {
                CreateFontW(
                    -scale(size, dpi),
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    DEFAULT_PITCH.0.into(),
                    w!("Segoe UI Variable Text"),
                )
            }
        }))
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for font in self.0 {
            // SAFETY: our fonts are only selected into synchronous paint DCs and restored there.
            let _ = unsafe { DeleteObject(font.into()) };
        }
    }
}

struct Window<'a> {
    hwnd: HWND,
    lifetime: &'a Lifetime,
    options: &'a Options,
    defaults: DefaultFolders,
    registry: Registry,
    model: RefCell<Model>,
    dialog: RefCell<Option<IFileOpenDialog>>,
    picking: Cell<bool>,
    close_pending: Cell<bool>,
    dpi: Cell<u32>,
    epoch: Instant,
    motion: RefCell<Motion>,
    frame: Cell<Frame>,
    hover: Cell<Option<Control>>,
    timer: Cell<bool>,
    started: Cell<bool>,
    frozen: Cell<Option<f64>>,
    surface: RefCell<Option<Surface>>,
    error_label: RefCell<String>,
    page: Cell<Page>,
    recorder: RefCell<Recorder>,
    fonts: RefCell<Fonts>,
    tooltip: Cell<HWND>,
    tips: RefCell<[Vec<u16>; 2]>,
    icons: [HICON; 2],
}

#[derive(Clone, Copy, Default, PartialEq)]
struct ButtonFrame {
    checked: f64,
    hover: f64,
    press: f64,
    focus: f64,
}

#[derive(Clone, Copy, PartialEq)]
struct Frame {
    intro: f64,
    theme: f64,
    card: f64,
    selection: [f64; 2],
    folders: [f64; 2],
    status: f64,
    retry: f64,
    buttons: [ButtonFrame; CONTROLS],
    page: f64,
    content: f64,
    rows: [[f64; 3]; ROWS],
    /// The recorder outline, 1 at its strongest. Steady at 1 with reduced motion.
    pulse: f64,
}

impl Frame {
    fn layout(self) -> Layout {
        Layout {
            notes: (self.folders[0] * 15.0).round() as i32,
            captures: (self.folders[1] * 15.0).round() as i32,
        }
    }
}

impl Motion {
    fn frame(&self, now: f64) -> Frame {
        Frame {
            intro: self.intro.value(now),
            theme: self.theme.value(now),
            card: self.card.value(now),
            selection: self.selection.map(|track| track.value(now)),
            folders: self.folders.map(|track| track.value(now)),
            status: self.status.value(now),
            retry: self.retry.value(now),
            buttons: self.buttons.map(|button| ButtonFrame {
                checked: button.checked.value(now),
                hover: button.hover.value(now),
                press: button.press.value(now),
                focus: button.focus.value(now),
            }),
            page: self.page.value(now),
            content: self.content.value(now),
            rows: self.rows.map(|row| row.map(|track| track.value(now))),
            pulse: match self.pulse_since {
                Some(start) if self.enabled => {
                    0.5 + 0.5 * ((now - start) * std::f64::consts::TAU / PULSE_MS).cos()
                }
                // Reduced motion: a steady outline at full strength.
                Some(_) => 1.0,
                None => 0.0,
            },
        }
    }
}

impl Window<'_> {
    fn now(&self) -> f64 {
        self.frozen.get().unwrap_or_else(|| self.epoch.elapsed().as_secs_f64() * 1000.0)
    }

    fn invalidate(&self, rect: Option<RECT>) {
        // SAFETY: this STA owns the parent HWND, and Win32 copies the optional RECT.
        let _ = unsafe {
            InvalidateRect(Some(self.hwnd), rect.as_ref().map(std::ptr::from_ref), false)
        };
    }

    fn invalidate_item(&self, item: &Item) {
        self.invalidate(Some(self.frame.get().layout().rect(item, self.dpi.get())));
    }

    fn update_animation(&self) {
        let now = self.now();
        let (frame, active) = {
            let motion = self.motion.borrow();
            (motion.frame(now), motion.active(now))
        };
        let previous = self.frame.replace(frame);
        let layout = frame.layout();
        let prior = previous.layout();
        let dpi = self.dpi.get();
        if prior != layout {
            self.position_children(layout, Some(prior));
        }

        if frame.intro != previous.intro || frame.theme != previous.theme {
            self.invalidate(None);
        } else {
            if frame.page != previous.page {
                self.invalidate(Some(scaled(PAGE_SWITCH, dpi)));
            }
            if frame.content != previous.content {
                self.invalidate(Some(scaled(CONTENT, dpi)));
            }
            // Moving a row exposes the space it occupied and shifts the controls below it.
            let first_moved = if frame.folders[0] != previous.folders[0] {
                Some(NotesFallback)
            } else if frame.folders[1] != previous.folders[1] {
                Some(CapturesFallback)
            } else if frame.status != previous.status || frame.retry != previous.retry {
                Some(Status)
            } else {
                None
            };
            if let Some(id) = first_moved {
                let top = Item::lookup(id as i32).expect("moving row").rect[1] - 4
                    + prior.offset(id).min(layout.offset(id));
                self.invalidate(Some(scaled([0, top, WIDTH, HEIGHT - top], dpi)));
            }
            if frame.card != previous.card || frame.selection != previous.selection {
                let mut rect = CARD_PATCH;
                rect[1] += layout.offset(CardTitle);
                self.invalidate(Some(scaled(rect, dpi)));
            }
            if self.page.get() == Page::Shortcuts {
                let recording = self.recorder.borrow().capture.as_ref().map(|c| c.action as usize);
                for index in 0..ROWS {
                    if frame.rows[index] != previous.rows[index]
                        || (frame.pulse != previous.pulse && recording == Some(index))
                    {
                        self.invalidate(Some(scaled(row_rect(index), dpi)));
                    }
                }
            }
            for item in items().filter(|item| item.button() && self.page.get().contains(item.id)) {
                let index = item.id.index();
                if previous.buttons[index] != frame.buttons[index] {
                    self.invalidate_item(item);
                }
            }
        }

        let should_run = active && self.started.get() && self.frozen.get().is_none();
        if should_run != self.timer.get() {
            // SAFETY: this window owns the single timer and dispatches it on this STA.
            let running = unsafe {
                if should_run {
                    SetTimer(Some(self.hwnd), TIMER, 10, None) != 0
                } else {
                    let _ = KillTimer(Some(self.hwnd), TIMER);
                    false
                }
            };
            self.timer.set(running);
        }
    }

    fn position_children(&self, layout: Layout, prior: Option<Layout>) {
        let dpi = self.dpi.get();
        for item in items() {
            let rect = layout.rect(item, dpi);
            if prior.is_some_and(|old| old.rect(item, dpi) == rect) {
                continue;
            }
            // SAFETY: same-thread children, with parent responsible for the composed repaint.
            let _ = unsafe {
                SetWindowPos(
                    self.child(item.id),
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
                )
            };
        }
    }

    fn ui_state(&self) -> u32 {
        // SAFETY: scalar query to this thread's top-level window.
        unsafe { SendMessageW(self.hwnd, WM_QUERYUISTATE, None, None) }.0 as u32
    }

    fn change_ui_state(&self, action: u32, flags: u32) {
        let current = self.ui_state();
        if (action == UIS_SET && current & flags != flags)
            || (action == UIS_CLEAR && current & flags != 0)
        {
            // SAFETY: normal dialog UI cue propagation, with scalar flags.
            unsafe {
                SendMessageW(
                    self.hwnd,
                    WM_CHANGEUISTATE,
                    Some(WPARAM((action | flags << 16) as usize)),
                    None,
                )
            };
        }
    }

    fn button_state(&self, item: &Item, child: HWND) {
        // SAFETY: all queries are scalar and this thread owns the native button.
        let (enabled, pressed, focused) = unsafe {
            (
                IsWindowEnabled(child).as_bool(),
                SendMessageW(child, BM_GETSTATE, None, None).0 as u32 & BST_PUSHED != 0,
                GetFocus() == child,
            )
        };
        let show_focus = self.ui_state() & UISF_HIDEFOCUS == 0;
        let now = self.now();
        let mut motion = self.motion.borrow_mut();
        let enter = motion.duration(80.0);
        let leave = motion.duration(120.0);
        let focus_in = motion.duration(FOCUS_MS);
        let button = &mut motion.buttons[item.id.index()];
        let pointer_effects = item.pointer_effects();
        let hover = pointer_effects && enabled && self.hover.get() == Some(item.id);
        let pressed = pointer_effects && enabled && pressed;
        button.hover.retarget(
            if hover { 1.0 } else { 0.0 },
            now,
            if hover { enter } else { leave },
        );
        button.press.retarget(
            if pressed { 1.0 } else { 0.0 },
            now,
            if pressed { enter } else { leave },
        );
        let focus = enabled && focused && show_focus;
        button.focus.retarget(
            if focus { 1.0 } else { 0.0 },
            now,
            if focus { focus_in } else { 0.0 },
        );
    }

    fn sync_buttons(&self) {
        for item in items().filter(|item| item.button() && self.page.get().contains(item.id)) {
            self.button_state(item, self.child(item.id));
        }
        self.update_animation();
    }

    fn observe_hover(&self, item: &Item, hwnd: HWND, msg: u32, point: LPARAM) {
        let mut bounds = RECT::default();
        let x = i32::from(point.0 as i16);
        let y = i32::from((point.0 >> 16) as i16);
        // SAFETY: the current native button owns its client rectangle. A captured button
        // can also receive WM_MOUSEMOVE with coordinates outside that rectangle.
        let inside = msg == WM_MOUSEMOVE
            && unsafe { GetClientRect(hwnd, &mut bounds) }.is_ok()
            && x >= bounds.left
            && x < bounds.right
            && y >= bounds.top
            && y < bounds.bottom;
        if inside && self.hover.get() != Some(item.id) {
            let old_hover = self.hover.replace(Some(item.id));
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: track is an initialized request for a live native button.
            let _ = unsafe { TrackMouseEvent(&mut track) };
            if let Some(old) = old_hover.and_then(|id| Item::lookup(id as i32)) {
                self.button_state(old, self.child(old.id));
            }
        } else if !inside && self.hover.get() == Some(item.id) {
            self.hover.set(None);
        }
    }
}

impl Window<'_> {
    fn child(&self, id: Control) -> HWND {
        // SAFETY: this is our parent HWND, and a partially initialized child may be absent.
        unsafe { GetDlgItem(Some(self.hwnd), id as i32) }.unwrap_or_default()
    }

    fn set_theme(&self) {
        let dark = self.options.test.as_ref().map_or_else(system_dark, |test| test.dark);
        let now = self.now();
        {
            let mut motion = self.motion.borrow_mut();
            let duration = if self.started.get() { motion.duration(THEME_MS) } else { 0.0 };
            motion.theme.retarget(if dark { 1.0 } else { 0.0 }, now, duration);
        }
        self.update_animation();
        let immersive = i32::from(dark);
        // SAFETY: DWM copies the DWORD immediately; the HWND belongs to this thread.
        let _ = unsafe {
            DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&immersive as *const i32).cast(),
                size_of::<i32>() as u32,
            )
        };
        let tooltip = self.tooltip.get();
        if !tooltip.is_invalid() {
            let palette = if dark { DARK } else { LIGHT };
            // SAFETY: the tooltip is settings-owned; the two empty strings select classic
            // tooltip painting so it honors our theme colors.
            unsafe {
                let _ = SetWindowTheme(tooltip, w!(""), w!(""));
                SendMessageW(
                    tooltip,
                    TTM_SETTIPBKCOLOR,
                    Some(WPARAM(palette.chip.0 as usize)),
                    None,
                );
                SendMessageW(
                    tooltip,
                    TTM_SETTIPTEXTCOLOR,
                    Some(WPARAM(palette.text.0 as usize)),
                    None,
                );
            }
        }
        // SAFETY: both icon handles remain owned by this window until it is destroyed.
        unsafe {
            SendMessageW(
                self.hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(self.icons[usize::from(dark)].0 as isize)),
            );
            SendMessageW(
                self.hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(self.icons[usize::from(dark)].0 as isize)),
            );
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn init_children(&self, instance: HINSTANCE) -> Result<()> {
        for item in items() {
            let caption = match item.id.shortcut_row() {
                Some((action, ShortcutPart::Reset)) => {
                    wide(format!("Reset shortcut for {}", action.label()))
                }
                _ => wide(item.caption()),
            };
            let class = if item.button() { w!("BUTTON") } else { w!("STATIC") };
            let rect = scaled(item.rect, self.dpi.get());
            // SAFETY: captions are copied, IDs are unique, and this thread owns the parent.
            let child = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    PCWSTR(caption.as_ptr()),
                    item.style(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    Some(self.hwnd),
                    Some(HMENU(item.id as usize as *mut c_void)),
                    Some(instance),
                    None,
                )?
            };
            {
                // SAFETY: same-thread subclass of our child. The parent state outlives the child.
                if !unsafe {
                    SetWindowSubclass(child, Some(control_proc), 1, self as *const _ as usize)
                }
                .as_bool()
                {
                    return Err(Error::from_thread());
                }
            }
        }
        // SAFETY: tooltip keeps this HWND as its owner; initialization is on the owner thread.
        let tooltip = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                TOOLTIPS_CLASSW,
                None,
                WINDOW_STYLE(WS_POPUP.0 | TTS_ALWAYSTIP | TTS_NOPREFIX),
                0,
                0,
                0,
                0,
                Some(self.hwnd),
                None,
                Some(instance),
                None,
            )?
        };
        self.tooltip.set(tooltip);
        Ok(())
    }

    fn relayout(&self, dpi: u32) {
        self.dpi.set(dpi);
        let previous_fonts = self.fonts.replace(Fonts::new(dpi));
        self.position_children(self.frame.get().layout(), None);
        for item in items() {
            let child = self.child(item.id);
            let font = self.fonts.borrow().0
                [usize::from(!matches!(item.kind, Kind::Title | Kind::Heading))];
            // SAFETY: this STA owns the child and retains the new font.
            unsafe { SendMessageW(child, WM_SETFONT, Some(WPARAM(font.0 as usize)), None) };
        }
        self.invalidate(None);
        // SAFETY: this is the settings-owned tooltip; both message parameters are scalar.
        unsafe {
            SendMessageW(
                self.tooltip.get(),
                TTM_SETMAXTIPWIDTH,
                None,
                Some(LPARAM(scale(WIDTH, dpi) as isize)),
            )
        };
        drop(previous_fonts);
    }

    fn adjust_size(&self, at: Option<(i32, i32)>) {
        let mut bounds = scaled([0, 0, WIDTH, HEIGHT], self.dpi.get());
        // SAFETY: a writable RECT for the real window style and target DPI.
        let _ = unsafe {
            AdjustWindowRectExForDpi(
                &mut bounds,
                STYLE,
                false,
                EX_STYLE,
                GetDpiForWindow(self.hwnd),
            )
        };
        let (x, y) = at.unwrap_or_default();
        let flags = if at.is_some() {
            SWP_NOZORDER | SWP_NOACTIVATE
        } else {
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE
        };
        // SAFETY: owned HWND, scalar position and size.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                flags,
            )
        };
    }
}

impl Window<'_> {
    fn text(&self, id: Control, text: &str) {
        let text = wide(text);
        // SAFETY: same-thread child; Win32 copies the terminated UTF-16 string.
        let _ = unsafe { SetWindowTextW(self.child(id), PCWSTR(text.as_ptr())) };
    }
}

impl Window<'_> {
    fn set_page(&self, page: Page) {
        if self.page.replace(page) == page {
            return;
        }
        self.cancel_capture();
        // Focus must not stay on a control that is about to hide.
        let tab = self.child(if page == Page::Shortcuts { ShortcutsTab } else { GeneralTab });
        // SAFETY: focus queries and moves stay within this STA's own children.
        unsafe {
            let focused = GetFocus();
            if !focused.is_invalid()
                && focused != tab
                && GetParent(focused).ok() == Some(self.hwnd)
            {
                let _ = SetFocus(Some(tab));
            }
        }
        let now = self.now();
        {
            let mut motion = self.motion.borrow_mut();
            let duration = motion.duration(PAGE_MS);
            motion.page.retarget(if page == Page::Shortcuts { 1.0 } else { 0.0 }, now, duration);
            motion.content.retarget(0.0, now, 0.0);
            motion.content.retarget(1.0, now, duration);
        }
        self.refresh();
    }

    fn show(&self, id: Control, visible: bool, enabled: bool) {
        let child = self.child(id);
        // SAFETY: the settings STA owns this child. Showing it never activates another window.
        unsafe {
            let _ = EnableWindow(child, enabled);
            let _ = ShowWindow(child, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }

    /// The modifiers held for the key message being handled. The self-test replaces them,
    /// because a sent message does not change the keyboard state.
    fn held(&self) -> Held {
        if self.options.test.is_some() {
            *self.lifetime.test_modifiers.lock().unwrap_or_else(|e| e.into_inner())
        } else {
            held_for_message()
        }
    }

    /// The chord a row shows: unavailable actions show their default.
    fn row_chord(&self, action: Action) -> Option<Chord> {
        if action.available() {
            self.model.borrow().settings.shortcuts.get(action)
        } else {
            action.default_chord()
        }
    }

    fn row_note(&self, action: Action, chord: Option<Chord>) -> Note {
        let recorder = self.recorder.borrow();
        let capture = recorder.capture.as_ref().filter(|capture| capture.action == action);
        let checking = recorder.proposal.as_ref().is_some_and(|proposal| proposal.action == action);
        if !action.available() {
            Note::new("Not available yet", Tone::Muted)
        } else if let Some(error) = capture.and_then(|capture| capture.error.as_ref()) {
            Note::new(error.clone(), Tone::Error)
        } else if capture.is_some() || checking {
            // The check takes a moment; the prompt stays until its answer.
            Note::new(RECORDING_PROMPT, Tone::Muted)
        } else if let Some(note) = &recorder.notes[action as usize] {
            note.clone()
        } else {
            let status = recorder.registration.iter().find(|(row, _)| *row == action);
            match (chord, status.map(|(_, status)| status)) {
                (Some(chord), Some(Registration::Taken)) => {
                    Note::new(taken_message(&chord), Tone::Error)
                }
                (Some(_), Some(Registration::Failed(message))) => {
                    Note::new(message.clone(), Tone::Error)
                }
                // Pen and Highlighter: their keys stay free until a recording starts.
                (Some(_), Some(Registration::WhileRecording)) => {
                    Note::new(WHILE_RECORDING, Tone::Muted)
                }
                _ => Note::default(),
            }
        }
    }

    fn refresh_shortcuts(&self) {
        let now = self.now();
        let on_page = self.page.get() == Page::Shortcuts;
        let loaded = self.model.borrow().load_error.is_none();
        for &action in Action::ALL {
            let index = action as usize;
            let chord = self.row_chord(action);
            let note = self.row_note(action, chord);
            let chips = {
                let recorder = self.recorder.borrow();
                let capture = recorder.capture.as_ref().filter(|capture| capture.action == action);
                let proposal = recorder.proposal.as_ref().filter(|proposal| proposal.action == action);
                match (capture, proposal) {
                    (Some(capture), _) => modifier_chips(capture.held),
                    (None, Some(proposal)) => {
                        proposal.value.get(action).as_ref().map(chord_chips).unwrap_or_default()
                    }
                    (None, None) => chord.as_ref().map(chord_chips).unwrap_or_default(),
                }
            };
            let differs = action.available() && chord != action.default_chord();
            let recorder = Control::shortcut(action, ShortcutPart::Recorder);
            let reset = Control::shortcut(action, ShortcutPart::Reset);
            let shown = chord.map_or_else(|| "none".to_owned(), |chord| display_for_layout(&chord));
            self.text(recorder, &format!("{}, shortcut {shown}, press to change", action.label()));
            self.text(Control::shortcut(action, ShortcutPart::Status), &note.text);
            {
                let mut state = self.recorder.borrow_mut();
                let view = &mut state.rows[index];
                let mut motion = self.motion.borrow_mut();
                let duration =
                    if on_page && self.started.get() { motion.duration(ROW_MS) } else { 0.0 };
                let tracks = &mut motion.rows[index];
                if chips != view.chips {
                    view.kept = view.chips.iter().zip(&chips).take_while(|(a, b)| a == b).count();
                    view.chips = chips;
                    tracks[CHIPS].retarget(0.0, now, 0.0);
                    tracks[CHIPS].retarget(1.0, now, duration);
                }
                tracks[RESET].retarget(if differs { 1.0 } else { 0.0 }, now, duration);
                if note.text.is_empty() {
                    // The old text stays while it fades out.
                    tracks[NOTE].retarget(0.0, now, duration);
                } else {
                    if note != view.note {
                        view.note = note;
                        tracks[NOTE].retarget(0.0, now, 0.0);
                    }
                    tracks[NOTE].retarget(1.0, now, duration);
                }
            }
            // SAFETY: focus query and move within this STA's own children.
            unsafe {
                if !differs && GetFocus() == self.child(reset) {
                    let _ = SetFocus(Some(self.child(recorder)));
                }
            }
            for part in [ShortcutPart::Label, ShortcutPart::Status] {
                self.show(Control::shortcut(action, part), on_page, on_page);
            }
            self.show(recorder, on_page, on_page && loaded && action.available());
            self.show(reset, on_page && differs, on_page && loaded && differs);
        }
        self.update_animation();
        if on_page {
            self.invalidate(Some(scaled(CONTENT, self.dpi.get())));
        }
    }

    fn records(&self, action: Action) -> bool {
        self.recorder.borrow().capture.as_ref().is_some_and(|capture| capture.action == action)
    }

    /// Click, Space or Enter on a recorder: wait for keys, with the hotkeys paused.
    fn begin_capture(&self, action: Action) {
        if !action.available() || self.model.borrow().load_error.is_some() {
            return;
        }
        let switching = {
            let mut recorder = self.recorder.borrow_mut();
            if recorder.proposal.is_some() {
                return;
            }
            recorder.notes[action as usize] = None;
            recorder.swallow = None;
            recorder
                .capture
                .replace(Capture { action, held: Held::default(), error: None })
                .is_some()
        };
        // Another row was recording: the service is already paused.
        if !switching {
            self.lifetime.emit(Request::Pause);
        }
        self.motion.borrow_mut().pulse_since = Some(self.now());
        self.refresh_shortcuts();
    }

    /// Esc, focus loss, a page change: keep the old shortcut and resume the hotkeys.
    fn cancel_capture(&self) {
        let ended = self.recorder.borrow_mut().capture.take();
        if ended.is_some() {
            self.motion.borrow_mut().pulse_since = None;
            self.lifetime.emit(Request::Resume);
            self.refresh_shortcuts();
        }
    }

    /// The window is closing: drop any recording or check, and resume the hotkeys.
    fn end_recorder(&self) {
        let open = {
            let mut recorder = self.recorder.borrow_mut();
            let capture = recorder.capture.take();
            let proposal = recorder.proposal.take();
            capture.is_some() || proposal.is_some()
        };
        if open {
            self.motion.borrow_mut().pulse_since = None;
            self.lifetime.emit(Request::Resume);
        }
    }

    /// One key message to the recording recorder. `down` is a new press (WM_KEYDOWN or
    /// WM_SYSKEYDOWN, not a repeat); anything else only updates the held modifiers.
    fn recorder_key(&self, vk: u16, down: bool) {
        let held = self.held();
        let action = {
            let mut recorder = self.recorder.borrow_mut();
            let Some(capture) = recorder.capture.as_mut() else { return };
            capture.held = held;
            capture.action
        };
        let input = if down { chord_from_key_event(vk, held) } else { RecorderInput::Pending };
        let chord = match input {
            RecorderInput::Pending => return self.refresh_shortcuts(),
            RecorderInput::Cancel => {
                self.recorder.borrow_mut().swallow = Some(vk);
                return self.cancel_capture();
            }
            RecorderInput::Clear => None,
            RecorderInput::Chord(chord) => Some(chord),
        };
        let mut proposed = self.model.borrow().settings.shortcuts.clone();
        if let Err(error) = proposed.set(action, chord) {
            if let Some(capture) = self.recorder.borrow_mut().capture.as_mut() {
                capture.error = Some(error.to_string());
            }
            return self.refresh_shortcuts();
        }
        {
            let mut recorder = self.recorder.borrow_mut();
            recorder.capture = None;
            recorder.swallow = Some(vk);
        }
        self.motion.borrow_mut().pulse_since = None;
        self.check(action, proposed);
    }

    fn reset_shortcut(&self, action: Action) {
        if !action.available() || self.recorder.borrow().proposal.is_some() {
            return;
        }
        self.cancel_capture();
        let current = self.model.borrow().settings.shortcuts.clone();
        let mut proposed = current.clone();
        proposed.reset(action);
        if proposed != current {
            self.check(action, proposed);
        }
    }

    /// Asks the UI loop to register `proposed`; it is saved when the tagged answer comes.
    fn check(&self, action: Action, proposed: Shortcuts) {
        let tag = NEXT_TAG.fetch_add(1, SeqCst);
        {
            let mut recorder = self.recorder.borrow_mut();
            recorder.notes[action as usize] = None;
            recorder.proposal = Some(Proposal { action, tag, value: proposed.clone() });
        }
        self.lifetime.emit(Request::Check { tag, proposed });
        self.refresh_shortcuts();
    }

    fn receive_registration(&self) {
        let reports =
            std::mem::take(&mut *self.lifetime.reports.lock().unwrap_or_else(|e| e.into_inner()));
        for report in reports {
            if report.tag == 0 {
                self.recorder.borrow_mut().registration = report.entries;
                continue;
            }
            // A report for an older check, or for none, is ignored.
            let proposal = {
                let mut recorder = self.recorder.borrow_mut();
                let current = recorder.proposal.as_ref().is_some_and(|p| p.tag == report.tag);
                if current { recorder.proposal.take() } else { None }
            };
            if let Some(proposal) = proposal {
                self.finish(proposal, &report.entries);
            }
        }
        self.refresh_shortcuts();
    }

    /// The UI loop registered a proposal. Another program holds it: the old shortcut stays
    /// (it was never saved) and the row says so. Otherwise save it.
    fn finish(&self, proposal: Proposal, entries: &[(Action, Registration)]) {
        let action = proposal.action;
        let chord = proposal.value.get(action);
        let status = entries.iter().find(|(row, _)| *row == action).map(|(_, status)| status);
        let note = match (status, chord) {
            (Some(Registration::Taken), Some(chord)) => {
                Some(Note::new(taken_message(&chord), Tone::Error))
            }
            _ => {
                let old =
                    std::mem::replace(&mut self.model.borrow_mut().settings.shortcuts, proposal.value);
                if self.save() {
                    match (status, chord) {
                        (Some(Registration::Failed(message)), Some(_)) => {
                            Some(Note::new(message.clone(), Tone::Error))
                        }
                        (_, Some(chord)) => {
                            shortcut_warning(&chord).map(|hint| Note::new(hint, Tone::Muted))
                        }
                        _ => None,
                    }
                } else {
                    let mut model = self.model.borrow_mut();
                    model.settings.shortcuts = old;
                    model.save_error.clone().map(|error| Note::new(error, Tone::Error))
                }
            }
        };
        self.recorder.borrow_mut().notes[action as usize] = note;
        self.lifetime.emit(Request::Resume);
    }

    /// Whether a queued key message belongs to a recorder rather than to dialog navigation:
    /// the recorder is recording, or it is the repeat or release of the key that ended it.
    fn recorder_owns(&self, message: &MSG) -> bool {
        // SAFETY: scalar queries for a window of this thread's queue.
        let (parent, id) = unsafe { (GetParent(message.hwnd).ok(), GetDlgCtrlID(message.hwnd)) };
        let row = Item::lookup(id).and_then(|item| item.id.shortcut_row());
        let Some((action, ShortcutPart::Recorder)) = row else {
            return false;
        };
        if parent != Some(self.hwnd) {
            return false;
        }
        if self.records(action) {
            return true;
        }
        let vk = message.wParam.0 as u16;
        let release = matches!(message.message, WM_KEYUP | WM_SYSKEYUP);
        let repeat = message.lParam.0 & (1 << 30) != 0;
        self.recorder.borrow().swallow == Some(vk) && (release || repeat)
    }

    /// Eats the repeats and the release of the key that ended a recording.
    fn swallows(&self, vk: u16, msg: u32, lp: LPARAM) -> bool {
        let mut recorder = self.recorder.borrow_mut();
        if recorder.swallow != Some(vk) {
            return false;
        }
        if matches!(msg, WM_KEYUP | WM_SYSKEYUP) {
            recorder.swallow = None;
            true
        } else if lp.0 & (1 << 30) != 0 {
            true
        } else {
            recorder.swallow = None;
            false
        }
    }
}

impl Window<'_> {
    fn refresh(&self) {
        let (
            checked,
            loaded,
            launch_available,
            reset_notes,
            reset_captures,
            status,
            retry,
            dimensions,
            keys_name,
        ) = {
            let model = self.model.borrow();
            let settings = &model.settings;
            let dimensions = settings.card_size.thumb_max();
            (
                [
                    (Screenshots, settings.is_enabled(SCREENSHOTS)),
                    (Keep, settings.keep_every_screenshot),
                    (Notes, settings.is_enabled(NOTES)),
                    (Recording, settings.is_enabled(RECORDING)),
                    (Small, settings.card_size == CardSize::Small),
                    (Medium, settings.card_size == CardSize::Medium),
                    (Large, settings.card_size == CardSize::Large),
                    (Launch, model.launch.as_ref().is_ok_and(|&on| on)),
                ],
                model.load_error.is_none(),
                model.launch.is_ok(),
                settings.notes_folder.is_some(),
                settings.captures_folder.is_some(),
                model.status(),
                model.save_error.is_some(),
                format!("{} × {} px", dimensions.width, dimensions.height),
                recording_keys_name(&settings.shortcuts),
            )
        };
        let now = self.now();
        {
            let mut motion = self.motion.borrow_mut();
            let duration = if self.started.get() { motion.duration(SWITCH_MS) } else { 0.0 };
            for (id, selected) in checked {
                if matches!(id, Screenshots | Keep | Notes | Recording | Launch) {
                    motion.buttons[id.index()].checked.retarget(
                        if selected { 1.0 } else { 0.0 },
                        now,
                        duration,
                    );
                }
            }
            let card = if checked[6].1 {
                2
            } else if checked[5].1 {
                1
            } else {
                0
            };
            let duration = if self.started.get() { motion.duration(CARD_MS) } else { 0.0 };
            motion.select_card(card, now, duration);
        }
        let page = self.page.get();
        let tabs = [(GeneralTab, page == Page::General), (ShortcutsTab, page == Page::Shortcuts)];
        for (id, checked) in checked.into_iter().chain(tabs) {
            // SAFETY: checkbox and radio BUTTONs accept scalar BM_SETCHECK parameters.
            unsafe {
                SendMessageW(
                    self.child(id),
                    BM_SETCHECK,
                    Some(WPARAM(if checked { BST_CHECKED.0 } else { BST_UNCHECKED.0 } as usize)),
                    None,
                )
            };
        }
        self.text(CardDimensions, &dimensions);
        self.text(RecordingKeys, &keys_name);
        self.text(Status, &status);
        let show_status = !status.is_empty();
        if show_status {
            self.error_label.replace(status);
        }
        {
            let mut motion = self.motion.borrow_mut();
            let duration = motion.duration(ROW_MS);
            motion.status.retarget(if show_status { 1.0 } else { 0.0 }, now, duration);
            motion.retry.retarget(if retry { 1.0 } else { 0.0 }, now, duration);
        }
        // The hidden page's controls are hidden and disabled: Tab and assistive technology
        // skip them.
        for item in ITEMS {
            let on_page = page.contains(item.id);
            let visible = on_page
                && match item.id {
                    Status => show_status,
                    Retry => retry,
                    _ => true,
                };
            let enabled = on_page
                && (matches!(item.id, GeneralTab | ShortcutsTab)
                    || !item.button()
                    || loaded
                        && match item.id {
                            Launch => launch_available,
                            ResetNotes => reset_notes,
                            ResetCaptures => reset_captures,
                            Retry => retry,
                            _ => true,
                        });
            self.show(item.id, visible, enabled);
        }
        self.refresh_shortcuts();
        self.sync_buttons();
        self.invalidate(None);
    }

    fn refresh_folders(&self, init: bool) {
        for (index, (id, fallback)) in
            [(NotesPath, NotesFallback), (CapturesPath, CapturesFallback)].into_iter().enumerate()
        {
            let (path, used_default) = self.model.borrow().folders[index].clone();
            {
                let now = self.now();
                let mut motion = self.motion.borrow_mut();
                let duration = motion.duration(ROW_MS);
                motion.folders[index].retarget(if used_default { 1.0 } else { 0.0 }, now, duration);
            }
            let full = display_path(&path);
            let child = self.child(id);
            let width = scaled(Item::lookup(id as i32).expect("path control").rect, self.dpi.get());
            // SAFETY: the child is live and the DC is released on this thread below.
            let dc = unsafe { GetDC(Some(child)) };
            let font = self.fonts.borrow().0[1];
            // SAFETY: the DC and font belong to this thread. Restore the prior font below.
            let previous = unsafe { SelectObject(dc, font.into()) };
            let short = shorten(&full, |label| text_width(dc, label) <= width.right - width.left);
            // SAFETY: restores the original selection and releases our exact DC.
            unsafe {
                SelectObject(dc, previous);
                ReleaseDC(Some(child), dc);
            }
            self.text(id, &short);
            self.text(
                fallback,
                if used_default {
                    "The chosen folder is unavailable. Using the default."
                } else {
                    ""
                },
            );
            // Keep both the old buffer and the new buffer live throughout the update.
            let mut text = wide(full.as_ref());
            let tool = TTTOOLINFOW {
                cbSize: size_of::<TTTOOLINFOW>() as u32,
                uFlags: TTF_IDISHWND | TTF_SUBCLASS,
                hwnd: self.hwnd,
                uId: child.0 as usize,
                lpszText: windows::core::PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            // SAFETY: synchronous same-thread message; all pointed-to data is live and the
            // new tooltip text buffer stays owned by the window after the message returns.
            unsafe {
                SendMessageW(
                    self.tooltip.get(),
                    if init { TTM_ADDTOOLW } else { TTM_UPDATETIPTEXTW },
                    None,
                    Some(LPARAM((&tool as *const TTTOOLINFOW) as isize)),
                )
            };
            self.tips.borrow_mut()[index] = text;
        }
        self.update_animation();
    }

    /// Adds the recordings that showed the zoom hint to the settings and saves. Not when the
    /// file could not be loaded: then nothing is saved (the counts are dropped).
    fn count_hints(&self) {
        let count = self.lifetime.hint_bumps.swap(0, SeqCst);
        if count == 0 || self.model.borrow().load_error.is_some() {
            return;
        }
        {
            let mut model = self.model.borrow_mut();
            model.settings.recording_hints_shown =
                model.settings.recording_hints_shown.saturating_add(count);
        }
        self.save();
    }

    fn save(&self) -> bool {
        let settings = self.model.borrow().settings.clone();
        let result = settings.save(&self.options.path);
        self.model.borrow_mut().save_error =
            result.as_ref().err().map(|err| format!("Settings could not be saved. {err}"));
        if result.is_ok() {
            self.lifetime.emit(Request::Saved(settings));
        }
        self.refresh();
        result.is_ok()
    }

    fn set_folder(&self, folder: Folder, path: Option<PathBuf>) {
        let mut model = self.model.borrow_mut();
        *folder.chosen(&mut model.settings) = path;
        model.folders[folder as usize] = folder.resolve(&model.settings, &self.defaults);
        drop(model);
        self.refresh_folders(false);
        self.save();
    }

    fn command(&self, id: i32) {
        let Some(item) = Item::lookup(id) else { return };
        if !self.page.get().contains(item.id) || self.picking.get() || self.close_pending.get() {
            return;
        }
        match item.id {
            GeneralTab => return self.set_page(Page::General),
            ShortcutsTab => return self.set_page(Page::Shortcuts),
            _ => {}
        }
        if self.picking.get()
            || self.model.borrow().load_error.is_some()
            || self.close_pending.get()
        {
            return;
        }
        let child = self.child(item.id);
        // SAFETY: only the state of a child of this window is queried.
        if !item.button() || !unsafe { IsWindowEnabled(child) }.as_bool() {
            return;
        }
        match item.id.shortcut_row() {
            Some((action, ShortcutPart::Recorder)) => return self.begin_capture(action),
            Some((action, ShortcutPart::Reset)) => return self.reset_shortcut(action),
            Some(_) => return,
            None => {}
        }
        self.model.borrow_mut().action_error = None;
        match item.id {
            ChooseNotes | ChooseCaptures => {
                if self.options.test.is_none() {
                    // SAFETY: queue a scalar command so the original button call has returned
                    // by the time the picker enters its nested message loop.
                    let _ = unsafe {
                        PostMessageW(Some(self.hwnd), PICK, WPARAM(id as usize), LPARAM(0))
                    };
                }
                return;
            }
            OpenNotes | OpenCaptures => {
                if self.options.test.is_none() {
                    let folder = usize::from(item.id == OpenCaptures);
                    let path = wide(self.model.borrow().folders[folder].0.as_os_str());
                    // SAFETY: the owner is live and ShellExecuteW receives a terminated path.
                    let result = unsafe {
                        ShellExecuteW(
                            Some(self.hwnd),
                            w!("open"),
                            PCWSTR(path.as_ptr()),
                            None,
                            None,
                            SW_SHOWNORMAL,
                        )
                    };
                    if result.0 as isize <= 32 {
                        self.model.borrow_mut().action_error = Some(format!(
                            "Could not open folder. File Explorer error {}.",
                            result.0 as isize
                        ));
                    }
                    self.refresh();
                }
                return;
            }
            ResetNotes => return self.set_folder(Folder::Notes, None),
            ResetCaptures => return self.set_folder(Folder::Captures, None),
            Screenshots => {
                let mut model = self.model.borrow_mut();
                let on = model.settings.is_enabled(SCREENSHOTS);
                model.settings.set_enabled(SCREENSHOTS, !on);
            }
            Notes => {
                let mut model = self.model.borrow_mut();
                let on = model.settings.is_enabled(NOTES);
                model.settings.set_enabled(NOTES, !on);
            }
            // Off during a recording: the app saves that recording first.
            Recording => {
                let mut model = self.model.borrow_mut();
                let on = model.settings.is_enabled(RECORDING);
                model.settings.set_enabled(RECORDING, !on);
            }
            Keep => {
                let mut model = self.model.borrow_mut();
                model.settings.keep_every_screenshot = !model.settings.keep_every_screenshot;
            }
            Small | Medium | Large => {
                self.model.borrow_mut().settings.card_size = match item.id {
                    Small => CardSize::Small,
                    Medium => CardSize::Medium,
                    _ => CardSize::Large,
                };
            }
            Launch => {
                let on = self.model.borrow().launch.as_ref().is_ok_and(|&on| on);
                let result =
                    std::env::current_exe().and_then(|exe| set_launch(&self.registry, &exe, !on));
                let mut model = self.model.borrow_mut();
                match result {
                    Ok(on) => {
                        model.launch = Ok(on);
                        model.settings.launch_at_login = on;
                    }
                    Err(error) => {
                        model.launch = self.registry.exists();
                        model.action_error =
                            Some(format!("Could not change Launch at login. {error}"));
                        drop(model);
                        self.refresh();
                        return;
                    }
                }
            }
            Retry => {}
            _ => return,
        }
        self.save();
    }

    fn choose(&self, folder: Folder) {
        if self.picking.get() || self.close_pending.get() || self.lifetime.stop.load(SeqCst) {
            return;
        }
        self.picking.set(true);
        // Also guards the setup calls, which can dispatch a window message through COM.
        let result = self.show_picker(folder);
        self.dialog.replace(None);
        self.picking.set(false);
        if self.close_pending.get() || self.lifetime.stop.load(SeqCst) {
            self.request_close();
        } else {
            match result {
                Ok(Some(path)) => self.set_folder(folder, Some(path)),
                Ok(None) => {}
                Err(error) => {
                    self.model.borrow_mut().action_error =
                        Some(format!("Could not choose a folder. {error}"));
                    self.refresh();
                }
            }
        }
    }

    fn show_picker(&self, folder: Folder) -> Result<Option<PathBuf>> {
        // SAFETY: COM was initialized on this thread, and owns the returned interface.
        let dialog: IFileOpenDialog =
            unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)? };
        self.dialog.replace(Some(dialog.clone()));
        let path = wide(self.model.borrow().folders[folder as usize].0.as_os_str());
        // SAFETY: owned COM interfaces, a terminated path, and no borrow across COM calls.
        unsafe {
            dialog.SetOptions(dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)?;
            dialog.SetTitle(match folder {
                Folder::Notes => w!("Choose notes folder"),
                Folder::Captures => w!("Choose captures folder"),
            })?;
            if let Ok(start) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(path.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&start);
            }
        }
        if self.close_pending.get() || self.lifetime.stop.load(SeqCst) {
            return Ok(None);
        }
        // SAFETY: the owner and COM interface live through the modal call. WM_CLOSE cancels
        // the dialog on this STA and defers destroying its owner until Show has returned.
        if let Err(error) = unsafe { dialog.Show(Some(self.hwnd)) } {
            return if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: copy the terminated path returned by the Shell and release its allocation.
        let path = unsafe {
            let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = PathBuf::from(OsString::from_wide(name.as_wide()));
            CoTaskMemFree(Some(name.0.cast()));
            path
        };
        Ok(Some(path))
    }

    fn request_close(&self) {
        let already_closing = self.close_pending.replace(true);
        self.end_recorder();
        let dialog = self.dialog.borrow().clone();
        if let Some(dialog) = dialog {
            // SAFETY: Close is called on the STA that owns Show, without a borrowed model.
            if !already_closing {
                let _ = unsafe { dialog.Close(HRESULT::from_win32(ERROR_CANCELLED.0)) };
            }
        } else if !self.picking.get() {
            // SAFETY: this thread owns the HWND. Window state survives DestroyWindow.
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }

    fn activate(&self) {
        if self.options.test.is_some() {
            return;
        }
        // SAFETY: all HWNDs belong to this STA. Even when modal, activate the popup.
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(GetLastActivePopup(self.hwnd));
        }
    }
}

impl Drop for Window<'_> {
    fn drop(&mut self) {
        // Every other way out has ended the recorder already; this one covers a window
        // destroyed without WM_CLOSE.
        let recorder = self.recorder.get_mut();
        if recorder.capture.is_some() || recorder.proposal.is_some() {
            self.lifetime.emit(Request::Resume);
        }
        // SAFETY: both handles were created here, and the window has been destroyed.
        for icon in self.icons.iter().copied().filter(|icon| !icon.is_invalid()) {
            let _ = unsafe { DestroyIcon(icon) };
        }
    }
}

struct Apartment;

impl Apartment {
    fn new() -> Result<Self> {
        // SAFETY: called once on our new thread. A successful call is balanced below.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: apartment guard stayed on its creating thread throughout.
        unsafe { CoUninitialize() };
    }
}

fn settings_icon(instance: HINSTANCE, color: [u8; 4]) -> HICON {
    let pixmap = crate::tray::logo_pixmap(32, color);
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    let mut mask = [0u8; 128];
    for (i, pixel) in pixmap.pixels().iter().enumerate() {
        let pixel = pixel.demultiply();
        bgra.extend([pixel.blue(), pixel.green(), pixel.red(), pixel.alpha()]);
        if pixel.alpha() == 0 {
            mask[i / 8] |= 0x80 >> (i % 8);
        }
    }
    // SAFETY: padded 32x32 mask at 1 bpp and a 32x32 BGRA buffer at 32 bpp.
    unsafe { CreateIcon(Some(instance), 32, 32, 1, 32, mask.as_ptr(), bgra.as_ptr()) }
        .unwrap_or_default()
}

fn run_window(options: &Options, lifetime: &Lifetime) -> Result<()> {
    let _apartment = Apartment::new()?;
    let defaults = options.test.as_ref().map_or_else(DefaultFolders::from_system, |test| {
        DefaultFolders { notes: test.notes.clone(), captures: test.captures.clone() }
    });
    let registry =
        options.test.as_ref().map_or_else(|| Registry(wide("Igorio")), TestMode::registry);
    let model = Model::load(&options.path, &defaults, &registry);
    if lifetime.stop.load(SeqCst) {
        return Ok(());
    }
    // SAFETY: static module and cursor identifiers, no borrowed pointers.
    let (instance, cursor) =
        unsafe { (HINSTANCE(GetModuleHandleW(None)?.0), LoadCursorW(None, IDC_ARROW)?) };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: CLASS,
        hCursor: cursor,
        ..Default::default()
    };
    // SAFETY: class fields refer to fixed code/data and valid handles.
    if unsafe { RegisterClassW(&class) } == 0 {
        let error = Error::from_thread();
        if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(error);
        }
    }
    let controls = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_WIN95_CLASSES,
    };
    // SAFETY: structure is valid for the duration of InitCommonControlsEx.
    let _ = unsafe { InitCommonControlsEx(&controls) };
    let monitor = monitor_at(cursor_pos());
    let at = monitor.work_area;
    let dpi = options.test.as_ref().map_or((monitor.scale * 96.0).round() as u32, |test| test.dpi);
    // First put the hidden HWND on the target monitor so Windows picks its actual DPI.
    // SAFETY: registered class, static caption and no creation pointer.
    let hwnd = unsafe {
        CreateWindowExW(
            EX_STYLE,
            CLASS,
            w!("Igorio Settings"),
            STYLE,
            (at.x + at.width / 2.0) as i32,
            (at.y + at.height / 2.0) as i32,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )?
    };
    let window = Box::new(Window {
        hwnd,
        lifetime,
        options,
        defaults,
        registry,
        model: RefCell::new(model),
        dialog: RefCell::new(None),
        picking: Cell::new(false),
        close_pending: Cell::new(false),
        dpi: Cell::new(dpi),
        epoch: Instant::now(),
        motion: RefCell::new(Motion::new(animation_effects())),
        frame: Cell::new(Motion::new(false).frame(0.0)),
        hover: Cell::new(None),
        timer: Cell::new(false),
        started: Cell::new(false),
        frozen: Cell::new(None),
        surface: RefCell::new(None),
        error_label: RefCell::new(String::new()),
        page: Cell::new(Page::General),
        recorder: RefCell::new(Recorder::default()),
        fonts: RefCell::new(Fonts::new(dpi)),
        tooltip: Cell::new(HWND::default()),
        tips: RefCell::new(Default::default()),
        icons: [
            settings_icon(instance, theme::TEXT),
            settings_icon(instance, [234, 240, 252, 255]),
        ],
    });
    // SAFETY: everything reachable from this shared pointer outlives DestroyWindow.
    // WM_NCDESTROY detaches it, and only this STA reads it.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&*window as *const Window<'_>) as isize) };
    let result = (|| {
        window.init_children(instance)?;
        // SAFETY: select the General page in the native radio group before showing it.
        unsafe { SendMessageW(window.child(GeneralTab), BM_SETCHECK, Some(WPARAM(1)), None) };
        window.set_theme();
        window.relayout(dpi);
        window.refresh_folders(true);
        window.refresh();
        window.change_ui_state(UIS_SET, UISF_HIDEFOCUS | UISF_HIDEACCEL);
        let mut bounds = RECT::default();
        window.adjust_size(None);
        // SAFETY: same-thread window and writable RECT.
        unsafe { GetWindowRect(hwnd, &mut bounds)? };
        let width = f64::from(bounds.right - bounds.left);
        let height = f64::from(bounds.bottom - bounds.top);
        window.adjust_size(Some((
            (at.x + ((at.width - width) / 2.0).max(0.0)) as i32,
            (at.y + ((at.height - height) / 2.0).max(0.0)) as i32,
        )));
        lifetime.hwnd.store(hwnd.0 as isize, SeqCst);
        window.count_hints();
        lifetime.emit(Request::Status);
        if !lifetime.stop.load(SeqCst) {
            let now = window.now();
            {
                let mut motion = window.motion.borrow_mut();
                let duration = motion.duration(OPEN_MS);
                motion.intro.retarget(1.0, now, duration);
            }
            window.started.set(true);
            window.update_animation();
            window.invalidate(None);
            // SAFETY: this STA owns the window. Test mode leaves the foreground app alone.
            let _ = unsafe {
                ShowWindow(
                    hwnd,
                    if options.test.is_some() { SW_SHOWNOACTIVATE } else { SW_SHOWNORMAL },
                )
            };
            window.activate();
            pump(&window)?;
        }
        Ok(())
    })();
    // SAFETY: the STA owns the HWND, and still owns the state if creation failed partway.
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
    }
    result
}

fn pump(window: &Window<'_>) -> Result<()> {
    let mut message = MSG::default();
    loop {
        // SAFETY: stack MSG for this STA's queue; GetMessage returns -1 on failure.
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        match status {
            0 => return Ok(()),
            -1 => return Err(Error::from_thread()),
            _ => {}
        }
        if matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP)
            && window.recorder_owns(&message)
        {
            // Straight to the recorder: no dialog keys, menu cues or characters.
            // SAFETY: dispatch a message from this same queue to our own child.
            unsafe { DispatchMessageW(&message) };
            continue;
        }
        if message.message == WM_KEYDOWN || message.message == WM_SYSKEYDOWN {
            window.change_ui_state(UIS_CLEAR, UISF_HIDEFOCUS);
            if message.wParam.0 == VK_MENU.0 as usize {
                window.change_ui_state(UIS_CLEAR, UISF_HIDEACCEL);
            }
        } else if matches!(message.message, WM_KEYUP | WM_SYSKEYUP)
            && message.wParam.0 == VK_MENU.0 as usize
        {
            window.change_ui_state(UIS_SET, UISF_HIDEACCEL);
        }
        if message.message == WM_KEYDOWN && message.wParam.0 == VK_TAB.0 as usize {
            let held = window.held();
            // SAFETY: scalar parent query for a window of this thread's queue.
            let ours = message.hwnd == window.hwnd
                || unsafe { GetParent(message.hwnd) }.ok() == Some(window.hwnd);
            if ours && held.ctrl && !held.alt && !held.win {
                // Ctrl+Tab and Ctrl+Shift+Tab: with two pages, both go to the other one.
                window.set_page(match window.page.get() {
                    Page::General => Page::Shortcuts,
                    Page::Shortcuts => Page::General,
                });
                continue;
            }
        }
        // SAFETY: GetFocus is this STA's focused control. Only click our direct child.
        let enter = unsafe {
            let focused = GetFocus();
            if message.message == WM_KEYDOWN
                && message.wParam.0 == VK_RETURN.0 as usize
                && GetParent(focused).ok() == Some(window.hwnd)
            {
                SendMessageW(focused, BM_CLICK, None, None);
                true
            } else {
                false
            }
        };
        // SAFETY: both the message and its target belong to this STA.
        if !enter && !unsafe { IsDialogMessageW(window.hwnd, &message) }.as_bool() {
            // SAFETY: dispatch the unhandled message from this same queue.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}

impl Surface {
    fn new(width: u32, height: u32) -> Result<Self> {
        let mut bits = std::ptr::null_mut();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: info describes a top-down, 32-bit DIB, and bits is a valid out pointer.
        let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
        // SAFETY: the new memory DC is private to the STA.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            // SAFETY: the bitmap was never selected.
            let _ = unsafe { DeleteObject(bitmap.into()) };
            return Err(Error::from_thread());
        }
        // SAFETY: bitmap and DC are live, owned here, and the selection is restored in Drop.
        let old = unsafe { SelectObject(dc, bitmap.into()) };
        Ok(Self { dc, bitmap, old, bits: bits.cast(), width, height })
    }

    fn canvas(&mut self, dirty: RECT) -> Canvas<'_> {
        // SAFETY: GdiFlush finishes prior reads/writes to the selected DIB before CPU access.
        let _ = unsafe { GdiFlush() };
        // SAFETY: this owns the live DIB of the declared dimensions, borrowed for the canvas.
        let bits = unsafe {
            std::slice::from_raw_parts_mut(self.bits, (self.width * self.height * 4) as usize)
        };
        Canvas {
            pixmap: tiny_skia::PixmapMut::from_bytes(bits, self.width, self.height)
                .expect("valid DIB"),
            labels: Vec::new(),
            dc: self.dc,
            dirty,
            dx: 0,
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: restore the original selection before releasing these owned GDI handles.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Label {
    rect: RECT,
    text: String,
    ink: COLORREF,
    format: DRAW_TEXT_FORMAT,
    font: HFONT,
}

struct Canvas<'a> {
    pixmap: tiny_skia::PixmapMut<'a>,
    labels: Vec<Label>,
    dc: HDC,
    dirty: RECT,
    /// Moves everything drawn sideways, for the page slide.
    dx: i32,
}

fn intersects(a: RECT, b: RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn inset(rect: RECT, amount: i32) -> RECT {
    RECT {
        left: rect.left + amount,
        top: rect.top + amount,
        right: rect.right - amount,
        bottom: rect.bottom - amount,
    }
}

fn move_y(mut rect: RECT, amount: i32) -> RECT {
    rect.top += amount;
    rect.bottom += amount;
    rect
}

fn rounded_path(rect: RECT, radius: f32) -> Option<tiny_skia::Path> {
    let (l, t, r, b) = (rect.left as f32, rect.top as f32, rect.right as f32, rect.bottom as f32);
    let d = radius.min((r - l) / 2.0).min((b - t) / 2.0);
    if d <= 0.0 {
        return tiny_skia::Rect::from_ltrb(l, t, r, b).map(tiny_skia::PathBuilder::from_rect);
    }
    let k = d * 0.552_284_8;
    let mut path = tiny_skia::PathBuilder::new();
    path.move_to(l + d, t);
    path.line_to(r - d, t);
    path.cubic_to(r - d + k, t, r, t + d - k, r, t + d);
    path.line_to(r, b - d);
    path.cubic_to(r, b - d + k, r - d + k, b, r - d, b);
    path.line_to(l + d, b);
    path.cubic_to(l + d - k, b, l, b - d + k, l, b - d);
    path.line_to(l, t + d);
    path.cubic_to(l, t + d - k, l + d - k, t, l + d, t);
    path.close();
    path.finish()
}

impl Canvas<'_> {
    fn moved(&self, rect: RECT) -> RECT {
        RECT { left: rect.left + self.dx, right: rect.right + self.dx, ..rect }
    }

    fn visible(&self, rect: RECT) -> bool {
        intersects(self.moved(rect), self.dirty)
    }

    fn shape(&mut self, rect: RECT, radius: f32, color: COLORREF) {
        let rect = self.moved(rect);
        if intersects(rect, self.dirty)
            && let Some(path) = rounded_path(rect, radius)
        {
            let mut paint = tiny_skia::Paint::default();
            // The DIB is BGRA. Swap red and blue when feeding tiny-skia's RGBA rasterizer.
            paint.set_color_rgba8((color.0 >> 16) as u8, (color.0 >> 8) as u8, color.0 as u8, 255);
            self.pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }

    fn outlined(&mut self, rect: RECT, radius: f32, face: COLORREF, edge: COLORREF) {
        self.shape(rect, radius, edge);
        self.shape(inset(rect, 1), (radius - 1.0).max(0.0), face);
    }

    fn text(
        &mut self,
        rect: RECT,
        text: impl Into<String>,
        ink: COLORREF,
        format: DRAW_TEXT_FORMAT,
        font: HFONT,
    ) {
        let rect = self.moved(rect);
        if intersects(rect, self.dirty) {
            self.labels.push(Label { rect, text: text.into(), ink, format, font });
        }
    }

    fn width(&self, text: &str, font: HFONT) -> i32 {
        // SAFETY: font and canvas DC are live, and the previous selection is restored.
        unsafe {
            let old = SelectObject(self.dc, font.into());
            let width = text_width(self.dc, text);
            SelectObject(self.dc, old);
            width
        }
    }
}
fn text_width(dc: HDC, text: &str) -> i32 {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut size = windows::Win32::Foundation::SIZE::default();
    // SAFETY: writable size and initialized UTF-16 slice for this live DC.
    let _ = unsafe { GetTextExtentPoint32W(dc, &units, &mut size) };
    size.cx
}

fn draw_text(dc: HDC, mut rect: RECT, text: &str, color: COLORREF, format: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: live DC and valid text and rectangle. DT_MODIFYSTRING is never set.
    unsafe {
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, color);
        DrawTextW(dc, &mut units, &mut rect, format);
    }
}

fn with_paint(hwnd: HWND, draw: impl FnOnce(HDC)) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: initialized paint storage for this same-thread HWND's WM_PAINT.
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    if !dc.is_invalid() {
        draw(dc);
    }
    // SAFETY: match the BeginPaint call using the same structure and HWND.
    let _ = unsafe { EndPaint(hwnd, &paint) };
}

impl Window<'_> {
    fn paint(&self, target: HDC) {
        let mut client = RECT::default();
        let mut dirty = RECT::default();
        // SAFETY: the DC belongs to WM_PAINT or PrintWindow; both rectangles are writable.
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut client);
            GetClipBox(target, &mut dirty);
        }
        let dirty = RECT {
            left: dirty.left.max(client.left),
            top: dirty.top.max(client.top),
            right: dirty.right.min(client.right),
            bottom: dirty.bottom.min(client.bottom),
        };
        if dirty.left >= dirty.right || dirty.top >= dirty.bottom {
            return;
        }
        let frame = self.frame.get();
        let hide_accel = self.ui_state() & UISF_HIDEACCEL != 0;
        let dpi = self.dpi.get();
        let width = client.right as u32;
        let height = client.bottom.max(scale(HEIGHT + STATUS_HEIGHT + 30, dpi)) as u32;
        let mut storage = self.surface.borrow_mut();
        if storage.as_ref().is_none_or(|surface| surface.width != width || surface.height != height)
        {
            *storage = Surface::new(width, height).ok();
        }
        let Some(surface) = storage.as_mut() else { return };
        let palette = Palette::at(frame.theme);
        let mut canvas = surface.canvas(dirty);
        canvas.shape(dirty, 0.0, palette.paper);
        self.paint_content(&mut canvas, frame, palette, hide_accel);
        let Canvas { labels, .. } = canvas;
        // SAFETY: the selected DIB is live, and all font selections are restored after drawing.
        unsafe {
            let level = SaveDC(surface.dc);
            if level > 0 {
                IntersectClipRect(surface.dc, dirty.left, dirty.top, dirty.right, dirty.bottom);
                for label in labels {
                    SelectObject(surface.dc, label.font.into());
                    draw_text(surface.dc, label.rect, &label.text, label.ink, label.format);
                }
                let _ = RestoreDC(surface.dc, level);
            }
            let _ = BitBlt(
                target,
                dirty.left,
                dirty.top,
                dirty.right - dirty.left,
                dirty.bottom - dirty.top,
                Some(surface.dc),
                dirty.left,
                dirty.top,
                SRCCOPY,
            );
        }
    }

    /// The page switch: the card-size segment's look and its sliding highlight.
    fn paint_pages(&self, canvas: &mut Canvas<'_>, frame: Frame, p: Palette) {
        let dpi = self.dpi.get();
        let px = |size| scale(size, dpi);
        let fade = |ink| mix(p.paper, ink, frame.intro);
        let rect = |id: Control| scaled(Item::lookup(id as i32).expect("page tab").rect, dpi);
        let (general, shortcuts) = (rect(GeneralTab), rect(ShortcutsTab));
        let fill = mix(p.patch, p.chip, 0.55);
        canvas.shape(inset(RECT { right: shortcuts.right, ..general }, 1), px(6) as f32, fade(fill));
        let along = |from: i32, to: i32| from + (f64::from(to - from) * frame.page).round() as i32;
        let highlight = RECT {
            left: along(general.left, shortcuts.left),
            right: along(general.right, shortcuts.right),
            ..general
        };
        canvas.shape(inset(highlight, px(3)), px(4) as f32, fade(p.selected));
        let font = self.fonts.borrow().0[1];
        for (index, id) in [GeneralTab, ShortcutsTab].into_iter().enumerate() {
            let bounds = rect(id);
            let shape = inset(bounds, px(3));
            let focus = frame.buttons[id.index()].focus;
            // The segment is drawn above, so only the ring and its gap are needed here.
            paint_focus(canvas, shape, [px(4), px(1), px(2)], fade(fill), fade(p.accent), focus);
            if focus > 0.0 {
                let current = frame.page.round() as usize == index;
                canvas.shape(shape, px(4) as f32, fade(if current { p.selected } else { fill }));
            }
            let text = Item::lookup(id as i32).expect("page tab").text;
            canvas.text(bounds, text, fade(p.text), DT_CENTER | DT_VCENTER | DT_SINGLELINE, font);
        }
    }

    fn paint_shortcuts(&self, canvas: &mut Canvas<'_>, frame: Frame, p: Palette) {
        let dpi = self.dpi.get();
        let px = |size| scale(size, dpi);
        let fade = |ink| mix(p.paper, ink, frame.intro);
        let fonts = self.fonts.borrow().0;
        let center = DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
        let line = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX;
        let recorder = self.recorder.borrow();
        for &action in Action::ALL {
            let index = action as usize;
            if !canvas.visible(scaled(row_rect(index), dpi)) {
                continue;
            }
            let tracks = frame.rows[index];
            let view = &recorder.rows[index];
            let available = action.available();
            let recording = recorder.capture.as_ref().is_some_and(|c| c.action == action);
            let part = |part| {
                let item = Item::lookup(Control::shortcut(action, part) as i32).expect("row part");
                scaled(item.rect, dpi)
            };
            let title = if available { p.text } else { p.muted };
            canvas.text(part(ShortcutPart::Label), action.label(), fade(title), line, fonts[0]);

            // The recorder: a field of key chips. It waits with an accent outline that pulses.
            let id = Control::shortcut(action, ShortcutPart::Recorder);
            let state = frame.buttons[id.index()];
            let shape = inset(part(ShortcutPart::Recorder), px(3));
            paint_focus(canvas, shape, [px(5), px(1), px(2)], fade(p.paper), fade(p.accent), state.focus);
            let face = if available {
                mix(mix(p.paper, p.selected, state.hover * 0.6), p.outline, state.press * 0.27)
            } else {
                p.paper
            };
            let (edge, width) = if recording {
                (mix(p.on, p.accent, 0.4 + 0.6 * frame.pulse), px(2))
            } else if available {
                (p.outline, 1)
            } else {
                (p.disabled, 1)
            };
            canvas.shape(shape, px(5) as f32, fade(edge));
            canvas.shape(inset(shape, width), (px(5) - width) as f32, fade(face));
            let inner = inset(shape, px(2));
            let amount = tracks[CHIPS];
            if view.chips.is_empty() {
                let text = if recording { "Press keys" } else { "None" };
                let ink = if available { p.muted } else { mix(p.patch, p.muted, 0.72) };
                let bounds = RECT { left: inner.left + px(7), ..inner };
                canvas.text(bounds, text, fade(mix(face, ink, amount)), line, fonts[1]);
            } else {
                let (chip, ink) = if available {
                    (p.chip, p.text)
                } else {
                    (p.patch, mix(p.patch, p.muted, 0.72))
                };
                let top = inner.top + (inner.bottom - inner.top - px(20)) / 2;
                let mut left = inner.left + px(4);
                for (n, text) in view.chips.iter().enumerate() {
                    let shown = if n < view.kept { 1.0 } else { amount };
                    let width = (canvas.width(text, fonts[2]) + px(10)).max(px(20));
                    if left + width > inner.right - px(3) {
                        let rest = RECT { left, right: inner.right, ..inner };
                        canvas.text(rest, "…", fade(p.muted), line, fonts[2]);
                        break;
                    }
                    // New chips rise into place as they fade in.
                    let rise = px((3.0 * (1.0 - shown)).round() as i32);
                    let bounds = RECT {
                        left,
                        right: left + width,
                        top: top + rise,
                        bottom: top + px(20) + rise,
                    };
                    canvas.shape(bounds, px(4) as f32, fade(mix(face, chip, shown)));
                    canvas.text(bounds, text.as_str(), fade(mix(face, ink, shown)), center, fonts[2]);
                    left += width + px(4);
                }
            }

            let opacity = tracks[RESET];
            if opacity > 0.0 {
                let id = Control::shortcut(action, ShortcutPart::Reset);
                let state = frame.buttons[id.index()];
                let bounds = part(ShortcutPart::Reset);
                let shape = inset(bounds, px(3));
                let color = |ink| fade(mix(p.paper, ink, opacity));
                paint_focus(canvas, shape, [px(4), px(1), px(2)], fade(p.paper), fade(p.accent), state.focus * opacity);
                let face =
                    mix(mix(p.chip, p.selected, state.hover * 0.6), p.outline, state.press * 0.27);
                canvas.outlined(shape, px(4) as f32, color(face), color(p.outline));
                canvas.text(bounds, "Reset", color(p.text), center, fonts[1]);
            }

            let opacity = tracks[NOTE];
            if opacity > 0.0 && !view.note.text.is_empty() {
                let bounds = move_y(part(ShortcutPart::Status), px((3.0 * (1.0 - opacity)).round() as i32));
                let ink = if view.note.tone == Tone::Error { p.error } else { p.muted };
                canvas.text(bounds, view.note.text.as_str(), fade(mix(p.paper, ink, opacity)), line, fonts[1]);
            }
            if index + 1 < ROWS {
                let rule = scaled([24, row_y(index) + 57, 496, 1], dpi);
                canvas.shape(rule, 0.0, fade(mix(p.patch, p.chip, 0.5)));
            }
        }
    }

    fn paint_content(&self, canvas: &mut Canvas<'_>, frame: Frame, p: Palette, hide_accel: bool) {
        self.paint_pages(canvas, frame, p);
        // The shown page fades in and slides in from the side of its tab.
        let direction = if self.page.get() == Page::Shortcuts { 1.0 } else { -1.0 };
        canvas.dx = scale(((1.0 - frame.content) * 16.0 * direction).round() as i32, self.dpi.get());
        let page_frame = Frame { intro: frame.intro * frame.content, ..frame };
        if self.page.get() == Page::Shortcuts {
            self.paint_shortcuts(canvas, page_frame, p);
        } else {
            self.paint_general(canvas, page_frame, p, hide_accel);
        }
        canvas.dx = 0;
    }

    fn paint_general(&self, canvas: &mut Canvas<'_>, frame: Frame, p: Palette, hide_accel: bool) {
        let dpi = self.dpi.get();
        let px = |size| scale(size, dpi);
        let layout = frame.layout();
        let fade = |color| mix(p.paper, color, frame.intro);
        let mut card_patch = CARD_PATCH;
        card_patch[1] += layout.offset(CardTitle);
        let mut keep_patch = KEEP_PATCH;
        keep_patch[1] += layout.offset(KeepTitle);
        for rect in [keep_patch, card_patch] {
            canvas.shape(scaled(rect, dpi), px(8) as f32, fade(p.patch));
        }
        let radio =
            |index| layout.rect(Item::lookup(Small as i32 + index).expect("size choice"), dpi);
        let segment = inset(RECT { right: radio(2).right, ..radio(0) }, 1);
        let segment_fill = mix(p.patch, p.chip, 0.55);
        canvas.shape(segment, px(6) as f32, fade(segment_fill));
        let highlight = RECT {
            left: px(frame.selection[0].round() as i32),
            right: px(frame.selection[1].round() as i32),
            ..radio(0)
        };
        canvas.shape(inset(highlight, px(3)), px(4) as f32, fade(p.selected));

        let fonts = self.fonts.borrow().0;
        for item in ITEMS.iter().filter(|item| !matches!(item.id, GeneralTab | ShortcutsTab)) {
            let mut rect = layout.rect(item, dpi);
            let local = match item.id {
                NotesFallback => frame.folders[0],
                CapturesFallback => frame.folders[1],
                Status => frame.status,
                Retry => frame.retry,
                _ => 1.0,
            };
            if local == 0.0 {
                continue;
            }
            let slide = px((4.0 * (1.0 - local)).round() as i32);
            rect = match item.id {
                NotesFallback | CapturesFallback => {
                    let next = if item.id == NotesFallback { CapturesFolder } else { CardTitle };
                    let next =
                        layout.rect(Item::lookup(next as i32).expect("row after fallback"), dpi);
                    let mut rect = move_y(rect, slide);
                    rect.bottom = rect.bottom.min(next.top - px(2));
                    rect
                }
                _ => move_y(rect, -slide),
            };
            if rect.bottom <= rect.top {
                continue;
            }
            if !intersects(rect, canvas.dirty) {
                continue;
            }
            let font = fonts[match item.kind {
                Kind::Heading | Kind::Title => 0,
                Kind::Keys => 2,
                _ => 1,
            }];
            let center = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
            let state = frame.buttons[item.id.index()];
            // SAFETY: an enabled-state query for a real child of this window.
            let disabled =
                item.button() && !unsafe { IsWindowEnabled(self.child(item.id)) }.as_bool();
            let color = |background, ink| fade(mix(background, ink, local));
            match item.kind {
                Kind::Push(label) => {
                    let face = mix(
                        mix(p.chip, p.selected, state.hover * 0.60),
                        p.outline,
                        state.press * 0.27,
                    );
                    let face = if disabled { p.chip } else { face };
                    let edge = if disabled { p.disabled } else { p.outline };
                    let shape = inset(rect, px(3));
                    paint_focus(
                        canvas,
                        shape,
                        [px(4), px(1), px(2)],
                        fade(p.paper),
                        fade(p.accent),
                        state.focus,
                    );
                    canvas.outlined(
                        shape,
                        px(4) as f32,
                        color(p.paper, face),
                        color(p.paper, edge),
                    );
                    let flags = if hide_accel { center | DT_HIDEPREFIX } else { center };
                    canvas.text(
                        rect,
                        label,
                        color(p.paper, if disabled { p.muted } else { p.text }),
                        flags,
                        font,
                    );
                }
                Kind::Switch => {
                    let top = rect.top + (rect.bottom - rect.top - px(20)) / 2;
                    let rail = RECT {
                        left: rect.right - px(44),
                        right: rect.right - px(4),
                        top,
                        bottom: top + px(20),
                    };
                    let background = if item.id == Keep { p.patch } else { p.paper };
                    let track = if disabled { p.disabled } else { mix(p.off, p.on, state.checked) };
                    paint_focus(
                        canvas,
                        rail,
                        [px(10), px(1), px(2)],
                        fade(background),
                        fade(p.accent),
                        state.focus,
                    );
                    canvas.shape(rail, px(10) as f32, fade(track));
                    let x = f64::from(rail.left + px(11))
                        + state.checked * f64::from(rail.right - rail.left - 2 * px(11));
                    let y = f64::from(rail.top + rail.bottom) / 2.0;
                    let radius =
                        f64::from(dpi) / 96.0 * (6.0 + state.hover * 0.8 + state.press * 1.2);
                    let thumb = RECT {
                        left: (x - radius).round() as i32,
                        right: (x + radius).round() as i32,
                        top: (y - radius).round() as i32,
                        bottom: (y + radius).round() as i32,
                    };
                    let ink = if disabled { mix(track, p.thumb, 0.5) } else { p.thumb };
                    canvas.shape(thumb, radius as f32, fade(ink));
                }
                Kind::Radio => {
                    let shape = inset(rect, px(3));
                    // The segment is drawn below, so only the ring and its gap are needed here.
                    paint_focus(
                        canvas,
                        shape,
                        [px(4), px(1), px(2)],
                        fade(segment_fill),
                        fade(p.accent),
                        state.focus,
                    );
                    if state.focus > 0.0 {
                        canvas.shape(
                            shape,
                            px(4) as f32,
                            fade(if frame.card.round() as i32 == item.id as i32 - Small as i32 {
                                p.selected
                            } else {
                                segment_fill
                            }),
                        );
                    }
                    canvas.text(
                        rect,
                        item.text,
                        fade(if disabled { p.muted } else { p.text }),
                        center,
                        font,
                    );
                }
                Kind::Keys => {
                    let mut x = rect.left;
                    let mut key = |label: &str, chip: bool| {
                        let width = (canvas.width(label, font) + px(10)).max(px(19));
                        let bounds = RECT { left: x, right: x + width, ..rect };
                        if chip {
                            canvas.shape(bounds, px(4) as f32, fade(p.chip));
                        }
                        canvas.text(bounds, label, fade(p.muted), center | DT_NOPREFIX, font);
                        x += width + px(3);
                    };
                    if item.id == RecordingKeys {
                        // The user's own chords: one chip each, unset ones left out.
                        for chord in recording_chords(&self.model.borrow().settings.shortcuts) {
                            key(&display_for_layout(&chord), true);
                        }
                    }
                    for group in item.text.split('|') {
                        for part in key_chips(group) {
                            key(part, !PLAIN_WORDS.contains(&part));
                        }
                    }
                }
                Kind::Preview => {
                    let small = CardSize::Small.thumb_max();
                    let large = CardSize::Large.thumb_max();
                    let factor = 0.65 * f64::from(dpi) / 96.0;
                    let w = ((small.width + (large.width - small.width) * frame.card / 2.0)
                        * factor)
                        .round() as i32;
                    let h = ((small.height + (large.height - small.height) * frame.card / 2.0)
                        * factor)
                        .round() as i32;
                    let left = rect.left + (rect.right - rect.left - w) / 2;
                    let top = rect.top + (rect.bottom - rect.top - h) / 2;
                    let card = RECT { left, top, right: left + w, bottom: top + h };
                    canvas.outlined(card, px(6) as f32, fade(p.paper), fade(p.outline));
                    let image = inset(card, px(5));
                    canvas.shape(image, px(4) as f32, fade(p.selected));
                    let sun = RECT {
                        left: image.right - px(15),
                        right: image.right - px(8),
                        top: image.top + px(6),
                        bottom: image.top + px(13),
                    };
                    canvas.shape(sun, px(4) as f32, fade(p.accent));
                    let line = RECT {
                        left: image.left + px(6),
                        right: image.right - px(6),
                        top: image.bottom - px(7),
                        bottom: image.bottom - px(5),
                    };
                    canvas.shape(line, 0.0, fade(p.outline));
                }
                kind => {
                    let text = match item.id {
                        NotesFallback | CapturesFallback => {
                            "The chosen folder is unavailable. Using the default.".to_owned()
                        }
                        Status => self.error_label.borrow().clone(),
                        _ if item.text.is_empty() => window_text(self.child(item.id)),
                        _ => item.text.to_owned(),
                    };
                    let ink = match kind {
                        Kind::Title => p.text,
                        Kind::Status => p.error,
                        _ => p.muted,
                    };
                    let format = if matches!(kind, Kind::Status) {
                        DT_WORDBREAK | DT_END_ELLIPSIS
                    } else {
                        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS
                    };
                    canvas.text(rect, text, color(p.paper, ink), format | DT_NOPREFIX, font);
                }
            }
        }
    }
}

/// The outer 2 dp ring surrounds a 1 dp gap painted in the surrounding surface color.
fn paint_focus(
    canvas: &mut Canvas<'_>,
    shape: RECT,
    [radius, gap, ring]: [i32; 3],
    back: COLORREF,
    ink: COLORREF,
    opacity: f64,
) {
    if opacity > 0.0 {
        canvas.shape(
            inset(shape, -(gap + ring)),
            (radius + gap + ring) as f32,
            mix(back, ink, opacity),
        );
        canvas.shape(inset(shape, -gap), (radius + gap) as f32, back);
    }
}

fn window_text(hwnd: HWND) -> String {
    // SAFETY: same-thread child HWND. The returned length excludes the terminator.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    let mut text = vec![0u16; len as usize + 1];
    // SAFETY: writable buffer with space for a terminator.
    let len = unsafe { GetWindowTextW(hwnd, &mut text) };
    String::from_utf16_lossy(&text[..len as usize])
}

/// Keep native input, check state, tab order and accessibility. The parent paints all
/// pixels in one pass, including the areas occupied by these real control HWNDs.
unsafe extern "system" fn control_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: the owning STA attached the stable parent pointer; its children die first.
    let window = unsafe { &*(data as *const Window<'_>) };
    match msg {
        WM_PAINT => {
            with_paint(hwnd, |_| {});
            return LRESULT(0);
        }
        WM_PRINT | WM_PRINTCLIENT => return LRESULT(0),
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCDESTROY => {
            // SAFETY: remove this exact subclass on the child's final message.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(control_proc), id) };
        }
        _ => {}
    }
    // SAFETY: querying the ID of our native child uses only its HWND.
    let item = Item::lookup(unsafe { GetDlgCtrlID(hwnd) }).filter(|item| item.button());
    if let Some((action, ShortcutPart::Recorder)) = item.and_then(|item| item.id.shortcut_row()) {
        let recording = window.records(action);
        let key = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP);
        match msg {
            WM_GETDLGCODE if recording => {
                // SAFETY: keep the native dialog flags and ask for every key while recording,
                // Tab, Enter and Esc included.
                let native = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
                return LRESULT(native.0 | DLGC_WANTALLKEYS as isize);
            }
            // Never DefWindowProc while recording: F10 and Alt would open the menu loop and
            // Alt+F4 would close the window.
            _ if key && recording => {
                // An auto-repeat (bit 30) is not a new press: the key that opened the
                // recorder (Enter) may still be held.
                let press = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && lp.0 & (1 << 30) == 0;
                window.recorder_key(wp.0 as u16, press);
                return LRESULT(0);
            }
            _ if key && window.swallows(wp.0 as u16, msg, lp) => return LRESULT(0),
            WM_CHAR | WM_SYSCHAR if recording => return LRESULT(0),
            WM_KILLFOCUS => {
                window.recorder.borrow_mut().swallow = None;
                if recording {
                    window.cancel_capture();
                }
            }
            _ => {}
        }
    }
    if let Some(item) = item {
        if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
            window.change_ui_state(UIS_SET, UISF_HIDEFOCUS);
        }
        if item.pointer_effects()
            && matches!(msg, WM_MOUSEMOVE | WM_MOUSELEAVE)
        {
            window.observe_hover(item, hwnd, msg, lp);
        }
    }
    // SAFETY: forward the original native control message, including WM_GETOBJECT.
    let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    if let Some(item) = item
        && matches!(
            msg,
            BM_SETSTATE
                | WM_ENABLE
                | WM_SETFOCUS
                | WM_KILLFOCUS
                | WM_MOUSEMOVE
                | WM_MOUSELEAVE
                | WM_LBUTTONUP
                | WM_LBUTTONDOWN
                | WM_CAPTURECHANGED
                | WM_CANCELMODE
        )
    {
        window.button_state(item, hwnd);
        if msg == WM_KILLFOCUS {
            window.motion.borrow_mut().buttons[item.id.index()].focus.retarget(0.0, 0.0, 0.0);
        }
        window.update_animation();
        if msg == WM_ENABLE {
            window.invalidate_item(item);
        }
    }
    result
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: this class owns GWLP_USERDATA. Before attachment the value is zero.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Window<'_>;
    // SAFETY: run_window holds the allocation through DestroyWindow, and only the STA reads it.
    if let Some(window) = unsafe { ptr.as_ref() } {
        match msg {
            WM_COMMAND if wp.0 >> 16 == BN_CLICKED as usize => {
                let id = (wp.0 & 0xffff) as i32;
                if id == 2 {
                    window.request_close();
                } else {
                    window.command(id);
                }
                return LRESULT(0);
            }
            PICK if window.options.test.is_none() => {
                window.choose(if wp.0 == ChooseNotes as usize {
                    Folder::Notes
                } else {
                    Folder::Captures
                });
                return LRESULT(0);
            }
            TEST_CHOOSE if window.options.test.is_some() => {
                let next =
                    window.lifetime.test_choice.lock().ok().and_then(|mut choice| choice.take());
                if let Some((folder, path)) = next
                    && window.model.borrow().load_error.is_none()
                {
                    window.set_folder(folder, Some(path));
                }
                return LRESULT(0);
            }
            TEST_SCENE if window.options.test.is_some() => {
                window.test_scene(wp.0);
                return LRESULT(0);
            }
            TEST_DIE if window.options.test.is_some() => {
                window.lifetime.muted.store(true, SeqCst);
                // SAFETY: ends this STA's own message loop; run_window then destroys the window.
                unsafe { PostQuitMessage(0) };
                return LRESULT(0);
            }
            TEST_IDLE if window.options.test.is_some() => {
                let idle = !window.timer.get() && !window.motion.borrow().active(window.now());
                return LRESULT(isize::from(idle));
            }
            OPEN => {
                window.activate();
                return LRESULT(0);
            }
            SHORTCUT_STATUS => {
                window.receive_registration();
                return LRESULT(0);
            }
            HINT_SHOWN => {
                window.count_hints();
                return LRESULT(0);
            }
            WM_INPUTLANGCHANGE => window.refresh_shortcuts(),
            WM_CLOSE => {
                window.request_close();
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                window.lifetime.hwnd.store(WORKER_EXITED, SeqCst);
                // SAFETY: detach before returning to any late messages.
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    PostQuitMessage(0);
                }
            }
            WM_SETTINGCHANGE | WM_THEMECHANGED => {
                let enabled = animation_effects();
                {
                    let mut motion = window.motion.borrow_mut();
                    motion.enabled = enabled;
                    if !enabled {
                        motion.settle();
                    }
                }
                window.set_theme();
            }
            WM_TIMER if wp.0 == TIMER => {
                window.update_animation();
                return LRESULT(0);
            }
            WM_UPDATEUISTATE => {
                // SAFETY: let Win32 update and propagate the native UI cue bits first.
                let result = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
                window.sync_buttons();
                if (wp.0 >> 16) as u32 & UISF_HIDEACCEL != 0 {
                    for item in ITEMS.iter().filter(|item| matches!(item.kind, Kind::Push(_))) {
                        window.invalidate_item(item);
                    }
                }
                return result;
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                window.change_ui_state(UIS_SET, UISF_HIDEFOCUS);
            }
            WM_CTLCOLORBTN | WM_CTLCOLORSTATIC if lp.0 != 0 => {
                // The native button can paint immediately outside WM_PAINT on state changes.
                // SAFETY: WM_CTLCOLOR passes a live child HWND and a borrowed painting DC.
                let child = HWND(lp.0 as *mut c_void);
                // SAFETY: scalar child ID query for the HWND supplied by WM_CTLCOLOR.
                if Item::lookup(unsafe { GetDlgCtrlID(child) }).is_none() {
                    return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
                }
                let mut rect = RECT::default();
                unsafe {
                    let _ = GetClientRect(child, &mut rect);
                    ExcludeClipRect(
                        HDC(wp.0 as *mut c_void),
                        rect.left,
                        rect.top,
                        rect.right,
                        rect.bottom,
                    );
                    return LRESULT(GetStockObject(NULL_BRUSH).0 as isize);
                }
            }
            WM_DPICHANGED if window.options.test.is_none() && lp.0 != 0 => {
                // SAFETY: lParam of WM_DPICHANGED points to the suggested RECT for this call.
                let bounds = unsafe { *(lp.0 as *const RECT) };
                window.relayout((wp.0 & 0xffff) as u32);
                window.adjust_size(Some((bounds.left, bounds.top)));
                window.refresh_folders(false);
                return LRESULT(0);
            }
            WM_DRAWITEM => return LRESULT(1),
            WM_PAINT => {
                with_paint(hwnd, |dc| window.paint(dc));
                return LRESULT(0);
            }
            WM_PRINTCLIENT => {
                window.paint(HDC(wp.0 as *mut c_void));
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_SETFOCUS if !window.picking.get() => {
                // SAFETY: set focus to the first tabbable child on the same thread.
                unsafe {
                    if let Ok(first) = GetNextDlgTabItem(hwnd, None, false) {
                        let _ = SetFocus(Some(first));
                    }
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }
    // SAFETY: forward the original message to Win32, including WM_GETOBJECT.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_fit_without_overlap_at_supported_dpis() {
        for dpi in [96, 120, 144, 192] {
            for notes in [0, 15] {
                for captures in [0, 15] {
                    let layout = Layout { notes, captures };
                    let general = ITEMS
                        .iter()
                        .filter(|item| match item.id {
                            NotesFallback => notes != 0,
                            CapturesFallback => captures != 0,
                            _ => true,
                        })
                        .collect::<Vec<_>>();
                    let tabs = &ITEMS[..2];
                    let shortcuts = tabs.iter().chain(&SHORTCUT_ITEMS).collect::<Vec<_>>();
                    for page in [general, shortcuts] {
                        for (i, item) in page.iter().enumerate() {
                            let a = layout.rect(item, dpi);
                            assert!(
                                a.left >= 0
                                    && a.top >= 0
                                    && a.right <= scale(WIDTH, dpi)
                                    && a.bottom <= scale(HEIGHT, dpi),
                                "{dpi}: {:?}",
                                item.id
                            );
                            for other in &page[..i] {
                                assert!(
                                    !intersects(a, layout.rect(other, dpi)),
                                    "{dpi}: {:?} overlaps {:?}",
                                    item.id,
                                    other.id
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_fixed_window_fits_small_and_scaled_screens() {
        // The work area is the screen less a 48 px (logical) taskbar.
        for (width, height, dpi) in [(1366, 768, 96), (1920, 1080, 144)] {
            let taskbar = scale(48, dpi);
            let (w, h) = test_window_size(dpi);
            println!("{width}x{height} at {dpi} dpi: window {w} x {h}, work area {width} x {}", height - taskbar);
            assert!(w <= width && h <= height - taskbar, "{w} x {h} at {dpi} dpi");
        }
    }

    #[test]
    fn shortcut_ids_follow_their_rows() {
        for &action in Action::ALL {
            for part in [ShortcutPart::Label, ShortcutPart::Recorder, ShortcutPart::Reset, ShortcutPart::Status] {
                assert_eq!(Control::shortcut(action, part).shortcut_row(), Some((action, part)));
            }
        }
        assert_eq!(Retry.shortcut_row(), None);
        assert_eq!(ShortcutsTab.shortcut_row(), None);
    }

    #[test]
    fn warnings_name_layout_characters_menus_and_common_app_keys() {
        let hint = |text: &str| shortcut_warning(&Chord::parse(text).unwrap());
        let character = Some("On some keyboard layouts this combination types a character.");
        let menu = Some("This may override menu shortcuts in other apps.");
        let common = Some("Other apps will no longer receive this shortcut.");
        for text in ["Ctrl+Alt+Q", "Ctrl+Alt+7", "Ctrl+Alt+Shift+E", "Ctrl+Alt+0"] {
            assert_eq!(hint(text), character, "{text}");
        }
        for text in ["Alt+K", "Alt+A"] {
            assert_eq!(hint(text), menu, "{text}");
        }
        for key in "CXZYASFPNOWT".chars() {
            assert_eq!(hint(&format!("Ctrl+{key}")), common, "Ctrl+{key}");
        }
        for text in [
            "Ctrl+Alt+F9",
            "Ctrl+Shift+S",
            "Alt+Shift+K",
            "Alt+5",
            "Ctrl+B",
            "Ctrl+Alt+Win+K",
            "Ctrl+Alt+[",
            "F9",
        ] {
            assert_eq!(hint(text), None, "{text}");
        }
    }

    #[test]
    fn ease_out_is_monotonic_with_exact_endpoints() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(0.125), 0.5);
        for i in 0..100 {
            assert!(ease(f64::from(i) / 100.0) < ease(f64::from(i + 1) / 100.0));
        }
    }

    #[test]
    fn retargeting_preserves_the_in_flight_value() {
        let mut track = Transition::default();
        track.retarget(1.0, 10.0, 160.0);
        assert_eq!(track.value(0.0), 0.0);
        assert_eq!(track.value(30.0), 0.5);
        track.retarget(0.0, 30.0, 160.0);
        assert_eq!(track.value(30.0), 0.5);
        assert_eq!(track.value(50.0), 0.25);
        assert_eq!(track.value(190.0), 0.0);
    }

    #[test]
    fn clock_runs_until_the_last_transition_and_stops_in_reduced_motion() {
        let mut motion = Motion::new(true);
        let duration = motion.duration(OPEN_MS);
        motion.intro.retarget(1.0, 0.0, duration);
        motion.select_card(2, 0.0, CARD_MS);
        assert!(motion.active(OPEN_MS));
        assert!(!motion.active(CARD_MS));
        motion.enabled = false;
        motion.settle();
        let duration = motion.duration(SWITCH_MS);
        motion.buttons[Keep.index()].checked.retarget(1.0, 0.0, duration);
        assert!(!motion.active(0.0));
        assert_eq!(motion.frame(0.0).intro, 1.0);
        assert_eq!(motion.frame(0.0).card, 2.0);
        assert_eq!(motion.frame(0.0).buttons[Keep.index()].checked, 1.0);
    }

    #[test]
    fn path_shortening_keeps_both_ends_and_obeys_the_measured_width() {
        let fits = |limit| move |label: &str| label.chars().count() <= limit;
        assert_eq!(shorten(r"C:\Notes", fits(9)), r"C:\Notes");
        assert_eq!(shorten(r"C:\📁\Very long folder\Notes", fits(12)), r"C:\📁\V…Notes");
        assert_eq!(shorten("12345", fits(1)), "…");
        assert_eq!(shorten("12345", fits(0)), "");
        assert_eq!(shorten("wide-tail", |s| !s.contains('w') || s == "w…l"), "w…l");
    }

    #[test]
    fn key_chips_split_modifiers_and_trim_empty_parts() {
        assert_eq!(key_chips(" Win + Shift++ S ").collect::<Vec<_>>(), ["Win", "Shift", "S"]);
        assert_eq!(key_chips("PrtScn").collect::<Vec<_>>(), ["PrtScn"]);
        assert_eq!(key_chips(" + ").count(), 0);
    }

    fn luminance(color: COLORREF) -> f64 {
        [0, 8, 16]
            .into_iter()
            .zip([0.2126, 0.7152, 0.0722])
            .map(|(shift, weight)| {
                let s = f64::from((color.0 >> shift) & 255) / 255.0;
                weight * if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
            })
            .sum()
    }

    #[test]
    fn both_palettes_meet_wcag_contrast_thresholds() {
        for (name, p) in [("light", LIGHT), ("dark", DARK)] {
            for (label, foreground, background, minimum) in [
                ("body on window", p.text, p.paper, 4.5),
                ("body on patch", p.text, p.patch, 4.5),
                ("detail on window", p.muted, p.paper, 4.5),
                ("detail on patch", p.muted, p.patch, 4.5),
                ("heading", p.muted, p.paper, 4.5),
                ("key chips", p.muted, p.chip, 4.5),
                ("buttons", p.text, p.chip, 4.5),
                ("selected text", p.text, p.selected, 4.5),
                ("error text", p.error, p.paper, 4.5),
                ("outline against window", p.outline, p.paper, 3.0),
                ("outline against patch", p.outline, p.patch, 3.0),
                ("accent against patch", p.accent, p.patch, 3.0),
                ("on switch against patch", p.on, p.patch, 3.0),
                ("thumb against on switch", p.thumb, p.on, 3.0),
                ("thumb against off switch", p.thumb, p.off, 3.0),
                ("focus against window", p.accent, p.paper, 3.0),
                ("focus against button", p.accent, p.chip, 3.0),
                ("focus against selection", p.accent, p.selected, 3.0),
                ("focus against on switch", p.accent, p.on, 3.0),
                ("focus against off switch", p.accent, p.off, 3.0),
            ] {
                let a = luminance(foreground);
                let b = luminance(background);
                let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                println!("{name} {label} {ratio:.2}");
                assert!(ratio >= minimum, "{name} {label}: {ratio:.2}");
            }
        }
    }

    #[derive(Default)]
    struct FakeRun(RefCell<Option<Vec<u16>>>);

    impl RunValue for FakeRun {
        fn exists(&self) -> io::Result<bool> {
            Ok(self.0.borrow().is_some())
        }

        fn write(&self, command: Option<&[u16]>) -> io::Result<()> {
            self.0.replace(command.map(<[u16]>::to_vec));
            Ok(())
        }
    }

    #[test]
    fn launch_at_login_mirrors_the_registry_and_quotes_the_executable() {
        let run = FakeRun::default();
        let path = Path::new(r"C:\Program Files\Igorio\igorio.exe");
        assert!(!run.exists().unwrap());
        assert!(set_launch(&run, path, true).unwrap());
        assert_eq!(*run.0.borrow(), Some(wide(r#""C:\Program Files\Igorio\igorio.exe""#)));
        assert!(!set_launch(&run, path, false).unwrap());
        assert_eq!(*run.0.borrow(), None);
    }

    #[test]
    fn saved_snapshot_maps_detector_size_and_worker_settings() {
        let settings = Settings::default();
        assert!(LiveSettings::from(settings.clone()).screenshot_detector);
        let mut settings = Settings {
            keep_every_screenshot: true,
            notes_folder: Some("note path".into()),
            captures_folder: Some("capture path".into()),
            card_size: CardSize::Large,
            ..settings
        };
        settings.set_enabled(SCREENSHOTS, false);
        let live = LiveSettings::from(settings.clone());
        assert!(!live.screenshot_detector);
        assert_eq!(live.card_size, CardSize::Large);
        assert_eq!(live.snapshot, settings);
        // Notes are on unless switched off, like every capability.
        assert!(live.notes);
        settings.set_enabled(NOTES, false);
        assert!(!LiveSettings::from(settings).notes);
    }
}
