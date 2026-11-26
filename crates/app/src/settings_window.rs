//! Native settings UI. A fresh STA owns each window, its controls, and the settings store.
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

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
    EnableWindow, GetFocus, IsWindowEnabled, SetFocus, VK_RETURN,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    IShellItem, RemoveWindowSubclass, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, HRESULT, PCWSTR, Result, w};

use crate::layout::CardSize;
use crate::overlay::{cursor_pos, monitor_at};
use crate::settings::{DefaultFolders, Settings};

const SCREENSHOTS: &str = "fairy.screenshots";
const CLASS: PCWSTR = w!("IgorioSettings");
const OPEN: u32 = WM_APP + 1;
const PICK: u32 = WM_APP + 2;
const WORKER_EXITED: isize = -1;
const STYLE: WINDOW_STYLE =
    WINDOW_STYLE(WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_CLIPCHILDREN.0);
const EX_STYLE: WINDOW_EX_STYLE = WS_EX_APPWINDOW;

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

/// Everything the overlay and workers need from a saved settings snapshot.
pub struct LiveSettings {
    pub screenshot_detector: bool,
    pub card_size: CardSize,
    pub snapshot: Settings,
}

impl From<Settings> for LiveSettings {
    fn from(snapshot: Settings) -> Self {
        Self {
            screenshot_detector: snapshot.is_enabled(SCREENSHOTS),
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
    saved: Sender<Settings>,
    test_choice: Mutex<Option<(Folder, PathBuf)>>,
}

impl Lifetime {
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
pub struct SettingsWindow {
    options: Options,
    wake: Arc<Wake>,
    saved: Sender<Settings>,
    snapshots: Receiver<Settings>,
    worker: Option<(Arc<Lifetime>, JoinHandle<()>)>,
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
        let (saved, snapshots) = mpsc::channel();
        Ok(Self { options, wake, saved, snapshots, worker: None })
    }

    pub fn wake_handle(&self) -> HANDLE {
        self.wake.0
    }

    pub fn saved(&self) -> impl Iterator<Item = Settings> + '_ {
        self.snapshots.try_iter()
    }

    pub fn open(&mut self) -> io::Result<()> {
        if let Some((life, thread)) = &self.worker
            && !thread.is_finished()
            && life.hwnd.load(SeqCst) != WORKER_EXITED
        {
            life.post(OPEN);
            return Ok(());
        }
        let life = Arc::new(Lifetime {
            hwnd: AtomicIsize::new(0),
            stop: AtomicBool::new(false),
            wake: Arc::clone(&self.wake),
            saved: self.saved.clone(),
            test_choice: Mutex::new(None),
        });
        let options = self.options.clone();
        let worker_life = Arc::clone(&life);
        let thread = thread::Builder::new().name("settings".into()).spawn(move || {
            if let Err(error) = run_window(&options, &worker_life) {
                eprintln!("could not open settings: {error}");
            }
            // run_window has destroyed the HWND and released the STA.
            worker_life.hwnd.store(WORKER_EXITED, SeqCst);
        })?;
        self.worker = Some((life, thread));
        Ok(())
    }

    /// Requests closure, including from within the picker's modal loop, and bounds the
    /// wait on quit even if a filesystem or Shell provider is unresponsive.
    pub fn close(&mut self) -> bool {
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
    accent: rgb(0x215bc1),
    selected: rgb(0xdce9fc),
    on: token(theme::TEXT),
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
    accent: rgb(0x91baff),
    selected: rgb(0x344a6b),
    on: rgb(0xb1ceff),
    thumb: rgb(0x192435),
    disabled: rgb(0x525d70),
    error: rgb(0xffaaa4),
};

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
}

struct Item {
    id: Control,
    kind: Kind,
    text: &'static str,
    rect: [i32; 4],
}

const fn item(id: Control, kind: Kind, text: &'static str, rect: [i32; 4]) -> Item {
    Item { id, kind, text, rect }
}

const WIDTH: i32 = 512;
const HEIGHT: i32 = 590;
const STATUS_HEIGHT: i32 = 53;
const KEEP_PATCH: [i32; 4] = [24, 116, 464, 36];

const ITEMS: &[Item] = &[
    item(Capabilities, Kind::Heading, "Capabilities", [24, 19, 464, 22]),
    item(ScreenshotsTitle, Kind::Title, "Screenshots", [24, 48, 408, 22]),
    item(Screenshots, Kind::Switch, "Screenshots", [438, 46, 50, 28]),
    item(ScreenshotKeys, Kind::Keys, "Win+Shift+S|or|PrtScn", [24, 73, 408, 20]),
    item(
        ScreenshotDetail,
        Kind::Detail,
        "Use Win+Shift+S or PrtScn and your cursor carries the shot.",
        [24, 93, 464, 17],
    ),
    item(KeepTitle, Kind::Title, "Keep every screenshot", [36, 122, 380, 24]),
    item(Keep, Kind::Switch, "Keep every screenshot", [426, 120, 50, 28]),
    item(NotesTitle, Kind::Title, "Notes", [24, 161, 45, 22]),
    item(NotesKeys, Kind::Keys, "Alt+Click", [75, 162, 335, 20]),
    item(Notes, Kind::Switch, "Notes, not available yet", [438, 158, 50, 28]),
    item(NotesDetail, Kind::Detail, "Not available yet", [24, 185, 400, 18]),
    item(RecordingTitle, Kind::Title, "Screen recording", [24, 214, 119, 22]),
    item(RecordingKeys, Kind::Keys, "Ctrl+Shift+6|Ctrl+Shift+7|Ctrl+Shift+8", [149, 215, 283, 20]),
    item(Recording, Kind::Switch, "Screen recording, not available yet", [438, 211, 50, 28]),
    item(RecordingDetail, Kind::Detail, "Not available yet", [24, 238, 400, 18]),
    item(General, Kind::Heading, "General", [24, 270, 464, 22]),
    item(NotesFolder, Kind::Title, "Notes folder", [24, 299, 464, 20]),
    item(ChooseNotes, Kind::Push("&Choose…"), "&Choose notes folder…", [24, 322, 77, 24]),
    item(OpenNotes, Kind::Push("Open"), "Open notes folder", [107, 322, 55, 24]),
    item(ResetNotes, Kind::Push("Reset"), "Reset notes folder", [168, 322, 58, 24]),
    item(NotesPath, Kind::Path, "", [236, 322, 252, 24]),
    item(NotesFallback, Kind::Detail, "", [24, 347, 464, 17]),
    item(CapturesFolder, Kind::Title, "Captures folder", [24, 367, 464, 20]),
    item(ChooseCaptures, Kind::Push("Ch&oose…"), "Ch&oose captures folder…", [24, 390, 77, 24]),
    item(OpenCaptures, Kind::Push("Open"), "Open captures folder", [107, 390, 55, 24]),
    item(ResetCaptures, Kind::Push("Reset"), "Reset captures folder", [168, 390, 58, 24]),
    item(CapturesPath, Kind::Path, "", [236, 390, 252, 24]),
    item(CapturesFallback, Kind::Detail, "", [24, 415, 464, 17]),
    item(CardTitle, Kind::Title, "Card size", [36, 448, 331, 20]),
    item(Small, Kind::Radio, "Small", [36, 471, 66, 26]),
    item(Medium, Kind::Radio, "Medium", [108, 471, 75, 26]),
    item(Large, Kind::Radio, "Large", [189, 471, 65, 26]),
    item(Preview, Kind::Preview, "Card size preview", [380, 448, 96, 68]),
    item(CardDimensions, Kind::Detail, "", [36, 500, 328, 20]),
    item(LaunchTitle, Kind::Title, "Launch at login", [24, 542, 404, 24]),
    item(Launch, Kind::Switch, "Launch at login", [438, 540, 50, 28]),
    item(Status, Kind::Status, "", [24, 583, 382, 44]),
    item(Retry, Kind::Push("&Retry"), "&Retry saving settings", [418, 588, 70, 29]),
];

// Keep a fixed client size. Reserve a line under each folder for fallback feedback.
const CARD_PATCH: [i32; 4] = [24, 437, 464, 91];

fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

#[doc(hidden)]
pub fn test_client_size(dpi: u32) -> (i32, i32) {
    (scale(WIDTH, dpi), scale(HEIGHT, dpi))
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
        matches!(self.kind, Kind::Switch | Kind::Push(_) | Kind::Radio)
    }

    fn caption(&self) -> &'static str {
        match self.id {
            ScreenshotKeys => "Windows plus Shift plus S, or Print Screen",
            NotesKeys => "Alt plus click",
            RecordingKeys => "Control plus Shift plus 6, 7, or 8",
            _ => self.text,
        }
    }

    fn style(&self) -> WINDOW_STYLE {
        let style = match self.kind {
            Kind::Switch => BS_CHECKBOX as u32 | WS_TABSTOP.0,
            Kind::Radio if self.id == Small => {
                BS_AUTORADIOBUTTON as u32 | WS_TABSTOP.0 | WS_GROUP.0
            }
            Kind::Radio => BS_AUTORADIOBUTTON as u32,
            Kind::Push(_) => BS_OWNERDRAW as u32 | WS_TABSTOP.0 | WS_GROUP.0,
            Kind::Path => SS_OWNERDRAW.0 | SS_NOTIFY.0 | WS_GROUP.0,
            _ => SS_OWNERDRAW.0 | WS_GROUP.0,
        };
        let visible = if matches!(self.id, Status | Retry) { 0 } else { WS_VISIBLE.0 };
        WINDOW_STYLE(style | WS_CHILD.0 | visible)
    }

    fn lookup(id: i32) -> Option<&'static Self> {
        ITEMS.iter().find(|item| item.id as i32 == id)
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
    dark: Cell<bool>,
    large_client: Cell<bool>,
    fonts: RefCell<Fonts>,
    tooltip: Cell<HWND>,
    tips: RefCell<[Vec<u16>; 2]>,
    icons: [HICON; 2],
}

impl Window<'_> {
    fn child(&self, id: Control) -> HWND {
        // SAFETY: this is our parent HWND, and a partially initialized child may be absent.
        unsafe { GetDlgItem(Some(self.hwnd), id as i32) }.unwrap_or_default()
    }

    fn palette(&self) -> Palette {
        if self.dark.get() { DARK } else { LIGHT }
    }

    fn set_theme(&self) {
        let dark = self.options.test.as_ref().map_or_else(system_dark, |test| test.dark);
        self.dark.set(dark);
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
            let palette = self.palette();
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
            let _ = RedrawWindow(Some(self.hwnd), None, None, RDW_INVALIDATE | RDW_ALLCHILDREN);
        }
    }

    fn init_children(&self, instance: HINSTANCE) -> Result<()> {
        for item in ITEMS {
            let caption = wide(item.caption());
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
            if matches!(item.kind, Kind::Switch | Kind::Radio) {
                // SAFETY: same-thread subclass of our child. The parent state outlives the child.
                if !unsafe {
                    SetWindowSubclass(child, Some(check_proc), 1, self as *const _ as usize)
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
        for item in ITEMS {
            let child = self.child(item.id);
            let rect = scaled(item.rect, dpi);
            // SAFETY: all children are owned by this thread, and the arguments are scalar.
            unsafe {
                let _ = SetWindowPos(
                    child,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                let font = self.fonts.borrow().0
                    [usize::from(!matches!(item.kind, Kind::Title | Kind::Heading))];
                SendMessageW(child, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            }
        }
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
        let dpi = self.dpi.get();
        let height = HEIGHT + if self.large_client.get() { STATUS_HEIGHT } else { 0 };
        let mut bounds = scaled([0, 0, WIDTH, height], dpi);
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
        ) = {
            let model = self.model.borrow();
            let settings = &model.settings;
            let dimensions = settings.card_size.thumb_max();
            (
                [
                    (Screenshots, settings.is_enabled(SCREENSHOTS)),
                    (Keep, settings.keep_every_screenshot),
                    (Notes, false),
                    (Recording, false),
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
            )
        };
        for (id, checked) in checked {
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
        for item in ITEMS.iter().filter(|item| item.button()) {
            let enabled = loaded
                && match item.id {
                    Notes | Recording => false,
                    Launch => launch_available,
                    ResetNotes => reset_notes,
                    ResetCaptures => reset_captures,
                    Retry => retry,
                    _ => true,
                };
            // SAFETY: only same-thread settings child windows are enabled or disabled here.
            let _ = unsafe { EnableWindow(self.child(item.id), enabled) };
        }
        self.text(CardDimensions, &dimensions);
        self.text(Status, &status);
        let show_status = !status.is_empty();
        if self.large_client.replace(show_status) != show_status {
            self.adjust_size(None);
        }
        // SAFETY: owned HWNDs, show or hide their status controls, then invalidate the content.
        unsafe {
            let _ = ShowWindow(
                self.child(Status),
                if show_status { SW_SHOWNOACTIVATE } else { SW_HIDE },
            );
            let _ = ShowWindow(self.child(Retry), if retry { SW_SHOWNOACTIVATE } else { SW_HIDE });
            let _ = RedrawWindow(Some(self.hwnd), None, None, RDW_INVALIDATE | RDW_ALLCHILDREN);
        }
    }

    fn refresh_folders(&self, init: bool) {
        for (index, (id, fallback)) in
            [(NotesPath, NotesFallback), (CapturesPath, CapturesFallback)].into_iter().enumerate()
        {
            let (path, used_default) = self.model.borrow().folders[index].clone();
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
    }

    fn save(&self) {
        let settings = self.model.borrow().settings.clone();
        let result = settings.save(&self.options.path);
        self.model.borrow_mut().save_error =
            result.as_ref().err().map(|err| format!("Settings could not be saved. {err}"));
        if result.is_ok() && self.lifetime.saved.send(settings).is_ok() {
            self.lifetime.signal();
        }
        self.refresh();
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
        dark: Cell::new(false),
        large_client: Cell::new(false),
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
        window.set_theme();
        window.relayout(dpi);
        window.refresh_folders(true);
        window.refresh();
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
        if !lifetime.stop.load(SeqCst) {
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

fn fill(dc: HDC, rect: RECT, color: COLORREF) {
    // SAFETY: a live paint DC and RECT. The brush is only borrowed, then deleted.
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

fn rounded(dc: HDC, rect: RECT, diameter: i32, color: COLORREF, edge: COLORREF) {
    // SAFETY: use two owned GDI objects in a live DC, restoring both selections before deletion.
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, edge);
        let brush = CreateSolidBrush(color);
        let old_pen = SelectObject(dc, pen.into());
        let old_brush = SelectObject(dc, brush.into());
        let _ = RoundRect(dc, rect.left, rect.top, rect.right, rect.bottom, diameter, diameter);
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        let _ = DeleteObject(brush.into());
        let _ = DeleteObject(pen.into());
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
    fn paint_background(&self, dc: HDC) {
        let mut area = RECT::default();
        // SAFETY: writable output and the live parent HWND.
        let _ = unsafe { GetClientRect(self.hwnd, &mut area) };
        let palette = self.palette();
        let dpi = self.dpi.get();
        fill(dc, area, palette.paper);
        for patch in [KEEP_PATCH, CARD_PATCH] {
            rounded(dc, scaled(patch, dpi), scale(16, dpi), palette.patch, palette.patch);
        }
    }

    fn paint_item(&self, item: &Item, dc: HDC, rect: RECT, state: ODS_FLAGS) {
        let palette = self.palette();
        let dpi = self.dpi.get();
        let px = |size| scale(size, dpi);
        let back = if matches!(
            item.id,
            Keep | KeepTitle | CardTitle | Small | Medium | Large | Preview | CardDimensions
        ) {
            palette.patch
        } else {
            palette.paper
        };
        fill(dc, rect, back);
        let font = self.fonts.borrow().0[match item.kind {
            Kind::Heading | Kind::Title => 0,
            Kind::Keys => 2,
            _ => 1,
        }];
        // SAFETY: the selected font belongs to this window and the DC is live for the draw.
        let old_font = unsafe { SelectObject(dc, font.into()) };
        let center = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
        let has = |flag: ODS_FLAGS| state.0 & flag.0 != 0;
        let disabled = has(ODS_DISABLED);
        let focus = has(ODS_FOCUS) && !has(ODS_NOFOCUSRECT);
        match item.kind {
            Kind::Push(label) => {
                let edge = if disabled {
                    palette.disabled
                } else if focus {
                    palette.accent
                } else {
                    palette.outline
                };
                let face = if has(ODS_SELECTED) { palette.selected } else { palette.chip };
                rounded(dc, rect, px(8), face, edge);
                let flags = if has(ODS_NOACCEL) { center | DT_HIDEPREFIX } else { center };
                draw_text(
                    dc,
                    rect,
                    label,
                    if disabled { palette.muted } else { palette.text },
                    flags,
                );
            }
            Kind::Switch | Kind::Radio => {
                // SAFETY: these native button styles implement BM_GETCHECK.
                let on =
                    unsafe { SendMessageW(self.child(item.id), BM_GETCHECK, None, None) }.0 != 0;
                if matches!(item.kind, Kind::Switch) {
                    let top = rect.top + (rect.bottom - rect.top - px(20)) / 2;
                    let rail = RECT {
                        left: rect.right - px(39),
                        right: rect.right - px(2),
                        top,
                        bottom: top + px(20),
                    };
                    let color = if disabled {
                        palette.disabled
                    } else if on {
                        palette.on
                    } else {
                        palette.outline
                    };
                    rounded(dc, rail, px(20), color, color);
                    let left = if on { rail.right - px(17) } else { rail.left + px(3) };
                    let thumb =
                        RECT { left, right: left + px(14), top: top + px(3), bottom: top + px(17) };
                    rounded(dc, thumb, px(14), palette.thumb, palette.thumb);
                } else {
                    let face = if on { palette.selected } else { back };
                    let edge = if disabled {
                        palette.disabled
                    } else if on {
                        palette.accent
                    } else {
                        palette.outline
                    };
                    rounded(dc, rect, px(8), face, edge);
                    draw_text(dc, rect, item.text, palette.text, center);
                }
            }
            Kind::Keys => {
                let mut x = rect.left;
                let mut key = |label: &str, chip: bool| {
                    let width = (text_width(dc, label) + px(10)).max(px(19));
                    let bounds = RECT { left: x, right: x + width, ..rect };
                    if chip {
                        rounded(dc, bounds, px(7), palette.chip, palette.chip);
                    }
                    draw_text(dc, bounds, label, palette.muted, center | DT_NOPREFIX);
                    x += width + px(3);
                };
                for group in item.text.split('|') {
                    if item.id == ScreenshotKeys || item.id == NotesKeys {
                        for part in key_chips(group) {
                            key(part, part != "or");
                        }
                    } else {
                        key(group, true);
                    }
                }
            }
            Kind::Preview => {
                let size = self.model.borrow().settings.card_size.thumb_max();
                let w = px((size.width * 0.65).round() as i32);
                let h = px((size.height * 0.65).round() as i32);
                let left = rect.left + (rect.right - rect.left - w) / 2;
                let top = rect.top + (rect.bottom - rect.top - h) / 2;
                let card = RECT { left, top, right: left + w, bottom: top + h };
                rounded(dc, card, px(11), palette.paper, palette.outline);
                let image = RECT {
                    left: left + px(5),
                    top: top + px(5),
                    right: card.right - px(5),
                    bottom: card.bottom - px(5),
                };
                rounded(dc, image, px(6), palette.selected, palette.selected);
                let sun = RECT {
                    left: image.right - px(15),
                    right: image.right - px(8),
                    top: image.top + px(6),
                    bottom: image.top + px(13),
                };
                rounded(dc, sun, px(7), palette.accent, palette.accent);
                fill(
                    dc,
                    RECT {
                        left: image.left + px(6),
                        right: image.right - px(6),
                        top: image.bottom - px(7),
                        bottom: image.bottom - px(5),
                    },
                    palette.outline,
                );
            }
            kind => {
                let text = if item.text.is_empty() {
                    window_text(self.child(item.id))
                } else {
                    item.text.to_owned()
                };
                let ink = match kind {
                    Kind::Title => palette.text,
                    Kind::Status => palette.error,
                    _ => palette.muted,
                };
                let format = if matches!(kind, Kind::Status) {
                    DT_WORDBREAK | DT_END_ELLIPSIS
                } else {
                    DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS
                };
                draw_text(dc, rect, &text, ink, format | DT_NOPREFIX);
            }
        }
        if focus {
            let inset = px(3);
            let focus = RECT {
                left: rect.left + inset,
                top: rect.top + inset,
                right: rect.right - inset,
                bottom: rect.bottom - inset,
            };
            // SAFETY: same live DC, and DrawFocusRect only reads the rectangle.
            let _ = unsafe { DrawFocusRect(dc, &focus) };
        }
        // SAFETY: restore the original font selection before the DC is reused.
        unsafe { SelectObject(dc, old_font) };
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

/// Native BUTTON owns input, state, tab order and accessibility. Only replace its painting.
/// This works with the v5 and v6 common controls, including development builds with no manifest.
unsafe extern "system" fn check_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: the owning STA attached the stable parent pointer, and the child dies first.
    let window = unsafe { &*(data as *const Window<'_>) };
    if msg == WM_PAINT || msg == WM_PRINTCLIENT {
        // SAFETY: hwnd is our child, and rect is writable.
        let (control, rect, state) = unsafe {
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let mut state = ODS_FLAGS(0);
            if !IsWindowEnabled(hwnd).as_bool() {
                state.0 |= ODS_DISABLED.0;
            }
            if GetFocus() == hwnd {
                state.0 |= ODS_FOCUS.0;
            }
            (GetDlgCtrlID(hwnd), rect, state)
        };
        if let Some(item) = Item::lookup(control) {
            if msg == WM_PAINT {
                with_paint(hwnd, |dc| window.paint_item(item, dc, rect, state));
            } else {
                window.paint_item(item, HDC(wp.0 as *mut c_void), rect, state);
            }
            return LRESULT(0);
        }
    }
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    if msg == WM_NCDESTROY {
        // SAFETY: remove this exact subclass on the child's final message.
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(check_proc), id) };
    }
    // SAFETY: forward the unchanged message to the native button procedure.
    let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    if matches!(msg, BM_SETCHECK | BM_SETSTATE | WM_ENABLE | WM_SETFOCUS | WM_KILLFOCUS) {
        // SAFETY: redraw the child to reflect native state changes.
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
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
            OPEN => {
                window.activate();
                return LRESULT(0);
            }
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
            WM_SETTINGCHANGE | WM_THEMECHANGED => window.set_theme(),
            WM_DPICHANGED if window.options.test.is_none() && lp.0 != 0 => {
                // SAFETY: lParam of WM_DPICHANGED points to the suggested RECT for this call.
                let bounds = unsafe { *(lp.0 as *const RECT) };
                window.relayout((wp.0 & 0xffff) as u32);
                window.adjust_size(Some((bounds.left, bounds.top)));
                window.refresh_folders(false);
                return LRESULT(0);
            }
            WM_DRAWITEM if lp.0 != 0 => {
                // SAFETY: Win32 supplies a DRAWITEMSTRUCT valid through this call.
                let draw = unsafe { &*(lp.0 as *const DRAWITEMSTRUCT) };
                if let Some(item) = Item::lookup(draw.CtlID as i32) {
                    window.paint_item(item, draw.hDC, draw.rcItem, draw.itemState);
                    return LRESULT(1);
                }
            }
            WM_PAINT => {
                with_paint(hwnd, |dc| window.paint_background(dc));
                return LRESULT(0);
            }
            WM_PRINTCLIENT => {
                window.paint_background(HDC(wp.0 as *mut c_void));
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
            for (i, item) in ITEMS.iter().enumerate() {
                let a = scaled(item.rect, dpi);
                let height =
                    if matches!(item.id, Status | Retry) { HEIGHT + STATUS_HEIGHT } else { HEIGHT };
                assert!(
                    a.left >= 0
                        && a.top >= 0
                        && a.right <= scale(WIDTH, dpi)
                        && a.bottom <= scale(height, dpi),
                    "{dpi}: {:?}",
                    item.id
                );
                for other in &ITEMS[..i] {
                    let b = scaled(other.rect, dpi);
                    assert!(
                        a.right <= b.left
                            || b.right <= a.left
                            || a.bottom <= b.top
                            || b.bottom <= a.top,
                        "{dpi}: {:?} overlaps {:?}",
                        item.id,
                        other.id
                    );
                }
            }
        }
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
                ("thumb against off switch", p.thumb, p.outline, 3.0),
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
    }
}
