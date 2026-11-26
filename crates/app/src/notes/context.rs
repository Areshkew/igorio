//! What is under the cursor for notes: whether a spot is empty background, and where a note
//! made there comes from (desktop, app window, web page). From `NoteContext.swift`.
//!
//! Both rules are pure functions over a UI Automation answer ([`Outcome`], a [`Snapshot`]
//! from `uia`) plus a few Win32 facts about the window at the point ([`WindowFacts`]). The
//! facts are read by [`facts_at`], which only reads window manager state and the exe's
//! version resource (cached per path); it never waits on another process and never calls
//! UI Automation.
//!
//! # Empty space ([`is_empty_space`])
//!
//! Not empty: no answer (timed out, failed), our own windows, an elevated window, the
//! taskbar and other shell surfaces ([`SYSTEM_CLASSES`]), terminals, anything editable
//! (the hit or an ancestor takes text), and anything with a control or chrome on the way
//! up (`drop_router::ACTIVATING_ROLES` and `ACTIVATING_ARIA_ROLES`: title bars and
//! toolbars, menus, buttons, links, tabs, rows, scroll bars). In web content
//! (Chromium, Firefox, Electron) only the page counts: the hit must be inside a page, so
//! the browser's own tab strip and custom title bar are never empty.
//!
//! Then the hit's role decides: page and window background ([`BACKGROUND_ROLES`], as in
//! Swift; static text is left out on purpose: the margin of a page is empty, a paragraph is
//! not). On the desktop and in File Explorer the empty area is the items view, a list
//! ([`SHELL_BACKGROUND_ROLES`]); an icon or a file (a row) is not.
//!
//! # Origin ([`origin`])
//!
//! - The desktop: [`NoteOrigin::desktop`].
//! - A browser page ([`BROWSER_EXES`] and a page with an `http`, `https` or `file` address):
//!   `Web`, with the address and the page title (the page's name; the window title when
//!   the page has none).
//! - Anything else: `App`, with the app's friendly name (`FileDescription` from the exe's
//!   version resource, else the exe name without `.exe`; File Explorer is "File
//!   Explorer"), `bundle_id` the AppUserModelID of a packaged app or else the exe name, and
//!   the top-level window title. Electron apps are apps, not pages: their address is an
//!   internal one.
//!
//! The Swift code stores the window's document path (`AXDocument`). UI Automation has no
//! such property and nothing cheap in Win32 gives it, so `url` stays empty for apps; the
//! window title usually names the file ("notes.txt - Notepad").

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;

use igorio_core::editable::{AccessibilityNode, editable_self_or_ancestor};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, HWND, POINT,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId, WindowFromPoint,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::drop_router::{ACTIVATING_ARIA_ROLES, ACTIVATING_ROLES};
use crate::notes::store::{NoteKind, NoteOrigin};
use crate::uia::{Outcome, Shell, Snapshot, UiaNode, shell_kind};

/// Roles that are page or window background rather than content or a control
/// (`NoteContext.backgroundRoles`).
pub const BACKGROUND_ROLES: &[&str] = &[
    "AXWebArea",
    "AXGroup",
    "AXScrollArea",
    "AXSplitGroup",
    "AXLayoutArea",
    "AXWindow",
];

/// On the desktop and in File Explorer: the items view (a list) and its panes.
pub const SHELL_BACKGROUND_ROLES: &[&str] = &["AXList", "AXGroup", "AXWindow"];

/// Top-level window classes of shell surfaces that are never empty space: the taskbars,
/// the notification overflow, Start, search and the action center, Alt+Tab and Task View,
/// menus and pop-ups.
pub const SYSTEM_CLASSES: &[&str] = &[
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "NotifyIconOverflowWindow",
    "TopLevelWindowForOverflowXamlIsland",
    "Windows.UI.Core.CoreWindow",
    "XamlExplorerHostIslandWindow",
    "MultitaskingViewFrame",
    "TaskListThumbnailWnd",
    "ForegroundStaging",
    "Xaml_WindowedPopupClass",
    "#32768",
];

/// Browsers, by exe name (ASCII case-insensitive). A page in one of these is a `Web` origin.
pub const BROWSER_EXES: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "vivaldi.exe",
    "opera.exe",
    "arc.exe",
    "chromium.exe",
    "thorium.exe",
    "librewolf.exe",
    "waterfox.exe",
    "floorp.exe",
    "zen.exe",
];

/// `FrameworkId` of web content.
const WEB_FRAMEWORKS: &[&str] = &["Chrome", "Gecko", "MicrosoftEdge"];

/// Page addresses kept for a `Web` origin.
const WEB_SCHEMES: &[&str] = &["http://", "https://", "file://"];

/// Exe names cached by [`AppNames`].
const NAME_CACHE_LIMIT: usize = 64;

/// Is the spot of this answer empty background?
pub fn is_empty_space(outcome: &Outcome) -> bool {
    match outcome {
        Outcome::Found(snapshot) => is_empty_snapshot(snapshot),
        // Our own windows, elevated windows and no answer: no pill.
        Outcome::OwnProcess
        | Outcome::Elevated { .. }
        | Outcome::TimedOut { .. }
        | Outcome::Failed(_) => false,
    }
}

/// The empty-space rule over a snapshot (see the module docs).
pub fn is_empty_snapshot(snapshot: &Snapshot) -> bool {
    let target = &snapshot.target;
    if SYSTEM_CLASSES.contains(&target.class_name.as_str()) || target.is_terminal {
        return false;
    }
    let hit = &snapshot.hit;
    if editable_self_or_ancestor(hit).is_some() {
        return false;
    }
    let mut in_page = false;
    let mut node = Some(hit.clone());
    while let Some(current) = node {
        let e = current.element();
        if e.password
            || ACTIVATING_ROLES.contains(&e.role)
            || ACTIVATING_ARIA_ROLES.contains(&e.aria_role.as_str())
        {
            return false;
        }
        in_page |= e.role == "AXWebArea" && is_web(&e.framework);
        node = current.parent();
    }
    if is_web(&hit.element().framework) && !in_page {
        return false;
    }
    let role = hit.element().role;
    match target.shell {
        Some(_) => SHELL_BACKGROUND_ROLES.contains(&role),
        None => BACKGROUND_ROLES.contains(&role),
    }
}

/// Win32 facts about the top-level window at a point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowFacts {
    /// The top-level window's title.
    pub title: String,
    /// Its class.
    pub class_name: String,
    /// The exe file name of the app (for a UWP app, the app's, not `ApplicationFrameHost`'s).
    pub exe: Option<String>,
    /// `FileDescription` from the exe's version resource.
    pub description: Option<String>,
    /// The AppUserModelID of a packaged app.
    pub app_id: Option<String>,
}

/// Where a note made at the point of this answer comes from.
pub fn origin(outcome: &Outcome, facts: &WindowFacts) -> NoteOrigin {
    match outcome {
        Outcome::Found(snapshot) => snapshot_origin(snapshot, facts),
        // Over one of our own windows (a note): nothing to name.
        Outcome::OwnProcess => NoteOrigin::desktop(),
        Outcome::TimedOut { target } if target.shell == Some(Shell::Desktop) => {
            NoteOrigin::desktop()
        }
        Outcome::Elevated { .. } | Outcome::TimedOut { .. } | Outcome::Failed(_) => {
            app_origin(facts)
        }
    }
}

fn snapshot_origin(snapshot: &Snapshot, facts: &WindowFacts) -> NoteOrigin {
    if snapshot.target.shell == Some(Shell::Desktop) {
        return NoteOrigin::desktop();
    }
    let exe = facts.exe.as_deref().or(snapshot.target.exe.as_deref());
    let mut origin = app_origin(&WindowFacts {
        exe: exe.map(str::to_string),
        class_name: if facts.class_name.is_empty() {
            snapshot.target.class_name.clone()
        } else {
            facts.class_name.clone()
        },
        ..facts.clone()
    });
    if exe.is_some_and(is_browser)
        && let Some(page) = page_of(&snapshot.hit)
        && let Some(url) = page.element().url.as_deref()
        && WEB_SCHEMES.iter().any(|scheme| {
            url.len() > scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme)
        })
    {
        origin.kind = NoteKind::Web;
        origin.url = Some(url.to_string());
        let name = page.element().name.trim();
        if !name.is_empty() {
            origin.window_title = Some(name.to_string());
        }
    }
    origin
}

/// An `App` origin from the facts; the desktop when nothing is known.
fn app_origin(facts: &WindowFacts) -> NoteOrigin {
    if shell_kind(&facts.class_name) == Some(Shell::Desktop) {
        return NoteOrigin::desktop();
    }
    let Some(exe) = facts.exe.as_deref() else {
        return NoteOrigin::desktop();
    };
    let name = if facts.class_name == "CabinetWClass" {
        "File Explorer".to_string()
    } else {
        facts
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| exe_stem(exe).to_string())
    };
    let title = facts.title.trim();
    NoteOrigin {
        kind: NoteKind::App,
        app_name: Some(name),
        bundle_id: Some(facts.app_id.clone().unwrap_or_else(|| exe.to_string())),
        window_title: (!title.is_empty()).then(|| title.to_string()),
        url: None,
    }
}

/// The outermost page above (or at) the hit: inner ones are frames.
fn page_of(hit: &UiaNode) -> Option<UiaNode> {
    let mut found = None;
    let mut node = Some(hit.clone());
    while let Some(current) = node {
        if current.element().role == "AXWebArea" && current.element().url.is_some() {
            found = Some(current.clone());
        }
        node = current.parent();
    }
    found
}

fn is_web(framework: &str) -> bool {
    WEB_FRAMEWORKS.contains(&framework)
}

pub fn is_browser(exe: &str) -> bool {
    BROWSER_EXES.iter().any(|b| b.eq_ignore_ascii_case(exe))
}

fn exe_stem(exe: &str) -> &str {
    if exe.len() > 4 && exe[exe.len() - 4..].eq_ignore_ascii_case(".exe") {
        &exe[..exe.len() - 4]
    } else {
        exe
    }
}

// ---------------------------------------------------------------------------------------
// Win32 facts
// ---------------------------------------------------------------------------------------

/// Friendly app names by exe path, so the version resource is read once per exe.
#[derive(Debug, Default)]
pub struct AppNames {
    names: HashMap<String, Option<String>>,
}

impl AppNames {
    /// `FileDescription` of the exe at `path`, read once and cached.
    pub fn description(&mut self, path: &str) -> Option<String> {
        if let Some(name) = self.names.get(path) {
            return name.clone();
        }
        if self.names.len() >= NAME_CACHE_LIMIT {
            self.names.clear();
        }
        let name = file_description(path);
        self.names.insert(path.to_string(), name.clone());
        name
    }
}

/// The facts about the top-level window at `point` (physical px). `hit_pid` is the process
/// UI Automation found at the point, used for UWP apps whose window belongs to
/// `ApplicationFrameHost.exe`. Our own windows give empty facts (reading their title
/// would send a message to our own threads).
pub fn facts_at(point: (i32, i32), hit_pid: Option<u32>, names: &mut AppNames) -> WindowFacts {
    // SAFETY: value arguments only; reads window manager state, sends no message.
    let root = unsafe {
        let child = WindowFromPoint(POINT {
            x: point.0,
            y: point.1,
        });
        if child.is_invalid() {
            return WindowFacts::default();
        }
        let root = GetAncestor(child, GA_ROOT);
        if root.is_invalid() { child } else { root }
    };
    let mut pid = 0;
    // SAFETY: `pid` outlives the call.
    unsafe { GetWindowThreadProcessId(root, Some(&mut pid)) };
    // SAFETY: no arguments.
    if pid == 0 || pid == unsafe { GetCurrentProcessId() } {
        return WindowFacts::default();
    }
    let mut facts = WindowFacts {
        title: window_text(root),
        class_name: class_name(root),
        ..WindowFacts::default()
    };
    let mut path = process_path(pid);
    if path
        .as_deref()
        .is_some_and(|p| file_name(p).eq_ignore_ascii_case("ApplicationFrameHost.exe"))
        && let Some(hit_pid) = hit_pid.filter(|&h| h != pid && h != 0)
    {
        pid = hit_pid;
        path = process_path(pid);
    }
    if let Some(path) = path {
        facts.exe = Some(file_name(&path).to_string());
        facts.description = names.description(&path);
    }
    facts.app_id = app_user_model_id(pid);
    facts
}

fn file_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn window_text(hwnd: HWND) -> String {
    let mut text = [0u16; 512];
    // SAFETY: the buffer outlives the call and its length is passed with it. For a window
    // of another process this reads the caption without sending WM_GETTEXT.
    let len = unsafe { GetWindowTextW(hwnd, &mut text) };
    String::from_utf16_lossy(&text[..len.max(0) as usize])
}

fn class_name(hwnd: HWND) -> String {
    let mut name = [0u16; 256];
    // SAFETY: the buffer outlives the call and its length is passed with it.
    let len = unsafe { GetClassNameW(hwnd, &mut name) };
    String::from_utf16_lossy(&name[..len.max(0) as usize])
}

/// A process handle with query rights, closed on drop.
struct Process(HANDLE);

impl Process {
    fn open(pid: u32) -> Option<Process> {
        // SAFETY: value arguments; the handle is owned by the returned value.
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .ok()
            .map(Process)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: owned by `self`, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn process_path(pid: u32) -> Option<String> {
    let process = Process::open(pid)?;
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    // SAFETY: the buffer outlives the call and `len` holds its size in characters.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&path[..len as usize]))
}

/// The AppUserModelID of a packaged process; None for ordinary desktop apps.
fn app_user_model_id(pid: u32) -> Option<String> {
    let process = Process::open(pid)?;
    let mut len = 0u32;
    // SAFETY: asks for the length only; `len` outlives the call.
    let first = unsafe { GetApplicationUserModelId(process.0, &mut len, None) };
    if first != ERROR_INSUFFICIENT_BUFFER || len == 0 || len > 1024 {
        return None;
    }
    let mut id = vec![0u16; len as usize];
    // SAFETY: the buffer holds `len` characters, as the call was told.
    let result =
        unsafe { GetApplicationUserModelId(process.0, &mut len, Some(PWSTR(id.as_mut_ptr()))) };
    if result != ERROR_SUCCESS {
        return None;
    }
    let end = id.iter().position(|&c| c == 0).unwrap_or(id.len());
    Some(String::from_utf16_lossy(&id[..end])).filter(|s| !s.is_empty())
}

/// `FileDescription` from the version resource of the file at `path`.
fn file_description(path: &str) -> Option<String> {
    let path = HSTRING::from(path);
    // SAFETY: `path` is a valid null-terminated string for the call.
    let size = unsafe { GetFileVersionInfoSizeW(&path, None) };
    if size == 0 || size > 1 << 20 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: `data` holds `size` bytes, as the call is told.
    unsafe { GetFileVersionInfoW(&path, None, size, data.as_mut_ptr().cast()) }.ok()?;
    let mut languages = Vec::new();
    if let Some(table) = query(&data, "\\VarFileInfo\\Translation") {
        for pair in table.as_chunks::<4>().0 {
            let language = u16::from_le_bytes([pair[0], pair[1]]);
            let code_page = u16::from_le_bytes([pair[2], pair[3]]);
            languages.push((language, code_page));
        }
    }
    // US English, Unicode and Windows Latin-1: what most exes without a table use.
    languages.extend([(0x0409, 0x04B0), (0x0409, 0x04E4)]);
    for (language, code_page) in languages {
        let key = format!("\\StringFileInfo\\{language:04x}{code_page:04x}\\FileDescription");
        if let Some(bytes) = query(&data, &key) {
            let text: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .take_while(|&c| c != 0)
                .collect();
            let text = String::from_utf16_lossy(&text).trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// One value of a version resource, as bytes borrowed from `data`.
fn query<'a>(data: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let key = HSTRING::from(key);
    let mut value: *mut c_void = ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `data` is a version resource from `GetFileVersionInfoW`; the call writes a
    // pointer into it and the value's length.
    let found = unsafe {
        VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(key.as_ptr()),
            &mut value,
            &mut len,
        )
    };
    if !found.as_bool() || value.is_null() || len == 0 {
        return None;
    }
    let start = (value as usize).checked_sub(data.as_ptr() as usize)?;
    // String values report their length in characters, others in bytes; read at most to
    // the end of `data` and let the caller stop at the terminator.
    let bytes = if key.to_string().starts_with("\\StringFileInfo") {
        len as usize * 2
    } else {
        len as usize
    };
    let end = start.checked_add(bytes)?.min(data.len());
    (start < end).then(|| &data[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uia::{ElementInfo, Target};
    use std::time::Duration;
    use windows::Win32::UI::Accessibility::*;

    fn el(control: UIA_CONTROLTYPE_ID) -> ElementInfo {
        ElementInfo {
            control_type: control.0,
            enabled: true,
            framework: "Win32".into(),
            ..ElementInfo::default()
        }
    }

    fn web(control: UIA_CONTROLTYPE_ID) -> ElementInfo {
        ElementInfo {
            framework: "Chrome".into(),
            ..el(control)
        }
    }

    fn top(mut e: ElementInfo) -> ElementInfo {
        e.top_level = true;
        e
    }

    fn page(url: &str, title: &str) -> ElementInfo {
        let mut e = web(UIA_DocumentControlTypeId);
        e.value_pattern = true;
        e.value_read_only = Some(true);
        e.text_pattern = true;
        e.url = Some(url.into());
        e.name = title.into();
        e
    }

    fn target(exe: &str, class: &str) -> Target {
        Target {
            pid: 42,
            exe: Some(exe.into()),
            window: 1,
            child_window: 1,
            class_name: class.into(),
            is_terminal: crate::uia::is_terminal(Some(exe), class),
            shell: shell_kind(class),
        }
    }

    fn found(target: Target, chain: Vec<ElementInfo>) -> Outcome {
        Outcome::Found(Snapshot {
            point: (10, 10),
            target,
            hit: UiaNode::from_chain(chain).unwrap(),
            focused: None,
            calls: 1,
            elapsed: Duration::ZERO,
            truncated: false,
            probe: None,
        })
    }

    fn facts(exe: &str, class: &str, title: &str, description: Option<&str>) -> WindowFacts {
        WindowFacts {
            title: title.into(),
            class_name: class.into(),
            exe: Some(exe.into()),
            description: description.map(str::to_string),
            app_id: None,
        }
    }

    // --- The desktop ---

    fn desktop(hit: ElementInfo) -> Outcome {
        let mut view = el(UIA_PaneControlTypeId);
        view.class_name = "SHELLDLL_DefView".into();
        let mut progman = top(el(UIA_PaneControlTypeId));
        progman.class_name = "Progman".into();
        found(target("explorer.exe", "Progman"), vec![hit, view, progman])
    }

    #[test]
    fn desktop_background_is_empty_and_icons_are_not() {
        let mut list = el(UIA_ListControlTypeId);
        list.class_name = "SysListView32".into();
        list.name = "Desktop".into();
        assert!(is_empty_space(&desktop(list)));
        let icon = el(UIA_ListItemControlTypeId);
        assert!(!is_empty_space(&desktop(icon)));
    }

    #[test]
    fn desktop_origin_is_desktop() {
        let outcome = desktop(el(UIA_ListControlTypeId));
        let facts = facts(
            "explorer.exe",
            "Progman",
            "Program Manager",
            Some("Windows Explorer"),
        );
        assert_eq!(origin(&outcome, &facts), NoteOrigin::desktop());
        // Also when the lookup timed out over it.
        let timed_out = Outcome::TimedOut {
            target: target("explorer.exe", "WorkerW"),
        };
        assert_eq!(origin(&timed_out, &facts), NoteOrigin::desktop());
    }

    // --- Notepad ---

    fn notepad(hit: Vec<ElementInfo>) -> Outcome {
        let mut chain = hit;
        let mut window = top(el(UIA_WindowControlTypeId));
        window.name = "notes.txt - Notepad".into();
        chain.push(window);
        found(target("Notepad.exe", "Notepad"), chain)
    }

    #[test]
    fn notepad_text_menu_and_status_bar_are_not_empty() {
        let mut text = el(UIA_DocumentControlTypeId);
        text.text_pattern = true;
        text.focusable = true;
        text.class_name = "RichEditD2DPT".into();
        text.hwnd = 0x55;
        assert!(!is_empty_space(&notepad(vec![text])));
        let menu = el(UIA_MenuItemControlTypeId);
        assert!(!is_empty_space(&notepad(vec![
            menu,
            el(UIA_MenuBarControlTypeId)
        ])));
        let status = el(UIA_TextControlTypeId);
        assert!(!is_empty_space(&notepad(vec![
            status,
            el(UIA_StatusBarControlTypeId)
        ])));
        let title = el(UIA_TitleBarControlTypeId);
        assert!(!is_empty_space(&notepad(vec![title])));
    }

    #[test]
    fn notepad_origin_names_the_app_and_window() {
        let outcome = notepad(vec![el(UIA_PaneControlTypeId)]);
        let mut facts = facts(
            "Notepad.exe",
            "Notepad",
            "notes.txt - Notepad",
            Some("Notepad"),
        );
        facts.app_id = Some("Microsoft.WindowsNotepad_8wekyb3d8bbwe!App".into());
        let o = origin(&outcome, &facts);
        assert_eq!(o.kind, NoteKind::App);
        assert_eq!(o.app_name.as_deref(), Some("Notepad"));
        assert_eq!(
            o.bundle_id.as_deref(),
            Some("Microsoft.WindowsNotepad_8wekyb3d8bbwe!App")
        );
        assert_eq!(o.window_title.as_deref(), Some("notes.txt - Notepad"));
        assert_eq!(o.url, None);
        assert_eq!(o.label(), "Notepad");
    }

    #[test]
    fn without_a_description_the_exe_name_is_the_app_name() {
        let o = origin(
            &notepad(vec![el(UIA_PaneControlTypeId)]),
            &facts("tool.EXE", "ToolWnd", "", None),
        );
        assert_eq!(o.app_name.as_deref(), Some("tool"));
        assert_eq!(o.bundle_id.as_deref(), Some("tool.EXE"));
        assert_eq!(o.window_title, None);
    }

    // --- A Chrome page ---

    fn chrome(hit: Vec<ElementInfo>, url: &str) -> Outcome {
        let mut chain = hit;
        chain.push(page(url, "Rust Book"));
        found(target("chrome.exe", "Chrome_WidgetWin_1"), chain)
    }

    #[test]
    fn chrome_page_margin_is_empty_but_text_links_and_fields_are_not() {
        let url = "https://doc.rust-lang.org/book/";
        // The page itself (its margin), and a plain div.
        assert!(is_empty_space(&found(
            target("chrome.exe", "Chrome_WidgetWin_1"),
            vec![page(url, "Rust Book")]
        )));
        assert!(is_empty_space(&chrome(
            vec![web(UIA_GroupControlTypeId)],
            url
        )));
        // A paragraph's text, a link, a div with role=button, an input.
        assert!(!is_empty_space(&chrome(
            vec![web(UIA_TextControlTypeId)],
            url
        )));
        assert!(!is_empty_space(&chrome(
            vec![web(UIA_TextControlTypeId), web(UIA_HyperlinkControlTypeId)],
            url
        )));
        let mut button = web(UIA_GroupControlTypeId);
        button.aria_role = "button".into();
        assert!(!is_empty_space(&chrome(vec![button], url)));
        let mut input = web(UIA_EditControlTypeId);
        input.value_pattern = true;
        input.value_read_only = Some(false);
        input.focusable = true;
        assert!(!is_empty_space(&chrome(vec![input], url)));
    }

    #[test]
    fn chrome_tab_strip_and_title_area_are_not_empty() {
        // Outside the page: browser chrome, even where it is a plain pane.
        let window = top(web(UIA_PaneControlTypeId));
        assert!(!is_empty_space(&found(
            target("chrome.exe", "Chrome_WidgetWin_1"),
            vec![window.clone()]
        )));
        assert!(!is_empty_space(&found(
            target("chrome.exe", "Chrome_WidgetWin_1"),
            vec![
                web(UIA_PaneControlTypeId),
                web(UIA_TabControlTypeId),
                window
            ]
        )));
    }

    #[test]
    fn chrome_origin_is_the_page() {
        let outcome = chrome(
            vec![web(UIA_GroupControlTypeId)],
            "https://doc.rust-lang.org/book/",
        );
        let facts = facts(
            "chrome.exe",
            "Chrome_WidgetWin_1",
            "Rust Book - Google Chrome",
            Some("Google Chrome"),
        );
        let o = origin(&outcome, &facts);
        assert_eq!(o.kind, NoteKind::Web);
        assert_eq!(o.url.as_deref(), Some("https://doc.rust-lang.org/book/"));
        assert_eq!(o.window_title.as_deref(), Some("Rust Book"));
        assert_eq!(o.app_name.as_deref(), Some("Google Chrome"));
        assert_eq!(o.label(), "doc.rust-lang.org");
        // A page with an internal address is the browser app.
        let settings = chrome(vec![web(UIA_GroupControlTypeId)], "chrome://settings/");
        assert_eq!(origin(&settings, &facts).kind, NoteKind::App);
    }

    #[test]
    fn the_outermost_page_gives_the_address_not_a_frame() {
        let frame = page("https://ads.example/frame", "ad");
        let outcome = found(
            target("msedge.exe", "Chrome_WidgetWin_1"),
            vec![
                web(UIA_GroupControlTypeId),
                frame,
                web(UIA_GroupControlTypeId),
                page("https://news.example/story", "Story"),
            ],
        );
        let o = origin(
            &outcome,
            &facts("msedge.exe", "Chrome_WidgetWin_1", "", None),
        );
        assert_eq!(o.url.as_deref(), Some("https://news.example/story"));
    }

    // --- An Electron app ---

    #[test]
    fn electron_app_is_an_app_with_empty_page_areas() {
        let outcome = found(
            target("slack.exe", "Chrome_WidgetWin_1"),
            vec![
                web(UIA_GroupControlTypeId),
                page("https://app.slack.com/client/T1/C2", "general - Slack"),
            ],
        );
        assert!(is_empty_space(&outcome));
        let o = origin(
            &outcome,
            &facts(
                "slack.exe",
                "Chrome_WidgetWin_1",
                "general - Slack",
                Some("Slack"),
            ),
        );
        assert_eq!(o.kind, NoteKind::App);
        assert_eq!(o.app_name.as_deref(), Some("Slack"));
        assert_eq!(o.window_title.as_deref(), Some("general - Slack"));
        assert_eq!(o.url, None);
        // Its own title bar (outside the page) is not empty.
        let chrome = found(
            target("Code.exe", "Chrome_WidgetWin_1"),
            vec![top(web(UIA_PaneControlTypeId))],
        );
        assert!(!is_empty_space(&chrome));
    }

    // --- File Explorer ---

    fn explorer(hit: ElementInfo) -> Outcome {
        let mut items = el(UIA_PaneControlTypeId);
        items.class_name = "DirectUIHWND".into();
        let mut shell_view = el(UIA_PaneControlTypeId);
        shell_view.class_name = "SHELLDLL_DefView".into();
        let mut window = top(el(UIA_WindowControlTypeId));
        window.name = "Downloads - File Explorer".into();
        found(
            target("explorer.exe", "CabinetWClass"),
            vec![hit, items, shell_view, window],
        )
    }

    #[test]
    fn explorer_items_background_is_empty_and_files_are_not() {
        let mut list = el(UIA_ListControlTypeId);
        list.class_name = "UIItemsView".into();
        assert!(is_empty_space(&explorer(list)));
        assert!(!is_empty_space(&explorer(el(UIA_ListItemControlTypeId))));
        // The column header and the command bar's buttons.
        assert!(!is_empty_space(&explorer(el(UIA_HeaderItemControlTypeId))));
        assert!(!is_empty_space(&explorer(el(UIA_ButtonControlTypeId))));
    }

    #[test]
    fn explorer_origin_is_file_explorer_with_the_folder() {
        let o = origin(
            &explorer(el(UIA_ListControlTypeId)),
            &facts(
                "explorer.exe",
                "CabinetWClass",
                "Downloads",
                Some("Windows Explorer"),
            ),
        );
        assert_eq!(o.kind, NoteKind::App);
        assert_eq!(o.app_name.as_deref(), Some("File Explorer"));
        assert_eq!(o.bundle_id.as_deref(), Some("explorer.exe"));
        assert_eq!(o.window_title.as_deref(), Some("Downloads"));
    }

    // --- Elevated, own, no answer, system surfaces ---

    #[test]
    fn an_elevated_window_is_never_empty_but_still_has_an_origin() {
        let outcome = Outcome::Elevated { pid: 7 };
        assert!(!is_empty_space(&outcome));
        let o = origin(
            &outcome,
            &facts(
                "regedit.exe",
                "RegEdit_RegEdit",
                "Registry Editor",
                Some("Registry Editor"),
            ),
        );
        assert_eq!(o.kind, NoteKind::App);
        assert_eq!(o.app_name.as_deref(), Some("Registry Editor"));
        assert_eq!(o.window_title.as_deref(), Some("Registry Editor"));
    }

    #[test]
    fn own_windows_and_missing_answers_are_not_empty() {
        assert!(!is_empty_space(&Outcome::OwnProcess));
        assert!(!is_empty_space(&Outcome::Failed("x".into())));
        assert!(!is_empty_space(&Outcome::TimedOut {
            target: target("chrome.exe", "Chrome_WidgetWin_1")
        }));
        assert_eq!(
            origin(&Outcome::OwnProcess, &WindowFacts::default()),
            NoteOrigin::desktop()
        );
        // Nothing known at all: the desktop.
        assert_eq!(
            origin(&Outcome::Failed("x".into()), &WindowFacts::default()),
            NoteOrigin::desktop()
        );
    }

    #[test]
    fn taskbar_and_terminals_are_not_empty() {
        let taskbar = found(
            target("explorer.exe", "Shell_TrayWnd"),
            vec![top(el(UIA_PaneControlTypeId))],
        );
        assert!(!is_empty_space(&taskbar));
        let terminal = found(
            target("WindowsTerminal.exe", "CASCADIA_HOSTING_WINDOW_CLASS"),
            vec![el(UIA_PaneControlTypeId), top(el(UIA_WindowControlTypeId))],
        );
        assert!(!is_empty_space(&terminal));
    }

    #[test]
    fn a_dialog_background_is_empty() {
        let outcome = found(
            target("app.exe", "#32770"),
            vec![top(el(UIA_WindowControlTypeId))],
        );
        assert!(is_empty_space(&outcome));
    }

    #[test]
    fn facts_of_no_window_are_empty() {
        let mut names = AppNames::default();
        let facts = facts_at((-1_000_000, -1_000_000), None, &mut names);
        assert_eq!(facts, WindowFacts::default());
    }

    #[test]
    fn reads_the_description_of_a_system_exe() {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let mut names = AppNames::default();
        let path = format!(r"{windir}\explorer.exe");
        let description = names.description(&path);
        assert!(
            description
                .as_deref()
                .is_some_and(|d| d.contains("Explorer")),
            "{description:?}"
        );
        // Cached: a second read gives the same without touching the file.
        assert_eq!(names.description(&path), description);
        assert_eq!(names.description(r"C:\no\such\file.exe"), None);
    }
}
