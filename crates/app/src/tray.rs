//! The notification area (tray) icon and its menu, and the app's mark drawn as the icon.
//!
//! The icon lives on a hidden top-level window owned by the thread that calls
//! [`Tray::new`]. That thread must pump messages. The menu is rebuilt from
//! `build_menu` every time it opens, so it always shows the current state.
//!
//! Identity: the icon is identified by HWND + uID ([`TRAY_UID`]), not by a GUID.
//! Windows keys the user's "show in tray" choice on the exe path plus the uID, so the
//! choice survives restarts. A GUID is bound to the exe path the first time it is used:
//! when a dev build moves (another target dir, another worktree), `NIM_ADD` with that
//! GUID fails until the GUID changes. A uID has no such failure mode, and a crashed
//! process cannot leave a GUID registration behind that blocks the next run.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetSystemMetricsForDpi, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CreateIcon, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, EndMenu, FindWindowW, GWLP_USERDATA,
    GetSystemMetrics, GetWindowLongPtrW, HICON, HMENU, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, MSGFLT_ALLOW, PostMessageW, RegisterClassExW,
    RegisterWindowMessageW, SM_CXSMICON, SM_MENUDROPALIGNMENT, SetForegroundWindow,
    SetWindowLongPtrW, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON,
    TrackPopupMenuEx, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_ENDSESSION, WM_NCDESTROY, WM_NULL, WM_QUERYENDSESSION, WM_SETTINGCHANGE,
    WNDCLASSEXW, WS_EX_TOOLWINDOW,
};
use windows::core::{Error, PCWSTR, Result, w};

/// The icon's uID. Together with the exe path, Windows uses it to remember whether the
/// user keeps the icon visible or in the overflow area. Never change it.
pub const TRAY_UID: u32 = 1;

/// The callback message the shell sends to the hidden window.
const CALLBACK_MSG: u32 = WM_APP + 1;
/// `NIN_SELECT | NINF_KEY`; not exported by the `windows` crate.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const CLASS_NAME: PCWSTR = w!("IgorioTrayWindow");
/// Menu command ids are 16-bit in `WM_COMMAND`; stay inside that range.
const MAX_COMMANDS: usize = 0xFFFF;

/// One line of the tray menu.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuItem {
    /// A clickable item. `id` comes back through `on_action`; ids need not be unique,
    /// but `on_action` cannot tell duplicates apart.
    Action {
        id: u32,
        label: String,
        enabled: bool,
        checked: bool,
    },
    Submenu {
        label: String,
        items: Vec<MenuItem>,
    },
    /// A greyed, non-clickable line of text (a section title or status line).
    Label(String),
    Separator,
}

impl MenuItem {
    /// An enabled, unchecked action.
    pub fn action(id: u32, label: impl Into<String>) -> MenuItem {
        MenuItem::Action {
            id,
            label: label.into(),
            enabled: true,
            checked: false,
        }
    }
}

/// A request to end the app that reached the tray window (see [`Tray::on_end`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndRequest {
    /// WM_CLOSE: `taskkill` without /F, or any tool that closes the app's windows.
    Close,
    /// WM_QUERYENDSESSION: Windows asks whether the session may end. The tray answers yes.
    QueryEndSession,
    /// WM_ENDSESSION with wParam TRUE: the session ends. Windows may end the process as soon
    /// as the handler returns.
    EndSession,
}

/// The request a window message carries, if any. WM_ENDSESSION with wParam FALSE means the
/// session end was cancelled: nothing to do.
pub fn end_request(msg: u32, wparam: usize) -> Option<EndRequest> {
    match msg {
        WM_CLOSE => Some(EndRequest::Close),
        WM_QUERYENDSESSION => Some(EndRequest::QueryEndSession),
        WM_ENDSESSION if wparam != 0 => Some(EndRequest::EndSession),
        _ => None,
    }
}

/// Receives the [`EndRequest`]s, see [`Tray::on_end`].
type OnEnd = Box<dyn FnMut(EndRequest)>;

/// The tray icon. Not `Send`: it must be dropped on the thread that created it.
pub struct Tray {
    inner: Rc<Inner>,
}

/// State shared by [`Tray`] and the window procedure. The window holds one strong
/// reference (released in `WM_NCDESTROY`); every running window procedure call holds
/// another, so a `Tray` dropped from inside a callback cannot free state still in use.
struct Inner {
    hwnd: Cell<HWND>,
    taskbar_created: u32,
    tip: RefCell<String>,
    icon: Cell<HICON>,
    look: Cell<Look>,
    build_menu: RefCell<Box<dyn FnMut() -> Vec<MenuItem>>>,
    on_action: RefCell<Box<dyn FnMut(u32)>>,
    pending: RefCell<VecDeque<u32>>,
    on_end: RefCell<Option<OnEnd>>,
    menu_open: Cell<bool>,
    closing: Cell<bool>,
}

/// What the icon bitmap depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Look {
    size: u32,
    light_taskbar: bool,
}

impl Tray {
    /// Creates a hidden window on the calling thread (which must pump messages) and adds
    /// the icon. `build_menu` is called each time the menu is about to open and returns
    /// its items. `on_action` is called with the id of the chosen [`MenuItem::Action`]
    /// after the menu has closed.
    ///
    /// Fails if the shell refuses the icon (for example, Explorer is not running yet).
    pub fn new(
        tooltip: &str,
        build_menu: Box<dyn FnMut() -> Vec<MenuItem>>,
        on_action: Box<dyn FnMut(u32)>,
    ) -> Result<Tray> {
        // SAFETY: a null module name returns the handle of the exe; no pointers kept.
        let hinstance = unsafe { GetModuleHandleW(None) }?.into();
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        // SAFETY: `class` is fully initialised and its strings are 'static. A second
        // registration fails with ERROR_CLASS_ALREADY_EXISTS, which is fine.
        unsafe { RegisterClassExW(&class) };
        // SAFETY: a 'static, null-terminated string.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };

        let look = current_look();
        let icon = icon_from_pixmap(&logo_pixmap(look.size, stroke_color(look.light_taskbar)))?;
        let inner = Rc::new(Inner {
            hwnd: Cell::new(HWND::default()),
            taskbar_created,
            tip: RefCell::new(tooltip.to_owned()),
            icon: Cell::new(icon),
            look: Cell::new(look),
            build_menu: RefCell::new(build_menu),
            on_action: RefCell::new(on_action),
            pending: RefCell::new(VecDeque::new()),
            on_end: RefCell::new(None),
            menu_open: Cell::new(false),
            closing: Cell::new(false),
        });

        // A hidden top-level window, not a message-only one: message-only windows do
        // not get broadcasts (TaskbarCreated, WM_SETTINGCHANGE, WM_DISPLAYCHANGE).
        // SAFETY: the class was registered above; all pointers are 'static strings.
        let created = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!("Igorio tray"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinstance),
                None,
            )
        };
        let hwnd = match created {
            Ok(hwnd) => hwnd,
            Err(e) => {
                // SAFETY: the icon was created above and nothing else refers to it.
                unsafe { DestroyIcon(icon) }.ok();
                return Err(e);
            }
        };
        inner.hwnd.set(hwnd);
        let raw = Rc::into_raw(inner.clone());
        // SAFETY: `hwnd` is our window. The pointer stays valid while the window holds
        // this strong count; `wndproc` releases it in WM_NCDESTROY.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize) };
        if taskbar_created != 0 {
            // Lets TaskbarCreated through UIPI when the app runs elevated.
            // SAFETY: `hwnd` is our window; no out-pointer.
            unsafe { ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, None) }.ok();
        }

        // From here on `Drop` cleans up whatever was created.
        let tray = Tray { inner };
        tray.inner.add()?;
        Ok(tray)
    }

    /// Changes the tooltip. It is cut to 127 UTF-16 units (the shell's limit).
    pub fn set_tooltip(&mut self, tooltip: &str) -> Result<()> {
        *self.inner.tip.borrow_mut() = tooltip.to_owned();
        let mut data = self.inner.data(NIF_TIP | NIF_SHOWTIP);
        data.szTip = tip_wide(tooltip);
        notify(NIM_MODIFY, &data, "NIM_MODIFY (tooltip)")
    }

    /// Where the icon is on screen, in physical pixels. Fails when the shell cannot
    /// say, which can happen while the icon sits in the overflow area.
    pub fn icon_rect(&self) -> Result<RECT> {
        let id = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.inner.hwnd.get(),
            uID: TRAY_UID,
            ..Default::default()
        };
        // SAFETY: `id` is a fully initialised identifier that outlives the call.
        unsafe { Shell_NotifyIconGetRect(&id) }
    }

    /// Hands WM_CLOSE, WM_QUERYENDSESSION and WM_ENDSESSION(TRUE) to `on_end` instead of the
    /// default handling (which destroys the window on WM_CLOSE). It runs inside the window
    /// procedure, so for [`EndRequest::EndSession`] it must finish its work before it returns.
    /// The tray answers WM_QUERYENDSESSION with TRUE: it never blocks the session end.
    pub fn on_end(&self, on_end: Box<dyn FnMut(EndRequest)>) {
        *self.inner.on_end.borrow_mut() = Some(on_end);
    }

    /// A short notice from the icon (shown as a notification on Windows 10 and 11), for news
    /// the overlay cannot show because nothing is on the pile. No sound. The title and text
    /// are cut to the shell's limits (63 and 255 UTF-16 units).
    pub fn notice(&self, title: &str, text: &str) -> Result<()> {
        let mut data = self.inner.data(NIF_INFO);
        data.szInfoTitle = fixed_wide(title);
        data.szInfo = fixed_wide(text);
        data.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
        notify(NIM_MODIFY, &data, "NIM_MODIFY (notice)")
    }

    /// The hidden window that owns the icon. Valid until the `Tray` is dropped.
    pub fn hwnd(&self) -> HWND {
        self.inner.hwnd.get()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let inner = &self.inner;
        inner.closing.set(true);
        if inner.menu_open.get() {
            // Drop runs inside the menu's modal loop (from some other window's handler).
            // Close the menu; the window procedure sees `closing` when it returns.
            // SAFETY: no arguments; acts on this thread's active menu.
            unsafe { EndMenu() }.ok();
        }
        let data = inner.data(NOTIFY_ICON_DATA_FLAGS(0));
        notify(NIM_DELETE, &data, "NIM_DELETE").ok();
        // SAFETY: our window, on its own thread (`Tray` is not Send). WM_NCDESTROY
        // releases the window's reference to `Inner`; ours is still alive here.
        unsafe { DestroyWindow(inner.hwnd.get()) }.ok();
        let icon = inner.icon.replace(HICON::default());
        if !icon.is_invalid() {
            // SAFETY: we created this icon and the shell no longer shows it.
            unsafe { DestroyIcon(icon) }.ok();
        }
    }
}

impl Inner {
    /// An identifier-only NOTIFYICONDATAW with the given flags.
    fn data(&self, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd.get(),
            uID: TRAY_UID,
            uFlags: flags,
            ..Default::default()
        }
    }

    /// NIM_ADD then NIM_SETVERSION. Used at start and after Explorer restarts.
    fn add(&self) -> Result<()> {
        // NIF_SHOWTIP: with version 4 the standard tooltip is off unless asked for.
        let mut data = self.data(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        data.uCallbackMessage = CALLBACK_MSG;
        data.hIcon = self.icon.get();
        data.szTip = tip_wide(&self.tip.borrow());
        notify(NIM_ADD, &data, "NIM_ADD")?;
        let mut data = self.data(NOTIFY_ICON_DATA_FLAGS(0));
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        notify(NIM_SETVERSION, &data, "NIM_SETVERSION")
    }

    /// Redraws the icon if the tray size or taskbar theme changed. Returns true when the
    /// bitmap was replaced.
    fn update_look(&self) -> bool {
        let look = current_look();
        if look == self.look.get() {
            return false;
        }
        let Ok(icon) = icon_from_pixmap(&logo_pixmap(look.size, stroke_color(look.light_taskbar)))
        else {
            return false;
        };
        self.look.set(look);
        let old = self.icon.replace(icon);
        let mut data = self.data(NIF_ICON);
        data.hIcon = icon;
        // Fails harmlessly when Explorer is gone; the next `add` uses the new icon.
        notify(NIM_MODIFY, &data, "NIM_MODIFY (icon)").ok();
        // SAFETY: the shell copies the icon on NIM_ADD/NIM_MODIFY, so the old one is
        // no longer used by anyone.
        unsafe { DestroyIcon(old) }.ok();
        true
    }

    fn handle(&self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if self.closing.get() {
            // SAFETY: forwarding the same arguments the system gave us.
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        if let Some(request) = end_request(msg, wparam.0)
            && self.on_end.try_borrow().is_ok_and(|on_end| on_end.is_some())
        {
            // Busy means a request is being handled further up the stack (it pumped
            // messages); the quit it started covers this one too.
            if let Ok(mut on_end) = self.on_end.try_borrow_mut()
                && let Some(on_end) = on_end.as_mut()
            {
                on_end(request);
            }
            return LRESULT((request == EndRequest::QueryEndSession) as isize);
        }
        match msg {
            CALLBACK_MSG => {
                let event = decode_callback(wparam.0, lparam.0);
                if event.icon_id == TRAY_UID
                    && matches!(event.event, NIN_SELECT | NIN_KEYSELECT | WM_CONTEXTMENU)
                {
                    self.show_menu(hwnd, event.x, event.y);
                }
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                if setting_is_color_set(lparam) {
                    self.update_look();
                }
                // SAFETY: forwarding the same arguments the system gave us.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_DPICHANGED | WM_DISPLAYCHANGE => {
                self.update_look();
                LRESULT(0)
            }
            _ if self.taskbar_created != 0 && msg == self.taskbar_created => {
                self.update_look();
                self.add().ok();
                LRESULT(0)
            }
            // SAFETY: forwarding the same arguments the system gave us.
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn show_menu(&self, hwnd: HWND, x: i32, y: i32) {
        if self.menu_open.get() {
            return;
        }
        // Busy means `build_menu` itself pumped messages and the user clicked again.
        let Ok(items) = self.build_menu.try_borrow_mut().map(|mut build| build()) else {
            return;
        };
        if self.closing.get() {
            return; // Dropped from inside `build_menu`.
        }
        let Ok(menu) = PopupMenu::build(&items) else {
            return;
        };
        // SAFETY: plain call on our own window. The shell lets us take the foreground
        // after a click on our icon; without it the menu would not close on outside
        // clicks.
        let _ = unsafe { SetForegroundWindow(hwnd) };
        // SAFETY: no pointers.
        let align = if unsafe { GetSystemMetrics(SM_MENUDROPALIGNMENT) } != 0 {
            TPM_RIGHTALIGN
        } else {
            TPM_LEFTALIGN
        };
        let flags = align | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY;
        self.menu_open.set(true);
        // SAFETY: `menu` and `hwnd` stay valid for the call: `menu` is owned here, and
        // `Drop` ends the menu before it destroys `hwnd`. With TPM_RETURNCMD the result
        // is the chosen command id, or 0.
        let command = unsafe { TrackPopupMenuEx(menu.handle(), flags.0, x, y, hwnd, None) };
        self.menu_open.set(false);
        if self.closing.get() {
            return;
        }
        // The documented follow-up: makes the next click elsewhere dismiss menus properly.
        // SAFETY: posting a no-op message to our own window.
        unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) }.ok();
        if let Some(id) = menu.action_id(command.0 as u32) {
            drop(menu);
            self.run_action(id);
        }
    }

    /// Runs `on_action` for `id`. If `on_action` is already running further up the stack
    /// (it pumped messages and the user chose again), the id is queued and that outer
    /// call runs it when it returns, so `on_action` never runs re-entrantly.
    fn run_action(&self, id: u32) {
        self.pending.borrow_mut().push_back(id);
        let Ok(mut on_action) = self.on_action.try_borrow_mut() else {
            return;
        };
        while !self.closing.get() {
            let Some(id) = self.pending.borrow_mut().pop_front() else {
                break;
            };
            on_action(id);
        }
    }
}

fn notify(message: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW, what: &str) -> Result<()> {
    // SAFETY: `data` is fully initialised with the right cbSize and outlives the call.
    if unsafe { Shell_NotifyIconW(message, data) }.as_bool() {
        Ok(())
    } else {
        // Shell_NotifyIconW does not reliably set the last error.
        Err(Error::new(
            E_FAIL,
            format!("Shell_NotifyIconW {what} failed"),
        ))
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: reading our own window's user data.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Inner;
    if ptr.is_null() {
        // Messages during CreateWindowExW, before the state is attached.
        // SAFETY: forwarding the same arguments the system gave us.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    if msg == WM_NCDESTROY {
        // SAFETY: detach first so no later message sees the pointer, then give back the
        // strong count taken by `Rc::into_raw` in `Tray::new`.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        // SAFETY: `ptr` came from `Rc::into_raw` and its count has not been released.
        drop(unsafe { Rc::from_raw(ptr) });
        // SAFETY: forwarding the same arguments the system gave us.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // Hold our own strong count for the whole call. Callbacks may pump messages or drop
    // the `Tray` (which destroys the window and releases the window's count), and `Inner`
    // must outlive this frame.
    // SAFETY: `ptr` came from `Rc::into_raw` and the window still holds that count.
    let inner = unsafe {
        Rc::increment_strong_count(ptr);
        Rc::from_raw(ptr)
    };
    inner.handle(hwnd, msg, wparam, lparam)
}

/// True when a WM_SETTINGCHANGE is about the light/dark theme.
fn setting_is_color_set(lparam: LPARAM) -> bool {
    if lparam.0 == 0 {
        return false;
    }
    // SAFETY: for WM_SETTINGCHANGE a non-zero lParam is a null-terminated string that
    // lives for the duration of the message.
    let name = PCWSTR(lparam.0 as *const u16);
    // SAFETY: see above; the slice is used only inside this statement.
    "ImmersiveColorSet"
        .encode_utf16()
        .eq(unsafe { name.as_wide() }.iter().copied())
}

// ---------------------------------------------------------------------------
// Pure helpers

/// A decoded NOTIFYICON_VERSION_4 callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Callback {
    /// NIN_SELECT, NIN_KEYSELECT, WM_CONTEXTMENU, WM_MOUSEMOVE, ...
    event: u32,
    icon_id: u32,
    /// Anchor point in screen coordinates (signed: monitors left of or above the primary).
    x: i32,
    y: i32,
}

/// Version 4 layout: LOWORD(lParam) = event, HIWORD(lParam) = icon uID,
/// GET_X_LPARAM(wParam) / GET_Y_LPARAM(wParam) = anchor point.
fn decode_callback(wparam: usize, lparam: isize) -> Callback {
    Callback {
        event: (lparam as usize & 0xFFFF) as u32,
        icon_id: ((lparam as usize >> 16) & 0xFFFF) as u32,
        x: (wparam & 0xFFFF) as u16 as i16 as i32,
        y: ((wparam >> 16) & 0xFFFF) as u16 as i16 as i32,
    }
}

/// Menu text: `&` would mark a mnemonic and `\t` starts the accelerator column, so
/// escape the first and flatten the second. Null-terminated UTF-16.
fn menu_text(label: &str) -> Vec<u16> {
    let mut text = String::with_capacity(label.len());
    for c in label.chars() {
        match c {
            '&' => text.push_str("&&"),
            '\t' => text.push(' '),
            '\0' => {}
            c => text.push(c),
        }
    }
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The tooltip as the fixed `szTip` buffer: at most 127 units plus the null, never
/// ending in half a surrogate pair.
fn tip_wide(tip: &str) -> [u16; 128] {
    fixed_wide(tip)
}

/// `text` in a fixed buffer of `N` units: at most `N - 1` units plus the null, never ending in
/// half a surrogate pair.
fn fixed_wide<const N: usize>(text: &str) -> [u16; N] {
    let mut out = [0u16; N];
    let mut len = 0;
    for c in text.chars().filter(|&c| c != '\0') {
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        if len + units.len() > N - 1 {
            break;
        }
        out[len..len + units.len()].copy_from_slice(units);
        len += units.len();
    }
    out
}

/// Action ids in menu order (depth first). Command id `n` (1-based) is entry `n - 1`.
/// `PopupMenu::build` walks the items in the same order.
fn command_table(items: &[MenuItem]) -> Vec<u32> {
    fn walk(items: &[MenuItem], out: &mut Vec<u32>) {
        for item in items {
            match item {
                MenuItem::Action { id, .. } => out.push(*id),
                MenuItem::Submenu { items, .. } => walk(items, out),
                MenuItem::Label(_) | MenuItem::Separator => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(items, &mut out);
    out
}

fn action_for_command(table: &[u32], command: u32) -> Option<u32> {
    let index = (command as usize).checked_sub(1)?;
    table.get(index).copied()
}

// ---------------------------------------------------------------------------
// Menu

/// A built Win32 popup menu and the table that maps its command ids back to
/// [`MenuItem::Action`] ids. Destroyed on drop. Public so tests can inspect it.
pub struct PopupMenu {
    hmenu: HMENU,
    commands: Vec<u32>,
}

impl PopupMenu {
    pub fn build(items: &[MenuItem]) -> Result<PopupMenu> {
        let commands = command_table(items);
        if commands.len() > MAX_COMMANDS {
            return Err(Error::new(E_INVALIDARG, "too many menu items"));
        }
        // SAFETY: no arguments; the handle is owned by the returned value.
        let menu = PopupMenu {
            hmenu: unsafe { CreatePopupMenu() }?,
            commands,
        };
        let mut next = 1;
        append_items(menu.hmenu, items, &mut next)?;
        Ok(menu)
    }

    pub fn handle(&self) -> HMENU {
        self.hmenu
    }

    /// The [`MenuItem::Action`] id for a Win32 command id (0 means "nothing chosen").
    pub fn action_id(&self, command: u32) -> Option<u32> {
        action_for_command(&self.commands, command)
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        // SAFETY: we own the menu; this also destroys its submenus.
        unsafe { DestroyMenu(self.hmenu) }.ok();
    }
}

fn append_items(hmenu: HMENU, items: &[MenuItem], next: &mut usize) -> Result<()> {
    for item in items {
        match item {
            MenuItem::Action {
                label,
                enabled,
                checked,
                ..
            } => {
                let mut flags = MF_STRING;
                if !enabled {
                    flags |= MF_GRAYED;
                }
                if *checked {
                    flags |= MF_CHECKED;
                }
                append(hmenu, flags, *next, Some(label))?;
                *next += 1;
            }
            MenuItem::Submenu { label, items } => {
                // SAFETY: no arguments.
                let sub = unsafe { CreatePopupMenu() }?;
                // Attach first so the parent owns (and on error destroys) the submenu.
                if let Err(e) = append(hmenu, MF_STRING | MF_POPUP, sub.0 as usize, Some(label)) {
                    // SAFETY: not attached, so we still own it.
                    unsafe { DestroyMenu(sub) }.ok();
                    return Err(e);
                }
                append_items(sub, items, next)?;
            }
            MenuItem::Label(label) => append(hmenu, MF_STRING | MF_GRAYED, 0, Some(label))?,
            MenuItem::Separator => append(hmenu, MF_SEPARATOR, 0, None)?,
        }
    }
    Ok(())
}

fn append(hmenu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, label: Option<&str>) -> Result<()> {
    let text = label.map(menu_text);
    let ptr = text.as_ref().map_or(PCWSTR::null(), |t| PCWSTR(t.as_ptr()));
    // SAFETY: `text` is null-terminated and outlives the call; the menu copies it.
    unsafe { AppendMenuW(hmenu, flags, id, ptr) }
}

// ---------------------------------------------------------------------------
// Icon

/// The mark: a friendly mouse pointer as one closed, rounded outline with two happy
/// closed eyes. Design space about 227 x 291 units, y down (assets/igorio-mark.svg).
const MARK_START: (f32, f32) = (52.8, 9.0);
/// The outline's cubic segments after `MARK_START`: two controls and the end point.
const MARK_OUTLINE: [[f32; 6]; 12] = [
    [29.0, 9.0, 19.5, 41.3, 16.0, 61.0],
    [10.8, 90.5, 9.0, 128.5, 9.0, 161.0],
    [9.0, 198.5, 17.8, 245.0, 44.0, 245.0],
    [56.5, 245.0, 70.2, 230.5, 82.8, 230.5],
    [91.5, 230.5, 93.8, 243.5, 94.8, 254.8],
    [95.7, 266.0, 109.0, 281.8, 124.0, 281.8],
    [139.0, 281.8, 151.5, 263.5, 151.5, 251.0],
    [151.5, 239.8, 152.1, 212.3, 158.2, 208.0],
    [167.5, 201.5, 187.9, 203.5, 199.0, 201.5],
    [213.8, 198.9, 220.4, 185.8, 217.8, 171.0],
    [215.1, 156.2, 164.8, 113.0, 136.5, 84.8],
    [108.2, 56.5, 70.2, 9.0, 52.8, 9.0],
];
/// The eyes: upward arcs, given by the middle of each arc's base line.
const MARK_EYES: [(f32, f32); 2] = [(52.7, 133.5), (115.5, 133.5)];
/// Half the width of an eye's arc, and how far its controls lift above the base line.
const MARK_EYE_HALF_WIDTH: f32 = 18.5;
const MARK_EYE_LIFT: f32 = 21.0;

/// Stroke widths (outline, eyes) in design units. Small icons get thicker strokes, so
/// the lines stay solid.
fn mark_strokes(size: u32) -> (f32, f32) {
    match size {
        0..=22 => (26.0, 13.0),
        23..=40 => (19.0, 17.0),
        _ => (17.5, 13.0),
    }
}

/// The mark's blue on light backgrounds and taskbars (straight RGBA).
pub const MARK_BLUE: [u8; 4] = [0x3b, 0x7b, 0xea, 255];
/// The mark's lighter blue on dark backgrounds and taskbars.
pub const MARK_BLUE_DARK: [u8; 4] = [0x6f, 0xa2, 0xf7, 255];

fn mark_outline() -> Path {
    let mut p = PathBuilder::new();
    p.move_to(MARK_START.0, MARK_START.1);
    for [x1, y1, x2, y2, x, y] in MARK_OUTLINE {
        p.cubic_to(x1, y1, x2, y2, x, y);
    }
    p.close();
    p.finish().expect("mark outline is not empty")
}

/// One eye's arc over its base-line middle (`x`, `y`).
fn mark_eye(x: f32, y: f32) -> Option<Path> {
    let (w, lift) = (MARK_EYE_HALF_WIDTH, MARK_EYE_LIFT);
    let mut p = PathBuilder::new();
    p.move_to(x - w, y);
    p.cubic_to(x - w, y - lift, x + w, y - lift, x + w, y);
    p.finish()
}

/// The mark in straight-alpha RGBA `color`, fitted and centred into a `size` x `size`
/// pixmap (premultiplied, like every tiny-skia pixmap). Sizes up to 22 px use the
/// thicker small-size outline.
///
/// Below 2.6 px per eye (sizes under about 22 px) the eyes are crisp whole pixels: a
/// small arch three pixels wide, or a two-pixel dash under 2 px. Anti-aliased arcs that small blur
/// into grey smears that touch the outline.
pub fn logo_pixmap(size: u32, color: [u8; 4]) -> Pixmap {
    let size = size.max(1);
    let mut pixmap = Pixmap::new(size, size).expect("size is at least 1");
    let (outline_width, eye_width) = mark_strokes(size);
    let stroke = |width| Stroke {
        width,
        line_cap: tiny_skia::LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        ..Default::default()
    };
    let Some(outline) = mark_outline().stroke(&stroke(outline_width), 1.0) else {
        return pixmap;
    };
    let Some(bounds) = outline.compute_tight_bounds() else {
        return pixmap;
    };
    // Fit the mark to the icon on the stroke's box (the eyes are inside it).
    let scale = size as f32 / bounds.width().max(bounds.height());
    let transform = Transform::from_translate(size as f32 / 2.0, size as f32 / 2.0)
        .pre_scale(scale, scale)
        .pre_translate(
            -(bounds.left() + bounds.right()) / 2.0,
            -(bounds.top() + bounds.bottom()) / 2.0,
        );

    let mut paint = Paint::default();
    paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
    paint.anti_alias = true;
    pixmap.fill_path(&outline, &paint, FillRule::Winding, transform, None);
    let eye_px = 2.0 * MARK_EYE_HALF_WIDTH * scale;
    if eye_px >= 2.6 {
        for (x, y) in MARK_EYES {
            if let Some(eye) = mark_eye(x, y).and_then(|eye| eye.stroke(&stroke(eye_width), 1.0)) {
                pixmap.fill_path(&eye, &paint, FillRule::Winding, transform, None);
            }
        }
        return pixmap;
    }
    paint.anti_alias = false;
    for (x, y) in MARK_EYES {
        // The arc's middle height.
        let mut middle = tiny_skia::Point::from_xy(x, y - 0.375 * MARK_EYE_LIFT);
        transform.map_point(&mut middle);
        // Pixels as (dx, dy, coverage): a two-pixel dash, or a three-pixel-wide arch whose
        // top corners are half covered, so it reads round.
        let (dots, left, top): (&[(f32, f32, f32)], f32, f32) = if eye_px < 2.0 {
            (&[(0.0, 0.0, 1.0), (1.0, 0.0, 1.0)], (middle.x - 1.0).round(), (middle.y - 0.5).round())
        } else {
            let arch = &[(0.0, 0.0, 0.5), (1.0, 0.0, 1.0), (2.0, 0.0, 0.5), (0.0, 1.0, 1.0), (2.0, 1.0, 1.0)];
            (arch, middle.x.floor() - 1.0, (middle.y - 1.0).round())
        };
        for &(dx, dy, coverage) in dots {
            paint.set_color_rgba8(color[0], color[1], color[2], (f32::from(color[3]) * coverage) as u8);
            if let Some(dot) = tiny_skia::Rect::from_xywh(left + dx, top + dy, 1.0, 1.0) {
                pixmap.fill_rect(dot, &paint, Transform::identity(), None);
            }
        }
    }
    pixmap
}

/// The mark's blue for the taskbar: the darker blue on a light taskbar, the lighter one
/// on a dark taskbar.
fn stroke_color(light_taskbar: bool) -> [u8; 4] {
    if light_taskbar { MARK_BLUE } else { MARK_BLUE_DARK }
}

/// The AND mask (1 bpp, rows padded to 16 bits, 1 = transparent) and the XOR bitmap
/// (32 bpp BGRA, straight alpha, top-down) for `CreateIcon`. Icon colour bitmaps use
/// straight alpha, like .ico files; tiny-skia pixmaps are premultiplied.
fn icon_bits(pixmap: &Pixmap) -> (Vec<u8>, Vec<u8>) {
    let width = pixmap.width() as usize;
    let stride = width.div_ceil(16) * 2;
    let mut mask = vec![0u8; stride * pixmap.height() as usize];
    let mut color = Vec::with_capacity(pixmap.pixels().len() * 4);
    for (i, px) in pixmap.pixels().iter().enumerate() {
        let c = px.demultiply();
        color.extend_from_slice(&[c.blue(), c.green(), c.red(), c.alpha()]);
        if c.alpha() == 0 {
            let (row, col) = (i / width, i % width);
            mask[row * stride + col / 8] |= 0x80 >> (col % 8);
        }
    }
    (mask, color)
}

fn icon_from_pixmap(pixmap: &Pixmap) -> Result<HICON> {
    let (mask, color) = icon_bits(pixmap);
    // SAFETY: `mask` and `color` have exactly the sizes CreateIcon reads for a
    // width x height icon at 1 plane, 32 bpp; it copies them.
    unsafe {
        CreateIcon(
            None,
            pixmap.width() as i32,
            pixmap.height() as i32,
            1,
            32,
            mask.as_ptr(),
            color.as_ptr(),
        )
    }
}

/// Icon size for the tray's DPI and the taskbar theme, read now.
fn current_look() -> Look {
    let dpi = tray_dpi();
    // SAFETY: no pointers.
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
    Look {
        size: if size > 0 { size as u32 } else { 16 },
        light_taskbar: taskbar_is_light(),
    }
}

/// DPI of the monitor with the main taskbar (where the notification area is).
fn tray_dpi() -> u32 {
    // SAFETY: 'static class name, no window name.
    let monitor: HMONITOR = match unsafe { FindWindowW(w!("Shell_TrayWnd"), None) } {
        // SAFETY: a window handle; the call only reads it.
        Ok(taskbar) => unsafe { MonitorFromWindow(taskbar, MONITOR_DEFAULTTOPRIMARY) },
        // SAFETY: plain value arguments.
        Err(_) => unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) },
    };
    let (mut x, mut y) = (96, 96);
    // SAFETY: both out-pointers point at live locals.
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_ok() && x > 0 {
        x
    } else {
        96
    }
}

/// `SystemUsesLightTheme` under `HKCU\...\Themes\Personalize`. Missing means a dark taskbar.
fn taskbar_is_light() -> bool {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` is a live u32 and `size` says so; the strings are 'static.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && value != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(id: u32, label: &str) -> MenuItem {
        MenuItem::action(id, label)
    }

    #[test]
    fn command_table_walks_depth_first() {
        let items = vec![
            MenuItem::Label("Status".into()),
            act(10, "A"),
            MenuItem::Separator,
            MenuItem::Submenu {
                label: "More".into(),
                items: vec![
                    act(20, "B"),
                    MenuItem::Submenu {
                        label: "Deeper".into(),
                        items: vec![act(30, "C")],
                    },
                ],
            },
            act(40, "D"),
        ];
        let table = command_table(&items);
        assert_eq!(table, vec![10, 20, 30, 40]);
        assert_eq!(action_for_command(&table, 0), None);
        assert_eq!(action_for_command(&table, 1), Some(10));
        assert_eq!(action_for_command(&table, 3), Some(30));
        assert_eq!(action_for_command(&table, 4), Some(40));
        assert_eq!(action_for_command(&table, 5), None);
    }

    #[test]
    fn command_ids_do_not_depend_on_user_ids() {
        // User ids may be 0 or above 16 bits; command ids are always 1..=n.
        let table = command_table(&[act(0, "zero"), act(u32::MAX, "max")]);
        assert_eq!(action_for_command(&table, 1), Some(0));
        assert_eq!(action_for_command(&table, 2), Some(u32::MAX));
    }

    #[test]
    fn menu_text_escapes_ampersand_and_tab() {
        let decode = |v: Vec<u16>| {
            assert_eq!(v.last(), Some(&0));
            String::from_utf16(&v[..v.len() - 1]).unwrap()
        };
        assert_eq!(decode(menu_text("Save & Quit")), "Save && Quit");
        assert_eq!(decode(menu_text("&&")), "&&&&");
        assert_eq!(decode(menu_text("a\tb")), "a b");
        assert_eq!(decode(menu_text("nul\0here")), "nulhere");
        assert_eq!(decode(menu_text("Settings…")), "Settings…");
    }

    #[test]
    fn tip_is_cut_at_127_units_without_splitting_pairs() {
        let short = tip_wide("Igorio");
        assert_eq!(String::from_utf16(&short[..6]).unwrap(), "Igorio");
        assert_eq!(short[6], 0);

        let long = tip_wide(&"x".repeat(300));
        assert!(long[..127].iter().all(|&u| u == 'x' as u16));
        assert_eq!(long[127], 0);

        // 126 units, then a pair that would need units 126 and 127: dropped whole.
        let tip = format!("{}😀", "x".repeat(126));
        let cut = tip_wide(&tip);
        assert_eq!(cut[125], 'x' as u16);
        assert_eq!(cut[126], 0);

        // The notice title has room for 63 units.
        let title: [u16; 64] = fixed_wide(&"t".repeat(100));
        assert!(title[..63].iter().all(|&u| u == u16::from(b't')));
        assert_eq!(title[63], 0);
    }

    #[test]
    fn decode_callback_version_4_layout() {
        // lParam: event in the low word, uID in the high word.
        let lparam = ((TRAY_UID as isize) << 16) | WM_CONTEXTMENU as isize;
        // wParam: x = -5 (left of the primary monitor), y = 1040.
        let wparam = ((1040usize) << 16) | (-5i16 as u16 as usize);
        let cb = decode_callback(wparam, lparam);
        assert_eq!(
            cb,
            Callback {
                event: WM_CONTEXTMENU,
                icon_id: TRAY_UID,
                x: -5,
                y: 1040
            }
        );
        let cb = decode_callback(0x8000_7FFF, ((2isize) << 16) | NIN_KEYSELECT as isize);
        assert_eq!(
            (cb.event, cb.icon_id, cb.x, cb.y),
            (NIN_KEYSELECT, 2, 32767, -32768)
        );
    }

    #[test]
    fn icon_bits_use_straight_alpha_and_mask_transparent_pixels() {
        let mut pixmap = Pixmap::new(17, 2).unwrap();
        let pixels = pixmap.pixels_mut();
        // Opaque white, half-covered white (premultiplied 128), fully transparent.
        pixels[0] = tiny_skia::PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap();
        pixels[1] = tiny_skia::PremultipliedColorU8::from_rgba(128, 128, 128, 128).unwrap();
        let (mask, color) = icon_bits(&pixmap);
        assert_eq!(&color[0..4], &[255, 255, 255, 255]);
        assert_eq!(&color[4..8], &[255, 255, 255, 128]);
        assert_eq!(&color[8..12], &[0, 0, 0, 0]);
        // 17 px wide: rows are 4 bytes (padded to 16 bits). First two pixels opaque.
        assert_eq!(mask.len(), 8);
        assert_eq!(mask[0], 0b0011_1111);
        assert_eq!(&mask[1..3], &[0xFF, 0x80]);
        assert_eq!(&mask[4..7], &[0xFF, 0xFF, 0x80]);
    }

    #[test]
    fn logo_fills_the_box_and_leaves_corners_clear() {
        for size in [16, 20, 24, 32, 256] {
            let pixmap = logo_pixmap(size, [255, 255, 255, 255]);
            let alpha = |x: u32, y: u32| pixmap.pixel(x, y).unwrap().alpha();
            assert_eq!(alpha(size - 1, size - 1), 0, "size {size}");
            assert_eq!(alpha(size - 1, 0), 0, "size {size}");
            let covered = pixmap.pixels().iter().filter(|p| p.alpha() > 128).count();
            assert!(
                covered > (size * size) as usize / 8,
                "size {size}: {covered}"
            );
            // Fitted: some ink in the top and bottom rows' neighbourhood.
            assert!((0..size).any(|x| alpha(x, 1) > 0), "size {size}");
            assert!((0..size).any(|x| alpha(x, size - 2) > 0), "size {size}");
        }
    }

    #[test]
    fn end_requests_from_window_messages() {
        assert_eq!(end_request(WM_CLOSE, 0), Some(EndRequest::Close));
        assert_eq!(end_request(WM_QUERYENDSESSION, 0), Some(EndRequest::QueryEndSession));
        assert_eq!(end_request(WM_ENDSESSION, 1), Some(EndRequest::EndSession));
        // The session end was cancelled.
        assert_eq!(end_request(WM_ENDSESSION, 0), None);
        assert_eq!(end_request(WM_NULL, 1), None);
        assert_eq!(end_request(CALLBACK_MSG, 0), None);
    }

    #[test]
    fn stroke_color_contrasts_with_taskbar() {
        assert_eq!(stroke_color(true), MARK_BLUE);
        assert_eq!(stroke_color(false), MARK_BLUE_DARK);
    }
}
