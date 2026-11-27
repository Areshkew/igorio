//! The notes panel: the tray's "Notes…" item opens it. Every note in the folder, newest edit
//! first, with a filter field, each note's state (shown, hidden or closed), and the actions to
//! show, hide or delete one, or to hide or show them all.
//!
//! It is a popup of the notes thread, drawn like the settings window and the slash menu: a
//! rounded card with a shadow, the light or dark palette, a selection highlight that slides,
//! and a short fade and rise on opening. The filter is a real EDIT control (IME, accessibility)
//! and the two "all" actions are real buttons; the list is drawn. The panel never acts on the
//! store itself: it sends [`PanelRequest`]s to the notes thread's hub, which answers with a new
//! list ([`Panel::set_notes`]).
//!
//! Keys, with the focus in the filter: type to filter, Up and Down to move, Enter to open the
//! selected note, Ctrl+H to show or hide it, Ctrl+Shift+Delete to delete it, Esc to clear the
//! filter (or close the panel when it is empty), Tab to the buttons.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::time::{Instant, SystemTime};

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL,
    VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_NEXT, VK_PRIOR, VK_RETURN, VK_SHIFT, VK_TAB,
    VK_UP,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use super::editor::{Canvas, Palette, Surface, Transition, mix, px};
use super::store::{Note, NoteId};
use super::window::ago;
use super::window::{focus_ring, hide_icon, trash_icon};

/// Where a note is: on screen, hidden (open but off screen until shown), or closed (put away).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteState {
    Shown,
    Hidden,
    Closed,
}

impl NoteState {
    pub fn label(self) -> &'static str {
        match self {
            NoteState::Shown => "Shown",
            NoteState::Hidden => "Hidden",
            NoteState::Closed => "Closed",
        }
    }
}

/// One row of the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelNote {
    pub id: NoteId,
    pub title: String,
    pub preview: String,
    /// The title and the text in lower case, for the filter.
    pub search: String,
    pub updated_at: SystemTime,
    pub state: NoteState,
}

impl PanelNote {
    pub fn new(note: &Note, state: NoteState) -> Self {
        let title = note.title();
        PanelNote {
            id: note.id,
            preview: preview(&note.text),
            search: format!("{}\n{}", title, note.text).to_lowercase(),
            title,
            updated_at: note.updated_at,
            state,
        }
    }
}

/// What the panel asks of the notes thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelRequest {
    /// Show the note (from hidden or closed) and bring it to the front with focus.
    Open(NoteId),
    /// Hide a shown note, or show a hidden or closed one.
    Toggle(NoteId),
    Delete(NoteId),
    HideAll,
    ShowAll,
    Close,
}

/// The rows a filter keeps, as indices into `notes` (already newest edit first): every word of
/// the query, in lower case, appears somewhere in the title or the text.
pub fn filter(notes: &[PanelNote], query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    notes
        .iter()
        .enumerate()
        .filter(|(_, note)| words.iter().all(|word| note.search.contains(word.as_str())))
        .map(|(index, _)| index)
        .collect()
}

/// How many notes the tray's "Recent Notes" lists.
pub const MENU_NOTES: usize = 5;

/// The notes the tray's "Recent Notes" lists: the five changed last (`recent` is newest first).
pub fn menu_notes<T>(recent: &[T]) -> &[T] {
    &recent[..recent.len().min(MENU_NOTES)]
}

/// Newest edit first (the order of the list), ties by title.
pub fn sort(notes: &mut [PanelNote]) {
    notes.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.title.cmp(&b.title))
    });
}

/// A short preview: the first line with words after the title line, its Markdown markers
/// left out, at most 120 characters.
pub fn preview(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let _title = lines.next();
    let Some(line) = lines.find(|line| line.chars().any(char::is_alphanumeric)) else {
        return String::new();
    };
    let line = line
        .trim_start_matches(['#', '>', '-', '*', '+', ' '])
        .trim_start_matches("[ ] ")
        .trim_start_matches("[x] ")
        .trim_start_matches("[X] ");
    let cleaned: String = line
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '~' | '_'))
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() > 120 {
        let head: String = cleaned.chars().take(119).collect();
        format!("{head}…")
    } else {
        cleaned.to_owned()
    }
}

/// Where the panel goes: next to the tray icon `anchor` on the taskbar's side, inside the work
/// area; when the anchor is missing or not on this monitor (the icon is in the overflow area),
/// in the work area's corner nearest the taskbar. All in physical pixels.
pub fn place(anchor: Option<RECT>, size: (i32, i32), monitor: RECT, work: RECT, gap: i32) -> POINT {
    let (w, h) = size;
    // The taskbar is on the side where the work area is smaller than the monitor.
    let bottom = work.bottom < monitor.bottom;
    let top = work.top > monitor.top;
    let left = work.left > monitor.left;
    let right = work.right < monitor.right;
    let inside = |r: RECT| {
        r.left < monitor.right
            && r.right > monitor.left
            && r.top < monitor.bottom
            && r.bottom > monitor.top
    };
    let anchor = anchor.filter(|r| inside(*r) && r.right > r.left);
    let clamp_x = |x: i32| x.min(work.right - w - gap).max(work.left + gap);
    let clamp_y = |y: i32| y.min(work.bottom - h - gap).max(work.top + gap);
    match anchor {
        Some(a) => {
            let cx = (a.left + a.right) / 2;
            let cy = (a.top + a.bottom) / 2;
            if left {
                POINT {
                    x: work.left + gap,
                    y: clamp_y(cy - h / 2),
                }
            } else if right {
                POINT {
                    x: work.right - w - gap,
                    y: clamp_y(cy - h / 2),
                }
            } else if top {
                POINT {
                    x: clamp_x(cx - w / 2),
                    y: work.top + gap,
                }
            } else {
                POINT {
                    x: clamp_x(cx - w / 2),
                    y: work.bottom - h - gap,
                }
            }
        }
        None => {
            // The corner on the taskbar's side; bottom right when there is no taskbar edge.
            let x = if left {
                work.left + gap
            } else {
                work.right - w - gap
            };
            let y = if top && !bottom {
                work.top + gap
            } else {
                work.bottom - h - gap
            };
            POINT { x, y }
        }
    }
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

/// Logical sizes.
const WIDTH: f64 = 360.0;
const HEIGHT: f64 = 460.0;
const PAD: f64 = 14.0;
const FIELD_H: f64 = 32.0;
const ROW_H: f64 = 56.0;
const FOOTER_H: f64 = 52.0;
const ACTION: f64 = 24.0;
const MOTION_MS: f64 = 120.0;
const TIMER: usize = 1;

pub const FILTER_ID: i32 = 201;
pub const HIDE_ALL_ID: i32 = 202;
pub const SHOW_ALL_ID: i32 = 203;
const CLASS: PCWSTR = w!("IgorioNotesPanel");

/// Fonts for the panel at one DPI, made once per DPI for the process.
#[derive(Clone, Copy)]
struct Fonts {
    title: HFONT,
    body: HFONT,
    small: HFONT,
    button: HFONT,
}

static FONT_SETS: std::sync::Mutex<Vec<(u32, [isize; 4])>> = std::sync::Mutex::new(Vec::new());

impl Fonts {
    fn new(dpi: u32) -> Self {
        let mut sets = FONT_SETS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handles = match sets.iter().find(|(made_for, _)| *made_for == dpi) {
            Some((_, handles)) => *handles,
            None => {
                let make = |size: f64, weight: i32| {
                    // SAFETY: a static family name; the font is kept for the process's life.
                    unsafe {
                        CreateFontW(
                            -px(size, dpi),
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
                        .0 as isize
                    }
                };
                let handles = [
                    make(14.0, 600),
                    make(12.5, 400),
                    make(11.0, 400),
                    make(12.5, 600),
                ];
                sets.push((dpi, handles));
                handles
            }
        };
        let [title, body, small, button] = handles.map(|handle| HFONT(handle as *mut c_void));
        Fonts {
            title,
            body,
            small,
            button,
        }
    }
}

/// The panel window. Dropping it destroys the window.
pub struct Panel {
    state: Box<State>,
}

struct State {
    hwnd: HWND,
    edit: HWND,
    buttons: [HWND; 2],
    notes: RefCell<Vec<PanelNote>>,
    shown: RefCell<Vec<usize>>,
    selected: Cell<usize>,
    highlight: Cell<Transition>,
    hover: Cell<Option<usize>>,
    hot_action: Cell<Option<(usize, usize)>>,
    hot_button: Cell<Option<usize>>,
    scroll: Cell<i32>,
    intro: Cell<Transition>,
    at: Cell<POINT>,
    palette: Cell<Palette>,
    dpi: u32,
    fonts: Fonts,
    motion: bool,
    timer: Cell<bool>,
    epoch: Instant,
    frozen: Cell<Option<f64>>,
    surface: RefCell<Option<Surface>>,
    field_brush: Cell<HBRUSH>,
    request: Box<dyn Fn(PanelRequest)>,
    now: Box<dyn Fn() -> SystemTime>,
}

impl Panel {
    /// Opens the panel near `anchor` (the tray icon), with `notes` newest first. `request`
    /// carries the panel's actions to the hub. `activate` false shows it without taking the
    /// foreground (tests).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        instance: HINSTANCE,
        anchor: Option<RECT>,
        notes: Vec<PanelNote>,
        palette: Palette,
        motion: bool,
        dpi_override: Option<u32>,
        activate: bool,
        request: Box<dyn Fn(PanelRequest)>,
    ) -> Result<Panel> {
        register(instance);
        // The monitor the anchor (or the cursor) is on, and its DPI.
        let mut cursor = POINT::default();
        // SAFETY: reads (never moves) the cursor position into a local.
        let _ = unsafe { GetCursorPos(&mut cursor) };
        let probe = anchor.map_or(cursor, |a| POINT {
            x: (a.left + a.right) / 2,
            y: (a.top + a.bottom) / 2,
        });
        // SAFETY: plain monitor queries into locals.
        let (monitor, work, dpi) = unsafe {
            let handle = MonitorFromPoint(probe, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(handle, &mut info);
            let (mut x, mut y) = (96, 96);
            let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(
                handle,
                windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
                &mut x,
                &mut y,
            );
            (info.rcMonitor, info.rcWork, x.max(96))
        };
        let dpi = dpi_override.unwrap_or(dpi);
        let size = (px(WIDTH, dpi), px(HEIGHT, dpi));
        let at = place(anchor, size, monitor, work, px(10.0, dpi));
        // SAFETY: a top-level popup of our class on the notes thread; captions are static.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_CONTROLPARENT | WS_EX_LAYERED,
                CLASS,
                w!("Notes"),
                WINDOW_STYLE(WS_POPUP.0 | WS_THICKFRAME.0 | WS_CLIPCHILDREN.0),
                at.x,
                at.y,
                size.0,
                size.1,
                None,
                None,
                Some(instance),
                None,
            )?
        };
        let child = |class: PCWSTR, text: PCWSTR, style: u32, id: i32| {
            // SAFETY: a child control of our panel.
            unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    text,
                    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | style),
                    0,
                    0,
                    1,
                    1,
                    Some(hwnd),
                    Some(HMENU(id as usize as *mut c_void)),
                    Some(instance),
                    None,
                )
            }
        };
        let edit = child(w!("EDIT"), w!(""), ES_AUTOHSCROLL as u32, FILTER_ID)?;
        let hide_all = child(
            w!("BUTTON"),
            w!("Hide all"),
            BS_PUSHBUTTON as u32,
            HIDE_ALL_ID,
        )?;
        let show_all = child(
            w!("BUTTON"),
            w!("Show all"),
            BS_PUSHBUTTON as u32,
            SHOW_ALL_ID,
        )?;
        let shown = (0..notes.len()).collect();
        let state = Box::new(State {
            hwnd,
            edit,
            buttons: [hide_all, show_all],
            notes: RefCell::new(notes),
            shown: RefCell::new(shown),
            selected: Cell::new(0),
            highlight: Cell::new(Transition::at(0.0)),
            hover: Cell::new(None),
            hot_action: Cell::new(None),
            hot_button: Cell::new(None),
            scroll: Cell::new(0),
            intro: Cell::new(Transition::at(0.0)),
            at: Cell::new(at),
            palette: Cell::new(palette),
            dpi,
            fonts: Fonts::new(dpi),
            motion,
            timer: Cell::new(false),
            epoch: Instant::now(),
            frozen: Cell::new(None),
            surface: RefCell::new(None),
            field_brush: Cell::new(HBRUSH::default()),
            request,
            now: Box::new(SystemTime::now),
        });
        let pointer = &*state as *const State;
        // SAFETY: the State box outlives the windows (Drop destroys them first); the subclasses
        // are removed on WM_NCDESTROY.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
            let _ = SetWindowSubclass(edit, Some(edit_proc), 1, pointer as usize);
            for button in [hide_all, show_all] {
                let _ = SetWindowSubclass(button, Some(button_proc), 1, pointer as usize);
            }
            SendMessageW(
                edit,
                WM_SETFONT,
                Some(WPARAM(state.fonts.body.0 as usize)),
                Some(LPARAM(1)),
            );
            let corner = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
                4,
            );
            let dark = i32::from(palette.dark);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&dark as *const i32).cast(),
                4,
            );
            let margins = MARGINS {
                cxLeftWidth: 0,
                cxRightWidth: 0,
                cyTopHeight: 0,
                cyBottomHeight: 1,
            };
            let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        }
        let panel = Panel { state };
        panel.state.hide_cues();
        panel.state.set_brush();
        panel.state.layout();
        let now = panel.state.now_ms();
        let mut intro = Transition::at(0.0);
        intro.retarget(1.0, now, if motion { MOTION_MS } else { 0.0 });
        panel.state.intro.set(intro);
        panel.state.step();
        // SAFETY: our panel, shown and given the keyboard focus (its filter field).
        unsafe {
            let _ = ShowWindow(hwnd, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
            if activate {
                let _ = SetForegroundWindow(hwnd);
                let _ = SetFocus(Some(edit));
            }
        }
        panel.state.start_timer();
        Ok(panel)
    }

    pub fn hwnd(&self) -> HWND {
        self.state.hwnd
    }

    pub fn filter_hwnd(&self) -> HWND {
        self.state.edit
    }

    pub fn button_hwnds(&self) -> [HWND; 2] {
        self.state.buttons
    }

    /// A new list (the notes changed): the filter and the selected note are kept.
    pub fn set_notes(&self, notes: Vec<PanelNote>) {
        let s = &*self.state;
        let selected = s.selected_note().map(|note| note.id);
        *s.notes.borrow_mut() = notes;
        s.refilter(selected);
    }

    pub fn set_palette(&self, palette: Palette) {
        let dark = i32::from(palette.dark);
        // SAFETY: DWM copies the value during the call; the window is ours.
        let _ = unsafe {
            DwmSetWindowAttribute(
                self.state.hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&dark as *const i32).cast(),
                4,
            )
        };
        self.state.palette.set(palette);
        self.state.set_brush();
        self.state.invalidate();
    }

    /// The rows the filter keeps, in order, with each note's state, and the selected one
    /// (tests).
    pub fn rows(&self) -> (Vec<(NoteId, NoteState)>, Option<NoteId>) {
        let s = &*self.state;
        let notes = s.notes.borrow();
        let rows = s
            .shown
            .borrow()
            .iter()
            .map(|&index| (notes[index].id, notes[index].state))
            .collect();
        drop(notes);
        (rows, s.selected_note().map(|note| note.id))
    }

    /// Holds the animations at a time (tests), or lets them run (`None`).
    pub fn freeze(&self, at: Option<f64>) {
        self.state.frozen.set(at);
        self.state.step();
    }

    pub fn now_ms(&self) -> f64 {
        self.state.now_ms()
    }

    /// Is a timer running (tests: an idle panel runs none)?
    pub fn animating(&self) -> bool {
        self.state.timer.get()
    }

    /// Clicks a row's own action (tests and the mouse): 0 shows or hides, 1 deletes.
    pub fn row_action(&self, row: usize, action: usize) {
        self.state.row_action(row, action);
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        let s = &*self.state;
        // SAFETY: our own windows and brush; the State they point to is freed after this.
        unsafe {
            let _ = KillTimer(Some(s.hwnd), TIMER);
            if IsWindow(Some(s.hwnd)).as_bool() {
                let _ = DestroyWindow(s.hwnd);
            }
            if !s.field_brush.get().is_invalid() {
                let _ = DeleteObject(s.field_brush.get().into());
            }
        }
    }
}

fn register(instance: HINSTANCE) {
    // SAFETY: static class data; a second registration fails harmlessly.
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(panel_proc),
            hInstance: instance,
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
    }
}

fn key_down(vk: u16) -> bool {
    // SAFETY: a plain key state query for this thread.
    unsafe { GetKeyState(i32::from(vk)) < 0 }
}

impl State {
    fn s(&self, value: f64) -> i32 {
        px(value, self.dpi)
    }

    fn now_ms(&self) -> f64 {
        self.frozen
            .get()
            .unwrap_or_else(|| self.epoch.elapsed().as_secs_f64() * 1000.0)
    }

    fn client(&self) -> RECT {
        let mut rect = RECT::default();
        // SAFETY: our window, a writable local.
        let _ = unsafe { GetClientRect(self.hwnd, &mut rect) };
        rect
    }

    fn invalidate(&self) {
        // SAFETY: our window and its buttons (they paint their part of the card).
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
            for button in self.buttons {
                let _ = InvalidateRect(Some(button), None, false);
            }
        }
    }

    fn set_brush(&self) {
        let palette = self.palette.get();
        // SAFETY: a solid brush we own; the old one is deleted after the swap.
        unsafe {
            let old = self
                .field_brush
                .replace(CreateSolidBrush(field_colour(palette)));
            if !old.is_invalid() {
                let _ = DeleteObject(old.into());
            }
        }
    }

    fn field_rect(&self) -> RECT {
        let client = self.client();
        RECT {
            left: self.s(PAD),
            top: self.s(PAD),
            right: client.right - self.s(PAD),
            bottom: self.s(PAD + FIELD_H),
        }
    }

    fn list_rect(&self) -> RECT {
        let client = self.client();
        RECT {
            left: self.s(8.0),
            top: self.s(PAD + FIELD_H + 10.0),
            right: client.right - self.s(8.0),
            bottom: client.bottom - self.s(FOOTER_H),
        }
    }

    fn button_rects(&self) -> [RECT; 2] {
        let client = self.client();
        let h = self.s(30.0);
        let top = client.bottom - self.s(FOOTER_H) + (self.s(FOOTER_H) - h) / 2;
        let show = RECT {
            left: client.right - self.s(PAD + 90.0),
            top,
            right: client.right - self.s(PAD),
            bottom: top + h,
        };
        let hide = RECT {
            left: show.left - self.s(8.0 + 90.0),
            top,
            right: show.left - self.s(8.0),
            bottom: top + h,
        };
        [hide, show]
    }

    fn layout(&self) {
        let field = self.field_rect();
        let inset = self.s(30.0);
        let line = self.s(20.0);
        let top = field.top + (field.bottom - field.top - line) / 2;
        // SAFETY: our child controls.
        unsafe {
            let _ = SetWindowPos(
                self.edit,
                None,
                field.left + inset,
                top,
                field.right - field.left - inset - self.s(10.0),
                line,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            for (button, rect) in self.buttons.iter().zip(self.button_rects()) {
                let _ = SetWindowPos(
                    *button,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }

    fn query(&self) -> String {
        let mut buffer = [0u16; 512];
        // SAFETY: our edit control copies its text into the local buffer.
        let length = unsafe { GetWindowTextW(self.edit, &mut buffer) };
        String::from_utf16_lossy(&buffer[..length.max(0) as usize])
    }

    fn selected_note(&self) -> Option<PanelNote> {
        let shown = self.shown.borrow();
        let index = *shown.get(self.selected.get())?;
        self.notes.borrow().get(index).cloned()
    }

    /// The filter changed or the list did: keeps `keep` selected when it is still listed.
    fn refilter(&self, keep: Option<NoteId>) {
        let query = self.query();
        let shown = filter(&self.notes.borrow(), &query);
        let notes = self.notes.borrow();
        let selected = keep
            .and_then(|id| shown.iter().position(|&index| notes[index].id == id))
            .unwrap_or(0);
        drop(notes);
        *self.shown.borrow_mut() = shown;
        self.selected.set(selected);
        self.highlight.set(Transition::at(selected as f64));
        self.scroll.set(0);
        self.ensure_visible();
        self.invalidate();
    }

    fn select(&self, index: usize) {
        let count = self.shown.borrow().len();
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        if index == self.selected.get() {
            return;
        }
        self.selected.set(index);
        let mut track = self.highlight.get();
        track.retarget(
            index as f64,
            self.now_ms(),
            if self.motion { MOTION_MS } else { 0.0 },
        );
        self.highlight.set(track);
        self.ensure_visible();
        self.invalidate();
        self.start_timer();
    }

    fn visible_rows(&self) -> i32 {
        let list = self.list_rect();
        ((list.bottom - list.top) / self.s(ROW_H)).max(1)
    }

    fn max_scroll(&self) -> i32 {
        let list = self.list_rect();
        let total = self.shown.borrow().len() as i32 * self.s(ROW_H);
        (total - (list.bottom - list.top)).max(0)
    }

    fn ensure_visible(&self) {
        let list = self.list_rect();
        let row = self.s(ROW_H);
        let top = self.selected.get() as i32 * row;
        let height = list.bottom - list.top;
        let mut scroll = self.scroll.get();
        if top < scroll {
            scroll = top;
        } else if top + row > scroll + height {
            scroll = top + row - height;
        }
        self.scroll.set(scroll.clamp(0, self.max_scroll()));
    }

    fn row_rect(&self, row: usize) -> RECT {
        let list = self.list_rect();
        let top = list.top + row as i32 * self.s(ROW_H) - self.scroll.get();
        RECT {
            left: list.left,
            top,
            right: list.right,
            bottom: top + self.s(ROW_H),
        }
    }

    /// The row and, when on one of its action icons, which (0 show/hide, 1 delete).
    fn hit(&self, x: i32, y: i32) -> Option<(usize, Option<usize>)> {
        let list = self.list_rect();
        if x < list.left || x >= list.right || y < list.top || y >= list.bottom {
            return None;
        }
        let row = ((y - list.top + self.scroll.get()) / self.s(ROW_H)) as usize;
        if row >= self.shown.borrow().len() {
            return None;
        }
        let actions = self.action_rects(self.row_rect(row));
        let action = actions
            .iter()
            .position(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom);
        Some((row, action))
    }

    /// The state pill at a row's bottom right.
    fn pill_rect(&self, row: RECT) -> RECT {
        let right = row.right - self.s(10.0);
        RECT {
            left: right - self.s(62.0),
            top: row.bottom - self.s(28.0),
            right,
            bottom: row.bottom - self.s(10.0),
        }
    }

    /// A row's actions (show or hide, delete), left of its state pill.
    fn action_rects(&self, row: RECT) -> [RECT; 2] {
        let size = self.s(ACTION);
        let pill = self.pill_rect(row);
        let top = (pill.top + pill.bottom - size) / 2;
        let delete = RECT {
            left: pill.left - self.s(6.0) - size,
            top,
            right: pill.left - self.s(6.0),
            bottom: top + size,
        };
        let toggle = RECT {
            left: delete.left - self.s(4.0) - size,
            top,
            right: delete.left - self.s(4.0),
            bottom: top + size,
        };
        [toggle, delete]
    }

    fn open_selected(&self) {
        if let Some(note) = self.selected_note() {
            (self.request)(PanelRequest::Open(note.id));
        }
    }

    fn row_action(&self, row: usize, action: usize) {
        let index = self.shown.borrow().get(row).copied();
        let Some(note) = index.and_then(|index| self.notes.borrow().get(index).cloned()) else {
            return;
        };
        (self.request)(if action == 0 {
            PanelRequest::Toggle(note.id)
        } else {
            PanelRequest::Delete(note.id)
        });
    }

    fn start_timer(&self) {
        let now = self.now_ms();
        let run = self.frozen.get().is_none()
            && (self.intro.get().active(now) || self.highlight.get().active(now));
        if run != self.timer.get() {
            // SAFETY: our window's own timer; the flag follows what Windows did.
            let running = unsafe {
                if run {
                    SetTimer(Some(self.hwnd), TIMER, 10, None) != 0
                } else {
                    let _ = KillTimer(Some(self.hwnd), TIMER);
                    false
                }
            };
            self.timer.set(running);
        }
    }

    /// One animation frame: the panel's fade and rise, the highlight's slide.
    fn step(&self) {
        let now = self.now_ms();
        let intro = self.intro.get().value(now).clamp(0.0, 1.0);
        let rise = ((1.0 - intro) * f64::from(self.s(8.0))).round() as i32;
        let at = self.at.get();
        // SAFETY: our window: its opacity and place for this frame.
        unsafe {
            let _ = SetLayeredWindowAttributes(
                self.hwnd,
                COLORREF(0),
                (intro * 255.0).round() as u8,
                LWA_ALPHA,
            );
            let _ = SetWindowPos(
                self.hwnd,
                None,
                at.x,
                at.y + rise,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.invalidate();
        self.start_timer();
    }

    fn paint_at(&self, target: HDC, origin: POINT, size: Option<(i32, i32)>) {
        let client = self.client();
        let (width, height) = (client.right.max(1) as u32, client.bottom.max(1) as u32);
        let Ok(mut storage) = self.surface.try_borrow_mut() else {
            return;
        };
        if storage
            .as_ref()
            .is_none_or(|surface| surface.width != width || surface.height != height)
        {
            *storage = Surface::new(width, height).ok();
        }
        let Some(surface) = storage.as_mut() else {
            return;
        };
        {
            let mut canvas = surface.canvas();
            self.draw(&mut canvas, client);
            canvas.finish_labels();
        }
        // SAFETY: both DCs are live; the copy covers the target's part of the card.
        unsafe {
            let _ = BitBlt(
                target,
                0,
                0,
                size.map_or(client.right - origin.x, |(w, _)| w),
                size.map_or(client.bottom - origin.y, |(_, h)| h),
                Some(surface.dc),
                origin.x,
                origin.y,
                SRCCOPY,
            );
        }
    }

    fn draw(&self, canvas: &mut Canvas<'_>, client: RECT) {
        let p = self.palette.get();
        let s = |value: f64| self.s(value);
        let fonts = self.fonts;
        let now = self.now_ms();
        canvas.fill(client, 0.0, p.paper, 1.0);
        // The filter field: a rounded well with a magnifier; the edit control sits inside it.
        let field = self.field_rect();
        // SAFETY: focus query on this thread.
        let focused = unsafe { GetFocus() };
        canvas.fill(field, s(9.0) as f32, field_colour(p), 1.0);
        if focused == self.edit {
            focus_ring(canvas, field, s(9.0), s(2.0), p);
        }
        let (cx, cy) = (field.left + s(15.0), (field.top + field.bottom) / 2);
        let glass = RECT {
            left: cx - s(5.0),
            top: cy - s(6.0),
            right: cx + s(4.0),
            bottom: cy + s(3.0),
        };
        canvas.stroke(glass, s(4.5) as f32, p.muted);
        canvas.line(
            ((cx + s(3.0)) as f32, (cy + s(2.0)) as f32),
            ((cx + s(6.0)) as f32, (cy + s(5.0)) as f32),
            p.muted,
            s(1.4) as f32,
        );
        // The list.
        let list = self.list_rect();
        let shown = self.shown.borrow();
        let notes = self.notes.borrow();
        if shown.is_empty() {
            let message = if notes.is_empty() {
                "No notes yet. Hold Alt on empty space and choose New note."
            } else {
                "No notes match the filter."
            };
            let rect = RECT {
                left: list.left + s(16.0),
                top: list.top + s(40.0),
                right: list.right - s(16.0),
                bottom: list.top + s(100.0),
            };
            canvas.label(rect, message, p.muted, fonts.body, DT_CENTER | DT_WORDBREAK);
        }
        let highlight = self.highlight.get().value(now);
        let row_h = s(ROW_H);
        if !shown.is_empty() {
            let top = list.top + (highlight * f64::from(row_h)).round() as i32 - self.scroll.get();
            let rect = RECT {
                left: list.left,
                top: top + s(2.0),
                right: list.right,
                bottom: top + row_h - s(2.0),
            };
            if rect.bottom > list.top && rect.top < list.bottom {
                canvas.fill(clip_y(rect, list), s(8.0) as f32, p.selected, 1.0);
            }
        }
        let now_time = (self.now)();
        for (row, &index) in shown.iter().enumerate() {
            let rect = self.row_rect(row);
            if rect.bottom <= list.top || rect.top >= list.bottom {
                continue;
            }
            let note = &notes[index];
            let hovered = self.hover.get() == Some(row);
            if hovered && row != self.selected.get() {
                canvas.fill(
                    clip_y(
                        RECT {
                            top: rect.top + s(2.0),
                            bottom: rect.bottom - s(2.0),
                            ..rect
                        },
                        list,
                    ),
                    s(8.0) as f32,
                    p.hover,
                    0.7,
                );
            }
            let inner = RECT {
                left: rect.left + s(10.0),
                right: rect.right - s(10.0),
                ..rect
            };
            let title = RECT {
                top: rect.top + s(8.0),
                bottom: rect.top + s(28.0),
                right: inner.right - s(80.0),
                ..inner
            };
            let time = RECT {
                left: inner.right - s(78.0),
                top: title.top,
                right: inner.right,
                bottom: title.bottom,
            };
            let pill = self.pill_rect(rect);
            let [toggle, delete] = self.action_rects(rect);
            let actions = hovered || row == self.selected.get();
            let preview = RECT {
                top: rect.top + s(28.0),
                bottom: rect.bottom - s(8.0),
                right: if actions { toggle.left } else { pill.left } - s(8.0),
                ..inner
            };
            let ink = if note.state == NoteState::Shown {
                p.text
            } else {
                mix(p.paper, p.text, 0.8)
            };
            let label_in = |canvas: &mut Canvas<'_>, r: RECT, text: &str, colour, font, format| {
                if r.bottom > list.top && r.top < list.bottom {
                    canvas.label(clip_y(r, list), text, colour, font, format);
                }
            };
            label_in(
                canvas,
                title,
                &note.title,
                ink,
                fonts.title,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
            label_in(
                canvas,
                time,
                &ago(note.updated_at, now_time),
                p.muted,
                fonts.small,
                DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
            );
            let preview_text = if note.preview.is_empty() {
                " "
            } else {
                note.preview.as_str()
            };
            label_in(
                canvas,
                preview,
                preview_text,
                p.muted,
                fonts.body,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
            // The state pill.
            if pill.top >= list.top && pill.bottom <= list.bottom {
                let (fill, colour) = match note.state {
                    NoteState::Shown => (mix(p.paper, p.accent, 0.14), p.accent),
                    NoteState::Hidden => (mix(p.chip, p.muted, 0.12), p.text),
                    NoteState::Closed => (p.chip, p.muted),
                };
                canvas.fill(pill, s(9.0) as f32, fill, 1.0);
                canvas.label(
                    pill,
                    note.state.label(),
                    colour,
                    fonts.small,
                    DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                );
            }
            if actions {
                // Row actions: show or hide, and delete.
                for (action, r) in [toggle, delete].into_iter().enumerate() {
                    if r.top < list.top || r.bottom > list.bottom {
                        continue;
                    }
                    let hot = self.hot_action.get() == Some((row, action));
                    canvas.fill(
                        r,
                        s(ACTION / 2.0) as f32,
                        mix(p.selected, p.muted, 0.12),
                        if hot { 1.0 } else { 0.0 },
                    );
                    let ink = if hot { p.text } else { p.muted };
                    if action == 1 {
                        trash_icon(canvas, r, ink, self.dpi);
                    } else if note.state == NoteState::Shown {
                        hide_icon(canvas, r, ink, self.dpi);
                    } else {
                        show_icon(canvas, r, ink, self.dpi);
                    }
                }
            }
        }
        // A thin thumb when the list is longer than the panel.
        let max = self.max_scroll();
        if max > 0 {
            let height = list.bottom - list.top;
            let total = height + max;
            let length = (height * height / total).max(s(18.0));
            let top = list.top + (height - length) * self.scroll.get() / max;
            canvas.fill(
                RECT {
                    left: list.right - s(3.0),
                    top,
                    right: list.right,
                    bottom: top + length,
                },
                s(1.5) as f32,
                p.muted,
                0.35,
            );
        }
        // The footer: a hairline, the count, and the two buttons.
        let footer_top = client.bottom - s(FOOTER_H);
        canvas.fill(
            RECT {
                left: s(12.0),
                top: footer_top,
                right: client.right - s(12.0),
                bottom: footer_top + 1,
            },
            0.0,
            p.hairline,
            1.0,
        );
        let count = if shown.len() == notes.len() {
            plural_notes(notes.len())
        } else {
            format!("{} of {}", shown.len(), plural_notes(notes.len()))
        };
        canvas.label(
            RECT {
                left: s(PAD),
                top: footer_top,
                right: client.right / 2,
                bottom: client.bottom,
            },
            &count,
            p.muted,
            fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
        for (index, (rect, text)) in self
            .button_rects()
            .into_iter()
            .zip(["Hide all", "Show all"])
            .enumerate()
        {
            let hot = self.hot_button.get() == Some(index);
            // SAFETY: a scalar button state query.
            let pressed = unsafe { SendMessageW(self.buttons[index], BM_GETSTATE, None, None) }.0
                as u32
                & BST_PUSHED
                != 0;
            let fill = if pressed {
                mix(p.hover, p.muted, 0.2)
            } else if hot {
                p.hover
            } else {
                p.chip
            };
            canvas.fill(rect, s(8.0) as f32, fill, 1.0);
            canvas.label(
                rect,
                text,
                p.text,
                fonts.button,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
            if focused == self.buttons[index] && self.keyboard_cues() {
                focus_ring(canvas, rect, s(8.0), s(2.0), p);
            }
        }
    }

    /// Focus rings on the buttons only after keyboard use, as in the settings window (a
    /// click hides them again).
    fn keyboard_cues(&self) -> bool {
        // SAFETY: a scalar query to our own window.
        let state = unsafe { SendMessageW(self.hwnd, WM_QUERYUISTATE, None, None) }.0 as u32;
        state & UISF_HIDEFOCUS == 0
    }

    fn hide_cues(&self) {
        // SAFETY: the usual UI cue change, sent to our own top-level window.
        unsafe {
            SendMessageW(
                self.hwnd,
                WM_CHANGEUISTATE,
                Some(WPARAM((UIS_SET | UISF_HIDEFOCUS << 16) as usize)),
                None,
            )
        };
    }

    fn set_hover(&self, hover: Option<usize>) {
        if self.hover.get() != hover {
            self.hover.set(hover);
            self.invalidate();
        }
    }
}

fn plural_notes(count: usize) -> String {
    if count == 1 {
        "1 note".to_owned()
    } else {
        format!("{count} notes")
    }
}

fn field_colour(p: Palette) -> COLORREF {
    if p.dark {
        mix(p.paper, p.chip, 1.6)
    } else {
        p.chip
    }
}

fn clip_y(rect: RECT, bounds: RECT) -> RECT {
    RECT {
        top: rect.top.max(bounds.top),
        bottom: rect.bottom.min(bounds.bottom),
        ..rect
    }
}

/// The show action: a small window outline.
fn show_icon(canvas: &mut Canvas<'_>, rect: RECT, ink: COLORREF, dpi: u32) {
    let s = |v: f64| px(v, dpi);
    let (cx, cy) = ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
    let square = RECT {
        left: cx - s(4.5),
        top: cy - s(4.0),
        right: cx + s(4.5),
        bottom: cy + s(4.0),
    };
    canvas.stroke(square, s(1.5) as f32, ink);
    canvas.fill(
        RECT {
            bottom: square.top + s(2.5),
            ..square
        },
        s(1.0) as f32,
        ink,
        1.0,
    );
}

unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the panel's State while it lives (0 before and after).
    let state = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref() };
    // SAFETY: default handling.
    let default = || unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    let Some(s) = state else {
        if msg == WM_NCCALCSIZE && wp.0 != 0 {
            return LRESULT(0);
        }
        return default();
    };
    match msg {
        WM_NCCALCSIZE if wp.0 != 0 => return LRESULT(0),
        WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
        WM_NCACTIVATE => return LRESULT(1),
        WM_ACTIVATE if (wp.0 & 0xffff) as u32 == WA_INACTIVE => {
            // The panel closes when another window takes the foreground.
            (s.request)(PanelRequest::Close);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_ACTIVATE as isize),
        WM_SETFOCUS => {
            // SAFETY: focus to our own filter field.
            let _ = unsafe { SetFocus(Some(s.edit)) };
            return LRESULT(0);
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xffff) as i32;
            let code = (wp.0 >> 16) as u32;
            match (id, code) {
                (FILTER_ID, EN_CHANGE) => s.refilter(None),
                (FILTER_ID, EN_SETFOCUS | EN_KILLFOCUS) => s.invalidate(),
                (HIDE_ALL_ID, BN_CLICKED) => (s.request)(PanelRequest::HideAll),
                (SHOW_ALL_ID, BN_CLICKED) => (s.request)(PanelRequest::ShowAll),
                _ => {}
            }
            return LRESULT(0);
        }
        WM_CTLCOLOREDIT => {
            let dc = HDC(wp.0 as *mut c_void);
            let p = s.palette.get();
            // SAFETY: the DC the edit control paints with, for this message.
            unsafe {
                SetTextColor(dc, p.text);
                SetBkColor(dc, field_colour(p));
            }
            return LRESULT(s.field_brush.get().0 as isize);
        }
        WM_CTLCOLORBTN => {
            // SAFETY: an empty brush; the buttons paint their part of the card.
            return LRESULT(unsafe { GetStockObject(NULL_BRUSH) }.0 as isize);
        }
        WM_MOUSEMOVE => {
            let (x, y) = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
            let hit = s.hit(x, y);
            s.hot_action
                .set(hit.and_then(|(row, action)| action.map(|action| (row, action))));
            s.set_hover(hit.map(|(row, _)| row));
            s.invalidate();
            let mut request = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: a leave request for our own window.
            let _ = unsafe { TrackMouseEvent(&mut request) };
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            s.hot_action.set(None);
            s.set_hover(None);
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let (x, y) = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
            if let Some((row, action)) = s.hit(x, y) {
                match action {
                    Some(action) => s.row_action(row, action),
                    None => {
                        s.select(row);
                        s.open_selected();
                    }
                }
            }
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let delta = i32::from((wp.0 >> 16) as u16 as i16);
            let scroll = (s.scroll.get() - delta * s.s(ROW_H) / 120).clamp(0, s.max_scroll());
            s.scroll.set(scroll);
            s.invalidate();
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER => {
            s.step();
            return LRESULT(0);
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: BeginPaint/EndPaint pair for our window.
            let dc = unsafe { BeginPaint(hwnd, &mut paint) };
            if !dc.is_invalid() {
                s.paint_at(dc, POINT::default(), None);
            }
            // SAFETY: matches BeginPaint.
            let _ = unsafe { EndPaint(hwnd, &paint) };
            return LRESULT(0);
        }
        WM_PRINTCLIENT => {
            s.paint_at(HDC(wp.0 as *mut c_void), POINT::default(), None);
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_UPDATEUISTATE => {
            // SAFETY: Windows updates and passes on the cue bits first.
            let result = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
            s.invalidate();
            return result;
        }
        WM_CLOSE => {
            (s.request)(PanelRequest::Close);
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            // SAFETY: detach before late messages.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        }
        _ => {}
    }
    default()
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: the panel attached its State; its children are destroyed before it is freed.
    let s = unsafe { &*(data as *const State) };
    match msg {
        WM_NCDESTROY => {
            // SAFETY: removes this subclass on the last message.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(edit_proc), id) };
        }
        WM_GETDLGCODE => {
            // Enter, Esc and the arrows are the panel's keys while typing; Tab still moves on.
            // SAFETY: the edit's own answer, extended; `lp` is the message being asked about,
            // or null.
            let native = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
            let tab = (lp.0 != 0)
                // SAFETY: a non-null lParam of WM_GETDLGCODE points at the MSG.
                && unsafe { (*(lp.0 as *const MSG)).wParam.0 } == usize::from(VK_TAB.0);
            let want = if tab {
                0
            } else {
                (DLGC_WANTARROWS | DLGC_WANTMESSAGE) as isize
            };
            return LRESULT(native.0 | want);
        }
        WM_KEYDOWN => {
            let vk = wp.0 as u16;
            let ctrl = key_down(VK_CONTROL.0);
            let shift = key_down(VK_SHIFT.0);
            let selected = s.selected.get();
            let page = s.visible_rows() as usize;
            let handled = match vk {
                v if v == VK_DOWN.0 => {
                    s.select(selected + 1);
                    true
                }
                v if v == VK_UP.0 => {
                    s.select(selected.saturating_sub(1));
                    true
                }
                v if v == VK_NEXT.0 => {
                    s.select(selected + page);
                    true
                }
                v if v == VK_PRIOR.0 => {
                    s.select(selected.saturating_sub(page));
                    true
                }
                v if (v == VK_HOME.0 || v == VK_END.0) && ctrl => {
                    s.select(if v == VK_HOME.0 { 0 } else { usize::MAX });
                    true
                }
                v if v == VK_RETURN.0 => {
                    s.open_selected();
                    true
                }
                v if v == VK_ESCAPE.0 => {
                    if s.query().is_empty() {
                        (s.request)(PanelRequest::Close);
                    } else {
                        // SAFETY: clears our own edit control.
                        unsafe {
                            let _ = SetWindowTextW(hwnd, w!(""));
                        }
                    }
                    true
                }
                v if v == u16::from(b'H') && ctrl && !shift => {
                    s.row_action(selected, 0);
                    true
                }
                v if v == VK_DELETE.0 && ctrl && shift => {
                    s.row_action(selected, 1);
                    true
                }
                _ => false,
            };
            if handled {
                return LRESULT(0);
            }
        }
        // Enter, Esc and Ctrl+H (a backspace character) were handled as keys: no beep, no
        // character.
        WM_CHAR
            if matches!(wp.0, 0x0d | 0x1b | 0x0a) || (wp.0 == 0x08 && key_down(VK_CONTROL.0)) =>
        {
            return LRESULT(0);
        }
        WM_PAINT => {
            // The edit paints itself; when it is empty the placeholder is drawn over it.
            // SAFETY: the edit's own paint first.
            let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
            // SAFETY: a length query, then drawing on our own control's DC.
            unsafe {
                if GetWindowTextLengthW(hwnd) == 0 {
                    let dc = GetDC(Some(hwnd));
                    if !dc.is_invalid() {
                        let mut rect = RECT::default();
                        let _ = GetClientRect(hwnd, &mut rect);
                        let old = SelectObject(dc, s.fonts.body.into());
                        SetBkMode(dc, TRANSPARENT);
                        SetTextColor(dc, s.palette.get().faint);
                        let mut text: Vec<u16> = "Filter notes".encode_utf16().collect();
                        rect.left += 2;
                        DrawTextW(
                            dc,
                            &mut text,
                            &mut rect,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                        );
                        SelectObject(dc, old);
                        ReleaseDC(Some(hwnd), dc);
                    }
                }
            }
            return result;
        }
        _ => {}
    }
    // SAFETY: the edit handles everything else (typing, IME, accessibility).
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

unsafe extern "system" fn button_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: the panel attached its State; its children are destroyed before it is freed.
    let s = unsafe { &*(data as *const State) };
    let index = s.buttons.iter().position(|button| *button == hwnd);
    match msg {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: the button paints its part of the panel's card, clipped to itself
            // (BUTTON has CS_PARENTDC, so its DC is not clipped otherwise).
            unsafe {
                let dc = BeginPaint(hwnd, &mut paint);
                let mut origin = [POINT::default()];
                MapWindowPoints(Some(hwnd), Some(s.hwnd), &mut origin);
                let mut own = RECT::default();
                let _ = GetClientRect(hwnd, &mut own);
                if !dc.is_invalid() {
                    let saved = SaveDC(dc);
                    IntersectClipRect(dc, 0, 0, own.right, own.bottom);
                    s.paint_at(dc, origin[0], Some((own.right, own.bottom)));
                    let _ = RestoreDC(dc, saved);
                }
                let _ = EndPaint(hwnd, &paint);
            }
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCDESTROY => {
            // SAFETY: removes this subclass on the last message.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(button_proc), id) };
        }
        WM_LBUTTONDOWN => s.hide_cues(),
        WM_MOUSEMOVE => {
            if s.hot_button.get() != index {
                s.hot_button.set(index);
                s.invalidate();
            }
            let mut request = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: a leave request for our own button.
            let _ = unsafe { TrackMouseEvent(&mut request) };
        }
        WM_MOUSELEAVE => {
            s.hot_button.set(None);
            s.invalidate();
        }
        _ => {}
    }
    // SAFETY: the native button handles focus, keys, clicks and accessibility.
    let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    if matches!(
        msg,
        WM_SETFOCUS | WM_KILLFOCUS | WM_LBUTTONDOWN | WM_LBUTTONUP | BM_SETSTATE
    ) {
        s.invalidate();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn note(n: u8, title: &str, text: &str, age_s: u64, state: NoteState) -> PanelNote {
        PanelNote {
            id: NoteId::from_bytes([n; 16]),
            title: title.to_owned(),
            preview: preview(text),
            search: format!("{title}\n{text}").to_lowercase(),
            updated_at: UNIX_EPOCH + Duration::from_secs(1_000_000 - age_s),
            state,
        }
    }

    #[test]
    fn the_filter_needs_every_word_somewhere_ignoring_case() {
        let notes = vec![
            note(1, "Groceries", "milk\neggs", 10, NoteState::Shown),
            note(
                2,
                "Meeting notes",
                "Discuss the Budget",
                20,
                NoteState::Hidden,
            ),
            note(3, "Budget 2027", "rent, food", 30, NoteState::Closed),
        ];
        assert_eq!(filter(&notes, ""), [0, 1, 2]);
        assert_eq!(filter(&notes, "budget"), [1, 2]);
        assert_eq!(filter(&notes, "BUDGET food"), [2]);
        assert_eq!(filter(&notes, "  milk   EGGS "), [0]);
        assert_eq!(filter(&notes, "nothing"), Vec::<usize>::new());
    }

    #[test]
    fn the_menu_lists_the_five_changed_last() {
        let recent: Vec<u32> = (0..8).collect();
        assert_eq!(menu_notes(&recent), [0, 1, 2, 3, 4]);
        assert_eq!(menu_notes(&recent[..3]), [0, 1, 2]);
        assert!(menu_notes::<u32>(&[]).is_empty());
    }

    #[test]
    fn newest_edit_first() {
        let mut notes = vec![
            note(1, "b", "", 300, NoteState::Shown),
            note(2, "a", "", 10, NoteState::Shown),
            note(3, "c", "", 10, NoteState::Shown),
        ];
        sort(&mut notes);
        let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["a", "c", "b"]);
    }

    #[test]
    fn previews_skip_the_title_and_markers() {
        assert_eq!(preview("# Title\n\n- [ ] buy **milk**\nmore"), "buy milk");
        assert_eq!(preview("Only a title"), "");
        assert_eq!(preview("Title\n---\n> a quote"), "a quote");
        assert!(preview(&format!("t\n{}", "x".repeat(300))).ends_with('…'));
    }

    #[test]
    fn the_panel_sits_by_the_tray_on_every_taskbar_edge() {
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let size = (360, 460);
        // Taskbar at the bottom, icon near the right.
        let work = RECT {
            bottom: 1032,
            ..monitor
        };
        let icon = RECT {
            left: 1700,
            top: 1040,
            right: 1724,
            bottom: 1064,
        };
        assert_eq!(
            place(Some(icon), size, monitor, work, 10),
            POINT { x: 1532, y: 562 }
        );
        // Near the right edge: kept inside.
        let icon = RECT {
            left: 1890,
            top: 1040,
            right: 1914,
            bottom: 1064,
        };
        assert_eq!(
            place(Some(icon), size, monitor, work, 10).x,
            1920 - 360 - 10
        );
        // Taskbar on top.
        let work = RECT { top: 48, ..monitor };
        let icon = RECT {
            left: 1700,
            top: 12,
            right: 1724,
            bottom: 36,
        };
        assert_eq!(
            place(Some(icon), size, monitor, work, 10),
            POINT { x: 1532, y: 58 }
        );
        // Taskbar on the left.
        let work = RECT {
            left: 62,
            ..monitor
        };
        let icon = RECT {
            left: 20,
            top: 900,
            right: 44,
            bottom: 924,
        };
        assert_eq!(
            place(Some(icon), size, monitor, work, 10),
            POINT { x: 72, y: 610 }
        );
        // Taskbar on the right.
        let work = RECT {
            right: 1858,
            ..monitor
        };
        let icon = RECT {
            left: 1876,
            top: 300,
            right: 1900,
            bottom: 324,
        };
        assert_eq!(
            place(Some(icon), size, monitor, work, 10),
            POINT { x: 1488, y: 82 }
        );
        // In the overflow area (no rectangle, or one off the monitor): the corner by the taskbar.
        let work = RECT {
            bottom: 1032,
            ..monitor
        };
        assert_eq!(
            place(None, size, monitor, work, 10),
            POINT { x: 1550, y: 562 }
        );
        let elsewhere = RECT {
            left: 5000,
            top: 5000,
            right: 5024,
            bottom: 5024,
        };
        assert_eq!(
            place(Some(elsewhere), size, monitor, work, 10),
            POINT { x: 1550, y: 562 }
        );
        let work = RECT { top: 48, ..monitor };
        assert_eq!(
            place(None, size, monitor, work, 10),
            POINT { x: 1550, y: 58 }
        );
        // At 150%, a bigger panel still fits.
        let work = RECT {
            bottom: 1032,
            ..monitor
        };
        let icon = RECT {
            left: 1700,
            top: 1040,
            right: 1724,
            bottom: 1064,
        };
        let at = place(Some(icon), (540, 690), monitor, work, 15);
        assert!(at.x + 540 <= 1920 - 15 && at.y + 690 <= 1032 - 15 && at.y >= 15);
    }
}
