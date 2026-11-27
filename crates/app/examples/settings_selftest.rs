//! Exercises only our settings window and scratch files. No foreground activation or synthetic input:
//! key and mouse messages go to our own test window only. With `IGORIO_TEST_MONITOR=secondary` the
//! windows open on a monitor other than the primary one.
#[path = "common/test_monitor.rs"]
mod test_monitor;

use std::cell::{Cell, RefCell};
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use igorio_app::input::Held;
use igorio_app::layout::CardSize;
use igorio_app::notes::summoner::{ToolId, ToolMenu};
use igorio_app::overlay::ensure_dpi_awareness;
use igorio_app::settings::Settings;
use igorio_app::settings_window::{
    Control, Control::*, SettingsWindow, ShortcutPart, ShortcutService, TestMode, TestScene,
    test_client_size, test_window_size,
};
use igorio_app::shortcuts::{
    Action, Chord, Registration, RegistrationReport, Shortcuts, display_for_layout,
};
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HWND, LPARAM, POINT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentProcess,
    GetProcessTimes, SYNCHRONIZATION_SYNCHRONIZE, SetWaitableTimer, TIMER_MODIFY_STATE,
    WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::*;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Checks {
    ok: bool,
}

impl Checks {
    fn check(&mut self, name: &str, pass: bool) {
        self.ok &= pass;
        println!("{} {name}", if pass { "PASS" } else { "FAIL" });
    }
}

fn child(hwnd: HWND, id: igorio_app::settings_window::Control) -> HWND {
    // SAFETY: a same-process window created by the self-test.
    unsafe { GetDlgItem(Some(hwnd), id as i32) }.unwrap_or_default()
}

fn text(hwnd: HWND) -> String {
    let mut units = [0u16; 1024];
    // SAFETY: the same-process window copies its title into the valid output slice.
    let len = unsafe { GetWindowTextW(hwnd, &mut units) };
    String::from_utf16_lossy(&units[..len as usize])
}

fn checked(hwnd: HWND) -> bool {
    // SAFETY: scalar query to a test BUTTON.
    unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None).0 == 1 }
}

fn enabled(hwnd: HWND) -> bool {
    // SAFETY: query a live test HWND.
    unsafe { IsWindowEnabled(hwnd).as_bool() }
}

fn click(hwnd: HWND, id: igorio_app::settings_window::Control) {
    // SAFETY: send the same control notification as a click, only to the window we created.
    unsafe {
        SendMessageW(
            hwnd,
            WM_COMMAND,
            Some(WPARAM(id as usize)),
            Some(LPARAM(child(hwnd, id).0 as isize)),
        );
    }
}

fn open(controller: &mut SettingsWindow) -> Result<HWND> {
    controller.open()?;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if let Some(hwnd) = controller.test_hwnd() {
            // SAFETY: this HWND was published by our test window only after setup.
            if unsafe { IsWindowVisible(hwnd).as_bool() } {
                return Ok(hwnd);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err("settings window did not become visible".into())
}

fn close(controller: &mut SettingsWindow, hwnd: HWND) -> bool {
    let at = Instant::now();
    let closed = controller.close();
    // SAFETY: same-process HWND. This checks that the old window has been destroyed.
    closed && at.elapsed() < Duration::from_secs(1) && !unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

fn disk(controller: &mut SettingsWindow, path: &Path, expected: &Settings) -> bool {
    Settings::load(path).is_ok_and(|settings| &settings == expected)
        && controller.saved(None, &expected.shortcuts) == [expected.clone()]
}

fn tab_order(hwnd: HWND) -> bool {
    // SAFETY: both iterators run only on the test window's child group.
    unsafe {
        let mut current = None;
        for id in [
            GeneralTab,
            Screenshots,
            Keep,
            Carry,
            Notes,
            Recording,
            ChooseNotes,
            OpenNotes,
            ResetNotes,
            ChooseCaptures,
            OpenCaptures,
            ResetCaptures,
            Medium,
            Launch,
        ] {
            let child = child(hwnd, id);
            if GetNextDlgTabItem(hwnd, current, false).ok() != Some(child) {
                return false;
            }
            current = Some(child);
        }
        GetNextDlgTabItem(hwnd, current, false).ok() == Some(child(hwnd, GeneralTab))
            && GetNextDlgGroupItem(hwnd, Some(child(hwnd, Small)), false).ok()
                == Some(child(hwnd, Medium))
            && GetNextDlgGroupItem(hwnd, Some(child(hwnd, Medium)), false).ok()
                == Some(child(hwnd, Large))
            && GetNextDlgGroupItem(hwnd, Some(child(hwnd, Large)), false).ok()
                == Some(child(hwnd, Small))
    }
}

fn lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).share_mode(0).open(path)
}

fn verify_controls(
    checks: &mut Checks,
    controller: &mut SettingsWindow,
    hwnd: HWND,
    path: &Path,
    mode: &TestMode,
) -> Result<()> {
    let mut expected = Settings::load(path)?;
    for (id, name, state, available) in [
        (Screenshots, "Screenshots", expected.is_enabled("igorio.screenshots"), true),
        (Keep, "Keep every screenshot", expected.keep_every_screenshot, true),
        (Carry, "Also carry copied images", expected.carry_copied_images, true),
        (Notes, "Notes", expected.is_enabled("igorio.notes"), true),
        (Recording, "Screen recording", expected.is_enabled("igorio.screen-recording"), true),
        (Small, "Small", false, true),
        (Medium, "Medium", true, true),
        (Large, "Large", false, true),
        (Launch, "Launch at login", false, true),
    ] {
        let control = child(hwnd, id);
        checks.check(
            name,
            text(control) == name && checked(control) == state && enabled(control) == available,
        );
    }
    for (id, label) in [
        (ChooseNotes, "&Choose notes folder…"),
        (OpenNotes, "Open notes folder"),
        (ResetNotes, "Reset notes folder"),
        (ChooseCaptures, "Ch&oose captures folder…"),
        (OpenCaptures, "Open captures folder"),
        (ResetCaptures, "Reset captures folder"),
    ] {
        let control = child(hwnd, id);
        checks.check(label, text(control) == label && enabled(control));
    }
    // The chips' accessible name follows the shortcuts.
    let recording_keys = [Action::ToggleRecording, Action::Pen, Action::Highlighter]
        .iter()
        .filter_map(|&action| {
            let chord = expected.shortcuts.get(action)?;
            Some(format!("{} {}", action.label(), display_for_layout(&chord)))
        })
        .collect::<Vec<_>>()
        .join(", ");
    checks.check(
        "window title and static names are available through Win32",
        text(hwnd) == "Igorio Settings"
            && [
                (Capabilities, "Capabilities"),
                (ScreenshotsTitle, "Screenshots"),
                (ScreenshotKeys, "Windows plus Shift plus S, or Print Screen"),
                (ScreenshotDetail, "Use Win+Shift+S or PrtScn and your cursor carries the shot."),
                (KeepTitle, "Keep every screenshot"),
                (CarryTitle, "Also carry copied images"),
                (CarryDetail, "Not saved by Keep every screenshot."),
                (NotesTitle, "Notes"),
                (NotesKeys, "Hold Alt and click"),
                (NotesDetail, "A sticky note opens where you click, saved as Markdown."),
                (RecordingTitle, "Screen recording"),
                (RecordingKeys, recording_keys.as_str()),
                (
                    RecordingDetail,
                    "Records the screen under the cursor as MP4. Alt + wheel zooms.",
                ),
                (General, "General"),
                (NotesFolder, "Notes folder"),
                (CapturesFolder, "Captures folder"),
                (CardTitle, "Card size"),
                (Preview, "Card size preview"),
                (LaunchTitle, "Launch at login"),
                (Retry, "&Retry saving settings"),
            ]
            .into_iter()
            .all(|(id, label)| text(child(hwnd, id)) == label),
    );
    checks.check(
        "initial folder paths and Medium dimensions",
        text(child(hwnd, NotesPath)).ends_with("Igorio Notes")
            && text(child(hwnd, CapturesPath)).ends_with("Screenshots")
            && text(child(hwnd, CardDimensions)) == "96 × 72 px",
    );
    checks.check(
        "General page: Tab visits each enabled control once and the size group has three choices",
        tab_order(hwnd),
    );
    controller.open()?;
    thread::sleep(Duration::from_millis(40));
    checks.check("second Open retains the existing HWND", controller.test_hwnd() == Some(hwnd));

    for on in [true, false, true] {
        click(hwnd, Screenshots);
        expected.set_enabled("igorio.screenshots", on);
        checks.check(
            &format!("Screenshots {on} saves and publishes"),
            disk(controller, path, &expected) && checked(child(hwnd, Screenshots)) == on,
        );
    }
    click(hwnd, Keep);
    expected.keep_every_screenshot = false;
    checks.check(
        "Keep every screenshot saves and publishes",
        disk(controller, path, &expected) && !checked(child(hwnd, Keep)),
    );
    for on in [true, false] {
        click(hwnd, Carry);
        expected.carry_copied_images = on;
        checks.check(
            &format!("Also carry copied images {on} saves and publishes"),
            disk(controller, path, &expected) && checked(child(hwnd, Carry)) == on,
        );
    }
    for _ in 0..2 {
        let on = !expected.is_enabled("igorio.notes");
        click(hwnd, Notes);
        expected.set_enabled("igorio.notes", on);
        checks.check(
            &format!("Notes {on} saves and publishes"),
            disk(controller, path, &expected) && checked(child(hwnd, Notes)) == on,
        );
    }
    for _ in 0..2 {
        let on = !expected.is_enabled("igorio.screen-recording");
        click(hwnd, Recording);
        expected.set_enabled("igorio.screen-recording", on);
        checks.check(
            &format!("Screen recording {on} saves and publishes"),
            disk(controller, path, &expected) && checked(child(hwnd, Recording)) == on,
        );
    }

    for (id, size, dimensions) in [
        (Large, CardSize::Large, "112 × 84 px"),
        (Small, CardSize::Small, "80 × 60 px"),
        (Medium, CardSize::Medium, "96 × 72 px"),
    ] {
        click(hwnd, id);
        expected.card_size = size;
        checks.check(
            &format!("{size:?} saves, checks one radio and updates dimensions"),
            disk(controller, path, &expected)
                && [Small, Medium, Large]
                    .into_iter()
                    .all(|radio| checked(child(hwnd, radio)) == (radio == id))
                && text(child(hwnd, CardDimensions)) == dimensions,
        );
    }

    for (notes, choose, open, reset, path_id, fallback, default) in [
        (true, ChooseNotes, OpenNotes, ResetNotes, NotesPath, NotesFallback, &mode.notes),
        (
            false,
            ChooseCaptures,
            OpenCaptures,
            ResetCaptures,
            CapturesPath,
            CapturesFallback,
            &mode.captures,
        ),
    ] {
        click(hwnd, choose);
        click(hwnd, open);
        checks.check(
            "Choose and Open test notifications leave disk unchanged",
            Settings::load(path)? == expected,
        );
        let chosen =
            default.with_file_name("A deliberately long user-selected folder").join("Chosen");
        controller.test_choose_folder(notes, &chosen);
        if notes {
            expected.notes_folder = Some(chosen);
        } else {
            expected.captures_folder = Some(chosen);
        }
        let label = text(child(hwnd, path_id));
        checks.check(
            if notes {
                "Choose notes folder saves, resolves and shortens the path"
            } else {
                "Choose captures folder saves, resolves and shortens the path"
            },
            disk(controller, path, &expected) && label.contains('…') && label.ends_with("Chosen"),
        );

        let unavailable = default.with_extension("file");
        fs::write(&unavailable, b"a file, not a folder")?;
        controller.test_choose_folder(notes, &unavailable);
        if notes {
            expected.notes_folder = Some(unavailable);
        } else {
            expected.captures_folder = Some(unavailable);
        }
        checks.check(
            "unusable folder shows default path and fallback note",
            disk(controller, path, &expected)
                && text(child(hwnd, fallback)).contains("Using the default")
                && text(child(hwnd, path_id))
                    .ends_with(default.file_name().unwrap().to_str().unwrap()),
        );

        click(hwnd, reset);
        if notes {
            expected.notes_folder = None;
        } else {
            expected.captures_folder = None;
        }
        checks.check(
            if notes {
                "Reset notes folder saves and hides fallback"
            } else {
                "Reset captures folder saves and hides fallback"
            },
            disk(controller, path, &expected)
                && text(child(hwnd, fallback)).is_empty()
                && !enabled(child(hwnd, reset)),
        );
    }

    for on in [true, false] {
        click(hwnd, Launch);
        expected.launch_at_login = on;
        checks.check(
            &format!("Launch at login {on} writes throwaway Run value and JSON mirror"),
            disk(controller, path, &expected) && checked(child(hwnd, Launch)) == on,
        );
    }

    let locked = lock(path)?;
    click(hwnd, Keep);
    checks.check(
        "failed save retains the choice, shows Retry, and publishes nothing",
        checked(child(hwnd, Keep))
            && text(child(hwnd, Status)).contains("could not be saved")
            && enabled(child(hwnd, Retry))
            && controller.saved(None, &expected.shortcuts).is_empty(),
    );
    drop(locked);
    expected.keep_every_screenshot = true;
    click(hwnd, Retry);
    checks.check(
        "Retry saves retained choice and clears the error",
        disk(controller, path, &expected) && text(child(hwnd, Status)).is_empty(),
    );
    Ok(())
}

/// One call the window made, through `saved`, to the stand-in for the hotkey service.
#[derive(Clone, Debug, PartialEq)]
enum Call {
    Paused(bool),
    Apply(Shortcuts),
    Report(usize),
}

/// Records what the window asks of the hotkey service and answers like it, without
/// registering anything. `taken` is a chord "another program" holds; `hold` keeps reports back.
struct FakeService {
    calls: RefCell<Vec<Call>>,
    applied: RefCell<Shortcuts>,
    paused: Cell<bool>,
    taken: Cell<Option<Chord>>,
    hold: Cell<bool>,
    queue: RefCell<Vec<RegistrationReport>>,
}

impl FakeService {
    fn new(applied: Shortcuts) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            applied: RefCell::new(applied),
            paused: Cell::new(false),
            taken: Cell::new(None),
            hold: Cell::new(false),
            queue: RefCell::new(Vec::new()),
        }
    }

    fn tags(&self) -> Vec<usize> {
        let calls = self.calls.borrow();
        calls.iter().filter_map(|call| if let Call::Report(tag) = call { Some(*tag) } else { None }).filter(|tag| *tag != 0).collect()
    }
}

impl ShortcutService for FakeService {
    fn set_paused(&self, paused: bool) {
        self.paused.set(paused);
        self.calls.borrow_mut().push(Call::Paused(paused));
    }

    fn apply(&self, shortcuts: &Shortcuts) {
        *self.applied.borrow_mut() = shortcuts.clone();
        self.calls.borrow_mut().push(Call::Apply(shortcuts.clone()));
    }

    fn report_after_commands(&self, tag: usize) -> bool {
        self.calls.borrow_mut().push(Call::Report(tag));
        let applied = self.applied.borrow();
        let entries = Action::ALL
            .iter()
            .map(|&action| {
                let status = match applied.get(action) {
                    _ if !action.available() => Registration::Unavailable,
                    None => Registration::NotSet,
                    // No recording runs in this test (`shortcuts::registration_plan`).
                    Some(_) if action.recording_only() => Registration::WhileRecording,
                    Some(chord) if Some(chord) == self.taken.get() => Registration::Taken,
                    Some(_) => Registration::Active,
                };
                (action, status)
            })
            .collect();
        self.queue.borrow_mut().push(RegistrationReport { tag, entries });
        true
    }

    fn reports(&self) -> Vec<RegistrationReport> {
        if self.hold.get() { Vec::new() } else { std::mem::take(&mut *self.queue.borrow_mut()) }
    }
}

/// Plays the UI loop: runs `saved` until `done` holds, for at most two seconds.
struct Loop<'a> {
    service: &'a FakeService,
    current: Shortcuts,
    saved: Vec<Settings>,
}

impl Loop<'_> {
    fn turn(&mut self, controller: &mut SettingsWindow) {
        for settings in controller.saved(Some(self.service), &self.current) {
            self.current = settings.shortcuts.clone();
            self.saved.push(settings);
        }
    }

    fn until(&mut self, controller: &mut SettingsWindow, done: impl Fn(&Self) -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            self.turn(controller);
            if done(self) {
                return true;
            }
            if Instant::now() > until {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Lets queued requests and reports settle for a moment.
    fn idle(&mut self, controller: &mut SettingsWindow) {
        for _ in 0..20 {
            self.turn(controller);
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn row(hwnd: HWND, action: Action, part: ShortcutPart) -> HWND {
    child(hwnd, Control::shortcut(action, part))
}

fn visible(hwnd: HWND) -> bool {
    // SAFETY: query a live test HWND.
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;
const VK_BACK: u16 = 0x08;
const VK_ESCAPE: u16 = 0x1B;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;

/// Sends one key message to a recorder, as Windows would post it, with the modifiers the
/// recorder reads through its test seam.
fn key(controller: &SettingsWindow, target: HWND, vk: u16, held: Held, down: bool, repeat: bool) {
    controller.test_modifiers(held);
    let message = match (down, held.alt) {
        (true, true) => WM_SYSKEYDOWN,
        (true, false) => WM_KEYDOWN,
        (false, true) => WM_SYSKEYUP,
        (false, false) => WM_KEYUP,
    };
    let mut flags = 1isize | if held.alt { 1 << 29 } else { 0 };
    if repeat || !down {
        flags |= 1 << 30;
    }
    if !down {
        flags |= 1 << 31;
    }
    // SAFETY: a key message to a control of our own test window, not synthetic input.
    unsafe { SendMessageW(target, message, Some(WPARAM(vk as usize)), Some(LPARAM(flags))) };
}

/// Presses a chord on a recorder: the modifiers one by one, the key, then all released.
fn press(controller: &SettingsWindow, target: HWND, text: &str) {
    let chord = Chord::parse(text).expect("test chord");
    let mut held = Held::default();
    for (on, vk) in [(chord.ctrl, VK_CONTROL), (chord.alt, VK_MENU), (chord.shift, VK_SHIFT), (chord.win, VK_LWIN)] {
        if on {
            match vk {
                VK_CONTROL => held.ctrl = true,
                VK_MENU => held.alt = true,
                VK_SHIFT => held.shift = true,
                _ => held.win = true,
            }
            key(controller, target, vk, held, true, false);
        }
    }
    key(controller, target, chord.key.vk(), held, true, false);
    key(controller, target, chord.key.vk(), held, false, false);
    key(controller, target, VK_CONTROL, Held::default(), false, false);
}

fn shown(chord: Option<&str>) -> String {
    chord.map_or_else(|| "none".to_owned(), |text| Chord::parse(text).unwrap().display())
}

fn recorder_name(action: Action, chord: Option<&str>) -> String {
    format!("{}, shortcut {}, press to change", action.label(), shown(chord))
}

fn refusal(text: &str, shortcuts: &Shortcuts, action: Action) -> String {
    shortcuts.clone().set(action, Some(Chord::parse(text).unwrap())).unwrap_err().to_string()
}

/// The tab stops of the current page, in order, each once.
fn tab_cycle(hwnd: HWND) -> Vec<HWND> {
    let mut stops = Vec::new();
    let mut current = None;
    // SAFETY: walks the test window's own children.
    while let Ok(next) = unsafe { GetNextDlgTabItem(hwnd, current, false) } {
        if stops.contains(&next) || stops.len() > 100 {
            break;
        }
        stops.push(next);
        current = Some(next);
    }
    stops
}

const PROMPT: &str = "Press the keys · Esc cancels · Backspace clears";

fn verify_pages(checks: &mut Checks, controller: &mut SettingsWindow, hwnd: HWND) {
    let general = [Capabilities, Screenshots, Keep, Carry, ChooseNotes, Small, Medium, Launch, WindowsShortcutNote];
    let shortcut_parts = Action::ALL.iter().flat_map(|&action| {
        [ShortcutPart::Label, ShortcutPart::Recorder, ShortcutPart::Status].map(|part| Control::shortcut(action, part))
    });
    checks.check(
        "General page: its controls show, the Shortcuts and Tool menu pages' are hidden and disabled",
        checked(child(hwnd, GeneralTab))
            && general.iter().all(|&id| visible(child(hwnd, id)))
            && shortcut_parts.clone().chain(TOOL_CONTROLS).all(|id| !visible(child(hwnd, id)) && !enabled(child(hwnd, id))),
    );
    click(hwnd, ShortcutsTab);
    checks.check(
        "Shortcuts page: General's controls are hidden and disabled, the rows show",
        checked(child(hwnd, ShortcutsTab))
            && !checked(child(hwnd, GeneralTab))
            && general.iter().all(|&id| !visible(child(hwnd, id)) && !enabled(child(hwnd, id)))
            && shortcut_parts.clone().all(|id| visible(child(hwnd, id))),
    );
    let expected: Vec<HWND> = [ShortcutsTab]
        .into_iter()
        .chain(Action::ALL.iter().filter(|action| action.available()).map(|&action| Control::shortcut(action, ShortcutPart::Recorder)))
        .map(|id| child(hwnd, id))
        .collect();
    checks.check("Shortcuts page: Tab visits each enabled control once, in row order", tab_cycle(hwnd) == expected);
    checks.check(
        "recording rows are editable (no service here: shortcuts are off, as for every row)",
        [Action::ToggleRecording, Action::Pen, Action::Highlighter].iter().all(|&action| {
            enabled(row(hwnd, action, ShortcutPart::Recorder))
                && text(row(hwnd, action, ShortcutPart::Status)) == "Keyboard shortcuts are off."
        }),
    );
    // Ctrl+Tab goes through the window's own message loop: post it, with Ctrl (and Shift) held
    // per the seam.
    let ctrl_tab = |target: HWND, shift: bool| {
        controller.test_modifiers(Held { ctrl: true, shift, ..Held::default() });
        // SAFETY: a key message posted to our own test window, not synthetic input.
        unsafe {
            let _ = PostMessageW(Some(target), WM_KEYDOWN, WPARAM(VK_TAB as usize), LPARAM(1));
        }
        thread::sleep(Duration::from_millis(100));
        controller.test_modifiers(Held::default());
    };
    let shown = |tab: Control, control: Control| checked(child(hwnd, tab)) && visible(child(hwnd, control));
    ctrl_tab(row(hwnd, Action::RestoreLast, ShortcutPart::Recorder), false);
    let tools = shown(ToolMenuTab, ToolRow0);
    ctrl_tab(child(hwnd, ToolRow0), false);
    let general = shown(GeneralTab, Screenshots);
    ctrl_tab(child(hwnd, Screenshots), true);
    let tools_back = shown(ToolMenuTab, ToolRow0);
    ctrl_tab(child(hwnd, ToolRow0), true);
    checks.check(
        "Ctrl+Tab goes Shortcuts, Tool menu, General; Ctrl+Shift+Tab back to Tool menu and Shortcuts",
        tools && general && tools_back
            && checked(child(hwnd, ShortcutsTab))
            && visible(row(hwnd, Action::RestoreLast, ShortcutPart::Label))
            && !visible(child(hwnd, ToolRow0)),
    );
}

fn verify_shortcuts(checks: &mut Checks, controller: &mut SettingsWindow, hwnd: HWND, path: &Path) -> Result<()> {
    let start = Settings::load(path)?;
    let service = FakeService::new(start.shortcuts.clone());
    let mut ui = Loop { service: &service, current: start.shortcuts.clone(), saved: Vec::new() };
    ui.idle(controller);
    verify_pages(checks, controller, hwnd);
    let recorder = |action| row(hwnd, action, ShortcutPart::Recorder);
    let status = |action| text(row(hwnd, action, ShortcutPart::Status));

    // Each available action: set, saved, shown. The service pauses at the start and
    // resumes at the end.
    let chords = [
        (Action::RestoreLast, "Ctrl+Alt+F9"),
        (Action::DiscardTop, "Ctrl+Shift+F10"),
        (Action::DiscardAll, "Alt+Shift+F11"),
        (Action::OpenSettings, "Ctrl+Alt+Shift+F7"),
    ];
    for (action, chord) in chords {
        service.calls.borrow_mut().clear();
        click(hwnd, Control::shortcut(action, ShortcutPart::Recorder));
        let paused = ui.until(controller, |ui| ui.service.paused.get());
        let prompt = status(action) == PROMPT;
        let saved_before = ui.saved.len();
        press(controller, recorder(action), chord);
        let done = ui.until(controller, |ui| ui.saved.len() > saved_before && !ui.service.paused.get());
        ui.idle(controller);
        let on_disk = Settings::load(path)?.shortcuts.get(action) == Chord::parse(chord).ok();
        let calls = service.calls.borrow().clone();
        let order = calls.first() == Some(&Call::Paused(true))
            && calls.iter().rposition(|call| *call == Call::Paused(false)) > calls.iter().position(|call| *call == Call::Paused(true));
        checks.check(
            &format!("{} records {chord}: paused, saved, shown, resumed", action.label()),
            paused && prompt && done && on_disk && order && !service.paused.get()
                && text(recorder(action)) == recorder_name(action, Some(chord))
                && status(action).is_empty()
                && visible(row(hwnd, action, ShortcutPart::Reset)),
        );
    }

    // Esc cancels; the repeat of Esc that follows does not reach the dialog (it would close it).
    click(hwnd, Control::shortcut(Action::RestoreLast, ShortcutPart::Recorder));
    let paused = ui.until(controller, |ui| ui.service.paused.get());
    // The Enter that opened a recorder can still repeat: a repeat is not a new press.
    key(controller, recorder(Action::RestoreLast), VK_RETURN, Held::default(), true, true);
    let repeat_ignored = status(Action::RestoreLast) == PROMPT;
    thread::sleep(Duration::from_millis(100));
    let pulsing = !controller.test_idle();
    let saved_before = ui.saved.len();
    key(controller, recorder(Action::RestoreLast), VK_ESCAPE, Held::default(), true, false);
    // SAFETY: key messages posted to our own test window, not synthetic input.
    unsafe {
        let target = Some(recorder(Action::RestoreLast));
        let _ = PostMessageW(target, WM_KEYDOWN, WPARAM(VK_ESCAPE as usize), LPARAM(1 | 1 << 30));
        let _ = PostMessageW(target, WM_KEYUP, WPARAM(VK_ESCAPE as usize), LPARAM(1 | 3 << 30));
    }
    let resumed = ui.until(controller, |ui| !ui.service.paused.get());
    ui.idle(controller);
    thread::sleep(Duration::from_millis(300));
    println!("INFO the recorder's pulse ran the animation clock while recording: {pulsing}");
    checks.check(
        "an Enter repeat while recording is ignored; the pulse and timer stop after recording",
        repeat_ignored && controller.test_idle(),
    );
    checks.check(
        "Esc cancels and keeps the old shortcut; its repeat does not close the window",
        paused && resumed && ui.saved.len() == saved_before
            && text(recorder(Action::RestoreLast)) == recorder_name(Action::RestoreLast, Some("Ctrl+Alt+F9"))
            && status(Action::RestoreLast).is_empty()
            // SAFETY: same-process HWND query.
            && unsafe { IsWindow(Some(hwnd)) }.as_bool()
            && controller.test_hwnd() == Some(hwnd),
    );

    // Refused chords keep recording and say why. A chord another row holds names it.
    click(hwnd, Control::shortcut(Action::DiscardTop, ShortcutPart::Recorder));
    ui.until(controller, |ui| ui.service.paused.get());
    let current = Settings::load(path)?.shortcuts;
    for chord in ["Win+L", "PrtScn", "Ctrl+V", "Ctrl+Alt+Shift+F7"] {
        press(controller, recorder(Action::DiscardTop), chord);
        ui.idle(controller);
        let expected = refusal(chord, &current, Action::DiscardTop);
        checks.check(
            &format!("{chord} is refused with \"{expected}\" and recording goes on"),
            status(Action::DiscardTop) == expected && service.paused.get() && ui.saved.len() == saved_before,
        );
    }
    checks.check(
        "a chord another action holds names that action",
        status(Action::DiscardTop) == "Already used by \"Open settings\".",
    );

    // Backspace clears.
    key(controller, recorder(Action::DiscardTop), VK_BACK, Held::default(), true, false);
    let cleared = ui.until(controller, |ui| ui.saved.last().is_some_and(|s| s.shortcuts.get(Action::DiscardTop).is_none()) && !ui.service.paused.get());
    ui.idle(controller);
    checks.check(
        "Backspace clears the shortcut, saves and resumes; Reset leaves with it",
        cleared && Settings::load(path)?.shortcuts.get(Action::DiscardTop).is_none()
            && text(recorder(Action::DiscardTop)) == recorder_name(Action::DiscardTop, None)
            && !visible(row(hwnd, Action::DiscardTop, ShortcutPart::Reset)),
    );

    // Reset is there only when the row differs from its default, and restores it.
    let resets = Action::ALL.iter().all(|&action| {
        let differs = action.available() && ui.current.get(action) != action.default_chord();
        visible(row(hwnd, action, ShortcutPart::Reset)) == differs
    });
    click(hwnd, Control::shortcut(Action::DiscardAll, ShortcutPart::Reset));
    let reset = ui.until(controller, |ui| ui.saved.last().is_some_and(|s| s.shortcuts.get(Action::DiscardAll) == Action::DiscardAll.default_chord()));
    ui.idle(controller);
    checks.check(
        "Reset shows only on changed rows and restores the default",
        resets && reset && !service.paused.get()
            && !visible(row(hwnd, Action::DiscardAll, ShortcutPart::Reset))
            && text(recorder(Action::DiscardAll)) == recorder_name(Action::DiscardAll, None),
    );

    // A recording-only row is edited like the others; its note says when it is registered.
    click(hwnd, Control::shortcut(Action::Pen, ShortcutPart::Recorder));
    let paused = ui.until(controller, |ui| ui.service.paused.get());
    let saved_before = ui.saved.len();
    press(controller, recorder(Action::Pen), "Ctrl+Alt+F6");
    let done = ui.until(controller, |ui| ui.saved.len() > saved_before && !ui.service.paused.get());
    ui.idle(controller);
    checks.check(
        "Pen records Ctrl+Alt+F6: saved, shown, Active while recording",
        paused && done
            && Settings::load(path)?.shortcuts.get(Action::Pen) == Chord::parse("Ctrl+Alt+F6").ok()
            && text(recorder(Action::Pen)) == recorder_name(Action::Pen, Some("Ctrl+Alt+F6"))
            && status(Action::Pen) == "Active while recording"
            && visible(row(hwnd, Action::Pen, ShortcutPart::Reset)),
    );
    click(hwnd, Control::shortcut(Action::Pen, ShortcutPart::Reset));
    let reset = ui.until(controller, |ui| {
        ui.saved.last().is_some_and(|s| s.shortcuts.get(Action::Pen) == Action::Pen.default_chord())
    });
    ui.idle(controller);
    checks.check("Pen's Reset restores Ctrl+Shift+7", reset);

    // Another program holds the chord: the old one stays, unsaved, and the row says so.
    let taken = "Ctrl+Alt+F8";
    service.taken.set(Chord::parse(taken).ok());
    let saved_before = ui.saved.len();
    click(hwnd, Control::shortcut(Action::RestoreLast, ShortcutPart::Recorder));
    ui.until(controller, |ui| ui.service.paused.get());
    press(controller, recorder(Action::RestoreLast), taken);
    let message = format!("Another program already uses {taken}");
    let reverted = ui.until(controller, |_| status(Action::RestoreLast) == message);
    ui.idle(controller);
    checks.check(
        "a Taken answer keeps the old shortcut, saves nothing, resumes and says so",
        reverted && ui.saved.len() == saved_before && !service.paused.get()
            && service.applied.borrow().get(Action::RestoreLast) == Chord::parse("Ctrl+Alt+F9").ok()
            && Settings::load(path)?.shortcuts.get(Action::RestoreLast) == Chord::parse("Ctrl+Alt+F9").ok()
            && text(recorder(Action::RestoreLast)) == recorder_name(Action::RestoreLast, Some("Ctrl+Alt+F9")),
    );
    service.taken.set(None);

    // A report with an old tag is ignored; the current one completes the change.
    let old_tag = *service.tags().last().expect("an earlier check");
    service.hold.set(true);
    click(hwnd, Control::shortcut(Action::RestoreLast, ShortcutPart::Recorder));
    ui.until(controller, |ui| ui.service.paused.get());
    press(controller, recorder(Action::RestoreLast), "Ctrl+Alt+Q");
    ui.until(controller, |ui| ui.service.tags().last() != Some(&old_tag));
    let taken_entries = Action::ALL.iter().map(|&action| (action, Registration::Taken)).collect();
    controller.publish_registration(RegistrationReport { tag: old_tag, entries: taken_entries });
    thread::sleep(Duration::from_millis(100));
    let ignored = status(Action::RestoreLast) == PROMPT && ui.saved.len() == saved_before;
    service.hold.set(false);
    let done = ui.until(controller, |ui| ui.saved.len() > saved_before && !ui.service.paused.get());
    ui.idle(controller);
    checks.check("a report with an old tag is ignored", ignored);
    checks.check(
        "the current report saves Ctrl+Alt+Q and shows the layout hint",
        done && text(recorder(Action::RestoreLast)) == recorder_name(Action::RestoreLast, Some("Ctrl+Alt+Q"))
            && status(Action::RestoreLast) == "On some keyboard layouts this combination types a character.",
    );

    // The shortcuts survive a reload.
    let reloaded = Settings::load(path)?.shortcuts;
    checks.check(
        "settings.json holds every recorded shortcut",
        reloaded.get(Action::RestoreLast) == Chord::parse("Ctrl+Alt+Q").ok()
            && reloaded.get(Action::DiscardTop).is_none()
            && reloaded.get(Action::DiscardAll).is_none()
            && reloaded.get(Action::OpenSettings) == Chord::parse("Ctrl+Alt+Shift+F7").ok(),
    );
    click(hwnd, GeneralTab);
    ui.idle(controller);
    Ok(())
}

const TOOL_CONTROLS: [Control; 4] = [ToolRow0, ToolSwitch0, ToolRow1, ToolSwitch1];
const VK_UP: u16 = 0x26;
const VK_DOWN: u16 = 0x28;
/// The tool list's row pitch, logical px.
const TOOL_PITCH: i32 = 52;

/// Sends one mouse message to a tool row, at a point of its client area, as Windows would.
fn mouse(target: HWND, message: u32, x: i32, y: i32) {
    let held = if message == WM_LBUTTONUP { 0 } else { 1 }; // MK_LBUTTON
    let point = ((y as u16 as u32) << 16 | x as u16 as u32) as isize;
    // SAFETY: a mouse message to a control of our own test window, not synthetic input.
    unsafe { SendMessageW(target, message, Some(WPARAM(held)), Some(LPARAM(point))) };
}

/// Drags the row at `from` by `dy` physical px, pressed 10 px into it.
fn drag(hwnd: HWND, from: Control, dy: i32) {
    let target = child(hwnd, from);
    mouse(target, WM_LBUTTONDOWN, 10, 10);
    mouse(target, WM_MOUSEMOVE, 10, 10 + dy / 2);
    mouse(target, WM_MOUSEMOVE, 10, 10 + dy);
    mouse(target, WM_LBUTTONUP, 10, 10 + dy);
}

fn tool_order(path: &Path) -> Vec<(ToolId, bool)> {
    Settings::load(path).map_or_else(
        |_| Vec::new(),
        |settings| settings.tool_menu.entries().iter().map(|e| (e.tool, e.shown)).collect(),
    )
}

fn row_name(label: &str, place: usize) -> String {
    format!("{label}, {place} of 2, Alt+Up or Alt+Down moves it")
}

/// The Tool menu page: names, tab stops, switches, Alt+Up and Alt+Down, and drags, each saved
/// and published at once.
fn verify_tool_menu(checks: &mut Checks, controller: &mut SettingsWindow, hwnd: HWND, path: &Path, dpi: u32) -> Result<()> {
    let mut expected = Settings::load(path)?;
    let note = ToolId::NewNote;
    let record = ToolId::RecordScreen;
    click(hwnd, ToolMenuTab);
    checks.check(
        "Tool menu page: rows and switches show and are named; the General page's are hidden",
        checked(child(hwnd, ToolMenuTab))
            && TOOL_CONTROLS.iter().all(|&id| visible(child(hwnd, id)) && enabled(child(hwnd, id)))
            && !visible(child(hwnd, Screenshots))
            && !visible(child(hwnd, ToolMenuEmpty))
            && text(child(hwnd, ToolRow0)) == row_name("New note", 1)
            && text(child(hwnd, ToolRow1)) == row_name("Record screen", 2)
            && text(child(hwnd, ToolSwitch0)) == "Show New note in the tool menu"
            && text(child(hwnd, ToolSwitch1)) == "Show Record screen in the tool menu"
            && checked(child(hwnd, ToolSwitch0))
            && checked(child(hwnd, ToolSwitch1))
            && text(child(hwnd, ToolMenuEmpty)) == "Every tool is hidden, so holding Alt opens no menu.",
    );
    let stops: Vec<HWND> = [ToolMenuTab, ToolRow0, ToolSwitch0, ToolRow1, ToolSwitch1].iter().map(|&id| child(hwnd, id)).collect();
    checks.check("Tool menu page: Tab visits the tab, then each row and its switch", tab_cycle(hwnd) == stops);

    // The switches hide and show a tool; with both hidden, the page says so.
    click(hwnd, ToolSwitch1);
    expected.tool_menu.set_shown(record, false);
    checks.check(
        "hiding Record screen saves, publishes and unchecks its switch",
        disk(controller, path, &expected) && !checked(child(hwnd, ToolSwitch1)) && !visible(child(hwnd, ToolMenuEmpty)),
    );
    click(hwnd, ToolSwitch0);
    expected.tool_menu.set_shown(note, false);
    checks.check(
        "hiding every tool saves and shows the empty line",
        disk(controller, path, &expected) && !expected.tool_menu.any_shown() && visible(child(hwnd, ToolMenuEmpty)),
    );
    click(hwnd, ToolSwitch0);
    expected.tool_menu.set_shown(note, true);
    checks.check("showing New note again hides the empty line", disk(controller, path, &expected) && !visible(child(hwnd, ToolMenuEmpty)));

    // Alt+Down on the first row moves it down; the switches go with their tools.
    let alt = Held { alt: true, ..Held::default() };
    key(controller, child(hwnd, ToolRow0), VK_DOWN, alt, true, false);
    key(controller, child(hwnd, ToolRow0), VK_DOWN, alt, false, false);
    expected.tool_menu = expected.tool_menu.moved(0, 1);
    checks.check(
        "Alt+Down moves New note below Record screen; saved; names and switches follow",
        disk(controller, path, &expected)
            && tool_order(path) == [(record, false), (note, true)]
            && text(child(hwnd, ToolRow0)) == row_name("Record screen", 1)
            && text(child(hwnd, ToolRow1)) == row_name("New note", 2)
            && !checked(child(hwnd, ToolSwitch0))
            && checked(child(hwnd, ToolSwitch1)),
    );
    // Nothing moves past the ends, and arrows without Alt do nothing.
    key(controller, child(hwnd, ToolRow0), VK_UP, alt, true, false);
    key(controller, child(hwnd, ToolRow1), VK_DOWN, alt, true, false);
    key(controller, child(hwnd, ToolRow1), VK_UP, Held::default(), true, false);
    controller.test_modifiers(Held::default());
    checks.check(
        "Alt+Up on the first row, Alt+Down on the last and a plain Up change nothing",
        Settings::load(path)? == expected && controller.saved(None, &expected.shortcuts).is_empty(),
    );
    key(controller, child(hwnd, ToolRow1), VK_UP, alt, true, false);
    controller.test_modifiers(Held::default());
    expected.tool_menu = expected.tool_menu.moved(1, 0);
    checks.check(
        "Alt+Up moves New note back to the top",
        disk(controller, path, &expected) && tool_order(path) == [(note, true), (record, false)],
    );

    // Drags: past half a row swaps the two; less leaves them; upward works too.
    let pitch = (TOOL_PITCH * dpi as i32 + 48) / 96;
    drag(hwnd, ToolRow0, pitch / 2 - 6);
    checks.check(
        "a drag of less than half a row puts the row back and saves nothing",
        Settings::load(path)? == expected && controller.saved(None, &expected.shortcuts).is_empty(),
    );
    drag(hwnd, ToolRow0, pitch / 2 + 6);
    expected.tool_menu = expected.tool_menu.moved(0, 1);
    checks.check(
        "dragging New note down past half a row drops it second; saved",
        disk(controller, path, &expected)
            && tool_order(path) == [(record, false), (note, true)]
            && text(child(hwnd, ToolRow1)) == row_name("New note", 2),
    );
    drag(hwnd, ToolRow1, -3 * pitch);
    expected.tool_menu = expected.tool_menu.moved(1, 0);
    checks.check(
        "dragging it far up drops it first",
        disk(controller, path, &expected) && tool_order(path) == [(note, true), (record, false)],
    );
    // Back to every tool shown, as the other checks expect.
    click(hwnd, ToolSwitch1);
    expected.tool_menu.set_shown(record, true);
    checks.check("showing Record screen again saves", disk(controller, path, &expected) && expected.tool_menu == ToolMenu::default());
    click(hwnd, GeneralTab);
    Ok(())
}

/// Each tool row and switch sits where it is painted: the row's patch fills it with paper
/// above it, and the switch's rail is drawn inside its box.
fn tool_rows_where_painted(checks: &mut Checks, hwnd: HWND, shot: &Shot, dpi: u32, label: &str) {
    let px = |v: i32| (v * dpi as i32 + 48) / 96;
    let paper = shot.at(2, 2);
    for (row, switch) in [(ToolRow0, ToolSwitch0), (ToolRow1, ToolSwitch1)] {
        let (Some(r), Some(s)) = (child_rect(hwnd, row), child_rect(hwnd, switch)) else {
            checks.check(&format!("{label}: {row:?} and {switch:?} are visible"), false);
            continue;
        };
        let middle = (r.top + r.bottom) / 2;
        let face = shot.at(r.right - px(6), middle);
        let above = shot.at(r.right - px(6), r.top - px(3));
        let below = shot.at(r.right - px(6), r.bottom + px(3));
        let rail = shot.at(s.right - px(24), (s.top + s.bottom) / 2);
        let beside = shot.at(s.left + px(2), (s.top + s.bottom) / 2);
        checks.check(
            &format!("{label}: {row:?} is painted inside its box, paper above and below; {switch:?}'s rail inside its box"),
            face != paper && above == paper && below == paper && rail != beside && beside == face,
        );
    }
}

/// Starts recording on Restore last screenshot and waits for the pause.
fn start_recording(controller: &mut SettingsWindow, hwnd: HWND, ui: &mut Loop<'_>) -> bool {
    click(hwnd, ShortcutsTab);
    click(hwnd, Control::shortcut(Action::RestoreLast, ShortcutPart::Recorder));
    ui.until(controller, |ui| ui.service.paused.get())
}

fn verify_window_fits(checks: &mut Checks) {
    for (width, height, dpi) in [(1366, 768, 96), (1920, 1080, 144)] {
        let work = height - (48 * dpi as i32 + 95) / 96;
        let (w, h) = test_window_size(dpi);
        let (cw, ch) = test_client_size(dpi);
        checks.check(
            &format!("{width}x{height} at {}%: window {w} x {h} (client {cw} x {ch}) fits the {width} x {work} work area", dpi * 100 / 96),
            w <= width && h <= work,
        );
    }
}

fn memory() -> Result<(usize, usize)> {
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: EX has the documented base prefix; the size declares the extended buffer.
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            memory.cb,
        )?
    };
    Ok((memory.WorkingSetSize, memory.PrivateUsage))
}

fn report_memory(label: &str) -> Result<()> {
    let (working, private) = memory()?;
    println!("INFO memory {label}, working set {working} bytes, private {private} bytes");
    Ok(())
}

fn process_cpu() -> Result<u64> {
    let (mut created, mut exited, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: four writable FILETIMEs and the pseudo-handle of this process.
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user)?
    };
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Ok(ticks(kernel) + ticks(user))
}

fn check_idle(checks: &mut Checks, window: &SettingsWindow, page: &str) -> Result<()> {
    thread::sleep(Duration::from_millis(250));
    let timer_stopped = window.test_idle();
    let before = process_cpu()?;
    let at = Instant::now();
    thread::sleep(Duration::from_secs(2));
    let cpu = process_cpu()? - before;
    let seconds = at.elapsed().as_secs_f64();
    checks.check(
        &format!("{page} page: animation timer is stopped before and after two idle seconds"),
        timer_stopped && window.test_idle(),
    );
    checks.check(
        &format!("{page} page: idle process CPU {:.3} ms over {seconds:.3} s", cpu as f64 / 10_000.0),
        cpu == 0,
    );
    Ok(())
}

fn child_rect(hwnd: HWND, id: Control) -> Option<RECT> {
    // SAFETY: a same-process child; its screen rectangle is mapped into the parent's client.
    unsafe {
        let child = GetDlgItem(Some(hwnd), id as i32).ok()?;
        if !IsWindowVisible(child).as_bool() {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(child, &mut rect).ok()?;
        let mut points = [POINT { x: rect.left, y: rect.top }, POINT { x: rect.right, y: rect.bottom }];
        MapWindowPoints(None, Some(hwnd), &mut points);
        Some(RECT { left: points[0].x, top: points[0].y, right: points[1].x, bottom: points[1].y })
    }
}

/// A captured client area, RGB.
struct Shot {
    width: i32,
    rgb: Vec<u8>,
}

impl Shot {
    fn at(&self, x: i32, y: i32) -> [u8; 3] {
        let i = ((y * self.width + x) * 3) as usize;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

fn capture(hwnd: HWND, path: &Path, dpi: u32) -> Result<bool> {
    Ok(capture_shot(hwnd, path, dpi)?.0)
}

fn capture_shot(hwnd: HWND, path: &Path, dpi: u32) -> Result<(bool, Shot)> {
    // SAFETY: wait for the compositor to consume the frozen client paint before PrintWindow.
    let _ = unsafe { DwmFlush() };
    let mut client = RECT::default();
    // SAFETY: this is our visible test HWND and client is writable.
    unsafe { GetClientRect(hwnd, &mut client)? };
    let (w, h) = (client.right, client.bottom);
    let mut bits = std::ptr::null_mut();
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: BITMAPINFO defines a 32-bit top-down DIB; bits is a writable out pointer.
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    // SAFETY: allocate a private memory DC, and restore its original bitmap before deletion.
    let dc = unsafe { CreateCompatibleDC(None) };
    let rendered = if dc.is_invalid() {
        false
    } else {
        // SAFETY: our bitmap and DC are live, and PrintWindow targets only the test HWND.
        unsafe {
            let previous = SelectObject(dc, bitmap.into());
            const PW_RENDERFULLCONTENT: u32 = 2;
            let printed =
                PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT))
                    .as_bool();
            let _ = GdiFlush();
            SelectObject(dc, previous);
            let _ = DeleteDC(dc);
            printed
        }
    };
    // SAFETY: the DIB is top-down, 32-bit, tightly packed, and alive through the copy.
    let rgb: Vec<u8> = unsafe { slice::from_raw_parts(bits.cast::<u8>(), (w * h * 4) as usize) }
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
        .collect();
    // SAFETY: the bitmap is no longer selected into any DC.
    let _ = unsafe { DeleteObject(bitmap.into()) };
    let mut encoder = png::Encoder::new(File::create(path)?, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgb)?;
    let pixels = rgb.as_chunks::<3>().0;
    let varied = pixels.iter().filter(|pixel| **pixel != pixels[0]).count() > 1000;
    Ok((rendered && varied && (w, h) == test_client_size(dpi), Shot { width: w, rgb }))
}

/// Waits up to two seconds for a saved snapshot with `count` recording hints.
fn saved_count(window: &mut SettingsWindow, shortcuts: &Shortcuts, count: u32) -> bool {
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        if window
            .saved(None, shortcuts)
            .iter()
            .any(|settings| settings.recording_hints_shown == count)
        {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

/// The recording hint counter, with the window closed (a short-lived thread counts) and open
/// (the window counts and saves), and a window opened right after a count while closed.
fn verify_hint_counter(checks: &mut Checks, path: &Path, defaults: &TestMode) -> Result<()> {
    let before = Settings::load(path)?;
    let start = before.recording_hints_shown;
    let expect = |count: u32| Settings { recording_hints_shown: count, ..before.clone() };
    let mut window = SettingsWindow::for_test(path.to_path_buf(), defaults.clone())?;
    window.note_recording_hint_shown();
    let saved = saved_count(&mut window, &before.shortcuts, start + 1);
    checks.check(
        "hint counter, window closed: a short thread counts it, the rest of settings.json unchanged",
        saved && Settings::load(path)? == expect(start + 1),
    );
    // Counted while closed, then opened at once: the window loads after the count.
    window.note_recording_hint_shown();
    let hwnd = open(&mut window)?;
    let first = saved_count(&mut window, &before.shortcuts, start + 2);
    window.note_recording_hint_shown();
    let second = saved_count(&mut window, &before.shortcuts, start + 3);
    checks.check(
        "hint counter, window open: the window sees the count made before it opened and saves the next one",
        first && second && Settings::load(path)? == expect(start + 3),
    );
    checks.check("hint counter: the window closes", close(&mut window, hwnd));
    // Back to what the other checks expect.
    before.save(path)?;
    Ok(())
}

fn run(cycles_done: &AtomicBool) -> Result<bool> {
    let mut checks = Checks { ok: true };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(format!("settings-selftest-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let root = fs::canonicalize(root)?;
    let path = root.join("settings.json");
    let defaults = TestMode {
        notes: root.join("Documents").join("Igorio Notes"),
        captures: root.join("Pictures").join("Screenshots"),
        dark: true,
        dpi: 96,
        run_suffix: 0,
        at: test_monitor::test_monitor_point(),
    };
    fs::create_dir_all(defaults.notes.parent().unwrap())?;
    fs::create_dir_all(defaults.captures.parent().unwrap())?;
    struct RemoveRun(TestMode);
    impl Drop for RemoveRun {
        fn drop(&mut self) {
            let _ = self.0.remove_run_value();
        }
    }
    let _remove_run = RemoveRun(defaults.clone());
    let mut baseline = Settings {
        card_size: CardSize::Medium,
        keep_every_screenshot: true,
        launch_at_login: true,
        notes_folder: Some(defaults.notes.clone()),
        captures_folder: Some(defaults.captures.clone()),
        ..Settings::default()
    };
    baseline.set_enabled("igorio.screenshots", false);
    baseline.save(&path)?;
    let mut controller = SettingsWindow::for_test(path.clone(), defaults.clone())?;
    report_memory("before first open")?;
    for cycle in 1..=3 {
        let hwnd = open(&mut controller)?;
        if cycle == 1 {
            let at = defaults.at;
            println!("INFO test windows open {}", at.map_or("on the cursor's monitor".into(), |(x, y)| format!("around ({x}, {y})")));
            if let Some((x, y)) = at {
                // SAFETY: monitor lookups for our own window and a point; no pointers kept.
                let same = unsafe {
                    MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) == MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL)
                };
                checks.check("the window opens on the test monitor", same);
            }
            // Include the composed surface after the opening transition settles.
            thread::sleep(Duration::from_millis(200));
            report_memory("while open")?;
            verify_controls(&mut checks, &mut controller, hwnd, &path, &defaults)?;
            verify_shortcuts(&mut checks, &mut controller, hwnd, &path)?;
            verify_tool_menu(&mut checks, &mut controller, hwnd, &path, defaults.dpi)?;
        } else {
            checks.check(
                &format!("cycle {cycle} reloads the saved check states and shortcuts"),
                checked(child(hwnd, Screenshots))
                    && checked(child(hwnd, Medium))
                    && checked(child(hwnd, Keep))
                    && !checked(child(hwnd, Launch))
                    && text(row(hwnd, Action::RestoreLast, ShortcutPart::Recorder))
                        == recorder_name(Action::RestoreLast, Some("Ctrl+Alt+Q")),
            );
        }
        let service = FakeService::new(Settings::load(&path)?.shortcuts);
        let mut ui = Loop { service: &service, current: service.applied.borrow().clone(), saved: Vec::new() };
        match cycle {
            2 => {
                let paused = start_recording(&mut controller, hwnd, &mut ui);
                let closed = close(&mut controller, hwnd);
                let resumed = ui.until(&mut controller, |ui| !ui.service.paused.get());
                checks.check(
                    "cycle 2 closes mid-recording within one second and the service resumes",
                    paused && closed && resumed,
                );
            }
            3 => {
                let paused = start_recording(&mut controller, hwnd, &mut ui);
                controller.test_die();
                let resumed = ui.until(&mut controller, |ui| !ui.service.paused.get());
                checks.check(
                    "cycle 3: the settings thread ends mid-recording without a word, the service resumes",
                    paused && resumed && ui.service.calls.borrow().last() == Some(&Call::Report(0)),
                );
                checks.check("cycle 3 joins the ended STA within one second", close(&mut controller, hwnd));
            }
            _ => checks.check(
                &format!("cycle {cycle} closes HWND and joins the STA within one second"),
                close(&mut controller, hwnd),
            ),
        }
        thread::sleep(Duration::from_millis(100));
    }
    cycles_done.store(true, Relaxed);
    thread::sleep(Duration::from_millis(250));
    report_memory("after third close")?;

    verify_hint_counter(&mut checks, &path, &defaults)?;

    {
        let mut window = SettingsWindow::for_test(path.clone(), defaults.clone())?;
        let hwnd = open(&mut window)?;
        let service = FakeService::new(Settings::load(&path)?.shortcuts);
        let mut ui = Loop { service: &service, current: service.applied.borrow().clone(), saved: Vec::new() };
        let paused = start_recording(&mut window, hwnd, &mut ui);
        let current = ui.current.clone();
        window.shutdown(Some(&service), &current);
        checks.check(
            "the app quits mid-recording: the window closes and the service resumes",
            paused && !service.paused.get()
                // SAFETY: same-process HWND query.
                && !unsafe { IsWindow(Some(hwnd)) }.as_bool(),
        );
    }
    verify_window_fits(&mut checks);

    let locked = lock(&path)?;
    let hwnd = open(&mut controller)?;
    checks.check(
        "locked file disables all buttons and shows a load error",
        [
            Screenshots,
            Keep,
            Carry,
            Notes,
            Recording,
            ChooseNotes,
            OpenNotes,
            ResetNotes,
            ChooseCaptures,
            OpenCaptures,
            ResetCaptures,
            Small,
            Medium,
            Large,
            Launch,
            Retry,
        ]
        .into_iter()
        .all(|id| !enabled(child(hwnd, id)))
            && text(child(hwnd, Status)).contains("could not be loaded"),
    );
    click(hwnd, Screenshots);
    drop(locked);
    checks.check(
        "locked-file window cannot overwrite after the file unlocks",
        Settings::load(&path)?.is_enabled("igorio.screenshots")
            && controller.saved(None, &Shortcuts::default()).is_empty(),
    );
    checks.check("locked-file window closes within one second", close(&mut controller, hwnd));

    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    // A mix of set and unset rows, one with a punctuation key.
    let mut seeded = Settings::default();
    for (action, chord) in [(Action::RestoreLast, "Ctrl+Alt+F9"), (Action::OpenSettings, "Ctrl+Shift+[")] {
        seeded.shortcuts.set(action, Chord::parse(chord).ok()).map_err(|error| error.to_string())?;
    }
    for (dark, dpi, suffix) in [(false, 96, "light-100"), (true, 120, "dark-125")] {
        seeded.save(&path)?;
        let mode = TestMode { dark, dpi, ..defaults.clone() };
        let mut window = SettingsWindow::for_test(path.clone(), mode)?;
        let hwnd = open(&mut window)?;
        let service = FakeService::new(seeded.shortcuts.clone());
        let mut ui = Loop { service: &service, current: seeded.shortcuts.clone(), saved: Vec::new() };
        ui.idle(&mut window);
        if dark {
            check_idle(&mut checks, &window, "General")?;
            click(hwnd, ShortcutsTab);
            ui.idle(&mut window);
            check_idle(&mut checks, &window, "Shortcuts")?;
            click(hwnd, ToolMenuTab);
            ui.idle(&mut window);
            check_idle(&mut checks, &window, "Tool menu")?;
            click(hwnd, GeneralTab);
            ui.idle(&mut window);
        }
        window.test_scene(TestScene::Settled);
        let name = format!("settings-pages-general-{suffix}.png");
        let output = folder.join(&name);
        checks.check(&name, capture(hwnd, &output, dpi)?);
        if dark {
            let mut frames = std::collections::BTreeSet::from([fs::read(&output)?]);
            for (scene, name) in [
                (TestScene::SwitchSlide, "settings-motion-switch.png"),
                (TestScene::CardSlide, "settings-motion-card.png"),
                (TestScene::ButtonHover, "settings-motion-button-hover.png"),
                (TestScene::ButtonPress, "settings-motion-button-pressed.png"),
                (TestScene::SwitchFocus, "settings-focus-switch.png"),
                (TestScene::CardFocus, "settings-focus-card.png"),
            ] {
                window.test_scene(scene);
                let output = folder.join(name);
                checks
                    .check(name, capture(hwnd, &output, dpi)? && frames.insert(fs::read(&output)?));
            }
        }
        click(hwnd, ShortcutsTab);
        ui.idle(&mut window);
        window.test_scene(TestScene::Settled);
        let name = format!("settings-pages-shortcuts-{suffix}.png");
        checks.check(&name, capture(hwnd, &folder.join(&name), dpi)?);
        if dark {
            window.test_scene(TestScene::PageSlide);
            checks.check("settings-pages-slide.png", capture(hwnd, &folder.join("settings-pages-slide.png"), dpi)?);
        } else {
            capture_rows(&mut checks, &mut window, hwnd, &mut ui, &folder)?;
        }
        click(hwnd, ToolMenuTab);
        ui.idle(&mut window);
        window.test_scene(TestScene::Settled);
        let name = format!("settings-pages-toolmenu-{suffix}.png");
        let (painted, shot) = capture_shot(hwnd, &folder.join(&name), dpi)?;
        checks.check(&name, painted);
        tool_rows_where_painted(&mut checks, hwnd, &shot, dpi, &name);
        if !dark {
            capture_tools(&mut checks, &window, hwnd, &folder)?;
        }
        checks.check("capture window closes within one second", close(&mut window, hwnd));
    }
    Ok(checks.ok)
}

/// The Tool menu page mid-drag, with a hidden tool and a tool whose capability is off, and
/// with every tool hidden, at 100% light.
fn capture_tools(checks: &mut Checks, window: &SettingsWindow, hwnd: HWND, folder: &Path) -> Result<()> {
    let shot = |checks: &mut Checks, name: &str| -> Result<()> {
        window.test_scene(TestScene::Settled);
        checks.check(name, capture(hwnd, &folder.join(name), 96)?);
        Ok(())
    };
    // New note held two thirds of a row down: Record screen has moved up under it.
    let row = child(hwnd, ToolRow0);
    mouse(row, WM_LBUTTONDOWN, 10, 10);
    mouse(row, WM_MOUSEMOVE, 10, 10 + TOOL_PITCH * 2 / 3);
    shot(checks, "settings-toolmenu-drag.png")?;
    // Let go where it started: nothing changes.
    mouse(row, WM_MOUSEMOVE, 10, 10);
    mouse(row, WM_LBUTTONUP, 10, 10);
    // New note hidden; Screen recording switched off, so its row is dimmed and says so.
    click(hwnd, ToolSwitch0);
    click(hwnd, GeneralTab);
    click(hwnd, Recording);
    click(hwnd, ToolMenuTab);
    shot(checks, "settings-toolmenu-hidden.png")?;
    click(hwnd, ToolSwitch1);
    shot(checks, "settings-toolmenu-empty.png")?;
    Ok(())
}

/// The Shortcuts page in its states, at 100% light.
fn capture_rows(
    checks: &mut Checks,
    window: &mut SettingsWindow,
    hwnd: HWND,
    ui: &mut Loop<'_>,
    folder: &Path,
) -> Result<()> {
    let recorder = |action| row(hwnd, action, ShortcutPart::Recorder);
    let shot = |checks: &mut Checks, window: &SettingsWindow, name: &str| -> Result<()> {
        window.test_scene(TestScene::Settled);
        checks.check(name, capture(hwnd, &folder.join(name), 96)?);
        Ok(())
    };
    let cancel = |window: &SettingsWindow, action| {
        key(window, recorder(action), VK_ESCAPE, Held::default(), true, false);
        key(window, recorder(action), VK_ESCAPE, Held::default(), false, false);
    };

    // A mix of set and unset rows, with a long chord.
    click(hwnd, Control::shortcut(Action::DiscardAll, ShortcutPart::Recorder));
    ui.until(window, |ui| ui.service.paused.get());
    press(window, recorder(Action::DiscardAll), "Ctrl+Alt+Shift+Win+F13");
    ui.until(window, |ui| !ui.service.paused.get());
    ui.idle(window);
    shot(checks, window, "settings-shortcuts-mixed.png")?;

    // Recording, Ctrl and Shift held.
    click(hwnd, Control::shortcut(Action::DiscardTop, ShortcutPart::Recorder));
    ui.until(window, |ui| ui.service.paused.get());
    let ctrl = Held { ctrl: true, ..Held::default() };
    key(window, recorder(Action::DiscardTop), VK_CONTROL, ctrl, true, false);
    key(window, recorder(Action::DiscardTop), VK_SHIFT, Held { shift: true, ..ctrl }, true, false);
    shot(checks, window, "settings-shortcuts-recording.png")?;

    // A refused chord.
    press(window, recorder(Action::DiscardTop), "Win+L");
    shot(checks, window, "settings-shortcuts-error.png")?;
    cancel(window, Action::DiscardTop);
    ui.until(window, |ui| !ui.service.paused.get());

    // Another program holds it.
    ui.service.taken.set(Chord::parse("Ctrl+Alt+F8").ok());
    click(hwnd, Control::shortcut(Action::DiscardTop, ShortcutPart::Recorder));
    ui.until(window, |ui| ui.service.paused.get());
    press(window, recorder(Action::DiscardTop), "Ctrl+Alt+F8");
    ui.until(window, |ui| !ui.service.paused.get());
    ui.idle(window);
    shot(checks, window, "settings-shortcuts-taken.png")?;
    ui.service.taken.set(None);
    click(hwnd, Control::shortcut(Action::DiscardTop, ShortcutPart::Recorder));
    cancel(window, Action::DiscardTop);
    ui.until(window, |ui| !ui.service.paused.get());

    // A hint after a successful set.
    click(hwnd, Control::shortcut(Action::DiscardTop, ShortcutPart::Recorder));
    ui.until(window, |ui| ui.service.paused.get());
    press(window, recorder(Action::DiscardTop), "Alt+K");
    ui.until(window, |ui| !ui.service.paused.get());
    ui.idle(window);
    shot(checks, window, "settings-shortcuts-warning.png")?;
    Ok(())
}

struct Timer(HANDLE);

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: this Timer owns the handle and closes it once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn main() -> Result<()> {
    ensure_dpi_awareness();
    // SAFETY: unnamed high-resolution timer, owned by Timer.
    let timer = Timer(unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            (TIMER_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE).0,
        )?
    });
    // Run Win32 probes on a test driver, leaving the stand-in UI thread free to tick.
    let cycles_done = std::sync::Arc::new(AtomicBool::new(false));
    let progress = cycles_done.clone();
    let driver = thread::spawn(move || run(&progress).map_err(|error| error.to_string()));
    let interval = Duration::from_nanos(1_000_000_000 / 120);
    let mut last = Instant::now();
    let mut next = last + interval;
    let mut worst = Duration::ZERO;
    let mut frames = 0;
    while !cycles_done.load(Relaxed) && !driver.is_finished() {
        let due =
            -((next.saturating_duration_since(Instant::now()).as_nanos() as i64 / 100).max(1));
        // SAFETY: the timer remains live and the relative due time outlives SetWaitableTimer.
        unsafe {
            SetWaitableTimer(timer.0, &due, 0, None, None, false)?;
            if WaitForSingleObject(timer.0, 100) != WAIT_OBJECT_0 {
                return Err("120 Hz timer wait failed".into());
            }
        }
        let now = Instant::now();
        worst = worst.max(now - last);
        last = now;
        next += interval;
        if next < now {
            next = now + interval;
        }
        frames += 1;
    }
    let result = driver.join().map_err(|_| "settings self-test driver panicked")?;
    let mut checks = Checks { ok: true };
    checks.check(
        &format!(
            "120 Hz stand-in worst frame gap {:.2} ms over {frames} frames",
            worst.as_secs_f64() * 1000.0
        ),
        worst < Duration::from_millis(20),
    );
    match result {
        Ok(true) if checks.ok => Ok(()),
        Ok(_) => Err("settings self-test failed".into()),
        Err(error) => {
            println!("FAIL {error}");
            Err(error.into())
        }
    }
}
