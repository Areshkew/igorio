//! The note editor: a RichEdit (Msftedit) control that shows Markdown as formatted blocks, the
//! way Notion does, while the text in it stays plain Markdown.
//!
//! How it works:
//!
//! - The control holds the Markdown source, every character of it. Markers (`# `, `**`, the
//!   brackets of a link, a list's `- `) carry hidden character formatting (`CFE_HIDDEN`), so the
//!   text reads as formatted. Inline markers, heading and quote markers come back, dimmed, on the
//!   lines the caret or selection is on. List, to-do and numbered markers never come back; their
//!   bullet, box or number is drawn in the paragraph's left indent instead.
//! - Formats go through the Text Object Model (`ITextRange::SetFont` / `SetPara`) on a range
//!   object of the editor's own, never the selection, so the user's selection, scroll position
//!   and IME composition are not touched. Only runs whose format changed are applied
//!   (`Applied::missing`); typing in a 20,000-character note restyles a handful of runs.
//! - Undo is the editor's own ([`UndoStack`]); RichEdit's is off (`EM_SETUNDOLIMIT 0`). The first
//!   design kept formatting out of RichEdit's undo with `ITextDocument::Undo(tomSuspend)`, but
//!   once RichEdit's spell checker had run, RichEdit stopped recording edits at all. With the
//!   editor's stack a formatting pass can never be an undo step, typing groups into words, and
//!   each edit the editor makes itself (Enter in a list, a typed `[] `, Ctrl+B, the slash menu,
//!   a checkbox click, a screenshot drop) is exactly one step.
//! - Bullets, numbers, checkboxes, dividers, quote bars, the code block background, image
//!   thumbnails and placeholders are drawn over the control: the control renders itself into a
//!   bitmap through `WM_PRINTCLIENT`, the drawings go on top, and the bitmap goes to the screen.
//!
//! The text the window stores is [`Editor::text`]: the control's text with `\n` line breaks.
//! The pure parts (layout, run diff, UTF-16 maps, line-break conversion) are at the top of the
//! file with their tests at the bottom.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use igorio_kit::theme;
use windows::Win32::Foundation::{
    COLORREF, E_FAIL, E_NOTIMPL, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE,
    WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_UNICODETEXT, DROPEFFECT, OLEINPLACEFRAMEINFO, OleGetClipboard, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MODIFIERKEYS_FLAGS, RECO_COPY, RECO_FLAGS};
use windows::Win32::UI::Controls::RichEdit::*;
use windows::Win32::UI::Controls::{
    EM_CANUNDO, EM_CHARFROMPOS, EM_GETSEL, EM_POSFROMCHAR, EM_SCROLLCARET, EM_SETSEL, EM_UNDO,
    SetWindowTheme, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_HOME, VK_INSERT, VK_LEFT,
    VK_MENU, VK_RETURN, VK_RMENU, VK_SHIFT, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SHCreateDataObject, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, BSTR, Error, HRESULT, Interface, PCWSTR, Result, w};

use super::markdown::{
    self, Block, BlockCommand, Change, Inline, Kind, Line, SlashItem, Span, blocks, line_at,
};

// ---------------------------------------------------------------------------
// Palette and motion (shared with the note window)
// ---------------------------------------------------------------------------

/// The note's colours in one theme. Light and dark follow the settings window's palettes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// The note card and the editor background.
    pub paper: COLORREF,
    pub text: COLORREF,
    pub muted: COLORREF,
    pub accent: COLORREF,
    /// Markdown markers while revealed: `MUTED` at 50% over the paper (`markdown::MARK`).
    pub mark: COLORREF,
    /// Placeholders.
    pub faint: COLORREF,
    pub hairline: COLORREF,
    /// The inline code chip and the code block background.
    pub chip: COLORREF,
    /// Hover and pressed fills of the header buttons and checkboxes.
    pub hover: COLORREF,
    /// The selected row of the slash menu.
    pub selected: COLORREF,
}

const fn rgb(hex: u32) -> COLORREF {
    COLORREF(((hex & 0xff) << 16) | (hex & 0xff00) | ((hex >> 16) & 0xff))
}

const fn token([r, g, b, _]: theme::Rgba) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

pub const LIGHT: Palette = Palette {
    dark: false,
    paper: rgb(0xffffff),
    text: token(theme::TEXT),
    muted: token(theme::MUTED),
    accent: rgb(0x2563eb),
    mark: rgb(0xacb1bb),
    faint: rgb(0xa9b0bc),
    hairline: rgb(0xe8ebf0),
    chip: rgb(0xeef1f6),
    hover: rgb(0xe9eef6),
    selected: rgb(0xdce9fc),
};

pub const DARK: Palette = Palette {
    dark: true,
    paper: rgb(0x222733),
    text: rgb(0xeaf0fc),
    muted: rgb(0xaab7cc),
    accent: rgb(0x8ab4ff),
    mark: rgb(0x666f80),
    faint: rgb(0x687286),
    hairline: rgb(0x343b4a),
    chip: rgb(0x2d3443),
    hover: rgb(0x313948),
    selected: rgb(0x344a6b),
};

impl Palette {
    pub fn for_theme(dark: bool) -> Self {
        if dark { DARK } else { LIGHT }
    }
}

/// Mixes two colours; `amount` 0 is `a`, 1 is `b`.
pub fn mix(a: COLORREF, b: COLORREF, amount: f64) -> COLORREF {
    let amount = amount.clamp(0.0, 1.0);
    COLORREF([0, 8, 16].into_iter().fold(0, |value, shift| {
        let from = f64::from((a.0 >> shift) & 255);
        let to = f64::from((b.0 >> shift) & 255);
        value | (((from + (to - from) * amount).round() as u32) << shift)
    }))
}

/// Is the system in dark mode for apps?
pub fn system_dark() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let mut light: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: the value is restricted to a DWORD and both out buffers are valid locals.
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

/// Client-area animations are on (Settings > Accessibility > Visual effects); off means
/// reduced motion and every transition is instant.
pub fn animation_effects() -> bool {
    let mut enabled = BOOL(0);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes exactly one BOOL into the local.
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok()
        && enabled.as_bool()
}

/// The settings window's easing: cubic-bezier(0, 0, 0, 1).
pub fn ease(progress: f64) -> f64 {
    let t = progress.clamp(0.0, 1.0).cbrt();
    t * t * (3.0 - 2.0 * t)
}

/// One animated value on a millisecond clock, as in the settings window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Transition {
    pub from: f64,
    pub to: f64,
    pub start: f64,
    pub duration: f64,
}

impl Transition {
    pub fn at(value: f64) -> Self {
        Self { from: value, to: value, start: 0.0, duration: 0.0 }
    }

    pub fn value(self, now: f64) -> f64 {
        if self.active(now) {
            self.from + (self.to - self.from) * ease((now - self.start) / self.duration)
        } else {
            self.to
        }
    }

    pub fn active(self, now: f64) -> bool {
        self.from != self.to && self.duration > 0.0 && now < self.start + self.duration
    }

    /// Heads for `to` from wherever it is now; a zero duration jumps.
    pub fn retarget(&mut self, to: f64, now: f64, duration: f64) {
        if duration <= 0.0 {
            *self = Self::at(to);
        } else if self.to != to {
            *self = Self { from: self.value(now), to, start: now, duration };
        }
    }

    /// Starts again from `from` to `to`.
    pub fn run(&mut self, from: f64, to: f64, now: f64, duration: f64) {
        *self =
            if duration <= 0.0 { Self::at(to) } else { Self { from, to, start: now, duration } };
    }
}

/// Logical pixels to physical at a DPI.
pub fn px(value: f64, dpi: u32) -> i32 {
    (value * f64::from(dpi) / 96.0).round() as i32
}

pub const REVEAL_MS: f64 = 120.0;
pub const HIDE_MS: f64 = 90.0;
pub const BLOCK_FADE_MS: f64 = 160.0;
pub const CHECK_MS: f64 = 180.0;
pub const HOVER_IN_MS: f64 = 80.0;
pub const HOVER_OUT_MS: f64 = 120.0;
pub const IMAGE_MS: f64 = 220.0;
pub const MENU_MS: f64 = 120.0;
/// A wheel notch eases over this long (pixels, not lines).
pub const SCROLL_MS: f64 = 120.0;

// ---------------------------------------------------------------------------
// Pure layout
// ---------------------------------------------------------------------------

/// Text without a font: which ink a run is drawn with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ink {
    Text,
    Muted,
    Accent,
    Mark,
    Faint,
}

/// One character format, as the editor applies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fmt {
    /// Tenths of a logical pixel (140 is the 14 px body).
    pub size: u16,
    /// Semibold (600) when true, regular (400) otherwise.
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub mono: bool,
    pub hidden: bool,
    /// Inline code: the chip background.
    pub chip: bool,
    pub ink: Ink,
    /// How much of the ink shows over the paper, 0 to 20 (20 is full).
    pub alpha: u8,
}

pub const ALPHA_STEPS: u8 = 20;

impl Fmt {
    const fn body() -> Self {
        Fmt {
            size: 140,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            mono: false,
            hidden: false,
            chip: false,
            ink: Ink::Text,
            alpha: ALPHA_STEPS,
        }
    }

    fn sized(size: f32) -> Self {
        Fmt { size: (size * 10.0).round() as u16, ..Self::body() }
    }

    fn faded(mut self, amount: f32) -> Self {
        let alpha = (amount.clamp(0.0, 1.0) * f32::from(ALPHA_STEPS)).round() as u8;
        self.alpha = self.alpha.min(alpha);
        self
    }

    /// Hidden marker text, at the size of the text around it.
    fn hidden(self) -> Self {
        Fmt { hidden: true, ..self }
    }

    /// The paragraph mark of a line that should take no room (a hidden fence or image line).
    fn tiny() -> Self {
        Fmt { size: 10, ..Self::body() }
    }
}

/// Paragraph format: left indent and spacing, logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Para {
    pub indent: u16,
    pub before: u16,
    pub after: u16,
}

/// A character format over a UTF-16 range of the control.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Run {
    pub range: Range<usize>,
    pub fmt: Fmt,
}

/// A paragraph format over a line (its UTF-16 range, paragraph mark included).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParaRun {
    pub range: Range<usize>,
    pub para: Para,
}

/// What is drawn for a line.
#[derive(Clone, Debug, PartialEq)]
pub enum DecoKind {
    Bullet,
    Number(u32),
    Check(bool),
    Divider,
    QuoteBar,
    /// A line of a fenced code block; true on the block's first and last lines.
    Code {
        first: bool,
        last: bool,
    },
    /// An image line: the path as written in the link.
    Image(String),
    /// Faint text on an empty line (an empty note, or an empty block with the caret).
    Placeholder(&'static str),
}

/// A drawing anchored to the first visible character of a line.
#[derive(Clone, Debug, PartialEq)]
pub struct Deco {
    pub line: usize,
    pub kind: DecoKind,
    pub depth: u8,
    /// UTF-16 index whose position places the drawing.
    pub anchor: usize,
    /// The line's text size in logical pixels (for vertical centring).
    pub size: f32,
    /// The line's markers are showing (the caret is on it).
    pub revealed: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    pub runs: Vec<Run>,
    pub paras: Vec<ParaRun>,
    pub decos: Vec<Deco>,
}

/// What the layout needs besides the text.
pub struct LayoutInput<'a> {
    /// How far each line's markers are revealed, 0 to 1.
    pub reveal: &'a dyn Fn(usize) -> f32,
    /// A line fading in after its block changed, and how far (0 to 1).
    pub fade: Option<(usize, f32)>,
    /// The thumbnail size (logical pixels) for an image path, when it loaded.
    pub thumb: &'a dyn Fn(&str) -> Option<(u16, u16)>,
    /// The line with the caret when it is on an empty block and the editor has focus.
    pub empty_caret_line: Option<usize>,
}

pub const LIST_STEP: u16 = 24;
pub const QUOTE_INDENT: u16 = 16;
pub const IMAGE_GAP: u16 = 6;
const BODY: f32 = 14.0;

/// The size of a line's text, logical pixels.
fn block_size(block: Block) -> f32 {
    match block {
        Block::Heading(level) => markdown::look(Kind::Heading(level)).size,
        Block::Code | Block::Fence => markdown::look(Kind::Code).size,
        _ => BODY,
    }
}

/// Maps between UTF-8 byte offsets of a text and UTF-16 units of the same text, in O(1).
#[derive(Clone, Debug, Default)]
pub struct Utf16Map {
    /// For every byte boundary, the UTF-16 offset (non-boundaries repeat the previous one).
    units: Vec<u32>,
    /// For every UTF-16 offset, the byte offset (the second unit of a pair maps to the end).
    bytes: Vec<u32>,
}

impl Utf16Map {
    pub fn new(text: &str) -> Self {
        let mut units = Vec::with_capacity(text.len() + 1);
        let mut bytes = Vec::with_capacity(text.len() + 1);
        let mut unit = 0u32;
        for (at, ch) in text.char_indices() {
            for _ in 0..ch.len_utf8() {
                units.push(unit);
            }
            bytes.push(at as u32);
            if ch.len_utf16() == 2 {
                bytes.push((at + ch.len_utf8()) as u32);
            }
            unit += ch.len_utf16() as u32;
        }
        units.push(unit);
        bytes.push(text.len() as u32);
        Self { units, bytes }
    }

    /// UTF-16 offset of a byte offset (clamped to the end).
    pub fn to16(&self, byte: usize) -> usize {
        self.units[byte.min(self.units.len() - 1)] as usize
    }

    /// Byte offset of a UTF-16 offset (clamped to the end).
    pub fn to8(&self, unit: usize) -> usize {
        self.bytes[unit.min(self.bytes.len() - 1)] as usize
    }

    pub fn len16(&self) -> usize {
        self.bytes.len() - 1
    }
}

/// The control's UTF-16 text for a note: `\n` becomes the `\r` paragraph mark. Same length.
pub fn control_units(text: &str) -> Vec<u16> {
    text.encode_utf16()
        .map(|unit| if unit == u16::from(b'\n') { u16::from(b'\r') } else { unit })
        .collect()
}

/// The note text of the control's UTF-16 text: `\r` and the vertical-tab soft break become
/// `\n`, one unit for one unit, so offsets stay the same.
pub fn from_control(units: &[u16]) -> String {
    let mapped: Vec<u16> =
        units.iter().map(|&unit| if unit == 0x0d || unit == 0x0b { 0x0a } else { unit }).collect();
    String::from_utf16_lossy(&mapped)
}

/// Text for the clipboard: the Markdown source with Windows line breaks.
pub fn clipboard_text(text: &str) -> String {
    text.replace('\n', "\r\n")
}

/// Do these lines reveal their markers when the caret is on them? List markers never do (their
/// bullet or box is drawn instead).
fn reveals_prefix(block: Block) -> bool {
    matches!(block, Block::Heading(_) | Block::Quote)
}

/// Builds every run, paragraph format and drawing for a note.
pub fn layout(text: &str, lines: &[Line], spans: &[Span], input: &LayoutInput) -> Layout {
    let map = Utf16Map::new(text);
    let numbers = list_numbers_of(text, lines);
    let mut byte_runs: Vec<(Range<usize>, Fmt)> = Vec::with_capacity(spans.len() + lines.len() * 2);
    let mut paras = Vec::with_capacity(lines.len());
    let mut decos = Vec::new();
    let mut span_index = 0;
    let edges = code_edges(lines);
    for (index, line) in lines.iter().enumerate() {
        let reveal = (input.reveal)(index);
        let revealed = reveal > 0.0;
        let fade = input.fade.filter(|(at, _)| *at == index).map(|(_, amount)| amount);
        let size = block_size(line.block);
        let mut base = Fmt::sized(size);
        if matches!(line.block, Block::Heading(_)) {
            base.bold = true;
        }
        if matches!(line.block, Block::Code | Block::Fence) {
            base.mono = true;
        }
        if line.block == Block::Todo(true) {
            base.ink = Ink::Muted;
            base.strike = true;
        }
        if let Some(amount) = fade {
            base = base.faded(0.35 + 0.65 * amount);
        }
        let marker = |fmt: Fmt| {
            if revealed {
                Fmt { ink: Ink::Mark, underline: false, strike: false, chip: false, ..fmt }
                    .faded(reveal)
            } else {
                fmt.hidden()
            }
        };
        let text_of = &text[line.range.clone()];
        let thumb = match line.block {
            Block::Image => markdown::image_target(text_of)
                .and_then(|path| (input.thumb)(path).map(|size| (path, size))),
            _ => None,
        };
        // The line's own characters.
        match line.block {
            Block::Fence | Block::Divider => {
                byte_runs.push((line.range.clone(), marker(base)));
            }
            Block::Image if thumb.is_some() => {
                let small = Fmt { ink: Ink::Faint, ..Fmt::sized(11.0) };
                byte_runs.push((line.range.clone(), marker(small)));
            }
            Block::Code => byte_runs.push((line.range.clone(), base)),
            _ => {
                let prefix = line.prefix();
                if !prefix.is_empty() {
                    let plain = Fmt { strike: false, ink: Ink::Text, ..base };
                    let fmt =
                        if reveals_prefix(line.block) { marker(plain) } else { plain.hidden() };
                    byte_runs.push((prefix.clone(), fmt));
                }
                // Inline spans over the content.
                while span_index < spans.len() && spans[span_index].range.end <= line.content {
                    span_index += 1;
                }
                let mut at = line.content;
                let mut i = span_index;
                while at < line.range.end {
                    let Some(span) = spans.get(i) else { break };
                    i += 1;
                    let start = span.range.start.max(at);
                    let end = span.range.end.min(line.range.end);
                    if start >= end {
                        continue;
                    }
                    let mut fmt = base;
                    match span.kind {
                        Kind::Bold => fmt.bold = true,
                        Kind::Italic => fmt.italic = true,
                        Kind::Code => {
                            fmt.mono = true;
                            fmt.size = (f32::from(fmt.size) - 15.0).max(10.0) as u16;
                            fmt.chip = !span.marker;
                        }
                        Kind::Link => {
                            fmt.ink = Ink::Accent;
                            fmt.underline = true;
                        }
                        Kind::Strikethrough => fmt.strike = true,
                        _ => {}
                    }
                    let fmt = if span.marker {
                        if span.kind == Kind::Body {
                            // An image link inside a sentence: shown, small and faint.
                            Fmt { ink: Ink::Faint, ..fmt }
                        } else {
                            marker(fmt)
                        }
                    } else {
                        fmt
                    };
                    byte_runs.push((start..end, fmt));
                    at = end;
                }
            }
        }
        // The paragraph mark (`\n`, or the control's final mark after the last line).
        let tiny = (matches!(line.block, Block::Fence)
            || (line.block == Block::Image && thumb.is_some()))
            && !revealed;
        let mark_fmt = if tiny { Fmt::tiny() } else { Fmt { strike: false, ..base } };
        byte_runs.push((line.range.end..line.range.end + 1, mark_fmt));

        // Paragraph format.
        let indent = match line.block {
            Block::Bullet | Block::Numbered(_) | Block::Todo(_) => {
                LIST_STEP * (u16::from(line.depth) + 1)
            }
            Block::Quote => QUOTE_INDENT,
            Block::Code | Block::Fence => 12,
            _ => 0,
        };
        let before = match line.block {
            Block::Heading(level) if index > 0 => [10, 8, 6][usize::from(level.clamp(1, 3) - 1)],
            Block::Divider => 4,
            _ => 0,
        };
        let after = match (line.block, thumb) {
            (Block::Image, Some((_, (_, height)))) => height + IMAGE_GAP * 2,
            (Block::Divider, _) => 4,
            _ => 2,
        };
        let start16 = map.to16(line.range.start);
        let end16 = map.to16(line.range.end) + 1;
        paras.push(ParaRun { range: start16..end16, para: Para { indent, before, after } });

        // Drawings.
        let first_visible =
            if reveals_prefix(line.block) && revealed { line.range.start } else { line.content };
        let anchor = if first_visible < line.range.end
            && !matches!(line.block, Block::Fence | Block::Divider | Block::Image)
        {
            map.to16(first_visible)
        } else {
            map.to16(line.range.end)
        };
        let deco = |kind| Deco { line: index, kind, depth: line.depth, anchor, size, revealed };
        match line.block {
            Block::Bullet => decos.push(deco(DecoKind::Bullet)),
            Block::Numbered(_) => decos.push(deco(DecoKind::Number(numbers[index].unwrap_or(1)))),
            Block::Todo(checked) => decos.push(deco(DecoKind::Check(checked))),
            Block::Divider if !revealed => decos.push(deco(DecoKind::Divider)),
            Block::Quote => decos.push(deco(DecoKind::QuoteBar)),
            Block::Code | Block::Fence => {
                let (first, last) = edges[index];
                decos.push(deco(DecoKind::Code { first, last }));
            }
            Block::Image => {
                if let Some((path, _)) = thumb {
                    decos.push(deco(DecoKind::Image(path.to_owned())));
                }
            }
            _ => {}
        }
        let empty = text[line.content..line.range.end].is_empty();
        let only_line = lines.len() == 1 && text.is_empty();
        // A placeholder sits where the first typed character goes: at the paragraph mark, after
        // any revealed marker, never on the marker itself.
        let hint_at = |kind| Deco { anchor: map.to16(line.range.end), ..deco(kind) };
        if only_line {
            decos.push(hint_at(DecoKind::Placeholder("Write, or type / for commands")));
        } else if empty && input.empty_caret_line == Some(index) {
            let hint = match line.block {
                Block::Heading(1) => Some("Heading 1"),
                Block::Heading(2) => Some("Heading 2"),
                Block::Heading(_) => Some("Heading 3"),
                Block::Bullet | Block::Numbered(_) => Some("List"),
                Block::Todo(_) => Some("To-do"),
                Block::Quote => Some("Quote"),
                Block::Paragraph => Some("Type / for commands"),
                _ => None,
            };
            if let Some(hint) = hint {
                decos.push(hint_at(DecoKind::Placeholder(hint)));
            }
        }
    }
    let mut runs: Vec<Run> = Vec::with_capacity(byte_runs.len());
    let total16 = map.len16();
    for (range, fmt) in byte_runs {
        let start = if range.start > text.len() { total16 } else { map.to16(range.start) };
        let end = if range.end > text.len() { total16 + 1 } else { map.to16(range.end) };
        if start >= end {
            continue;
        }
        if let Some(last) = runs.last_mut()
            && last.fmt == fmt
            && last.range.end == start
        {
            last.range.end = end;
            continue;
        }
        runs.push(Run { range: start..end, fmt });
    }
    Layout { runs, paras, decos }
}

fn list_numbers_of(text: &str, lines: &[Line]) -> Vec<Option<u32>> {
    // The markdown module counts from its own line split; the two splits are the same.
    let numbers = markdown::list_numbers(text);
    if numbers.len() == lines.len() { numbers } else { vec![None; lines.len()] }
}

/// Code block drawing flags, worked out after the layout (first and last line of each block).
pub fn code_edges(lines: &[Line]) -> Vec<(bool, bool)> {
    let inside = |index: usize| {
        matches!(lines.get(index).map(|l| l.block), Some(Block::Code | Block::Fence))
    };
    (0..lines.len())
        .map(|index| {
            if !inside(index) {
                return (false, false);
            }
            // A fence opens a block when the line after it is in the block and the line before
            // it is not, or the line before closes another block.
            let opens = lines[index].block == Block::Fence && fence_opens(lines, index);
            let first = opens || (index > 0 && !inside(index - 1));
            let closes = lines[index].block == Block::Fence && !opens;
            let last = closes || !inside(index + 1);
            (first, last)
        })
        .collect()
}

/// Is the fence at `index` an opening fence? Fences alternate from the top.
fn fence_opens(lines: &[Line], index: usize) -> bool {
    lines[..index].iter().filter(|l| l.block == Block::Fence).count() % 2 == 0
}

/// What is applied in the control: the runs and paragraph formats, keyed for exact lookups.
#[derive(Clone, Debug, Default)]
pub struct Applied {
    pub runs: HashSet<Run>,
    pub paras: HashSet<ParaRun>,
}

impl Applied {
    /// The text changed from UTF-16 `old` (a range of the old text) to `new_len` units: runs
    /// before stay, runs after move, runs touching the change are forgotten (so they are applied
    /// again). The control's final paragraph mark moves with the text.
    pub fn edited(&mut self, old: Range<usize>, new_len: usize) {
        let moved = |range: &Range<usize>| -> Option<Range<usize>> {
            if range.end < old.start
                || (range.end == old.start && old.start != old.end && range.end <= old.start)
            {
                Some(range.clone())
            } else if range.start > old.end || (range.start == old.end && old.start != old.end) {
                let shift = new_len as isize - (old.end - old.start) as isize;
                Some((range.start as isize + shift) as usize..(range.end as isize + shift) as usize)
            } else {
                None
            }
        };
        self.runs = self
            .runs
            .drain()
            .filter_map(|run| moved(&run.range).map(|range| Run { range, fmt: run.fmt }))
            .collect();
        self.paras = self
            .paras
            .drain()
            .filter_map(|para| moved(&para.range).map(|range| ParaRun { range, para: para.para }))
            .collect();
    }

    /// The runs and paragraph formats of `layout` that the control does not have yet.
    pub fn missing<'a>(&self, layout: &'a Layout) -> (Vec<&'a Run>, Vec<&'a ParaRun>) {
        (
            layout.runs.iter().filter(|run| !self.runs.contains(*run)).collect(),
            layout.paras.iter().filter(|para| !self.paras.contains(*para)).collect(),
        )
    }

    /// The control now has exactly `layout`.
    pub fn set(&mut self, layout: &Layout) {
        self.runs = layout.runs.iter().cloned().collect();
        self.paras = layout.paras.iter().cloned().collect();
    }
}

/// How a text change is recorded for undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recording {
    /// Typing, deleting, pasting: consecutive edits at the caret make one step.
    Typing,
    /// An edit of the editor's own (a conversion, a key command, a drop): one step of its own.
    Step,
    /// Undo or redo putting text back: not recorded.
    Off,
}

/// One undoable step: `old` was replaced by `new` at `at` (bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoStep {
    pub at: usize,
    pub old: String,
    pub new: String,
    pub before: Range<usize>,
    pub after: Range<usize>,
}

/// The editor's undo history. RichEdit's own undo is off: with its spell checker running, the
/// `tomSuspend` way of keeping formatting out of RichEdit's undo stopped recording edits
/// altogether (measured: `EM_CANUNDO` false after an `EM_REPLACESEL` once the spell checker had
/// run), so the editor records text changes itself. Formatting is never recorded.
#[derive(Clone, Debug, Default)]
pub struct UndoStack {
    undo: Vec<UndoStep>,
    redo: Vec<UndoStep>,
    /// The last step may still grow with typing.
    open: bool,
}

const UNDO_LIMIT: usize = 500;

impl UndoStack {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The next change starts a new step.
    pub fn close_group(&mut self) {
        self.open = false;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Records the change from `old` to `new`. Typing merges into the open step when it goes on
    /// where that step ended (more characters, or more Backspaces), and a word typed after a
    /// space starts a new step, the way text editors group typing.
    pub fn record(
        &mut self,
        old: &str,
        new: &str,
        before: Range<usize>,
        after: Range<usize>,
        typing: bool,
    ) {
        let change = markdown::change_between(old, new, after.clone());
        let removed = old[change.range.clone()].to_owned();
        let step =
            UndoStep { at: change.range.start, old: removed, new: change.text, before, after };
        if step.old.is_empty() && step.new.is_empty() {
            return;
        }
        self.redo.clear();
        if typing
            && self.open
            && let Some(last) = self.undo.last_mut()
        {
            let inserting = step.old.is_empty() && last.old.is_empty();
            let continues_insert = inserting && step.at == last.at + last.new.len();
            let starts_word = last.new.ends_with(char::is_whitespace)
                && !step.new.starts_with(char::is_whitespace);
            if continues_insert && !starts_word && !step.new.contains('\n') {
                last.new.push_str(&step.new);
                last.after = step.after;
                return;
            }
            let deleting = step.new.is_empty() && last.new.is_empty();
            if deleting && step.at + step.old.len() == last.at {
                last.old.insert_str(0, &step.old);
                last.at = step.at;
                last.after = step.after;
                return;
            }
        }
        self.undo.push(step);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.open = typing;
    }

    /// The change that undoes the last step in `text`, and the selection to show.
    pub fn undo(&mut self, text: &str) -> Option<(Change, Range<usize>)> {
        let step = self.undo.pop()?;
        self.open = false;
        let end = step.at + step.new.len();
        if end > text.len() || text.get(step.at..end) != Some(step.new.as_str()) {
            // The text no longer matches the history (it was reloaded): drop the history.
            self.clear();
            return None;
        }
        let change =
            Change { range: step.at..end, text: step.old.clone(), selection: step.before.clone() };
        let selection = step.before.clone();
        self.redo.push(step);
        Some((change, selection))
    }

    pub fn redo(&mut self, text: &str) -> Option<(Change, Range<usize>)> {
        let step = self.redo.pop()?;
        self.open = false;
        let end = step.at + step.old.len();
        if end > text.len() || text.get(step.at..end) != Some(step.old.as_str()) {
            self.clear();
            return None;
        }
        let change =
            Change { range: step.at..end, text: step.new.clone(), selection: step.after.clone() };
        let selection = step.after.clone();
        self.undo.push(step);
        Some((change, selection))
    }
}

/// The UTF-16 range in the old text and the new length that one edit changed, from the two
/// texts (common prefix and suffix).
pub fn changed_range(old: &[u16], new: &[u16]) -> (Range<usize>, usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix =
        old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    (prefix..old.len() - suffix, new.len() - suffix - prefix)
}

/// The lines a selection touches (bytes; a selection ending at a line start does not touch
/// that line).
pub fn touched_lines(lines: &[Line], sel: Range<usize>) -> Range<usize> {
    let first = line_at(lines, sel.start);
    let mut last = line_at(lines, sel.end);
    if sel.end > sel.start && last > first && lines[last].range.start == sel.end {
        last -= 1;
    }
    first..last + 1
}

/// Where a collapsed caret must go so it never rests inside a list marker that is hidden: the
/// start of the item's words. `None` when it can stay.
///
/// The rule: the caret rests at the start of a block's words. A list, to-do or numbered marker
/// is hidden, so the caret never rests in it. A heading or quote marker shows while the caret is
/// on its line; on an empty line (nothing typed after the marker) a click, a new block or any
/// other move snaps the caret after it, so typing goes after the marker and the caret has the
/// block's size. Only a deliberate keyboard move into the marker (Left from the words, `editing`
/// true) may leave it there, for editing the Markdown itself.
pub fn snap_caret(lines: &[Line], caret: usize, editing: bool) -> Option<usize> {
    let line = &lines[line_at(lines, caret)];
    if caret >= line.content || caret < line.range.start {
        return None;
    }
    let empty = line.content == line.range.end;
    let snaps = line.block.is_list() || (reveals_prefix(line.block) && empty && !editing);
    snaps.then_some(line.content)
}

/// Does typing `ch` at `sel` complete a Markdown shortcut (the line's block or its inline marks
/// change)? Then the editor closes the typing group before and after the character, so one
/// Ctrl+Z gives back exactly what was typed before the conversion.
pub fn completes_shortcut(text: &str, sel: Range<usize>, ch: char) -> bool {
    if !sel.is_empty() || sel.end > text.len() || ch.is_control() {
        return false;
    }
    let lines = blocks(text);
    let line = &lines[line_at(&lines, sel.start)];
    let before = &text[line.range.clone()];
    let mut after = String::with_capacity(before.len() + 4);
    after.push_str(&text[line.range.start..sel.start]);
    after.push(ch);
    after.push_str(&text[sel.start..line.range.end]);
    let block = |line: &str| markdown::blocks(line)[0].block;
    let marks = |line: &str| markdown::style(line).iter().filter(|span| span.marker).count();
    block(before) != block(&after) || marks(before) != marks(&after)
}

/// The size a thumbnail is drawn at, logical pixels: the note's width at most, 200 px tall at
/// most, never larger than the picture's own size.
pub fn thumb_size(natural: (u32, u32), scale: f64, width: f64) -> (u16, u16) {
    let (w, h) = (
        f64::from(natural.0.max(1)) / scale.max(0.5),
        f64::from(natural.1.max(1)) / scale.max(0.5),
    );
    let factor = (width.max(40.0) / w).min(200.0 / h).min(1.0);
    ((w * factor).round().max(1.0) as u16, (h * factor).round().max(1.0) as u16)
}

// ---------------------------------------------------------------------------
// The control
// ---------------------------------------------------------------------------

/// What the editor tells its window.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorEvent {
    /// The text changed (read it with [`Editor::text`]).
    Changed,
    /// Ctrl+Tab, or Shift+Tab outside a list: move focus to the next or previous control.
    FocusNext,
    FocusPrevious,
    /// Ctrl+click on a link.
    OpenLink(String),
    /// Double-click on a thumbnail.
    OpenImage(PathBuf),
}

pub struct EditorConfig {
    pub palette: Palette,
    pub dpi: u32,
    /// The notes folder: image links resolve against it.
    pub base: PathBuf,
    pub motion: bool,
}

const SUBCLASS: usize = 0x4e07;
const ANIM_TIMER: usize = 0x4e01;
const WATCHDOG_TIMER: usize = 0x4e02;
const RETRY_TIMER: usize = 0x4e03;

/// `IGORIO_DEBUG=1`: the repaint watchdog is on (a one-shot timer per repaint asked for).
pub fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    match DEBUG_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => *ENABLED
            .get_or_init(|| std::env::var_os("IGORIO_DEBUG").is_some_and(|value| value == "1")),
    }
}

static DEBUG_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Tests: debug logging on or off regardless of `IGORIO_DEBUG` (`None` follows it again).
#[doc(hidden)]
pub fn test_set_debug(enabled: Option<bool>) {
    let value = match enabled {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    DEBUG_OVERRIDE.store(value, std::sync::atomic::Ordering::Relaxed);
}

/// One line in the app's debug log (`%LOCALAPPDATA%\Igorio\debug.log`, or the file
/// `IGORIO_DEBUG_LOG` names), only with `IGORIO_DEBUG=1`. Never fails.
pub fn debug_line(text: &str) {
    use std::io::Write as _;
    if !debug_enabled() {
        return;
    }
    let path = std::env::var_os("IGORIO_DEBUG_LOG").map(PathBuf::from).or_else(|| {
        std::env::var_os("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("Igorio").join("debug.log"))
    });
    let Some(path) = path else { return };
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{text}");
    }
}

/// Starts the one-shot watchdog for a window, in debug runs only (no timer otherwise).
fn arm_watchdog(hwnd: HWND) {
    if debug_enabled() {
        // SAFETY: a timer on our own window, killed when it fires.
        unsafe { SetTimer(Some(hwnd), WATCHDOG_TIMER, 1200, None) };
    }
}

/// Asks for another paint a little later (a paint that could not draw).
fn retry_paint_later(hwnd: HWND) {
    // SAFETY: a one-shot timer on our own window.
    unsafe { SetTimer(Some(hwnd), RETRY_TIMER, 250, None) };
}
const RESYNC: u32 = WM_APP + 0x31;
const BODY_FONT: &str = "Segoe UI Variable Text";
const MONO_FONT: &str = "Consolas";
const POPUP_CLASS: PCWSTR = w!("IgorioNoteSlashMenu");
const MENU_ROW: f64 = 28.0;
const MENU_WIDTH: f64 = 208.0;
const MENU_PAD: f64 = 6.0;
const MENU_SHADOW: f64 = 10.0;

/// Loads Msftedit.dll (RichEdit 4.1 and later, class `RICHEDIT50W`). Windows 8 and later ship
/// RichEdit 8 in it, which has everything the editor uses (`tomSuspend` needs RichEdit 8). Only an
/// older RichEdit (`riched20.dll`, no `tomSuspend`) means formatting would become undo steps, so
/// the editor refuses to start and the window reports the error instead of a broken editor.
pub fn load_richedit() -> Result<()> {
    // SAFETY: loads a system library by name; the module stays loaded for the process.
    unsafe { LoadLibraryW(w!("Msftedit.dll")) }.map(|_| ())
}

struct Thumb {
    /// BGRA, premultiplied, at the picture's own size.
    pixmap: tiny_skia::Pixmap,
    /// Logical size for the layout (picture pixels / its DPI scale).
    natural: (u32, u32),
}

/// The slash menu as last put on screen: where, how big, and its premultiplied BGRA pixels.
pub type PopupImage = (POINT, u32, u32, Vec<u8>);

struct SlashMenu {
    /// The `/query` range in bytes.
    range: Range<usize>,
    items: Vec<SlashItem>,
    selected: usize,
    highlight: Transition,
    intro: Transition,
    closing: bool,
}

/// Hit areas from the last paint, client pixels.
#[derive(Default)]
struct Hits {
    checks: Vec<(RECT, usize)>,
    images: Vec<(RECT, String)>,
    /// The placeholder's text box and baseline, client pixels.
    placeholder: Option<(RECT, i32)>,
    /// The drawn scroll thumb and its track, when the text overflows.
    thumb: Option<(RECT, RECT)>,
}

struct State {
    hwnd: HWND,
    doc: ITextDocument,
    ole: IRichEditOle,
    callback: RefCell<Option<IRichEditOleCallback>>,
    text: RefCell<String>,
    units: RefCell<Vec<u16>>,
    lines: RefCell<Vec<Line>>,
    layout: RefCell<Layout>,
    applied: RefCell<Applied>,
    fonts: RefCell<HashMap<Fmt, ITextFont>>,
    para_formats: RefCell<HashMap<Para, ITextPara>>,
    range: ITextRange,
    palette: Cell<Palette>,
    dpi: Cell<u32>,
    system_dpi: u32,
    width: Cell<f64>,
    base: RefCell<PathBuf>,
    motion: Cell<bool>,
    focused: Cell<bool>,
    show_reveal: Cell<bool>,
    suppress: Cell<u32>,
    composing: Cell<bool>,
    eat_char: Cell<Option<u16>>,
    revealed: RefCell<Range<usize>>,
    reveal: RefCell<HashMap<usize, Transition>>,
    fade: Cell<Option<(usize, Transition)>>,
    check: Cell<Option<(usize, Transition)>>,
    hover: Cell<(Option<usize>, Transition)>,
    images_in: RefCell<HashMap<String, Transition>>,
    known_images: RefCell<HashSet<String>>,
    thumbs: RefCell<HashMap<PathBuf, Option<Thumb>>>,
    hits: RefCell<Hits>,
    slash: RefCell<Option<SlashMenu>>,
    popup: Cell<HWND>,
    popup_shown: Cell<bool>,
    undo: RefCell<UndoStack>,
    /// A paint is running: layout and text updates wait for the next message.
    painting: Cell<bool>,
    /// The control's text services for drawing (`None` inside when it has none).
    services: RefCell<Option<Option<ITextServices>>>,
    /// The user moved into a revealed marker with the keyboard to edit it (see `snap_caret`).
    marker_edit: Cell<bool>,
    /// When a repaint was asked for and has not happened yet (the watchdog reads it).
    asked: Cell<Option<Instant>>,
    /// When the last paint finished.
    painted: Cell<Option<Instant>>,
    /// Tests: paints are refused (to see the watchdog report a stuck note).
    block_paint: Cell<bool>,
    /// Tests: runs inside the next paint (to send updates while a paint is in progress).
    during_paint: RefCell<Option<Box<dyn FnOnce()>>>,
    /// An eased wheel scroll: from, to (pixels) on the animation clock.
    scroll: Cell<Option<Transition>>,
    /// Dragging the drawn scroll thumb: the grab offset inside the thumb.
    thumb_drag: Cell<Option<i32>>,
    /// UTF-8 to UTF-16 offsets of `text`, rebuilt when the text changes.
    map: RefCell<Utf16Map>,
    recording: Cell<Recording>,
    last_selection: Cell<Range<usize>>,
    popup_image: RefCell<Option<PopupImage>>,
    timer: Cell<bool>,
    epoch: Instant,
    frozen: Cell<Option<f64>>,
    surface: RefCell<Option<Surface>>,
    gdi: RefCell<Option<GdiFonts>>,
    sink: Box<dyn Fn(EditorEvent)>,
    last_apply: Cell<Duration>,
    last_runs: Cell<usize>,
}

/// The editor control. The window owns it; dropping it destroys the control.
pub struct Editor {
    state: Box<State>,
}

impl Editor {
    /// Creates the control as a child of `parent` with the control id `id`.
    pub fn create(
        parent: HWND,
        id: i32,
        config: EditorConfig,
        sink: Box<dyn Fn(EditorEvent)>,
    ) -> Result<Editor> {
        load_richedit()?;
        // SAFETY: the module handle of this process; no pointers kept.
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        let style = WINDOW_STYLE(
            WS_CHILD.0
                | WS_VISIBLE.0
                | WS_TABSTOP.0
                | ES_MULTILINE as u32
                | ES_AUTOVSCROLL as u32
                | ES_NOHIDESEL as u32
                | ES_SAVESEL,
        );
        // SAFETY: a child control of our own window, created on its thread with static strings.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("RICHEDIT50W"),
                w!(""),
                style,
                0,
                0,
                10,
                10,
                Some(parent),
                Some(HMENU(id as usize as *mut c_void)),
                Some(instance),
                None,
            )?
        };
        let send = |msg: u32, wp: usize, lp: isize| {
            // SAFETY: scalar or local-pointer messages to the control this thread just created.
            unsafe { SendMessageW(hwnd, msg, Some(WPARAM(wp)), Some(LPARAM(lp))) }
        };
        send(
            EM_SETTEXTMODE,
            (TM_RICHTEXT.0 | TM_MULTILEVELUNDO.0 | TM_MULTICODEPAGE.0) as usize,
            0,
        );
        // The editor keeps its own undo (UndoStack); RichEdit records nothing.
        send(EM_SETUNDOLIMIT, 0, 0);
        send(EM_AUTOURLDETECT, 0, 0);
        send(
            EM_SETTYPOGRAPHYOPTIONS,
            TO_ADVANCEDTYPOGRAPHY as usize,
            TO_ADVANCEDTYPOGRAPHY as isize,
        );
        let lang = send(EM_GETLANGOPTIONS, 0, 0).0 as u32;
        let lang = (lang | IMF_AUTOFONT | IMF_DUALFONT | IMF_SPELLCHECKING | IMF_TKBPREDICTION)
            & !IMF_AUTOKEYBOARD;
        send(EM_SETLANGOPTIONS, 0, lang as isize);
        send(
            EM_SETEDITSTYLE,
            (SES_CTFALLOWPROOFING | SES_SMARTDRAGDROP) as usize,
            (SES_CTFALLOWPROOFING | SES_SMARTDRAGDROP) as isize,
        );
        send(
            EM_SETEDITSTYLEEX,
            (SES_EX_NOTABLE | SES_EX_NOMATH) as usize,
            (SES_EX_NOTABLE | SES_EX_NOMATH) as isize,
        );
        send(EM_SETEVENTMASK, 0, (ENM_CHANGE | ENM_SELCHANGE) as isize);
        let mut unknown: Option<windows::core::IUnknown> = None;
        send(EM_GETOLEINTERFACE, 0, &mut unknown as *mut _ as isize);
        let ole: IRichEditOle = unknown.ok_or_else(|| Error::from(E_FAIL))?.cast()?;
        let doc: ITextDocument = ole.cast()?;
        // SAFETY: TOM call on this thread's control.
        let range = unsafe { doc.Range(0, 0)? };
        // SAFETY: plain scalar query.
        let system_dpi = unsafe { GetDpiForSystem() }.max(96);
        let state = Box::new(State {
            hwnd,
            doc,
            ole,
            callback: RefCell::new(None),
            text: RefCell::new(String::new()),
            units: RefCell::new(Vec::new()),
            lines: RefCell::new(blocks("")),
            layout: RefCell::new(Layout::default()),
            applied: RefCell::new(Applied::default()),
            fonts: RefCell::new(HashMap::new()),
            para_formats: RefCell::new(HashMap::new()),
            range,
            palette: Cell::new(config.palette),
            dpi: Cell::new(config.dpi),
            system_dpi,
            width: Cell::new(260.0),
            base: RefCell::new(config.base),
            motion: Cell::new(config.motion),
            focused: Cell::new(false),
            show_reveal: Cell::new(false),
            suppress: Cell::new(0),
            composing: Cell::new(false),
            eat_char: Cell::new(None),
            revealed: RefCell::new(0..0),
            reveal: RefCell::new(HashMap::new()),
            fade: Cell::new(None),
            check: Cell::new(None),
            hover: Cell::new((None, Transition::default())),
            images_in: RefCell::new(HashMap::new()),
            known_images: RefCell::new(HashSet::new()),
            thumbs: RefCell::new(HashMap::new()),
            hits: RefCell::new(Hits::default()),
            slash: RefCell::new(None),
            popup: Cell::new(HWND::default()),
            popup_shown: Cell::new(false),
            undo: RefCell::new(UndoStack::default()),
            painting: Cell::new(false),
            services: RefCell::new(None),
            marker_edit: Cell::new(false),
            asked: Cell::new(None),
            painted: Cell::new(None),
            block_paint: Cell::new(false),
            during_paint: RefCell::new(None),
            scroll: Cell::new(None),
            thumb_drag: Cell::new(None),
            map: RefCell::new(Utf16Map::new("")),
            recording: Cell::new(Recording::Typing),
            last_selection: Cell::new(0..0),
            popup_image: RefCell::new(None),
            timer: Cell::new(false),
            epoch: Instant::now(),
            frozen: Cell::new(None),
            surface: RefCell::new(None),
            gdi: RefCell::new(None),
            sink,
            last_apply: Cell::new(Duration::ZERO),
            last_runs: Cell::new(0),
        });
        let pointer = &*state as *const State;
        let callback = OleCallback::create(pointer)?;
        send(EM_SETOLECALLBACK, 0, callback.as_raw() as isize);
        *state.callback.borrow_mut() = Some(callback);
        // SAFETY: same-thread subclass; the State box outlives the control (Drop destroys the
        // control before the box is freed) and the subclass is removed on WM_NCDESTROY.
        if !unsafe { SetWindowSubclass(hwnd, Some(editor_proc), SUBCLASS, pointer as usize) }
            .as_bool()
        {
            return Err(Error::from_thread());
        }
        set_accessible_name(hwnd, "Note text");
        let editor = Editor { state };
        editor.apply_theme();
        editor.state.set_zoom();
        editor.set_text("");
        Ok(editor)
    }

    pub fn hwnd(&self) -> HWND {
        self.state.hwnd
    }

    /// Replaces the text (loading a note, or a reload from disk). Clears undo. Keeps the caret
    /// at `caret` (bytes, clamped) when given, else at the end.
    pub fn set_text(&self, text: &str) {
        let s = &*self.state;
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut units = control_units(&text);
        units.push(0);
        let settext = SETTEXTEX { flags: ST_DEFAULT, codepage: 1200 };
        s.suppress.set(s.suppress.get() + 1);
        // SAFETY: the struct and the NUL-terminated buffer live through the synchronous call.
        unsafe {
            SendMessageW(
                s.hwnd,
                EM_SETTEXTEX,
                Some(WPARAM(&settext as *const _ as usize)),
                Some(LPARAM(units.as_ptr() as isize)),
            )
        };
        s.suppress.set(s.suppress.get() - 1);
        units.pop();
        *s.known_images.borrow_mut() = blocks(&text)
            .iter()
            .filter_map(|line| markdown::image_target(&text[line.range.clone()]).map(str::to_owned))
            .collect();
        *s.lines.borrow_mut() = blocks(&text);
        *s.map.borrow_mut() = Utf16Map::new(&text);
        *s.text.borrow_mut() = text;
        *s.units.borrow_mut() = units;
        *s.applied.borrow_mut() = Applied::default();
        s.undo.borrow_mut().clear();
        s.last_selection.set(0..0);
        s.reveal.borrow_mut().clear();
        s.fade.set(None);
        // Setting the text resets RichEdit's zoom: put the DPI zoom back.
        s.set_zoom();
        s.relayout_all();
    }

    /// The note text (`\n` line breaks).
    pub fn text(&self) -> String {
        self.state.text.borrow().clone()
    }

    /// The selection in bytes of [`Editor::text`].
    pub fn selection(&self) -> Range<usize> {
        self.state.selection_bytes()
    }

    /// Selects a byte range (the caret when empty) and scrolls it into view.
    pub fn select(&self, range: Range<usize>) {
        self.state.select_bytes(range);
    }

    /// Notifications from the parent: `EN_CHANGE` from `WM_COMMAND`, `EN_SELCHANGE` from
    /// `WM_NOTIFY`.
    pub fn notify(&self, code: u32) {
        match code {
            EN_CHANGE => self.state.text_changed(),
            EN_SELCHANGE => self.state.selection_changed(),
            _ => {}
        }
    }

    pub fn set_palette(&self, palette: Palette) {
        if self.state.palette.get() != palette {
            self.state.palette.set(palette);
            self.apply_theme();
        }
    }

    fn apply_theme(&self) {
        let s = &*self.state;
        let palette = s.palette.get();
        // SAFETY: scalar message and a static theme name for this thread's control.
        unsafe {
            SendMessageW(
                s.hwnd,
                EM_SETBKGNDCOLOR,
                Some(WPARAM(0)),
                Some(LPARAM(palette.paper.0 as isize)),
            );
            let _ = SetWindowTheme(
                s.hwnd,
                if palette.dark { w!("DarkMode_Explorer") } else { w!("Explorer") },
                None,
            );
        }
        s.fonts.borrow_mut().clear();
        *s.applied.borrow_mut() = Applied::default();
        s.relayout_all();
        s.redraw_popup();
    }

    /// A new DPI (the window moved to another monitor): RichEdit's zoom and the drawn parts
    /// follow; formats keep their sizes in points.
    pub fn set_dpi(&self, dpi: u32) {
        let s = &*self.state;
        if s.dpi.get() != dpi {
            s.dpi.set(dpi);
            *s.gdi.borrow_mut() = None;
            s.set_zoom();
            s.invalidate();
        }
    }

    /// Moves the control (client pixels of the parent).
    pub fn set_rect(&self, rect: RECT) {
        let s = &*self.state;
        // SAFETY: same-thread child window.
        let _ = unsafe {
            SetWindowPos(
                s.hwnd,
                None,
                rect.left,
                rect.top,
                (rect.right - rect.left).max(1),
                (rect.bottom - rect.top).max(1),
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        let width = f64::from(rect.right - rect.left) * 96.0 / f64::from(s.dpi.get());
        if (width - s.width.get()).abs() >= 1.0 {
            s.width.set(width);
            s.relayout();
        }
    }

    pub fn set_base(&self, base: PathBuf) {
        *self.state.base.borrow_mut() = base;
        self.state.thumbs.borrow_mut().clear();
        self.state.relayout();
    }

    pub fn set_motion(&self, motion: bool) {
        self.state.motion.set(motion);
    }

    pub fn focus(&self) {
        // SAFETY: same-thread control.
        let _ = unsafe { SetFocus(Some(self.state.hwnd)) };
    }

    /// Inserts Markdown for screenshots (`markdown::insert_screenshot`) at a screen point over
    /// the text, or at the caret, as one undo step.
    pub fn insert_screenshots(&self, at: Option<POINT>, markdown_text: &str) {
        let s = &*self.state;
        let mut caret = s.selection_bytes().end;
        if let Some(point) = at {
            let mut client = point;
            // SAFETY: same-thread window and a writable local point.
            let inside = unsafe {
                let _ = ScreenToClient(s.hwnd, &mut client);
                let mut rect = RECT::default();
                let _ = GetClientRect(s.hwnd, &mut rect);
                client.x >= rect.left
                    && client.x < rect.right
                    && client.y >= rect.top
                    && client.y < rect.bottom
            };
            if inside {
                let local = windows::Win32::Foundation::POINTL { x: client.x, y: client.y };
                // SAFETY: EM_CHARFROMPOS reads the POINTL during the call.
                let unit = unsafe {
                    SendMessageW(
                        s.hwnd,
                        EM_CHARFROMPOS,
                        None,
                        Some(LPARAM(&local as *const _ as isize)),
                    )
                }
                .0 as usize;
                let map = s.map.borrow();
                caret = map.to8(unit);
                // A drop on a line goes after that line's words, never into a hidden marker.
                let lines = s.lines.borrow();
                let line = &lines[line_at(&lines, caret)];
                if caret < line.content {
                    caret = line.content;
                }
            }
        }
        let text = s.text.borrow().clone();
        let edit = markdown::insert_screenshot(&text, caret, markdown_text);
        let change =
            Change { range: edit.range, text: edit.text, selection: edit.cursor..edit.cursor };
        s.apply_change(&change);
    }

    /// Wraps an edit the window makes (or a test) as one undo step.
    pub fn apply(&self, change: &Change) {
        self.state.apply_change(change);
    }

    /// Restyles the whole note from scratch and returns how long it took.
    pub fn restyle_all(&self) -> Duration {
        let s = &*self.state;
        *s.applied.borrow_mut() = Applied::default();
        let started = Instant::now();
        s.relayout();
        started.elapsed()
    }

    /// How long the last formatting pass took and how many runs it applied.
    pub fn last_apply(&self) -> (Duration, usize) {
        (self.state.last_apply.get(), self.state.last_runs.get())
    }

    /// The window lost or gained focus as a whole; markers show only while the note is focused.
    pub fn set_focused(&self, focused: bool) {
        let s = &*self.state;
        s.show_reveal.set(focused);
        s.selection_changed();
    }

    /// Plain text from a data object, the way a paste takes it (tests pass an in-memory one).
    pub fn paste_data(&self, data: &IDataObject) -> bool {
        self.state.paste_object(data)
    }

    /// The clipboard text a copy of `range` (bytes) produces.
    pub fn copy_text(&self, range: Range<usize>) -> String {
        clipboard_text(self.state.text.borrow().get(range).unwrap_or_default())
    }

    /// The control's own OLE interface (tests import data through it).
    pub fn rich_edit_ole(&self) -> IRichEditOle {
        self.state.ole.clone()
    }

    /// Clicks the checkbox of the to-do on `line`, as a mouse click does.
    pub fn click_check(&self, line: usize) -> bool {
        self.state.toggle_check(line)
    }

    /// Runs a formatting key's action (tests and menus): Ctrl+B and the others.
    pub fn command(&self, command: EditorCommand) {
        self.state.run_command(command);
    }

    /// Holds animations at a time in milliseconds (tests), or lets them run again (`None`).
    pub fn freeze(&self, at: Option<f64>) {
        self.state.frozen.set(at);
        self.state.tick();
        // Formats for the held time (a reveal fade, a block fade), not only the drawings.
        self.state.relayout();
    }

    pub fn now(&self) -> f64 {
        self.state.now()
    }

    /// Is the slash menu open, with which entries and which one selected?
    pub fn slash_state(&self) -> Option<(Vec<SlashItem>, usize, HWND)> {
        self.state
            .slash
            .borrow()
            .as_ref()
            .filter(|menu| !menu.closing)
            .map(|menu| (menu.items.clone(), menu.selected, self.state.popup.get()))
    }

    /// Paints now and gives the placeholder's box and baseline, the revealed marker's box and
    /// baseline on the caret's line (if it has one), and the caret's x (tests).
    pub fn placeholder_geometry(&self) -> Option<PlaceholderGeometry> {
        let s = &*self.state;
        // SAFETY: a synchronous repaint of our own control.
        let _ = unsafe { RedrawWindow(Some(s.hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW) };
        let (placeholder, baseline) = s.hits.borrow().placeholder?;
        let caret = s.selection_units().end;
        let (caret_x, _) = s.position(caret);
        let marker = {
            let lines = s.lines.borrow();
            let map = s.map.borrow();
            let line = &lines[line_at(&lines, map.to8(caret))];
            (line.content > line.range.start && reveals_prefix(line.block))
                .then(|| (map.to16(line.range.start), map.to16(line.content)))
        };
        let marker = marker.and_then(|(start, end)| {
            let (left, top) = s.point_of(start, TA_TOP.0 as i32)?;
            let (right, bottom) = s.point_of(end, TA_BOTTOM.0 as i32)?;
            let (_, base) = s.point_of(start, TA_BASELINE.0 as i32)?;
            Some((RECT { left, top, right, bottom }, base))
        });
        Some(PlaceholderGeometry { placeholder, baseline, marker, caret_x })
    }

    /// The scroll position in pixels and the drawn thumb, after a paint (tests).
    pub fn scroll_state(&self) -> (i32, Option<RECT>) {
        let s = &*self.state;
        // SAFETY: a synchronous repaint of our own control.
        let _ = unsafe { RedrawWindow(Some(s.hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW) };
        let (y, _, _) = s.scroll_metrics();
        (y, s.hits.borrow().thumb.map(|(thumb, _)| thumb))
    }

    /// The system caret's rectangle in the editor's client pixels, when this control shows
    /// one (it needs keyboard focus), and the selection in bytes (tests).
    pub fn caret_rect(&self) -> (Option<RECT>, Range<usize>) {
        let s = &*self.state;
        let mut info =
            GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        // SAFETY: a query about this thread's caret into a local struct.
        let ok = unsafe {
            GetGUIThreadInfo(windows::Win32::System::Threading::GetCurrentThreadId(), &mut info)
        }
        .is_ok();
        let caret = (ok && info.hwndCaret == s.hwnd).then_some(info.rcCaret);
        (caret, s.selection_bytes())
    }

    /// Tests: refuse paints (true) to see the watchdog report a stuck note, or paint again.
    pub fn test_block_paint(&self, block: bool) {
        self.state.block_paint.set(block);
        self.state.invalidate();
    }

    /// Tests: run `hook` inside the next paint, while it is in progress.
    pub fn test_during_paint(&self, hook: Box<dyn FnOnce()>) {
        *self.state.during_paint.borrow_mut() = Some(hook);
        self.state.invalidate();
    }

    /// A repaint is owed and how long ago it was asked for (tests and the watchdog).
    pub fn paint_debt(&self) -> Option<Duration> {
        self.state.asked.get().map(|at| at.elapsed())
    }

    /// The top-left of a line's first character, client pixels (tests).
    pub fn line_point(&self, line: usize) -> (i32, i32) {
        let s = &*self.state;
        let start = s.lines.borrow().get(line).map_or(0, |line| line.range.start);
        let unit = s.map.borrow().to16(start);
        s.position(unit)
    }

    /// Is the caret inside the editor's visible area (tests)?
    pub fn caret_visible(&self) -> bool {
        let s = &*self.state;
        let caret = s.selection_units().end;
        let (x, y) = s.position(caret);
        let mut client = RECT::default();
        // SAFETY: same-thread window, writable local.
        let _ = unsafe { GetClientRect(s.hwnd, &mut client) };
        x >= client.left && x <= client.right && y >= client.top && y < client.bottom
    }

    /// Paints now and checks that every drawn checkbox sits on its own line (tests). `None` when
    /// they all do, else what is off.
    pub fn decos_aligned(&self) -> Option<String> {
        let s = &*self.state;
        // SAFETY: a synchronous repaint of our own control.
        let _ = unsafe { RedrawWindow(Some(s.hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW) };
        let layout = s.layout.borrow();
        for (rect, line) in s.hits.borrow().checks.iter() {
            let Some(deco) = layout.decos.iter().find(|deco| deco.line == *line) else {
                return Some(format!("box for line {line} has no drawing"));
            };
            let (_, top) = s.position(deco.anchor);
            let height = px(f64::from(deco.size) * 1.36, s.dpi.get());
            let middle = (rect.top + rect.bottom) / 2;
            if middle < top || middle > top + height {
                return Some(format!(
                    "box of line {line} at {middle}, its line spans {top}..{}",
                    top + height
                ));
            }
        }
        None
    }

    /// Is any animation timer running?
    pub fn animating(&self) -> bool {
        self.state.timer.get()
    }

    /// The layout's drawings (tests check bullets, boxes and images through it).
    pub fn decos(&self) -> Vec<Deco> {
        self.state.layout.borrow().decos.clone()
    }

    /// What a copy of the UTF-16 `range` puts on the clipboard, asked through RichEdit's own
    /// OLE callback (tests: the real clipboard is never written).
    pub fn copy_through_callback(&self, range: Range<usize>) -> Option<String> {
        let callback = self.state.callback.borrow().clone()?;
        let mut chars = CHARRANGE { cpMin: range.start as i32, cpMax: range.end as i32 };
        let mut object: Option<IDataObject> = None;
        // SAFETY: our own callback object; the range and out pointer are locals for the call.
        unsafe { callback.GetClipboardData(&mut chars, RECO_COPY.0, &mut object).ok()? };
        unicode_text(&object?)
    }

    /// The slash menu as last put on screen: its screen position, size and premultiplied BGRA
    /// pixels (tests compose it into pictures; a layered window cannot be printed).
    pub fn popup_image(&self) -> Option<PopupImage> {
        self.state.popup_image.borrow().clone()
    }

    /// Simulates a key the way the message loop delivers it (tests): `WM_KEYDOWN`, then
    /// `WM_CHAR` for the given characters, then `WM_KEYUP`.
    pub fn send_key(&self, vk: u16, chars: &str) {
        let hwnd = self.state.hwnd;
        NO_MODIFIERS.with(|flag| flag.set(true));
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                NO_MODIFIERS.with(|flag| flag.set(false));
            }
        }
        let _restore = Restore;
        // SAFETY: messages to our own control on its own thread.
        unsafe {
            SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(usize::from(vk))), Some(LPARAM(1)));
            for unit in chars.encode_utf16() {
                SendMessageW(hwnd, WM_CHAR, Some(WPARAM(usize::from(unit))), Some(LPARAM(1)));
            }
            SendMessageW(
                hwnd,
                WM_KEYUP,
                Some(WPARAM(usize::from(vk))),
                Some(LPARAM(0xC000_0001u32 as i32 as isize)),
            );
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        let s = &*self.state;
        // SAFETY: windows of this thread; the popup and the control are destroyed before the
        // State they point to is freed.
        unsafe {
            if !s.popup.get().is_invalid() {
                let _ = DestroyWindow(s.popup.get());
            }
            if IsWindow(Some(s.hwnd)).as_bool() {
                let _ = DestroyWindow(s.hwnd);
            }
        }
    }
}

/// Where a placeholder was drawn, against the marker before it (tests).
#[derive(Clone, Copy, Debug)]
pub struct PlaceholderGeometry {
    pub placeholder: RECT,
    pub baseline: i32,
    /// The revealed marker's box and baseline, when the line shows one.
    pub marker: Option<(RECT, i32)>,
    pub caret_x: i32,
}

/// Keyboard and menu actions of the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorCommand {
    Inline(Inline),
    Link,
    Block(BlockCommand),
    ToggleTodo,
}

/// The editor's keys: Ctrl+B, I, E, K, Ctrl+Shift+X, Ctrl+Alt+0 to 3 (left Alt only, so AltGr
/// characters still type), Ctrl+Shift+7, 8, 9.
pub fn command_for_key(vk: u16, ctrl: bool, shift: bool, left_alt: bool) -> Option<EditorCommand> {
    if !ctrl {
        return None;
    }
    let key = vk as u8;
    Some(match (shift, left_alt, key) {
        (false, false, b'B') => EditorCommand::Inline(Inline::Bold),
        (false, false, b'I') => EditorCommand::Inline(Inline::Italic),
        (false, false, b'E') => EditorCommand::Inline(Inline::Code),
        (true, false, b'X') => EditorCommand::Inline(Inline::Strike),
        (false, false, b'K') => EditorCommand::Link,
        (false, true, b'0') => EditorCommand::Block(BlockCommand::Text),
        (false, true, b'1') => EditorCommand::Block(BlockCommand::Heading(1)),
        (false, true, b'2') => EditorCommand::Block(BlockCommand::Heading(2)),
        (false, true, b'3') => EditorCommand::Block(BlockCommand::Heading(3)),
        (true, false, b'8') => EditorCommand::Block(BlockCommand::Bullet),
        (true, false, b'7') => EditorCommand::Block(BlockCommand::Numbered),
        (true, false, b'9') => EditorCommand::Block(BlockCommand::Todo),
        _ => return None,
    })
}

/// Ctrl combinations RichEdit may keep: clipboard, undo, select all, word moves and deletes.
/// Every other Ctrl key would apply rich formatting (Ctrl+E centres, Ctrl+1 sets line spacing,
/// Ctrl+Shift+L makes a numbered paragraph...), which must never get into a note.
pub fn ctrl_key_allowed(vk: u16, shift: bool) -> bool {
    let key = VIRTUAL_KEY(vk);
    matches!(vk as u8, b'A' | b'C' | b'V' | b'X' | b'Y' | b'Z')
        && !(shift && matches!(vk as u8, b'A' | b'C' | b'V' | b'Y'))
        || matches!(key, VK_LEFT | VK_UP | VK_DOWN | VK_HOME | VK_BACK | VK_DELETE | VK_INSERT)
        || matches!(vk, 0x21..=0x23 | 0x27) // Page Up, Page Down, End, Right
}

impl State {
    fn now(&self) -> f64 {
        self.frozen.get().unwrap_or_else(|| self.epoch.elapsed().as_secs_f64() * 1000.0)
    }

    fn duration(&self, ms: f64) -> f64 {
        if self.motion.get() { ms } else { 0.0 }
    }

    fn send(&self, msg: u32, wp: usize, lp: isize) -> LRESULT {
        // SAFETY: messages to this thread's control; pointer arguments are locals that live
        // through the synchronous call.
        unsafe { SendMessageW(self.hwnd, msg, Some(WPARAM(wp)), Some(LPARAM(lp))) }
    }

    fn set_zoom(&self) {
        // RichEdit sizes text in points at the system DPI; the zoom makes it the window's DPI.
        let (numerator, denominator) = (self.dpi.get(), self.system_dpi);
        self.send(EM_SETZOOM, numerator as usize, denominator as isize);
    }

    fn invalidate(&self) {
        if self.asked.get().is_none() {
            self.asked.set(Some(Instant::now()));
            arm_watchdog(self.hwnd);
        }
        // SAFETY: same-thread window.
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    /// One line of the editor's paint state, for the watchdog.
    fn paint_state(&self) -> String {
        format!(
            "editor painting={} suppress={} composing={} timer={} frozen={} asked={:?} painted={:?} surface={} layout_decos={} blocked={}",
            self.painting.get(),
            self.suppress.get(),
            self.composing.get(),
            self.timer.get(),
            self.frozen.get().is_some(),
            self.asked.get().map(|at| at.elapsed()),
            self.painted.get().map(|at| at.elapsed()),
            self.surface
                .try_borrow()
                .map_or("busy".to_owned(), |surface| surface.is_some().to_string()),
            self.layout.try_borrow().map_or(0, |layout| layout.decos.len()),
            self.block_paint.get(),
        )
    }

    /// The watchdog's check: a repaint asked for over a second ago on a visible control gets a
    /// debug line, and a forced repaint either way.
    fn watchdog(&self) {
        let Some(asked) = self.asked.get() else { return };
        // SAFETY: a plain query about our own window.
        let visible = unsafe { IsWindowVisible(self.hwnd) }.as_bool();
        if visible && asked.elapsed() > Duration::from_secs(1) {
            debug_line(&format!(
                "notes-paint: no paint for {:?} after asking: {}",
                asked.elapsed(),
                self.paint_state()
            ));
            // SAFETY: a synchronous repaint of our own control.
            let _ = unsafe {
                RedrawWindow(Some(self.hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW)
            };
        } else if visible {
            arm_watchdog(self.hwnd);
        }
    }

    /// The control's text, read fresh.
    fn read_units(&self) -> Vec<u16> {
        let length = GETTEXTLENGTHEX { flags: GTL_NUMCHARS | GTL_PRECISE, codepage: 1200 };
        let count =
            self.send(EM_GETTEXTLENGTHEX, &length as *const _ as usize, 0).0.max(0) as usize;
        let mut buffer = vec![0u16; count + 1];
        let request = GETTEXTEX {
            cb: (buffer.len() * 2) as u32,
            flags: GT_DEFAULT,
            codepage: 1200,
            lpDefaultChar: windows::core::PCSTR::null(),
            lpUsedDefChar: std::ptr::null_mut(),
        };
        let copied = self
            .send(EM_GETTEXTEX, &request as *const _ as usize, buffer.as_mut_ptr() as isize)
            .0
            .max(0) as usize;
        buffer.truncate(copied.min(count));
        buffer
    }

    fn selection_units(&self) -> Range<usize> {
        let mut range = CHARRANGE::default();
        self.send(EM_EXGETSEL, 0, &mut range as *mut _ as isize);
        let (a, b) = (range.cpMin.max(0) as usize, range.cpMax.max(0) as usize);
        a.min(b)..a.max(b)
    }

    fn selection_bytes(&self) -> Range<usize> {
        let units = self.selection_units();
        let map = self.map.borrow();
        map.to8(units.start)..map.to8(units.end)
    }

    fn select_units(&self, range: Range<usize>) {
        let range = CHARRANGE { cpMin: range.start as i32, cpMax: range.end as i32 };
        self.send(EM_EXSETSEL, 0, &range as *const _ as isize);
    }

    fn select_bytes(&self, range: Range<usize>) {
        let map = self.map.borrow();
        let units = map.to16(range.start)..map.to16(range.end);
        self.suppress.set(self.suppress.get() + 1);
        self.select_units(units);
        self.suppress.set(self.suppress.get() - 1);
        self.selection_changed();
    }

    /// The control's text changed (typing, undo, paste, an edit of ours).
    fn text_changed(&self) {
        if self.painting.get() {
            self.post_resync();
            return;
        }
        if self.suppress.get() > 0 {
            return;
        }
        if self.composing.get() {
            return;
        }
        let mut units = self.read_units();
        // No embedded objects in a note (a drop RichEdit took as an object): take them out.
        if units.contains(&0xfffc) {
            self.suppress.set(self.suppress.get() + 1);
            for (at, _) in units.iter().enumerate().rev().filter(|(_, unit)| **unit == 0xfffc) {
                self.replace_units(at..at + 1, &[]);
            }
            self.suppress.set(self.suppress.get() - 1);
            units = self.read_units();
        }
        let (old, new_len) = changed_range(&self.units.borrow(), &units);
        if old.is_empty() && new_len == 0 {
            return;
        }
        let old_caret_line = {
            let lines = self.lines.borrow();
            let map = self.map.borrow();
            line_at(&lines, map.to8(old.start))
        };
        let old_block = self.lines.borrow().get(old_caret_line).map(|line| line.block);
        let old_count = self.lines.borrow().len();
        self.applied.borrow_mut().edited(old, new_len);
        let text = from_control(&units);
        let map = Utf16Map::new(&text);
        let selection_after = {
            let units = self.selection_units();
            map.to8(units.start)..map.to8(units.end)
        };
        match self.recording.get() {
            Recording::Off => {}
            mode => {
                let before = self.last_selection.take();
                let old_text = self.text.borrow();
                self.undo.borrow_mut().record(
                    &old_text,
                    &text,
                    before,
                    selection_after.clone(),
                    mode == Recording::Typing,
                );
            }
        }
        self.last_selection.set(selection_after);
        let lines = blocks(&text);
        // A block that changed type in place fades in; typing itself never animates.
        let caret = map.to8(self.selection_units().end);
        let caret_line = line_at(&lines, caret);
        let now = self.now();
        if lines.len() == old_count
            && caret_line == old_caret_line
            && old_block.is_some_and(|block| block != lines[caret_line].block)
            && self.motion.get()
        {
            let mut fade = Transition::default();
            fade.run(0.0, 1.0, now, BLOCK_FADE_MS);
            self.fade.set(Some((caret_line, fade)));
        } else {
            self.fade.set(None);
        }
        // Images that are new to this note grow in.
        for line in &lines {
            if let Some(path) = markdown::image_target(&text[line.range.clone()]) {
                let fresh = self.known_images.borrow_mut().insert(path.to_owned());
                if fresh {
                    let mut appear = Transition::default();
                    appear.run(0.0, 1.0, now, self.duration(IMAGE_MS));
                    self.images_in.borrow_mut().insert(path.to_owned(), appear);
                }
            }
        }
        *self.units.borrow_mut() = units;
        *self.lines.borrow_mut() = lines;
        *self.text.borrow_mut() = text;
        *self.map.borrow_mut() = map;
        // Line numbers moved: the caret's lines show their markers at once.
        self.reveal.borrow_mut().clear();
        self.update_reveal(false);
        self.relayout();
        self.update_slash(false);
        self.start_timer();
        (self.sink)(EditorEvent::Changed);
    }

    fn selection_changed(&self) {
        if self.painting.get() {
            self.post_resync();
            return;
        }
        if self.suppress.get() > 0 || self.composing.get() {
            return;
        }
        let sel = self.selection_bytes();
        if sel.is_empty() {
            let target = snap_caret(&self.lines.borrow(), sel.start, self.marker_edit.get());
            if let Some(target) = target {
                let map = self.map.borrow();
                self.suppress.set(self.suppress.get() + 1);
                self.select_units(map.to16(target)..map.to16(target));
                self.suppress.set(self.suppress.get() - 1);
            }
        }
        // The caret moved away from where the last edit left it: typing starts a new undo step.
        let now_selected = self.selection_bytes();
        let last = self.last_selection.take();
        if now_selected != last {
            self.undo.borrow_mut().close_group();
        }
        self.last_selection.set(now_selected);
        if self.update_reveal(true) {
            self.relayout();
        } else {
            // The empty-line placeholder follows the caret.
            self.relayout_decos_only();
        }
        self.update_slash(false);
        self.start_timer();
    }

    /// Sets which lines show their markers. Returns true when that changed.
    fn update_reveal(&self, animate: bool) -> bool {
        let wanted = if self.show_reveal.get() {
            let sel = self.selection_bytes();
            touched_lines(&self.lines.borrow(), sel)
        } else {
            0..0
        };
        let changed = *self.revealed.borrow() != wanted;
        let now = self.now();
        let mut tracks = self.reveal.borrow_mut();
        for line in wanted.clone() {
            let track = tracks.entry(line).or_insert_with(|| Transition::at(0.0));
            track.retarget(1.0, now, if animate { self.duration(REVEAL_MS) } else { 0.0 });
        }
        for (line, track) in tracks.iter_mut() {
            if !wanted.contains(line) {
                track.retarget(0.0, now, if animate { self.duration(HIDE_MS) } else { 0.0 });
            }
        }
        tracks.retain(|_, track| track.to > 0.0 || track.active(now));
        *self.revealed.borrow_mut() = wanted;
        changed
    }

    fn empty_caret_line(&self) -> Option<usize> {
        if !self.focused.get() && !self.show_reveal.get() {
            return None;
        }
        let sel = self.selection_bytes();
        if !sel.is_empty() || self.text.borrow().is_empty() {
            return None;
        }
        let lines = self.lines.borrow();
        let index = line_at(&lines, sel.start);
        let line = &lines[index];
        (line.content == line.range.end).then_some(index)
    }

    fn thumb_for(&self, path: &str) -> Option<(u16, u16)> {
        let full = self.resolve(path)?;
        let mut thumbs = self.thumbs.borrow_mut();
        let thumb = thumbs.entry(full.clone()).or_insert_with(|| load_thumb(&full));
        let thumb = thumb.as_ref()?;
        Some(thumb_size(thumb.natural, 1.0, self.width.get() - 8.0))
    }

    fn resolve(&self, path: &str) -> Option<PathBuf> {
        if path.contains("://") {
            return None;
        }
        let path = Path::new(path);
        Some(if path.is_absolute() { path.to_owned() } else { self.base.borrow().join(path) })
    }

    fn compute_layout(&self) -> Layout {
        let text = self.text.borrow();
        let lines = self.lines.borrow();
        let spans = markdown::style(&text);
        let now = self.now();
        let reveal = self.reveal.borrow();
        // A marker being revealed takes its room from the first frame (only its colour fades
        // in), so the caret and the drawings are placed for the final layout at once.
        let reveal_of = |line: usize| {
            reveal.get(&line).map_or(0.0, |track| {
                let value = track.value(now) as f32;
                if track.to > 0.0 { value.max(f32::MIN_POSITIVE) } else { value }
            })
        };
        let fade = self.fade.get().map(|(line, track)| (line, track.value(now) as f32));
        let thumb = |path: &str| self.thumb_for(path);
        let input = LayoutInput {
            reveal: &reveal_of,
            fade,
            thumb: &thumb,
            empty_caret_line: self.empty_caret_line(),
        };
        layout(&text, &lines, &spans, &input)
    }

    fn relayout_all(&self) {
        *self.applied.borrow_mut() = Applied::default();
        self.relayout();
    }

    fn relayout_decos_only(&self) {
        let layout = self.compute_layout();
        if let Ok(mut current) = self.layout.try_borrow_mut() {
            current.decos = layout.decos;
        }
        self.invalidate();
    }

    /// Lays the note out and applies what changed.
    fn relayout(&self) {
        if self.painting.get() {
            self.post_resync();
            return;
        }
        let layout = self.compute_layout();
        // The new layout is in place before formats are applied: applying can paint at once
        // (Unfreeze), and a paint must never see drawings of an older text.
        if let Ok(mut slot) = self.layout.try_borrow_mut() {
            *slot = layout.clone();
        }
        self.apply_layout(&layout);
        self.refresh_caret();
        self.invalidate();
    }

    /// RichEdit places and sizes the caret when the selection changes, not when formats do: a
    /// format pass that reveals a marker or changes the line's size leaves the caret where the
    /// old layout had it. Setting the selection again puts it at the right place and size.
    fn refresh_caret(&self) {
        // SAFETY: focus query on this thread.
        if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() } != self.hwnd {
            return;
        }
        let range = self.selection_units();
        let total = self.units.borrow().len();
        // Away and back: setting the same selection keeps RichEdit's cached insertion format
        // (the caret's size); a real move makes it take the format at the caret again.
        let away = if range.start > 0 { range.start - 1 } else { (range.end + 1).min(total) };
        self.suppress.set(self.suppress.get() + 1);
        self.send(EM_HIDESELECTION, 1, 0);
        if away != range.start {
            self.select_units(away..away);
        }
        self.select_units(range);
        self.send(EM_HIDESELECTION, 0, 0);
        self.suppress.set(self.suppress.get() - 1);
    }

    /// Runs the text and selection updates again after the current message (used when they
    /// arrive while a paint is in progress).
    fn post_resync(&self) {
        // SAFETY: a scalar message to our own control.
        let _ = unsafe { PostMessageW(Some(self.hwnd), RESYNC, WPARAM(0), LPARAM(0)) };
    }

    fn font_for(&self, fmt: Fmt) -> Option<ITextFont> {
        if let Some(font) = self.fonts.borrow().get(&fmt) {
            return Some(font.clone());
        }
        let palette = self.palette.get();
        let ink = match fmt.ink {
            Ink::Text => palette.text,
            Ink::Muted => palette.muted,
            Ink::Accent => palette.accent,
            Ink::Mark => palette.mark,
            Ink::Faint => palette.faint,
        };
        let color = mix(palette.paper, ink, f64::from(fmt.alpha) / f64::from(ALPHA_STEPS));
        // SAFETY: TOM calls on this thread's document; the duplicate is a free-standing font
        // object that is only read when applied.
        let font = unsafe {
            (|| -> Result<ITextFont> {
                let font = self.doc.Range(0, 0)?.GetFont()?.GetDuplicate()?;
                font.SetName(&BSTR::from(if fmt.mono { MONO_FONT } else { BODY_FONT }))?;
                font.SetSize(f32::from(fmt.size) / 10.0 * 0.75)?;
                font.SetWeight(if fmt.bold { 600 } else { 400 })?;
                font.SetItalic(if fmt.italic { tomTrue.0 } else { tomFalse.0 })?;
                font.SetUnderline(if fmt.underline { tomSingle.0 } else { tomNone.0 })?;
                font.SetStrikeThrough(if fmt.strike { tomTrue.0 } else { tomFalse.0 })?;
                font.SetHidden(if fmt.hidden { tomTrue.0 } else { tomFalse.0 })?;
                font.SetProtected(tomFalse.0)?;
                font.SetForeColor(color.0 as i32)?;
                font.SetBackColor(if fmt.chip { palette.chip.0 as i32 } else { tomAutoColor.0 })?;
                Ok(font)
            })()
        }
        .ok()?;
        self.fonts.borrow_mut().insert(fmt, font.clone());
        Some(font)
    }

    fn para_for(&self, para: Para) -> Option<ITextPara> {
        if let Some(format) = self.para_formats.borrow().get(&para) {
            return Some(format.clone());
        }
        // SAFETY: TOM calls on this thread's document; the duplicate is free-standing.
        let format = unsafe {
            (|| -> Result<ITextPara> {
                let format = self.doc.Range(0, 0)?.GetPara()?.GetDuplicate()?;
                // A little room on the right for the drawn scroll thumb.
                format.SetIndents(0.0, f32::from(para.indent) * 0.75, 6.0 * 0.75)?;
                format.SetSpaceBefore(f32::from(para.before) * 0.75)?;
                format.SetSpaceAfter(f32::from(para.after) * 0.75)?;
                format.SetAlignment(tomAlignLeft.0)?;
                format.SetListType(tomListNone.0)?;
                Ok(format)
            })()
        }
        .ok()?;
        self.para_formats.borrow_mut().insert(para, format.clone());
        Some(format)
    }

    /// Applies the runs and paragraph formats the control does not have, outside undo.
    fn apply_layout(&self, layout: &Layout) {
        let started = Instant::now();
        let (runs, paras) = {
            let applied = self.applied.borrow();
            let (runs, paras) = applied.missing(layout);
            (
                runs.into_iter().cloned().collect::<Vec<_>>(),
                paras.into_iter().cloned().collect::<Vec<_>>(),
            )
        };
        if runs.is_empty() && paras.is_empty() {
            return;
        }
        let total = self.units.borrow().len() as i32 + 1;
        self.suppress.set(self.suppress.get() + 1);
        // SAFETY: TOM calls on this thread's document. RichEdit keeps no undo of its own (the
        // editor's UndoStack records text changes only), so formatting is never an undo step;
        // the range object is ours and never the selection, so the caret and scroll stay.
        unsafe {
            let _ = self.doc.Freeze();
            for para in &paras {
                if let Some(format) = self.para_for(para.para) {
                    let end = (para.range.end as i32).min(total);
                    if self.range.SetRange(para.range.start as i32, end).is_ok() {
                        let _ = self.range.SetPara(&format);
                    }
                }
            }
            for run in &runs {
                if let Some(font) = self.font_for(run.fmt) {
                    let end = (run.range.end as i32).min(total);
                    if self.range.SetRange(run.range.start as i32, end).is_ok() {
                        let _ = self.range.SetFont(&font);
                    }
                }
            }
            let _ = self.doc.Unfreeze();
        }
        self.suppress.set(self.suppress.get() - 1);
        self.applied.borrow_mut().set(layout);
        self.last_apply.set(started.elapsed());
        self.last_runs.set(runs.len() + paras.len());
    }

    /// Replaces UTF-16 `range` with `units` at exactly those positions. A TOM range, not the
    /// selection: RichEdit moves a selection that starts or ends inside hidden text, and list
    /// markers are hidden.
    fn replace_units(&self, range: Range<usize>, units: &[u16]) {
        let text = BSTR::from_wide(units);
        // SAFETY: TOM calls on this thread's document with our own range object.
        unsafe {
            if self.range.SetRange(range.start as i32, range.end as i32).is_ok() {
                let _ = self.range.SetText(&text);
            }
        }
    }

    /// Replaces text as one undo step and selects `change.selection` afterwards.
    fn apply_change(&self, change: &Change) {
        let (range16, selection16, inserted) = {
            let text = self.text.borrow();
            let map = self.map.borrow();
            let range16 = map.to16(change.range.start)..map.to16(change.range.end);
            let new_text = change.apply(&text);
            let new_map = Utf16Map::new(&new_text);
            let selection16 =
                new_map.to16(change.selection.start)..new_map.to16(change.selection.end);
            let inserted = control_units(&change.text);
            (range16, selection16, inserted)
        };
        self.undo.borrow_mut().close_group();
        self.suppress.set(self.suppress.get() + 1);
        if !change.is_empty() {
            self.replace_units(range16, &inserted);
        }
        self.select_units(selection16);
        self.suppress.set(self.suppress.get() - 1);
        self.recording.set(Recording::Step);
        self.text_changed();
        self.recording.set(Recording::Typing);
        self.undo.borrow_mut().close_group();
        self.selection_changed();
        self.send(EM_SCROLLCARET, 0, 0);
    }

    /// Ctrl+Z (`redo` false) or Ctrl+Y: puts back the text and the selection of a step.
    fn undo_redo(&self, redo: bool) -> bool {
        let step = {
            let text = self.text.borrow();
            let mut stack = self.undo.borrow_mut();
            if redo { stack.redo(&text) } else { stack.undo(&text) }
        };
        let Some((change, selection)) = step else { return false };
        let (range16, selection16, inserted) = {
            let text = self.text.borrow();
            let map = self.map.borrow();
            let range16 = map.to16(change.range.start)..map.to16(change.range.end);
            let new_text = change.apply(&text);
            let new_map = Utf16Map::new(&new_text);
            let selection16 = new_map.to16(selection.start)..new_map.to16(selection.end);
            let inserted = control_units(&change.text);
            (range16, selection16, inserted)
        };
        self.suppress.set(self.suppress.get() + 1);
        self.replace_units(range16, &inserted);
        self.select_units(selection16);
        self.suppress.set(self.suppress.get() - 1);
        self.recording.set(Recording::Off);
        self.text_changed();
        self.recording.set(Recording::Typing);
        self.selection_changed();
        self.send(EM_SCROLLCARET, 0, 0);
        true
    }

    fn run_command(&self, command: EditorCommand) {
        let text = self.text.borrow().clone();
        let sel = self.selection_bytes();
        let change = match command {
            EditorCommand::Inline(mark) => Some(markdown::toggle_inline(&text, sel, mark)),
            EditorCommand::Link => Some(markdown::toggle_link(&text, sel)),
            EditorCommand::Block(block) => Some(markdown::set_block(&text, sel, block)),
            EditorCommand::ToggleTodo => {
                let line = line_at(&self.lines.borrow(), sel.start);
                self.toggle_check(line);
                None
            }
        };
        if let Some(change) = change {
            self.apply_change(&change);
        }
    }

    fn toggle_check(&self, line: usize) -> bool {
        let (text, start, checked) = {
            let lines = self.lines.borrow();
            let Some(info) = lines.get(line) else { return false };
            let Block::Todo(checked) = info.block else { return false };
            (self.text.borrow().clone(), info.range.start, checked)
        };
        let sel = self.selection_bytes();
        let Some(change) = markdown::toggle_todo(&text, start, sel) else {
            return false;
        };
        let mut check = Transition::at(if checked { 1.0 } else { 0.0 });
        check.retarget(if checked { 0.0 } else { 1.0 }, self.now(), self.duration(CHECK_MS));
        self.apply_change(&change);
        self.check.set(Some((line, check)));
        self.start_timer();
        true
    }

    fn paste_object(&self, data: &IDataObject) -> bool {
        let Some(pasted) = unicode_text(data) else {
            return false;
        };
        let text = self.text.borrow().clone();
        let sel = self.selection_bytes();
        let change = markdown::paste(&text, sel, &pasted);
        self.apply_change(&change);
        true
    }

    fn paste_clipboard(&self) {
        // SAFETY: OLE is initialized on this thread (the notes thread calls OleInitialize).
        // The clipboard is only read; the data object is released when it drops.
        if let Ok(data) = unsafe { OleGetClipboard() } {
            self.paste_object(&data);
        }
    }

    // --- Slash menu ------------------------------------------------------------------------

    /// Opens, filters or closes the menu after a text or caret change. `typed_slash` opens it.
    fn update_slash(&self, typed_slash: bool) {
        let text = self.text.borrow().clone();
        let sel = self.selection_bytes();
        let query = sel.is_empty().then(|| markdown::slash_query(&text, sel.start)).flatten();
        let now = self.now();
        let mut menu = self.slash.borrow_mut();
        match (query, menu.as_mut()) {
            (Some((range, query)), Some(open))
                if !open.closing && open.range.start == range.start =>
            {
                let items = markdown::slash_matches(query);
                if items.is_empty() {
                    open.closing = true;
                    open.intro.retarget(0.0, now, self.duration(MENU_MS));
                } else {
                    if items != open.items {
                        open.selected = 0;
                        open.highlight = Transition::at(0.0);
                    }
                    open.items = items;
                    open.range = range;
                }
            }
            (Some((range, query)), None) if typed_slash => {
                let items = markdown::slash_matches(query);
                let mut intro = Transition::at(0.0);
                intro.retarget(1.0, now, self.duration(MENU_MS));
                *menu = Some(SlashMenu {
                    range,
                    items,
                    selected: 0,
                    highlight: Transition::at(0.0),
                    intro,
                    closing: false,
                });
            }
            (_, Some(open)) if !open.closing => {
                open.closing = true;
                open.intro.retarget(0.0, now, self.duration(MENU_MS));
            }
            _ => {}
        }
        drop(menu);
        self.place_popup();
        self.start_timer();
    }

    fn close_slash(&self) {
        let now = self.now();
        if let Some(menu) = self.slash.borrow_mut().as_mut()
            && !menu.closing
        {
            menu.closing = true;
            menu.intro.retarget(0.0, now, self.duration(MENU_MS));
        }
        self.place_popup();
        self.start_timer();
    }

    fn move_slash(&self, by: isize) {
        let now = self.now();
        if let Some(menu) = self.slash.borrow_mut().as_mut() {
            let count = menu.items.len() as isize;
            if count > 0 {
                menu.selected = (menu.selected as isize + by).rem_euclid(count) as usize;
                menu.highlight.retarget(menu.selected as f64, now, self.duration(MENU_MS));
            }
        }
        self.redraw_popup();
        self.start_timer();
    }

    fn pick_slash(&self, index: Option<usize>) -> bool {
        let picked = {
            let menu = self.slash.borrow();
            menu.as_ref().filter(|menu| !menu.closing).and_then(|menu| {
                menu.items
                    .get(index.unwrap_or(menu.selected))
                    .map(|item| (*item, menu.range.clone()))
            })
        };
        let Some((item, range)) = picked else { return false };
        *self.slash.borrow_mut() = None;
        self.hide_popup();
        let text = self.text.borrow().clone();
        let change = markdown::apply_slash(&text, range, item);
        self.apply_change(&change);
        true
    }

    fn popup_size(&self, rows: usize) -> (i32, i32) {
        let dpi = self.dpi.get();
        (
            px(MENU_WIDTH + MENU_SHADOW * 2.0, dpi),
            px(MENU_PAD * 2.0 + MENU_ROW * rows.max(1) as f64 + MENU_SHADOW * 2.0, dpi),
        )
    }

    fn ensure_popup(&self) -> HWND {
        if !self.popup.get().is_invalid() {
            return self.popup.get();
        }
        // SAFETY: module handle and a class with static data and our window procedure.
        let hwnd = unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None).map(|m| m.0).unwrap_or_default());
            let class = WNDCLASSW {
                lpfnWndProc: Some(popup_proc),
                hInstance: instance,
                lpszClassName: POPUP_CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&class);
            let owner = GetAncestor(self.hwnd, GA_ROOT);
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
                POPUP_CLASS,
                w!("Slash menu"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(owner),
                None,
                Some(instance),
                None,
            )
        };
        let Ok(hwnd) = hwnd else { return HWND::default() };
        // SAFETY: the popup reads this State only on this thread; the editor destroys the
        // popup before the State is freed.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, self as *const State as isize);
            let corner = DWMWCP_ROUNDSMALL;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
                4,
            );
        }
        self.popup.set(hwnd);
        hwnd
    }

    /// Positions and draws the popup for the current menu state.
    fn place_popup(&self) {
        let rows = self.slash.borrow().as_ref().map(|menu| menu.items.len());
        let Some(rows) = rows else {
            self.hide_popup();
            return;
        };
        let popup = self.ensure_popup();
        if popup.is_invalid() {
            return;
        }
        self.redraw_popup();
        if !self.popup_shown.get() {
            // SAFETY: our popup; shown without activation so the editor keeps focus.
            let _ = unsafe { ShowWindow(popup, SW_SHOWNOACTIVATE) };
            self.popup_shown.set(true);
        }
        let _ = rows;
    }

    fn hide_popup(&self) {
        if self.popup_shown.get() {
            // SAFETY: our popup.
            let _ = unsafe { ShowWindow(self.popup.get(), SW_HIDE) };
            self.popup_shown.set(false);
        }
    }

    /// Draws the popup into a per-pixel-alpha bitmap and puts it on screen at the caret.
    fn redraw_popup(&self) {
        let popup = self.popup.get();
        if popup.is_invalid() {
            return;
        }
        let now = self.now();
        let guard = self.slash.borrow();
        let Some(menu) = guard.as_ref() else { return };
        let dpi = self.dpi.get();
        let (width, height) = self.popup_size(menu.items.len());
        let intro = menu.intro.value(now);
        // Below the caret, or above it when there is no room.
        let caret = self.caret_screen_rect();
        // SAFETY: scalar monitor queries for a point on screen.
        let work = unsafe {
            let monitor = MonitorFromPoint(
                POINT { x: caret.left, y: caret.bottom },
                MONITOR_DEFAULTTONEAREST,
            );
            let mut info =
                MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(monitor, &mut info);
            info.rcWork
        };
        let shadow = px(MENU_SHADOW, dpi);
        let rise = ((1.0 - intro) * 6.0 * f64::from(dpi) / 96.0).round() as i32;
        let mut x = caret.left - shadow - px(8.0, dpi);
        let mut y = caret.bottom + px(4.0, dpi) - shadow + rise;
        if y + height > work.bottom {
            y = caret.top - height + shadow - px(4.0, dpi) - rise;
        }
        x = x.clamp(work.left - shadow, (work.right - width + shadow).max(work.left));
        let palette = self.palette.get();
        let fonts = self.gdi_fonts();
        let Ok(mut surface) = Surface::new(width as u32, height as u32) else { return };
        {
            let mut canvas = surface.canvas();
            canvas.clear();
            let card =
                RECT { left: shadow, top: shadow, right: width - shadow, bottom: height - shadow };
            canvas.shadow(card, px(10.0, dpi) as f32, shadow, palette.dark);
            canvas.fill(card, px(10.0, dpi) as f32, palette.paper, 1.0);
            canvas.stroke(card, px(10.0, dpi) as f32, palette.hairline);
            let row_h = px(MENU_ROW, dpi);
            let top = card.top + px(MENU_PAD, dpi);
            let highlight = menu.highlight.value(now);
            let hy = top + (highlight * f64::from(row_h)).round() as i32;
            let selected = RECT {
                left: card.left + px(4.0, dpi),
                top: hy,
                right: card.right - px(4.0, dpi),
                bottom: hy + row_h,
            };
            canvas.fill(selected, px(6.0, dpi) as f32, palette.selected, 1.0);
            for (index, item) in menu.items.iter().enumerate() {
                let row = RECT {
                    left: card.left + px(14.0, dpi),
                    top: top + row_h * index as i32,
                    right: card.right - px(10.0, dpi),
                    bottom: top + row_h * (index as i32 + 1),
                };
                let icon = RECT {
                    left: row.left,
                    top: row.top + px(5.0, dpi),
                    right: row.left + px(18.0, dpi),
                    bottom: row.bottom - px(5.0, dpi),
                };
                canvas.fill(icon, px(4.0, dpi) as f32, palette.chip, 1.0);
                canvas.label(
                    icon,
                    slash_glyph(*item),
                    palette.muted,
                    fonts.small,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                let text = RECT { left: icon.right + px(10.0, dpi), ..row };
                canvas.label(
                    text,
                    item.label(),
                    palette.text,
                    fonts.body,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
            }
            canvas.finish_labels();
            canvas.opaque_inside(card, px(10.0, dpi) as f32);
        }
        surface.present_layered(
            popup,
            POINT { x, y },
            (intro.clamp(0.0, 1.0) * 255.0).round() as u8,
        );
        *self.popup_image.borrow_mut() =
            Some((POINT { x, y }, surface.width, surface.height, surface.pixels()));
        let finished = menu.closing && !menu.intro.active(now);
        drop(guard);
        if finished {
            *self.slash.borrow_mut() = None;
            self.hide_popup();
        }
    }

    fn caret_screen_rect(&self) -> RECT {
        let caret = self.selection_units().end;
        let mut point = windows::Win32::Foundation::POINTL::default();
        self.send(EM_POSFROMCHAR, &mut point as *mut _ as usize, caret as isize);
        let line_height = px(20.0, self.dpi.get());
        let mut screen = POINT { x: point.x, y: point.y };
        // SAFETY: same-thread window, writable local.
        let _ = unsafe { ClientToScreen(self.hwnd, &mut screen) };
        RECT { left: screen.x, top: screen.y, right: screen.x + 1, bottom: screen.y + line_height }
    }

    fn popup_hit(&self, y: i32) -> Option<usize> {
        let dpi = self.dpi.get();
        let top = px(MENU_SHADOW + MENU_PAD, dpi);
        let row = px(MENU_ROW, dpi);
        let count = self.slash.borrow().as_ref().map_or(0, |menu| menu.items.len());
        let index = (y - top).div_euclid(row.max(1));
        (y >= top && (index as usize) < count).then_some(index as usize)
    }

    // --- Animation clock -------------------------------------------------------------------

    /// The scroll position, the text's full height and the visible height, pixels.
    fn scroll_metrics(&self) -> (i32, i32, i32) {
        let mut scroll = POINT::default();
        self.send(EM_GETSCROLLPOS, 0, &mut scroll as *mut POINT as isize);
        let total = self.units.borrow().len();
        let (_, end_y) = self.position(total);
        let content = end_y + scroll.y + px(22.0, self.dpi.get());
        let mut client = RECT::default();
        // SAFETY: same-thread window, writable local.
        let _ = unsafe { GetClientRect(self.hwnd, &mut client) };
        (scroll.y, content, client.bottom - client.top)
    }

    /// The drawn thumb and its track (client pixels), or `None` when the text fits.
    fn thumb_rects(&self, client: RECT) -> Option<(RECT, RECT)> {
        let s = |value: f64| px(value, self.dpi.get());
        let (scroll, content, visible) = self.scroll_metrics();
        if content <= visible + s(2.0) || visible <= 0 {
            return None;
        }
        let track_len = (visible - s(4.0)).max(1);
        let length = (i64::from(track_len) * i64::from(visible) / i64::from(content.max(1)))
            .clamp(i64::from(s(18.0).min(track_len)), i64::from(track_len))
            as i32;
        let top = s(2.0)
            + (i64::from(track_len - length) * i64::from(scroll.max(0))
                / i64::from((content - visible).max(1))) as i32;
        let thumb = RECT {
            left: client.right - s(4.0),
            top,
            right: client.right - s(1.0),
            bottom: top + length,
        };
        let track = RECT {
            left: client.right - s(10.0),
            top: s(2.0),
            right: client.right,
            bottom: s(2.0) + track_len,
        };
        Some((thumb, track))
    }

    fn set_scroll(&self, y: f64) {
        let point = POINT { x: 0, y: y.round() as i32 };
        self.send(EM_SETSCROLLPOS, 0, &point as *const POINT as isize);
        self.invalidate();
    }

    /// Scrolls by `dy` pixels (positive is down), eased unless motion is off; clamped to the
    /// text. A note shorter than its window does not move.
    fn scroll_by(&self, dy: f64) {
        let (current, content, visible) = self.scroll_metrics();
        let limit = f64::from((content - visible).max(0));
        let now = self.now();
        let from = f64::from(current);
        let base = self.scroll.get().map_or(from, |track| track.to);
        let target = (base + dy).clamp(0.0, limit);
        if (target - from).abs() < 0.5 && self.scroll.get().is_none() {
            return;
        }
        if self.motion.get() && self.frozen.get().is_none() {
            let mut track = Transition::at(from);
            track.retarget(target, now, SCROLL_MS);
            self.scroll.set(Some(track));
            self.start_timer();
        } else {
            self.scroll.set(None);
            self.set_scroll(target);
        }
    }

    /// One wheel message: the system's lines per notch times a line's height, in pixels, so
    /// high-resolution wheels (deltas below 120) scroll by their share.
    fn wheel(&self, delta: i16, horizontal: bool) {
        if horizontal {
            return; // Text wraps: there is nothing to scroll sideways.
        }
        let mut lines: u32 = 3;
        // SAFETY: SPI_GETWHEELSCROLLLINES writes one UINT into the local.
        let _ = unsafe {
            SystemParametersInfoW(
                SPI_GETWHEELSCROLLLINES,
                0,
                Some((&mut lines as *mut u32).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        let (_, _, visible) = self.scroll_metrics();
        let notch = if lines == u32::MAX {
            f64::from(visible) // "One screen at a time".
        } else {
            f64::from(lines.min(20)) * f64::from(px(BODY as f64 * 1.36, self.dpi.get()))
        };
        self.scroll_by(-f64::from(delta) / 120.0 * notch);
    }

    /// A press on the scroll track: drag the thumb, or page towards the press.
    fn thumb_press(&self, x: i32, y: i32) -> bool {
        let Some((thumb, track)) = self.hits.borrow().thumb else { return false };
        if x < track.left || x >= track.right || y < track.top || y >= track.bottom {
            return false;
        }
        if y >= thumb.top && y < thumb.bottom {
            self.thumb_drag.set(Some(y - thumb.top));
            // SAFETY: mouse capture for our own control while the button is down.
            unsafe {
                SetCapture(self.hwnd);
            }
            self.invalidate();
        } else {
            let (_, _, visible) = self.scroll_metrics();
            let page = f64::from(visible) * 0.9;
            self.scroll_by(if y < thumb.top { -page } else { page });
        }
        true
    }

    fn thumb_move(&self, y: i32) {
        let Some(grab) = self.thumb_drag.get() else { return };
        let Some((thumb, track)) = self.hits.borrow().thumb else { return };
        let (_, content, visible) = self.scroll_metrics();
        let room = (track.bottom - track.top) - (thumb.bottom - thumb.top);
        if room <= 0 {
            return;
        }
        let top = (y - grab - track.top).clamp(0, room);
        self.scroll.set(None);
        self.set_scroll(f64::from(top) * f64::from((content - visible).max(0)) / f64::from(room));
    }

    fn any_active(&self, now: f64) -> bool {
        self.reveal.borrow().values().any(|track| track.active(now))
            || self.fade.get().is_some_and(|(_, track)| track.active(now))
            || self.check.get().is_some_and(|(_, track)| track.active(now))
            || self.hover.get().1.active(now)
            || self.images_in.borrow().values().any(|track| track.active(now))
            || self.scroll.get().is_some()
            || self.slash.borrow().as_ref().is_some_and(|menu| {
                menu.intro.active(now) || menu.highlight.active(now) || menu.closing
            })
    }

    fn start_timer(&self) {
        let run = self.frozen.get().is_none() && self.any_active(self.now());
        if run != self.timer.get() {
            // SAFETY: the control's own timer, dispatched on this thread.
            // The flag follows what Windows did: a refused timer is tried again next time.
            let running = unsafe {
                if run {
                    SetTimer(Some(self.hwnd), ANIM_TIMER, 10, None) != 0
                } else {
                    let _ = KillTimer(Some(self.hwnd), ANIM_TIMER);
                    false
                }
            };
            self.timer.set(running);
        }
    }

    fn tick(&self) {
        let now = self.now();
        if let Some(track) = self.scroll.get() {
            self.set_scroll(track.value(now));
            if !track.active(now) {
                self.scroll.set(None);
            }
        }
        let formats =
            self.reveal.borrow().values().any(|track| track.active(now) || track.to == 0.0)
                || self.fade.get().is_some();
        if formats {
            self.reveal.borrow_mut().retain(|_, track| track.to > 0.0 || track.active(now));
            if self.fade.get().is_some_and(|(_, track)| !track.active(now)) {
                self.fade.set(None);
            }
            self.relayout();
        } else {
            self.invalidate();
        }
        self.images_in.borrow_mut().retain(|_, track| track.active(now));
        if self.slash.borrow().is_some() {
            self.redraw_popup();
        }
        // SAFETY: timer bookkeeping on this thread.
        if self.frozen.get().is_some() || !self.any_active(now) {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), ANIM_TIMER);
            }
            self.timer.set(false);
        }
        self.start_timer();
    }

    // --- Painting --------------------------------------------------------------------------

    fn gdi_fonts(&self) -> GdiFonts {
        if let Some(fonts) = *self.gdi.borrow() {
            return fonts;
        }
        let fonts = GdiFonts::new(self.dpi.get());
        *self.gdi.borrow_mut() = Some(fonts);
        fonts
    }

    fn paint(&self, target: HDC, dirty: RECT) {
        let mut client = RECT::default();
        // SAFETY: same-thread window, writable local.
        let _ = unsafe { GetClientRect(self.hwnd, &mut client) };
        let (width, height) = (client.right.max(1) as u32, client.bottom.max(1) as u32);
        if self.block_paint.get() {
            return;
        }
        // A paint never nests (RichEdit can paint while formats are applied): a nested one
        // only asks for another paint.
        if self.painting.get() {
            self.invalidate();
            return;
        }
        let Ok(mut storage) = self.surface.try_borrow_mut() else {
            self.invalidate();
            return;
        };
        self.painting.set(true);
        struct Painting<'a>(&'a Cell<bool>);
        impl Drop for Painting<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _painting = Painting(&self.painting);
        if storage.as_ref().is_none_or(|surface| surface.width != width || surface.height != height)
        {
            *storage = Surface::new(width, height).ok();
        }
        let Some(surface) = storage.as_mut() else {
            // No bitmap (GDI refused one): try again shortly instead of leaving stale pixels.
            retry_paint_later(self.hwnd);
            return;
        };
        if let Some(hook) = self.during_paint.borrow_mut().take() {
            hook();
        }
        self.render_text(surface.dc, client, dirty);
        let fonts = self.gdi_fonts();
        {
            let mut canvas = surface.canvas();
            self.paint_decos(&mut canvas, fonts, client);
            canvas.finish_labels();
        }
        self.asked.set(None);
        self.painted.set(Some(Instant::now()));
        // SAFETY: both DCs are live; the copy covers the dirty part of the client area.
        unsafe {
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

    /// A point of the character at `unit` from TOM, client pixels: `align` is a TA_ value
    /// (TA_BASELINE gives the baseline).
    fn point_of(&self, unit: usize, align: i32) -> Option<(i32, i32)> {
        let (mut x, mut y) = (0, 0);
        // SAFETY: TOM calls on this thread's document with a range of our own.
        unsafe {
            let range = self.doc.Range(unit as i32, unit as i32).ok()?;
            range
                .GetPoint(tomConstants(tomStart.0 | tomClientCoord.0 | align), &mut x, &mut y)
                .ok()?;
        }
        Some((x, y))
    }

    /// Renders the control's text into our bitmap. Through the text services' `TxDraw` (the
    /// call RichEdit's own WM_PAINT makes), not WM_PRINTCLIENT: once the control has had the
    /// keyboard focus and lost it, WM_PRINTCLIENT renders nothing at all (measured: 574 dark
    /// pixels before, 0 after, for good), so a note went blank on its next repaint (a hover,
    /// another window uncovering it). WM_PRINTCLIENT stays as the fallback when the text
    /// services cannot be had.
    fn render_text(&self, dc: HDC, client: RECT, dirty: RECT) {
        let services = self.text_services();
        let drawn = services.as_ref().is_some_and(|services| {
            let mut bounds = windows::Win32::Foundation::RECTL {
                left: client.left,
                top: client.top,
                right: client.right,
                bottom: client.bottom,
            };
            let mut update = dirty;
            // SAFETY: a TOM text-services draw into our own memory DC and its DIB, which live
            // through the call; the rectangles are locals.
            unsafe {
                let brush = CreateSolidBrush(self.palette.get().paper);
                FillRect(dc, &client, brush);
                let _ = DeleteObject(brush.into());
                services
                    .TxDraw(
                        DVASPECT_CONTENT,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        dc,
                        HDC::default(),
                        &mut bounds,
                        std::ptr::null_mut(),
                        &mut update,
                        0,
                        0,
                        TXTVIEW_ACTIVE.0,
                    )
                    .is_ok()
            }
        });
        if !drawn {
            // SAFETY: RichEdit renders its client area into our memory DC; the DC and its DIB
            // are ours and live through the call.
            unsafe {
                DefSubclassProc(
                    self.hwnd,
                    WM_PRINTCLIENT,
                    WPARAM(dc.0 as usize),
                    LPARAM((PRF_CLIENT | PRF_ERASEBKGND) as isize),
                );
            }
        }
    }

    /// The control's text services (`ITextServices`), asked for once by its IID (the windows
    /// crate has no constant for it: Msftedit exports it as data).
    fn text_services(&self) -> Option<ITextServices> {
        if let Some(services) = self.services.borrow().as_ref() {
            return services.clone();
        }
        const IID_ITEXTSERVICES: windows::core::GUID =
            windows::core::GUID::from_u128(0x8d33f740_cf58_11ce_a89d_00aa006cadc5);
        let mut raw = std::ptr::null_mut();
        // SAFETY: a plain QueryInterface on the control's own OLE object; on success `raw` is
        // an owned ITextServices pointer that `from_raw` takes over.
        let services = unsafe {
            (self.ole.query(&IID_ITEXTSERVICES, &mut raw).is_ok() && !raw.is_null())
                .then(|| ITextServices::from_raw(raw))
        };
        *self.services.borrow_mut() = Some(services.clone());
        services
    }

    fn position(&self, unit: usize) -> (i32, i32) {
        let mut point = windows::Win32::Foundation::POINTL::default();
        self.send(EM_POSFROMCHAR, &mut point as *mut _ as usize, unit as isize);
        (point.x, point.y)
    }

    fn paint_decos(&self, canvas: &mut Canvas<'_>, fonts: GdiFonts, client: RECT) {
        let Ok(layout) = self.layout.try_borrow() else { return };
        let palette = self.palette.get();
        let dpi = self.dpi.get();
        let scale = f64::from(dpi) / 96.0;
        let now = self.now();
        let s = |value: f64| (value * scale).round() as i32;
        let mut hits = Hits::default();
        let hover = self.hover.get();
        let check = self.check.get();
        let lines = self.lines.borrow();
        let edges = code_edges(&lines);
        // Code blocks: one background per block, from its first line to its last.
        let mut block_top: Option<i32> = None;
        for deco in layout.decos.iter() {
            let (x, y) = self.position(deco.anchor);
            let line_h = s(f64::from(deco.size) * 1.36);
            if y > client.bottom + line_h * 2 || y < -s(400.0) {
                continue;
            }
            match &deco.kind {
                DecoKind::Bullet => {
                    let cx = x - s(13.0);
                    let cy = y + line_h / 2;
                    let r = s(2.6).max(2);
                    let dot = RECT { left: cx - r, top: cy - r, right: cx + r, bottom: cy + r };
                    match deco.depth % 3 {
                        0 => canvas.fill(dot, r as f32, palette.text, 1.0),
                        1 => canvas.stroke(dot, r as f32, palette.text),
                        _ => canvas.fill(dot, 1.0, palette.text, 1.0),
                    }
                }
                DecoKind::Number(n) => {
                    let label = RECT {
                        left: x - s(LIST_STEP as f64 + 6.0),
                        top: y,
                        right: x - s(5.0),
                        bottom: y + line_h,
                    };
                    canvas.label(
                        label,
                        &format!("{n}."),
                        palette.text,
                        fonts.body,
                        DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
                    );
                }
                DecoKind::Check(checked) => {
                    let size = s(15.0);
                    let left = x - s(22.0);
                    let top = y + (line_h - size) / 2;
                    let rect = RECT { left, top, right: left + size, bottom: top + size };
                    let progress = match check {
                        Some((line, track)) if line == deco.line => track.value(now),
                        _ => f64::from(u8::from(*checked)),
                    };
                    let hovered = match hover {
                        (Some(line), track) if line == deco.line => track.value(now),
                        (_, track) if hover.0.is_none() => 0.0 * track.value(now),
                        _ => 0.0,
                    };
                    let radius = s(3.5) as f32;
                    canvas.fill(rect, radius, mix(palette.paper, palette.hover, hovered), 1.0);
                    canvas.stroke(rect, radius, mix(palette.muted, palette.accent, hovered * 0.6));
                    if progress > 0.0 {
                        canvas.fill(rect, radius, palette.accent, progress);
                        canvas.tick(rect, palette.paper, progress, scale);
                    }
                    let pad = s(4.0);
                    hits.checks.push((
                        RECT {
                            left: rect.left - pad,
                            top: rect.top - pad,
                            right: rect.right + pad,
                            bottom: rect.bottom + pad,
                        },
                        deco.line,
                    ));
                }
                DecoKind::Divider => {
                    let top = y + line_h / 2;
                    let line = RECT {
                        left: s(2.0),
                        top,
                        right: client.right - s(6.0),
                        bottom: top + s(1.0).max(1),
                    };
                    canvas.fill(line, 0.0, palette.hairline, 1.0);
                }
                DecoKind::QuoteBar => {
                    let bar = RECT {
                        left: x - s(QUOTE_INDENT as f64 - 2.0),
                        top: y + s(1.0),
                        right: x - s(QUOTE_INDENT as f64 - 5.0),
                        bottom: y + line_h - s(1.0),
                    };
                    canvas.fill(bar, s(1.5) as f32, palette.text, 1.0);
                }
                DecoKind::Code { .. } => {
                    let (first, last) = edges.get(deco.line).copied().unwrap_or((false, false));
                    if first {
                        block_top = Some(y - s(3.0));
                    }
                    if last && let Some(top) = block_top.take() {
                        let bottom =
                            y + if deco.revealed
                                || lines
                                    .get(deco.line)
                                    .is_some_and(|line| line.block == Block::Code)
                            {
                                line_h
                            } else {
                                s(2.0)
                            } + s(3.0);
                        let rect = RECT { left: s(2.0), top, right: client.right - s(4.0), bottom };
                        canvas.fill(rect, s(6.0) as f32, palette.chip, 0.75);
                    }
                }
                DecoKind::Image(path) => {
                    let Some(full) = self.resolve(path) else { continue };
                    let thumbs = self.thumbs.borrow();
                    let Some(Some(thumb)) = thumbs.get(&full) else { continue };
                    let (w, h) = thumb_size(thumb.natural, 1.0, self.width.get() - 8.0);
                    let appear =
                        self.images_in.borrow().get(path).map_or(1.0, |track| track.value(now));
                    let top =
                        y + if deco.revealed { s(15.0) } else { s(1.0) } + s(IMAGE_GAP as f64);
                    let grow = 0.96 + 0.04 * appear;
                    let (dw, dh) = (
                        (f64::from(w) * scale * grow).round() as i32,
                        (f64::from(h) * scale * grow).round() as i32,
                    );
                    let rect = RECT { left: s(2.0), top, right: s(2.0) + dw, bottom: top + dh };
                    canvas.image(&thumb.pixmap, rect, s(6.0) as f32, appear);
                    canvas.stroke(rect, s(6.0) as f32, palette.hairline);
                    hits.images.push((rect, path.clone()));
                }
                DecoKind::Placeholder(hint) => {
                    // On the paragraph mark's own baseline, in the block's font, just after the
                    // caret.
                    let block = lines.get(deco.line).map_or(Block::Paragraph, |line| line.block);
                    let font = match block {
                        Block::Heading(_) => fonts.heading(block),
                        _ => fonts.body,
                    };
                    let baseline = self
                        .point_of(deco.anchor, TA_BASELINE.0 as i32)
                        .map_or(y + line_h * 3 / 4, |(_, baseline)| baseline);
                    let left = x + s(1.0).max(1) + 1;
                    let (width, ascent, descent) = canvas.measure(hint, font);
                    canvas.label_at_baseline(left, baseline, hint, palette.faint, font);
                    hits.placeholder = Some((
                        RECT {
                            left,
                            top: baseline - ascent,
                            right: left + width,
                            bottom: baseline + descent,
                        },
                        baseline,
                    ));
                }
            }
        }
        // A thin scroll thumb when the text is taller than the editor (no classic scrollbar).
        if let Some((thumb, track)) = self.thumb_rects(client) {
            let dragging = self.thumb_drag.get().is_some();
            canvas.fill(thumb, s(1.5) as f32, palette.muted, if dragging { 0.6 } else { 0.35 });
            hits.thumb = Some((thumb, track));
        }
        *self.hits.borrow_mut() = hits;
    }

    fn check_at(&self, x: i32, y: i32) -> Option<usize> {
        self.hits
            .borrow()
            .checks
            .iter()
            .find(|(rect, _)| x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom)
            .map(|(_, line)| *line)
    }

    fn image_at(&self, x: i32, y: i32) -> Option<String> {
        self.hits
            .borrow()
            .images
            .iter()
            .find(|(rect, _)| x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom)
            .map(|(_, path)| path.clone())
    }

    /// A click in the left indent of a quote or list item (left of its text, where the bar,
    /// bullet or box is drawn): the caret goes to the start of the item's words. RichEdit treats
    /// that margin as a selection bar and selected the whole line, so typing replaced the
    /// marker.
    fn indent_click(&self, x: i32, y: i32) -> Option<usize> {
        let point = windows::Win32::Foundation::POINTL { x, y };
        let unit = self.send(EM_CHARFROMPOS, 0, &point as *const _ as isize).0.max(0) as usize;
        let lines = self.lines.borrow();
        let map = self.map.borrow();
        let line = &lines[line_at(&lines, map.to8(unit))];
        if !(line.block.is_list() || line.block == Block::Quote) {
            return None;
        }
        let first = if line.block.is_list() { line.content } else { line.range.start };
        let (text_x, top) = self.position(map.to16(first));
        let height = px(f64::from(block_size(line.block)) * 1.6, self.dpi.get());
        (x < text_x && y >= top && y < top + height).then_some(line.content)
    }

    fn link_at_point(&self, x: i32, y: i32) -> Option<String> {
        let point = windows::Win32::Foundation::POINTL { x, y };
        let unit = self.send(EM_CHARFROMPOS, 0, &point as *const _ as isize).0.max(0) as usize;
        let text = self.text.borrow();
        let byte = self.map.borrow().to8(unit);
        // The point must be on the character, not just nearest to it.
        let (cx, cy) = self.position(unit);
        if (y - cy).abs() > px(30.0, self.dpi.get()) || x < cx - px(20.0, self.dpi.get()) {
            return None;
        }
        markdown::link_at(&text, byte)
    }

    fn set_hover(&self, line: Option<usize>) {
        let (current, mut track) = self.hover.get();
        if current == line {
            return;
        }
        let now = self.now();
        if line.is_some() {
            track.run(0.0, 1.0, now, self.duration(HOVER_IN_MS));
            // SAFETY: a leave request for our own control.
            unsafe {
                let mut request = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: self.hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut request);
            }
            self.hover.set((line, track));
        } else {
            track.run(1.0, 0.0, now, self.duration(HOVER_OUT_MS));
            self.hover.set((current.filter(|_| track.active(now)), track));
            if !track.active(now) {
                self.hover.set((None, Transition::at(0.0)));
            }
        }
        self.invalidate();
        self.start_timer();
    }
}

/// A short glyph for each slash menu entry.
fn slash_glyph(item: SlashItem) -> &'static str {
    match item {
        SlashItem::Text => "T",
        SlashItem::Heading1 => "H1",
        SlashItem::Heading2 => "H2",
        SlashItem::Heading3 => "H3",
        SlashItem::Bullet => "•",
        SlashItem::Numbered => "1.",
        SlashItem::Todo => "☐",
        SlashItem::Quote => "❝",
        SlashItem::Code => "{}",
        SlashItem::Divider => "—",
    }
}

/// Gives a control an accessible name (UI Automation and MSAA) without changing its text: the
/// control's window text is the note itself, so the name comes from a direct annotation.
fn set_accessible_name(hwnd: HWND, name: &str) {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Accessibility::{
        CAccPropServices, IAccPropServices, Name_Property_GUID, PROPID_ACC_NAME,
    };
    let name = windows::core::HSTRING::from(name);
    // SAFETY: an in-process COM object on this (OLE-initialized) thread; the strings are copied
    // by the service; OBJID_CLIENT and CHILDID_SELF address the control itself.
    unsafe {
        if let Ok(service) =
            CoCreateInstance::<_, IAccPropServices>(&CAccPropServices, None, CLSCTX_INPROC_SERVER)
        {
            let client = OBJID_CLIENT.0 as u32;
            let _ = service.SetHwndPropStr(hwnd, client, 0, Name_Property_GUID, &name);
            let _ = service.SetHwndPropStr(hwnd, client, 0, PROPID_ACC_NAME, &name);
        }
    }
}
/// CF_UNICODETEXT from a data object, when it has some.
fn unicode_text(data: &IDataObject) -> Option<String> {
    let format = FORMATETC {
        cfFormat: CF_UNICODETEXT.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: the medium is released below; the global is locked only while copied, and the
    // copy stops at the terminator or the allocation's size.
    unsafe {
        let mut medium = data.GetData(&format).ok()?;
        let global = medium.u.hGlobal;
        let size = GlobalSize(global) / 2;
        let pointer = GlobalLock(global) as *const u16;
        let text = if pointer.is_null() {
            None
        } else {
            let units = std::slice::from_raw_parts(pointer, size);
            let end = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());
            Some(String::from_utf16_lossy(&units[..end]))
        };
        let _ = GlobalUnlock(global);
        ReleaseStgMedium(&mut medium);
        text
    }
}

/// A data object holding only CF_UNICODETEXT.
pub fn text_data_object(text: &str) -> Result<IDataObject> {
    // SAFETY: an empty shell data object; SetData takes ownership of the global (release true).
    unsafe {
        let data: IDataObject = SHCreateDataObject(None, None, None)?;
        set_global(
            &data,
            CF_UNICODETEXT.0,
            &text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect::<Vec<u8>>(),
        )?;
        Ok(data)
    }
}

/// Puts bytes into a data object under a clipboard format, as an HGLOBAL.
pub fn set_global(data: &IDataObject, format: u16, bytes: &[u8]) -> Result<()> {
    // SAFETY: the global is allocated, filled while locked, then owned by the data object.
    unsafe {
        let global: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let pointer = GlobalLock(global) as *mut u8;
        if pointer.is_null() {
            return Err(Error::from_thread());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
        let _ = GlobalUnlock(global);
        let formatetc = FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: windows::Win32::System::Com::STGMEDIUM_0 { hGlobal: global },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        data.SetData(&formatetc, &medium, true)
    }
}

fn load_thumb(path: &Path) -> Option<Thumb> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 64_000_000 {
        return None;
    }
    // Screenshots carry their DPI scale in pHYs (saver.rs writes it); show them at their size.
    let scale = reader
        .info()
        .pixel_dims
        .filter(|dims| dims.unit == png::Unit::Meter && dims.xppu > 0)
        .map_or(1.0, |dims| f64::from(dims.xppu) * 0.0254 / 96.0);
    let mut buffer = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let data = &buffer[..frame.buffer_size()];
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return None,
    };
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    for (index, pixel) in data.chunks_exact(channels).enumerate().take((width * height) as usize) {
        let (r, g, b, a) = match channels {
            4 => (pixel[0], pixel[1], pixel[2], pixel[3]),
            3 => (pixel[0], pixel[1], pixel[2], 255),
            2 => (pixel[0], pixel[0], pixel[0], pixel[1]),
            _ => (pixel[0], pixel[0], pixel[0], 255),
        };
        // Stored BGRA (the DIB's order), premultiplied.
        let premultiply = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        pixmap.pixels_mut()[index] = tiny_skia::PremultipliedColorU8::from_rgba(
            premultiply(b),
            premultiply(g),
            premultiply(r),
            a,
        )?;
    }
    let natural =
        ((f64::from(width) / scale).round() as u32, (f64::from(height) / scale).round() as u32);
    Some(Thumb { pixmap, natural })
}

// ---------------------------------------------------------------------------
// The control's window procedure
// ---------------------------------------------------------------------------

thread_local! {
    /// Tests: keys sent with `Editor::send_key` carry no modifiers, whatever the real keyboard
    /// of the person using the desktop holds at that moment.
    static NO_MODIFIERS: Cell<bool> = const { Cell::new(false) };
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    if NO_MODIFIERS.with(Cell::get) {
        return false;
    }
    // SAFETY: plain key state query for this thread.
    unsafe { GetKeyState(i32::from(vk.0)) < 0 }
}

unsafe extern "system" fn editor_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: the editor attached a pointer to its boxed State, which outlives the control.
    let s = unsafe { &*(data as *const State) };
    // SAFETY: forwards a message to RichEdit unchanged.
    let default = || unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    match msg {
        WM_NCDESTROY => {
            // SAFETY: removes this exact subclass on the control's last message.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(editor_proc), id) };
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: BeginPaint/EndPaint pair for this window's WM_PAINT.
            let dc = unsafe { BeginPaint(hwnd, &mut paint) };
            if !dc.is_invalid() {
                s.paint(dc, paint.rcPaint);
            }
            // SAFETY: matches BeginPaint above.
            let _ = unsafe { EndPaint(hwnd, &paint) };
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_TIMER if wp.0 == ANIM_TIMER => {
            s.tick();
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == WATCHDOG_TIMER || wp.0 == RETRY_TIMER => {
            // SAFETY: one-shot timers of our own control.
            let _ = unsafe { KillTimer(Some(hwnd), wp.0) };
            if wp.0 == RETRY_TIMER {
                s.invalidate();
            } else {
                s.watchdog();
            }
            return LRESULT(0);
        }
        RESYNC => {
            s.text_changed();
            s.selection_changed();
            return LRESULT(0);
        }
        WM_SETFOCUS => {
            s.focused.set(true);
            s.show_reveal.set(true);
            let result = default();
            s.selection_changed();
            return result;
        }
        WM_KILLFOCUS => {
            s.focused.set(false);
            s.show_reveal.set(false);
            // Focus left: any composition is over (cancelled or committed).
            if s.composing.replace(false) {
                s.post_resync();
            }
            let result = default();
            s.close_slash();
            s.selection_changed();
            return result;
        }
        WM_GETDLGCODE => {
            // Every key comes here, Tab and Enter included; the editor decides.
            return LRESULT(
                default().0 | (DLGC_WANTALLKEYS | DLGC_WANTCHARS | DLGC_WANTTAB) as isize,
            );
        }
        WM_IME_STARTCOMPOSITION => {
            s.composing.set(true);
            s.close_slash();
        }
        WM_IME_ENDCOMPOSITION => {
            s.composing.set(false);
            // SAFETY: posts to our own control; handled after the IME commits.
            let _ = unsafe { PostMessageW(Some(hwnd), RESYNC, WPARAM(0), LPARAM(0)) };
        }
        WM_PASTE => {
            s.paste_clipboard();
            return LRESULT(0);
        }
        EM_UNDO | WM_UNDO => return LRESULT(isize::from(s.undo_redo(false))),
        EM_REDO => return LRESULT(isize::from(s.undo_redo(true))),
        EM_CANUNDO => return LRESULT(isize::from(s.undo.borrow().can_undo())),
        EM_CANREDO => return LRESULT(isize::from(s.undo.borrow().can_redo())),
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            // A key that is not for an IME (VK_PROCESSKEY) means no composition is going on:
            // a composition flag left over (an IME that never sent its end) is cleared here.
            if wp.0 != 0xE5 && s.composing.replace(false) {
                s.post_resync();
            }
            if let Some(result) = s.key(VIRTUAL_KEY(wp.0 as u16), msg) {
                return result;
            }
        }
        WM_KEYUP if wp.0 == usize::from(VK_CONTROL.0) => {
            s.update_cursor();
        }
        WM_CHAR => {
            if let Some(result) = s.char(wp.0 as u16) {
                return result;
            }
        }
        WM_LBUTTONDOWN if s.thumb_press(i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16)) => {
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            s.marker_edit.set(false);
            let (x, y) = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
            if let Some(line) = s.check_at(x, y) {
                // SAFETY: keyboard focus to our own control, as a click would.
                let _ = unsafe { SetFocus(Some(hwnd)) };
                s.toggle_check(line);
                return LRESULT(0);
            }
            if msg == WM_LBUTTONDBLCLK
                && let Some(path) = s.image_at(x, y).and_then(|path| s.resolve(&path))
            {
                (s.sink)(EditorEvent::OpenImage(path));
                return LRESULT(0);
            }
            if msg == WM_LBUTTONDOWN
                && !key_down(VK_SHIFT)
                && let Some(target) = s.indent_click(x, y)
            {
                // SAFETY: keyboard focus to our own control, as a click would.
                let _ = unsafe { SetFocus(Some(hwnd)) };
                s.select_bytes(target..target);
                return LRESULT(0);
            }
            if key_down(VK_CONTROL)
                && let Some(url) = s.link_at_point(x, y)
            {
                (s.sink)(EditorEvent::OpenLink(url));
                return LRESULT(0);
            }
            s.close_slash();
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            s.wheel((wp.0 >> 16) as u16 as i16, msg == WM_MOUSEHWHEEL);
            return LRESULT(0);
        }
        WM_MOUSEMOVE if s.thumb_drag.get().is_some() => {
            s.thumb_move(i32::from((lp.0 >> 16) as i16));
            return LRESULT(0);
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED if s.thumb_drag.get().is_some() => {
            s.thumb_drag.set(None);
            if msg == WM_LBUTTONUP {
                // SAFETY: ends our own capture.
                let _ = unsafe { ReleaseCapture() };
            }
            s.invalidate();
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            let (x, y) = (i32::from(lp.0 as i16), i32::from((lp.0 >> 16) as i16));
            s.set_hover(s.check_at(x, y));
        }
        WM_MOUSELEAVE => s.set_hover(None),
        WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT && s.cursor_override() => {
            return LRESULT(1);
        }
        _ => {}
    }
    default()
}

impl State {
    fn cursor_point(&self) -> Option<(i32, i32)> {
        let mut point = POINT::default();
        // SAFETY: cursor position read (not moved) and converted for our window.
        unsafe {
            GetCursorPos(&mut point).ok()?;
            let _ = ScreenToClient(self.hwnd, &mut point);
        }
        Some((point.x, point.y))
    }

    /// A hand over checkboxes, and over links while Ctrl is held.
    fn cursor_override(&self) -> bool {
        let Some((x, y)) = self.cursor_point() else { return false };
        let over_track = self.hits.borrow().thumb.is_some_and(|(_, track)| {
            x >= track.left && x < track.right && y >= track.top && y < track.bottom
        });
        if over_track || self.thumb_drag.get().is_some() {
            // SAFETY: a shared system cursor.
            unsafe {
                if let Ok(cursor) = LoadCursorW(None, IDC_ARROW) {
                    SetCursor(Some(cursor));
                }
            }
            return true;
        }
        let hand = self.check_at(x, y).is_some()
            || (key_down(VK_CONTROL) && self.link_at_point(x, y).is_some());
        if hand {
            // SAFETY: a shared system cursor.
            unsafe {
                if let Ok(cursor) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(Some(cursor));
                }
            }
        }
        hand
    }

    fn update_cursor(&self) {
        if !self.cursor_override() {
            // SAFETY: a shared system cursor.
            unsafe {
                if let Ok(cursor) = LoadCursorW(None, IDC_IBEAM) {
                    SetCursor(Some(cursor));
                }
            }
        }
    }

    /// Key presses. `Some` when handled.
    fn key(&self, vk: VIRTUAL_KEY, msg: u32) -> Option<LRESULT> {
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let alt = key_down(VK_MENU) || msg == WM_SYSKEYDOWN;
        let alt_gr = key_down(VK_RMENU);
        let handled = Some(LRESULT(0));
        if vk == VK_CONTROL {
            self.update_cursor();
            return None;
        }
        let menu_open = self.slash.borrow().as_ref().is_some_and(|menu| !menu.closing);
        if menu_open {
            match vk {
                VK_UP => {
                    self.move_slash(-1);
                    return handled;
                }
                VK_DOWN => {
                    self.move_slash(1);
                    return handled;
                }
                VK_RETURN | VK_TAB if !ctrl => {
                    if self.pick_slash(None) {
                        self.eat_char.set(Some(if vk == VK_RETURN { 0x0d } else { 0x09 }));
                        return handled;
                    }
                }
                VK_ESCAPE => {
                    self.close_slash();
                    self.eat_char.set(Some(0x1b));
                    return handled;
                }
                _ => {}
            }
        }
        // Undo and redo are the editor's own (UndoStack).
        if ctrl && !alt && (vk.0 == u16::from(b'Z') || vk.0 == u16::from(b'Y')) {
            let redo = vk.0 == u16::from(b'Y') || shift;
            self.close_slash();
            self.undo_redo(redo);
            self.eat_char.set(Some(0xffff));
            return handled;
        }
        if alt && !ctrl && vk == VK_BACK {
            self.undo_redo(shift);
            self.eat_char.set(Some(0x08));
            return handled;
        }
        if alt_gr || (alt && !ctrl) {
            return None;
        }
        if let Some(command) = command_for_key(vk.0, ctrl, shift, alt) {
            self.run_command(command);
            self.eat_char.set(Some(0xffff));
            return handled;
        }
        let text = self.text.borrow().clone();
        let sel = self.selection_bytes();
        match vk {
            VK_RETURN if ctrl => {
                let line = line_at(&self.lines.borrow(), sel.start);
                self.toggle_check(line);
                self.eat_char.set(Some(0x0a));
                return handled;
            }
            VK_RETURN => {
                let change = markdown::enter(&text, sel.clone()).unwrap_or_else(|| {
                    // Shift+Enter and plain breaks go through here too, so the soft break
                    // (vertical tab) never gets into a note.
                    markdown::paste(&text, sel, "\n")
                });
                self.apply_change(&change);
                self.eat_char.set(Some(0x0d));
                return handled;
            }
            VK_BACK if !ctrl => {
                if let Some(change) = markdown::backspace(&text, sel) {
                    self.apply_change(&change);
                    self.eat_char.set(Some(0x08));
                    return handled;
                }
            }
            VK_DELETE if !ctrl && !shift => {
                if let Some(change) = delete_forward(&text, sel) {
                    self.apply_change(&change);
                    return handled;
                }
            }
            VK_TAB if ctrl => {
                (self.sink)(if shift {
                    EditorEvent::FocusPrevious
                } else {
                    EditorEvent::FocusNext
                });
                self.eat_char.set(Some(0x09));
                return handled;
            }
            VK_TAB => {
                if let Some(change) = markdown::indent(&text, sel, shift) {
                    self.apply_change(&change);
                } else if shift {
                    (self.sink)(EditorEvent::FocusPrevious);
                } else {
                    self.apply_change(&markdown::paste(&text, self.selection_bytes(), "\t"));
                }
                self.eat_char.set(Some(0x09));
                return handled;
            }
            VK_LEFT if !shift && !ctrl && sel.is_empty() => {
                // From an item's first word to the line above: never into the hidden marker.
                let lines = self.lines.borrow();
                let index = line_at(&lines, sel.start);
                let line = &lines[index];
                if line.block.is_list() && sel.start == line.content && index > 0 {
                    let target = lines[index - 1].range.end;
                    drop(lines);
                    self.select_bytes(target..target);
                    return handled;
                }
                // Left from a heading's or quote's words goes into its marker on purpose.
                let into_marker = reveals_prefix(line.block)
                    && sel.start == line.content
                    && line.content > line.range.start;
                drop(lines);
                self.marker_edit.set(into_marker);
            }
            VK_HOME if !shift && !ctrl && sel.is_empty() => {
                // Home goes to the start of the block's words, after its marker.
                let content = {
                    let lines = self.lines.borrow();
                    let line = &lines[line_at(&lines, sel.start)];
                    (line.content > line.range.start && sel.start != line.content)
                        .then_some(line.content)
                };
                if let Some(content) = content {
                    self.marker_edit.set(false);
                    self.select_bytes(content..content);
                    return handled;
                }
            }
            VK_ESCAPE => {
                // Escape does nothing in a note; swallow it so no dialog code acts.
                self.eat_char.set(Some(0x1b));
                return handled;
            }
            _ => {}
        }
        if ctrl && (vk.0 == u16::from(b'V')) && !shift && !alt {
            self.paste_clipboard();
            self.eat_char.set(Some(0x16));
            return handled;
        }
        if shift && !ctrl && vk == VK_INSERT {
            self.paste_clipboard();
            return handled;
        }
        if ctrl && !ctrl_key_allowed(vk.0, shift) && !matches!(vk, VK_SHIFT | VK_MENU) {
            self.eat_char.set(Some(0xffff));
            return handled;
        }
        None
    }

    fn char(&self, unit: u16) -> Option<LRESULT> {
        if let Some(eat) = self.eat_char.take()
            && (eat == unit || (eat == 0xffff && unit < 0x20) || (eat == 0x0d && unit == 0x0a))
        {
            return Some(LRESULT(0));
        }
        // Control characters from Ctrl+letter keys never reach the text.
        if unit < 0x20 && !matches!(unit, 0x08 | 0x09 | 0x0d) {
            return Some(LRESULT(0));
        }
        let ch = char::from_u32(u32::from(unit))?;
        if self.composing.get() || unit < 0x20 {
            return None;
        }
        let text = self.text.borrow().clone();
        let sel = self.selection_bytes();
        if let Some(change) = markdown::type_char(&text, sel.clone(), ch) {
            // The typed character goes in as typed (its own undo step), then the conversion as
            // another: one Ctrl+Z gives back exactly what was typed ("[] ", "```", "---").
            let converted = change.apply(&text);
            self.undo.borrow_mut().close_group();
            // SAFETY: the character goes to RichEdit as typed; EN_CHANGE updates our text.
            unsafe { DefSubclassProc(self.hwnd, WM_CHAR, WPARAM(usize::from(unit)), LPARAM(1)) };
            self.undo.borrow_mut().close_group();
            let typed = self.text.borrow().clone();
            self.apply_change(&markdown::change_between(&typed, &converted, change.selection));
            return Some(LRESULT(0));
        }
        let boundary = completes_shortcut(&text, sel, ch);
        if boundary {
            self.undo.borrow_mut().close_group();
        }
        // SAFETY: the character goes to RichEdit as typed.
        unsafe { DefSubclassProc(self.hwnd, WM_CHAR, WPARAM(usize::from(unit)), LPARAM(1)) };
        if boundary {
            self.undo.borrow_mut().close_group();
        }
        if ch == '/' {
            self.update_slash(true);
        }
        Some(LRESULT(0))
    }
}

/// Delete at the end of a line before a heading, list item, to-do or quote: the next line's
/// words join this one without its marker (the marker is hidden, so it would otherwise show up
/// in the middle of the text).
pub fn delete_forward(text: &str, sel: Range<usize>) -> Option<Change> {
    if !sel.is_empty() {
        return None;
    }
    let lines = blocks(text);
    let index = line_at(&lines, sel.start);
    let line = &lines[index];
    let next = lines.get(index + 1)?;
    if sel.start != line.range.end || next.content == next.range.start {
        return None;
    }
    if !matches!(
        next.block,
        Block::Heading(_) | Block::Bullet | Block::Numbered(_) | Block::Todo(_) | Block::Quote
    ) {
        return None;
    }
    let new = format!("{}{}", &text[..line.range.end], &text[next.content..]);
    let renumbered = markdown::renumber(&new);
    Some(markdown::change_between(text, &renumbered, sel.start..sel.start))
}

// ---------------------------------------------------------------------------
// Paste and copy through RichEdit's OLE callback
// ---------------------------------------------------------------------------

/// RichEdit asks this before a paste or a drop goes in, and for the data a copy puts on the
/// clipboard. Only plain text goes in; a copy gives the Markdown source.
///
/// A small COM object written by hand (the `implement` macro would need the `windows-core`
/// crate as a direct dependency): a vtable, a reference count and the editor's State.
#[repr(C)]
struct OleCallback {
    vtable: *const IRichEditOleCallback_Vtbl,
    refs: std::sync::atomic::AtomicU32,
    state: *const State,
}

impl OleCallback {
    fn create(state: *const State) -> Result<IRichEditOleCallback> {
        let object = Box::into_raw(Box::new(OleCallback {
            vtable: &CALLBACK_VTABLE,
            refs: std::sync::atomic::AtomicU32::new(1),
            state,
        }));
        // SAFETY: the box is a valid COM object (vtable first) with one reference, which the
        // returned interface owns and releases.
        Ok(unsafe { IRichEditOleCallback::from_raw(object.cast()) })
    }
}

static CALLBACK_VTABLE: IRichEditOleCallback_Vtbl = IRichEditOleCallback_Vtbl {
    base__: windows::core::IUnknown_Vtbl {
        QueryInterface: callback_query,
        AddRef: callback_add_ref,
        Release: callback_release,
    },
    GetNewStorage: callback_not_implemented_1,
    GetInPlaceContext: callback_in_place,
    ShowContainerUI: callback_ok_bool,
    QueryInsertObject: callback_insert_object,
    DeleteObject: callback_ok_pointer,
    QueryAcceptData: callback_accept_data,
    ContextSensitiveHelp: callback_ok_bool,
    GetClipboardData: callback_clipboard_data,
    GetDragDropEffect: callback_drag_drop,
    GetContextMenu: callback_context_menu,
};

unsafe extern "system" fn callback_query(
    this: *mut c_void,
    iid: *const windows::core::GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: COM contract: `iid` and `out` are valid; `this` is our object.
    unsafe {
        if out.is_null() || iid.is_null() {
            return windows::Win32::Foundation::E_POINTER;
        }
        if *iid == windows::core::IUnknown::IID || *iid == IRichEditOleCallback::IID {
            callback_add_ref(this);
            *out = this;
            HRESULT(0)
        } else {
            *out = std::ptr::null_mut();
            windows::Win32::Foundation::E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn callback_add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live OleCallback.
    let object = unsafe { &*(this as *const OleCallback) };
    object.refs.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

unsafe extern "system" fn callback_release(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live OleCallback; the last release frees the box it came from.
    unsafe {
        let object = &*(this as *const OleCallback);
        let left = object.refs.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this as *mut OleCallback));
        }
        left
    }
}

unsafe extern "system" fn callback_not_implemented_1(
    _this: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    if !out.is_null() {
        // SAFETY: RichEdit's out pointer.
        unsafe { *out = std::ptr::null_mut() };
    }
    E_NOTIMPL
}

unsafe extern "system" fn callback_in_place(
    _this: *mut c_void,
    _frame: *mut *mut c_void,
    _doc: *mut *mut c_void,
    _info: *mut OLEINPLACEFRAMEINFO,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn callback_ok_bool(_this: *mut c_void, _value: BOOL) -> HRESULT {
    HRESULT(0)
}

unsafe extern "system" fn callback_ok_pointer(_this: *mut c_void, _object: *mut c_void) -> HRESULT {
    HRESULT(0)
}

unsafe extern "system" fn callback_insert_object(
    _this: *mut c_void,
    _clsid: *mut windows::core::GUID,
    _storage: *mut c_void,
    _cp: i32,
) -> HRESULT {
    // No embedded objects in a note.
    E_FAIL
}

unsafe extern "system" fn callback_accept_data(
    _this: *mut c_void,
    data: *mut c_void,
    format: *mut u16,
    _reco: RECO_FLAGS,
    _really: BOOL,
    _metafile: HGLOBAL,
) -> HRESULT {
    if data.is_null() {
        return E_FAIL;
    }
    let text = FORMATETC {
        cfFormat: CF_UNICODETEXT.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: `data` is the IDataObject RichEdit lends for this call (borrowed, not owned);
    // `format` is RichEdit's out parameter for the format to use.
    unsafe {
        let Some(object) = IDataObject::from_raw_borrowed(&data) else { return E_FAIL };
        if object.QueryGetData(&text) != HRESULT(0) {
            // An image, a file or rich text without a plain version: nothing goes in.
            return E_FAIL;
        }
        if !format.is_null() {
            *format = CF_UNICODETEXT.0;
        }
    }
    HRESULT(0)
}

unsafe extern "system" fn callback_clipboard_data(
    this: *mut c_void,
    range: *mut CHARRANGE,
    _reco: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: `this` is our object; its State outlives the control that calls back; `range`
    // and `out` are RichEdit's for this call.
    unsafe {
        if range.is_null() || out.is_null() {
            return E_FAIL;
        }
        let state = &*(*(this as *const OleCallback)).state;
        let range = *range;
        let source = {
            let Ok(text) = state.text.try_borrow() else { return E_FAIL };
            let map = Utf16Map::new(&text);
            let start = map.to8(range.cpMin.max(0) as usize);
            let end = map.to8(range.cpMax.max(0) as usize);
            clipboard_text(&text[start.min(end)..end.max(start)])
        };
        match text_data_object(&source) {
            Ok(object) => {
                *out = object.into_raw();
                HRESULT(0)
            }
            Err(error) => error.code(),
        }
    }
}

unsafe extern "system" fn callback_drag_drop(
    _this: *mut c_void,
    _drag: BOOL,
    _keys: MODIFIERKEYS_FLAGS,
    _effect: *mut DROPEFFECT,
) -> HRESULT {
    HRESULT(0)
}

unsafe extern "system" fn callback_context_menu(
    _this: *mut c_void,
    _kind: RICH_EDIT_GET_CONTEXT_MENU_SEL_TYPE,
    _object: *mut c_void,
    _range: *mut CHARRANGE,
    _menu: *mut HMENU,
) -> HRESULT {
    E_NOTIMPL
}

// ---------------------------------------------------------------------------
// Slash menu window
// ---------------------------------------------------------------------------

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the owning editor's State (or 0 before it is set).
    let state = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref() };
    match (msg, state) {
        (WM_MOUSEACTIVATE, _) => return LRESULT(MA_NOACTIVATE as isize),
        (WM_MOUSEMOVE, Some(s)) => {
            let y = i32::from((lp.0 >> 16) as i16);
            let index = s.popup_hit(y);
            let current = s.slash.borrow().as_ref().map(|menu| menu.selected);
            if let (Some(index), Some(current)) = (index, current)
                && index != current
            {
                s.move_slash(index as isize - current as isize);
            }
            return LRESULT(0);
        }
        (WM_LBUTTONDOWN, Some(s)) => {
            let y = i32::from((lp.0 >> 16) as i16);
            if let Some(index) = s.popup_hit(y) {
                s.pick_slash(Some(index));
            }
            return LRESULT(0);
        }
        (WM_NCDESTROY, _) => {
            // SAFETY: detach before late messages.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        }
        _ => {}
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// GDI fonts for drawn labels (numbers, placeholders, the slash menu), at one DPI.
#[derive(Clone, Copy)]
pub struct GdiFonts {
    pub body: HFONT,
    pub small: HFONT,
    pub semibold: HFONT,
    pub h1: HFONT,
    pub h2: HFONT,
    pub h3: HFONT,
}

/// The fonts made so far, one set per DPI for the life of the process (GDI fonts are process
/// wide). Every note window and editor used to make its own set and never free it: 12 GDI
/// objects lost per note opened, against the process's 10,000-object quota.
static FONT_SETS: std::sync::Mutex<Vec<(u32, [isize; 6])>> = std::sync::Mutex::new(Vec::new());

impl GdiFonts {
    /// The fonts for a DPI, made once and then shared.
    pub fn new(dpi: u32) -> Self {
        let mut sets = FONT_SETS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, handles)) = sets.iter().find(|(made_for, _)| *made_for == dpi) {
            return Self::from_handles(*handles);
        }
        let fonts = Self::make(dpi);
        sets.push((dpi, fonts.handles()));
        fonts
    }

    fn handles(&self) -> [isize; 6] {
        [self.body, self.small, self.semibold, self.h1, self.h2, self.h3]
            .map(|font| font.0 as isize)
    }

    fn from_handles(handles: [isize; 6]) -> Self {
        let [body, small, semibold, h1, h2, h3] =
            handles.map(|handle| HFONT(handle as *mut c_void));
        Self { body, small, semibold, h1, h2, h3 }
    }

    fn make(dpi: u32) -> Self {
        let make = |size: f64, weight: i32| {
            // SAFETY: a static family name; the font is owned by the process for its lifetime
            // (a handful per DPI, kept like the settings window's fonts).
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
            }
        };
        Self {
            body: make(14.0, 400),
            small: make(11.5, 600),
            semibold: make(12.0, 600),
            h1: make(20.0, 600),
            h2: make(17.0, 600),
            h3: make(15.5, 600),
        }
    }

    fn heading(&self, block: Block) -> HFONT {
        match block {
            Block::Heading(1) => self.h1,
            Block::Heading(2) => self.h2,
            _ => self.h3,
        }
    }
}

/// A 32-bit top-down DIB selected into a memory DC.
pub struct Surface {
    pub dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    pub width: u32,
    pub height: u32,
}

impl Surface {
    pub fn new(width: u32, height: u32) -> Result<Self> {
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
        // SAFETY: a top-down 32-bit DIB; `bits` is a valid out pointer.
        let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
        // SAFETY: a private memory DC for this thread.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            // SAFETY: the bitmap was never selected.
            let _ = unsafe { DeleteObject(bitmap.into()) };
            return Err(Error::from_thread());
        }
        // SAFETY: selection restored in Drop.
        let old = unsafe { SelectObject(dc, bitmap.into()) };
        Ok(Self { dc, bitmap, old, bits: bits.cast(), width, height })
    }

    pub fn canvas(&mut self) -> Canvas<'_> {
        // SAFETY: GdiFlush finishes GDI work on the DIB before the CPU touches it; the slice is
        // the DIB's own memory, borrowed for the canvas's lifetime.
        let bits = unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, (self.width * self.height * 4) as usize)
        };
        Canvas {
            pixmap: tiny_skia::PixmapMut::from_bytes(bits, self.width, self.height)
                .expect("valid DIB"),
            labels: Vec::new(),
            dc: self.dc,
        }
    }

    /// A copy of the pixels (BGRA as GDI and tiny-skia left them).
    pub fn pixels(&mut self) -> Vec<u8> {
        // SAFETY: GDI work on the DIB is finished before the copy; the DIB is ours.
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts(self.bits, (self.width * self.height * 4) as usize).to_vec()
        }
    }

    /// Shows the surface (premultiplied BGRA) as a layered window at `at` with an overall alpha.
    pub fn present_layered(&self, hwnd: HWND, at: POINT, alpha: u8) {
        let size = SIZE { cx: self.width as i32, cy: self.height as i32 };
        let source = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: our layered window and memory DC; all pointers are locals for the call.
        let _ = unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(&at),
                Some(&size),
                Some(self.dc),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        };
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: restore the original selection, then free the owned GDI objects.
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
    font: HFONT,
    format: DRAW_TEXT_FORMAT,
    /// Drawn with its baseline at this point instead of inside `rect`.
    baseline: Option<(i32, i32)>,
}

/// tiny-skia drawing on a DIB (BGRA, so colours are fed with red and blue swapped), plus GDI
/// labels drawn after the shapes.
pub struct Canvas<'a> {
    pixmap: tiny_skia::PixmapMut<'a>,
    labels: Vec<Label>,
    dc: HDC,
}

pub fn rounded_path(rect: RECT, radius: f32) -> Option<tiny_skia::Path> {
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

fn paint_of(color: COLORREF, opacity: f64) -> tiny_skia::Paint<'static> {
    let mut paint = tiny_skia::Paint::default();
    let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    paint.set_color_rgba8((color.0 >> 16) as u8, (color.0 >> 8) as u8, color.0 as u8, alpha);
    paint.anti_alias = true;
    paint
}

impl Canvas<'_> {
    pub fn clear(&mut self) {
        self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
    }

    pub fn fill(&mut self, rect: RECT, radius: f32, color: COLORREF, opacity: f64) {
        if opacity <= 0.0 || rect.right <= rect.left || rect.bottom <= rect.top {
            return;
        }
        if let Some(path) = rounded_path(rect, radius) {
            self.pixmap.fill_path(
                &path,
                &paint_of(color, opacity),
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }

    /// A one-pixel outline inside the rectangle.
    pub fn stroke(&mut self, rect: RECT, radius: f32, color: COLORREF) {
        let inner = |r: RECT| RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
        if let Some(path) = rounded_path(inner(rect), radius) {
            let (l, t, r, b) = (
                rect.left as f32 + 0.5,
                rect.top as f32 + 0.5,
                rect.right as f32 - 0.5,
                rect.bottom as f32 - 0.5,
            );
            let _ = path;
            if let Some(path) = rounded_path_f(l, t, r, b, (radius - 0.5).max(0.0)) {
                let stroke = tiny_skia::Stroke { width: 1.0, ..Default::default() };
                self.pixmap.stroke_path(
                    &path,
                    &paint_of(color, 1.0),
                    &stroke,
                    tiny_skia::Transform::identity(),
                    None,
                );
            }
        }
    }

    /// A straight stroke with round caps.
    pub fn line(&mut self, from: (f32, f32), to: (f32, f32), color: COLORREF, width: f32) {
        let mut path = tiny_skia::PathBuilder::new();
        path.move_to(from.0, from.1);
        path.line_to(to.0, to.1);
        if let Some(path) = path.finish() {
            let stroke = tiny_skia::Stroke {
                width,
                line_cap: tiny_skia::LineCap::Round,
                ..Default::default()
            };
            self.pixmap.stroke_path(
                &path,
                &paint_of(color, 1.0),
                &stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }

    /// An x centred on a point.
    pub fn cross(&mut self, cx: f32, cy: f32, half: f32, color: COLORREF, width: f32) {
        self.line((cx - half, cy - half), (cx + half, cy + half), color, width);
        self.line((cx - half, cy + half), (cx + half, cy - half), color, width);
    }

    /// The check mark of a to-do box, drawn up to `progress` of its length.
    pub fn tick(&mut self, rect: RECT, color: COLORREF, progress: f64, scale: f64) {
        let (l, t) = (rect.left as f32, rect.top as f32);
        let w = (rect.right - rect.left) as f32;
        let points: [(f32, f32); 3] = [(0.24, 0.52), (0.43, 0.70), (0.77, 0.32)];
        let first = (points[1].0 - points[0].0).hypot(points[1].1 - points[0].1);
        let second = (points[2].0 - points[1].0).hypot(points[2].1 - points[1].1);
        let total = first + second;
        let along = progress.clamp(0.0, 1.0) as f32 * total;
        let mut path = tiny_skia::PathBuilder::new();
        path.move_to(l + w * points[0].0, t + w * points[0].1);
        if along <= first {
            let f = along / first;
            path.line_to(
                l + w * (points[0].0 + (points[1].0 - points[0].0) * f),
                t + w * (points[0].1 + (points[1].1 - points[0].1) * f),
            );
        } else {
            path.line_to(l + w * points[1].0, t + w * points[1].1);
            let f = (along - first) / second;
            path.line_to(
                l + w * (points[1].0 + (points[2].0 - points[1].0) * f),
                t + w * (points[1].1 + (points[2].1 - points[1].1) * f),
            );
        }
        if let Some(path) = path.finish() {
            let stroke = tiny_skia::Stroke {
                width: (1.8 * scale) as f32,
                line_cap: tiny_skia::LineCap::Round,
                line_join: tiny_skia::LineJoin::Round,
                ..Default::default()
            };
            self.pixmap.stroke_path(
                &path,
                &paint_of(color, 1.0),
                &stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }

    /// A picture scaled into `rect` with rounded corners.
    pub fn image(&mut self, pixmap: &tiny_skia::Pixmap, rect: RECT, radius: f32, opacity: f64) {
        let Some(path) = rounded_path(rect, radius) else { return };
        let sx = (rect.right - rect.left) as f32 / pixmap.width() as f32;
        let sy = (rect.bottom - rect.top) as f32 / pixmap.height() as f32;
        let transform =
            tiny_skia::Transform::from_row(sx, 0.0, 0.0, sy, rect.left as f32, rect.top as f32);
        let paint = tiny_skia::Paint {
            shader: tiny_skia::Pattern::new(
                pixmap.as_ref(),
                tiny_skia::SpreadMode::Pad,
                tiny_skia::FilterQuality::Bicubic,
                opacity.clamp(0.0, 1.0) as f32,
                transform,
            ),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
    }

    /// A soft shadow around a card on a transparent surface.
    pub fn shadow(&mut self, card: RECT, radius: f32, spread: i32, dark: bool) {
        let steps = spread.max(1);
        for step in (1..=steps).rev() {
            let grow = step;
            let rect = RECT {
                left: card.left - grow,
                top: card.top - grow + steps / 3,
                right: card.right + grow,
                bottom: card.bottom + grow + steps / 3,
            };
            let fall = 1.0 - f64::from(step) / f64::from(steps + 1);
            let opacity = (if dark { 0.10 } else { 0.035 }) * fall * fall;
            self.fill(rect, radius + grow as f32, COLORREF(0x3a261c), opacity);
        }
    }

    pub fn label(
        &mut self,
        rect: RECT,
        text: &str,
        ink: COLORREF,
        font: HFONT,
        format: DRAW_TEXT_FORMAT,
    ) {
        // An empty or inverted rectangle (a card still growing) draws nothing: DrawTextW with
        // DT_END_ELLIPSIS faults on a negative width (measured: an access violation in GDI).
        if rect.right <= rect.left || rect.bottom <= rect.top || text.is_empty() {
            return;
        }
        self.labels.push(Label { rect, text: text.to_owned(), ink, font, format, baseline: None });
    }

    /// Draws the queued labels with GDI (ClearType on the opaque background already there).
    /// Text with its baseline at (x, baseline), as the text control draws its own.
    pub fn label_at_baseline(
        &mut self,
        x: i32,
        baseline: i32,
        text: &str,
        ink: COLORREF,
        font: HFONT,
    ) {
        if text.is_empty() {
            return;
        }
        self.labels.push(Label {
            rect: RECT::default(),
            text: text.to_owned(),
            ink,
            font,
            format: DRAW_TEXT_FORMAT(0),
            baseline: Some((x, baseline)),
        });
    }

    /// Width, ascent and descent of a text in a font, pixels.
    pub fn measure(&self, text: &str, font: HFONT) -> (i32, i32, i32) {
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let mut metrics = TEXTMETRICW::default();
        // SAFETY: the surface's DC with a font selected for the queries, then restored.
        unsafe {
            let old = SelectObject(self.dc, font.into());
            let _ = GetTextExtentPoint32W(self.dc, &units, &mut size);
            let _ = GetTextMetricsW(self.dc, &mut metrics);
            SelectObject(self.dc, old);
        }
        (size.cx, metrics.tmAscent, metrics.tmDescent)
    }

    pub fn finish_labels(&mut self) {
        let labels = std::mem::take(&mut self.labels);
        if labels.is_empty() {
            return;
        }
        // SAFETY: the DC is the surface's; font selections are restored by RestoreDC.
        unsafe {
            let saved = SaveDC(self.dc);
            SetBkMode(self.dc, TRANSPARENT);
            for label in labels {
                SelectObject(self.dc, label.font.into());
                SetTextColor(self.dc, label.ink);
                let mut units: Vec<u16> = label.text.encode_utf16().collect();
                if let Some((x, baseline)) = label.baseline {
                    SetTextAlign(self.dc, TA_LEFT | TA_BASELINE);
                    let _ = TextOutW(self.dc, x, baseline, &units);
                    SetTextAlign(self.dc, TA_LEFT | TA_TOP);
                    continue;
                }
                let mut rect = label.rect;
                DrawTextW(self.dc, &mut units, &mut rect, label.format | DT_NOPREFIX);
            }
            let _ = RestoreDC(self.dc, saved);
            let _ = GdiFlush();
        }
    }

    /// GDI leaves alpha at 0 where it drew; inside the card everything is opaque. Sets alpha
    /// 255 there (premultiplied colours stay as drawn). On the card's antialiased edge, a
    /// pixel tiny-skia drew already holds its premultiplied colour and alpha and stays as it
    /// is; only a pixel whose alpha GDI cleared (alpha 0) is made the card's colour at that
    /// coverage. (Scaling every edge pixel by its coverage applied it twice: a dark rim.)
    pub fn opaque_inside(&mut self, card: RECT, radius: f32) {
        let Some(path) = rounded_path(card, radius) else { return };
        let mut mask =
            tiny_skia::Mask::new(self.pixmap.width(), self.pixmap.height()).expect("mask");
        mask.fill_path(&path, tiny_skia::FillRule::Winding, true, tiny_skia::Transform::identity());
        let coverage = mask.data();
        let pixels = self.pixmap.data_mut();
        for (index, cover) in coverage.iter().enumerate() {
            if *cover == 0 {
                continue;
            }
            let pixel = &mut pixels[index * 4..index * 4 + 4];
            if *cover == 255 {
                pixel[3] = 255;
            } else if pixel[3] == 0 {
                // An edge pixel GDI wrote (opaque colour, alpha 0): the card's coverage.
                let a = u16::from(*cover);
                for channel in &mut pixel[..3] {
                    *channel = ((u16::from(*channel) * a) / 255) as u8;
                }
                pixel[3] = pixel[3].max(*cover);
            }
        }
    }
}

fn rounded_path_f(l: f32, t: f32, r: f32, b: f32, radius: f32) -> Option<tiny_skia::Path> {
    let d = radius.min((r - l) / 2.0).min((b - t) / 2.0).max(0.0);
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

// Used by tests and the window to read RichEdit state.
#[doc(hidden)]
pub fn can_undo(hwnd: HWND) -> bool {
    // SAFETY: scalar query; RichEdit answers across threads through SendMessage.
    unsafe { SendMessageW(hwnd, EM_CANUNDO, None, None) }.0 != 0
}

#[doc(hidden)]
pub fn undo(hwnd: HWND) {
    // SAFETY: scalar message to a RichEdit control.
    unsafe { SendMessageW(hwnd, EM_UNDO, None, None) };
}

#[doc(hidden)]
pub fn get_sel(hwnd: HWND) -> (u32, u32) {
    let (mut start, mut end) = (0u32, 0u32);
    // SAFETY: EM_GETSEL writes two DWORDs into the locals during the call.
    unsafe {
        SendMessageW(
            hwnd,
            EM_GETSEL,
            Some(WPARAM(&mut start as *mut u32 as usize)),
            Some(LPARAM(&mut end as *mut u32 as isize)),
        )
    };
    (start, end)
}

#[doc(hidden)]
pub fn set_sel(hwnd: HWND, start: u32, end: u32) {
    // SAFETY: scalar message to a RichEdit control.
    unsafe {
        SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(start as usize)), Some(LPARAM(end as isize)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        reveal: &'a dyn Fn(usize) -> f32,
        thumb: &'a dyn Fn(&str) -> Option<(u16, u16)>,
    ) -> LayoutInput<'a> {
        LayoutInput { reveal, fade: None, thumb, empty_caret_line: None }
    }

    fn lay(text: &str, revealed: &[usize]) -> Layout {
        let reveal = |line: usize| if revealed.contains(&line) { 1.0 } else { 0.0 };
        let thumb = |_: &str| Some((120, 80));
        layout(text, &blocks(text), &markdown::style(text), &input(&reveal, &thumb))
    }

    fn fmt_at(layout: &Layout, unit: usize) -> Fmt {
        layout.runs.iter().find(|run| run.range.contains(&unit)).expect("covered").fmt
    }

    #[test]
    fn runs_cover_the_text_and_the_final_mark() {
        for text in
            ["", "a", "# T\n- [ ] x\n**b** `c`\n```\nz\n```\n![s](a.png)\n---\n> q\n1. n", "é😀 x"]
        {
            let layout = lay(text, &[]);
            let len = text.encode_utf16().count();
            let mut at = 0;
            for run in &layout.runs {
                assert_eq!(run.range.start, at, "gap in {text:?}");
                at = run.range.end;
            }
            assert_eq!(at, len + 1, "{text:?}");
            assert_eq!(layout.paras.len(), blocks(text).len());
        }
    }

    #[test]
    fn markers_hide_and_reveal_on_the_caret_line() {
        let text = "# Title\nsome **bold** text";
        let hidden = lay(text, &[]);
        assert!(fmt_at(&hidden, 0).hidden && fmt_at(&hidden, 1).hidden);
        let title = fmt_at(&hidden, 3);
        assert!(!title.hidden && title.bold && title.size == 200);
        let bold_start = text.find("**").unwrap();
        assert!(fmt_at(&hidden, bold_start).hidden);
        assert!(fmt_at(&hidden, bold_start + 2).bold);
        let shown = lay(text, &[0, 1]);
        let marker = fmt_at(&shown, 0);
        assert!(!marker.hidden && marker.ink == Ink::Mark && marker.size == 200);
        assert_eq!(fmt_at(&shown, bold_start).ink, Ink::Mark);
        assert!(!fmt_at(&shown, bold_start).hidden);
    }

    #[test]
    fn list_markers_stay_hidden_and_are_drawn() {
        let text = "- one\n  - two\n1. a\n- [x] done";
        let layout = lay(text, &[0, 1, 2, 3]);
        assert!(fmt_at(&layout, 0).hidden);
        let done_start = text.find("done").unwrap();
        let done = fmt_at(&layout, done_start);
        assert!(done.strike && done.ink == Ink::Muted);
        assert!(
            !fmt_at(&layout, text.find("[x]").unwrap()).strike,
            "the hidden marker is not struck"
        );
        let kinds: Vec<&DecoKind> = layout.decos.iter().map(|deco| &deco.kind).collect();
        assert_eq!(
            kinds,
            [&DecoKind::Bullet, &DecoKind::Bullet, &DecoKind::Number(1), &DecoKind::Check(true)]
        );
        assert_eq!(layout.decos[0].anchor, 2);
        assert_eq!(layout.decos[1].depth, 1);
        assert_eq!(layout.paras[1].para.indent, LIST_STEP * 2);
    }

    #[test]
    fn code_fences_images_and_dividers_take_little_room_when_hidden() {
        let text = "```\nlet x;\n```\n![s](attachments/a.png)\n---";
        let layout = lay(text, &[]);
        let fence_mark = fmt_at(&layout, 3);
        assert_eq!(fence_mark, Fmt::tiny());
        assert!(fmt_at(&layout, 4).mono && !fmt_at(&layout, 4).hidden);
        let image_line = text.find("![").unwrap();
        assert!(fmt_at(&layout, image_line).hidden);
        let image_para = &layout.paras[3].para;
        assert_eq!(image_para.after, 80 + IMAGE_GAP * 2);
        assert!(
            layout
                .decos
                .iter()
                .any(|deco| deco.kind == DecoKind::Image("attachments/a.png".into()))
        );
        assert!(layout.decos.iter().any(|deco| deco.kind == DecoKind::Divider));
        let shown = lay(text, &[3]);
        assert!(!fmt_at(&shown, image_line).hidden);
    }

    #[test]
    fn placeholders_for_an_empty_note_and_an_empty_caret_line() {
        let empty = lay("", &[]);
        assert!(
            empty
                .decos
                .iter()
                .any(|deco| deco.kind == DecoKind::Placeholder("Write, or type / for commands"))
        );
        let reveal = |_: usize| 0.0;
        let thumb = |_: &str| None;
        let text = "a\n# ";
        let mut with_caret = input(&reveal, &thumb);
        with_caret.empty_caret_line = Some(1);
        let layout = layout(text, &blocks(text), &markdown::style(text), &with_caret);
        assert!(layout.decos.iter().any(|deco| deco.kind == DecoKind::Placeholder("Heading 1")));
    }

    #[test]
    fn utf16_maps_both_ways() {
        let text = "aé😀\nb";
        let map = Utf16Map::new(text);
        assert_eq!(map.to16(0), 0);
        assert_eq!(map.to16(1), 1);
        assert_eq!(map.to16(3), 2);
        assert_eq!(map.to16(7), 4);
        assert_eq!(map.to16(text.len()), 6);
        assert_eq!(map.to8(2), 3);
        assert_eq!(map.to8(3), 7, "inside a pair maps to the end of the character");
        assert_eq!(map.to8(4), 7);
        assert_eq!(map.len16(), 6);
        let units = control_units(text);
        assert_eq!(units.len(), 6);
        assert_eq!(units[4], 0x0d);
        assert_eq!(from_control(&units), text);
        assert_eq!(from_control(&[0x61, 0x0b, 0x62, 0x0d]), "a\nb\n");
        assert_eq!(clipboard_text("a\nb"), "a\r\nb");
    }

    #[test]
    fn applied_runs_shift_and_diff() {
        let text = "# A\nplain";
        let first = lay(text, &[]);
        let mut applied = Applied::default();
        applied.set(&first);
        assert_eq!(applied.missing(&first), (vec![], vec![]));
        // Type "x" at the end: only runs at the end are forgotten.
        let old: Vec<u16> = control_units(text);
        let new_text = "# A\nplainx";
        let new: Vec<u16> = control_units(new_text);
        let (range, new_len) = changed_range(&old, &new);
        assert_eq!((range.clone(), new_len), (9..9, 1));
        applied.edited(range, new_len);
        let second = lay(new_text, &[]);
        let (runs, paras) = applied.missing(&second);
        assert!(
            runs.iter().all(|run| run.range.end > 4),
            "the heading is not applied again: {runs:?}"
        );
        assert!(paras.len() <= 1);
        // Typing at the start moves every run after it.
        let mut applied = Applied::default();
        applied.set(&first);
        applied.edited(0..0, 2);
        assert!(applied.runs.iter().all(|run| run.range.start >= 2
            || run.range.end == 0
            || run.range.start == 0 && false
            || run.range.start >= 2));
    }

    #[test]
    fn selection_helpers() {
        let lines = blocks("one\n- two\nthree");
        assert_eq!(touched_lines(&lines, 1..1), 0..1);
        assert_eq!(touched_lines(&lines, 1..6), 0..2);
        assert_eq!(touched_lines(&lines, 0..4), 0..1, "a selection ending at a line start");
        assert_eq!(snap_caret(&lines, 4, false), Some(6));
        assert_eq!(snap_caret(&lines, 5, false), Some(6));
        assert_eq!(snap_caret(&lines, 6, false), None);
        assert_eq!(snap_caret(&lines, 1, false), None);
        // An empty heading or quote: after the marker, unless the user moved in to edit it.
        let lines = blocks(
            "# 
> 
## Title",
        );
        assert_eq!(snap_caret(&lines, 0, false), Some(2));
        assert_eq!(snap_caret(&lines, 1, false), Some(2));
        assert_eq!(snap_caret(&lines, 1, true), None);
        assert_eq!(snap_caret(&lines, 3, false), Some(5));
        assert_eq!(
            snap_caret(&lines, 6, false),
            None,
            "a heading with words keeps a caret in its marker"
        );
        assert_eq!(snap_caret(&lines, 2, false), None);
    }

    #[test]
    fn shortcut_completion_closes_typing_groups() {
        assert!(completes_shortcut("#", 1..1, ' '));
        assert!(completes_shortcut("**bold*", 7..7, '*'));
        assert!(completes_shortcut("-", 1..1, ' '));
        assert!(!completes_shortcut("hello", 5..5, 'x'));
        assert!(!completes_shortcut("# Title", 7..7, 's'));
    }

    #[test]
    fn delete_joins_the_next_items_words() {
        let change = delete_forward("text\n- item", 4..4).unwrap();
        assert_eq!(change.apply("text\n- item"), "textitem");
        assert_eq!(delete_forward("text\nplain", 4..4), None);
        assert_eq!(delete_forward("te|xt", 2..2), None);
    }

    #[test]
    fn keys_map_to_commands_and_rich_formatting_keys_are_blocked() {
        assert_eq!(
            command_for_key(u16::from(b'B'), true, false, false),
            Some(EditorCommand::Inline(Inline::Bold))
        );
        assert_eq!(
            command_for_key(u16::from(b'X'), true, true, false),
            Some(EditorCommand::Inline(Inline::Strike))
        );
        assert_eq!(
            command_for_key(u16::from(b'2'), true, false, true),
            Some(EditorCommand::Block(BlockCommand::Heading(2)))
        );
        assert_eq!(
            command_for_key(u16::from(b'7'), true, true, false),
            Some(EditorCommand::Block(BlockCommand::Numbered))
        );
        assert_eq!(command_for_key(u16::from(b'B'), false, false, false), None);
        assert!(ctrl_key_allowed(u16::from(b'Z'), false));
        assert!(ctrl_key_allowed(u16::from(b'Z'), true));
        assert!(ctrl_key_allowed(VK_LEFT.0, true));
        assert!(!ctrl_key_allowed(u16::from(b'E'), false), "Ctrl+E centres text in RichEdit");
        assert!(!ctrl_key_allowed(u16::from(b'1'), false), "Ctrl+1 sets line spacing");
        assert!(!ctrl_key_allowed(u16::from(b'L'), true), "Ctrl+Shift+L makes a list");
    }

    #[test]
    fn thumbnails_fit_the_note() {
        assert_eq!(thumb_size((1000, 500), 1.0, 250.0), (250, 125));
        assert_eq!(thumb_size((100, 50), 1.0, 250.0), (100, 50));
        assert_eq!(thumb_size((400, 1000), 1.0, 250.0), (80, 200));
        assert_eq!(thumb_size((500, 250), 2.0, 250.0), (250, 125));
    }

    #[test]
    fn code_block_edges() {
        let lines = blocks("a\n```\nx\ny\n```\nb\n```\n```");
        let edges = code_edges(&lines);
        assert_eq!(edges[1], (true, false));
        assert_eq!(edges[2], (false, false));
        assert_eq!(edges[4], (false, true));
        assert_eq!(edges[6], (true, false));
        assert_eq!(edges[7], (false, true));
        assert_eq!(edges[0], (false, false));
    }

    /// Types `text` at the end of `current` one character at a time into the stack.
    fn type_into(stack: &mut UndoStack, current: &mut String, text: &str) {
        for ch in text.chars() {
            let before = current.len()..current.len();
            let mut next = current.clone();
            next.push(ch);
            stack.record(current, &next, before, next.len()..next.len(), true);
            *current = next;
        }
    }

    fn undo_all(stack: &mut UndoStack, mut text: String) -> Vec<String> {
        let mut seen = vec![text.clone()];
        while let Some((change, _)) = stack.undo(&text) {
            text = change.apply(&text);
            seen.push(text.clone());
        }
        seen
    }

    #[test]
    fn typing_undoes_word_by_word() {
        let mut stack = UndoStack::default();
        let mut text = String::new();
        type_into(&mut stack, &mut text, "two words");
        assert_eq!(undo_all(&mut stack, text), ["two words", "two ", ""]);
    }

    #[test]
    fn an_edit_of_the_editor_is_its_own_step() {
        let mut stack = UndoStack::default();
        let mut text = String::new();
        type_into(&mut stack, &mut text, "[]");
        stack.close_group();
        type_into(&mut stack, &mut text, " ");
        stack.close_group();
        let converted = "- [ ] ".to_owned();
        stack.record(&text, &converted, 3..3, 6..6, false);
        stack.close_group();
        assert_eq!(undo_all(&mut stack, converted), ["- [ ] ", "[] ", "[]", ""]);
    }

    #[test]
    fn backspaces_group_and_redo_replays() {
        let mut stack = UndoStack::default();
        let mut text = "hello".to_owned();
        for _ in 0..3 {
            let mut next = text.clone();
            next.pop();
            stack.record(&text, &next, text.len()..text.len(), next.len()..next.len(), true);
            text = next;
        }
        assert_eq!(text, "he");
        let (change, selection) = stack.undo(&text).expect("undo");
        let back = change.apply(&text);
        assert_eq!((back.as_str(), selection), ("hello", 5..5));
        let (change, selection) = stack.redo(&back).expect("redo");
        assert_eq!((change.apply(&back).as_str(), selection), ("he", 2..2));
        assert!(stack.can_undo() && !stack.can_redo());
    }

    #[test]
    fn a_caret_move_or_a_reload_ends_the_history_safely() {
        let mut stack = UndoStack::default();
        let mut text = String::new();
        type_into(&mut stack, &mut text, "ab");
        stack.close_group();
        type_into(&mut stack, &mut text, "cd");
        assert_eq!(undo_all(&mut stack.clone(), text.clone()), ["abcd", "ab", ""]);
        // The text was replaced from disk: the history no longer fits and is dropped.
        assert_eq!(stack.undo("something else"), None);
        assert!(!stack.can_undo());
    }

    #[test]
    fn transitions_ease_and_settle() {
        let mut t = Transition::at(0.0);
        t.retarget(1.0, 0.0, 100.0);
        assert!(t.active(50.0) && t.value(50.0) > 0.5);
        assert_eq!(t.value(100.0), 1.0);
        t.retarget(0.0, 100.0, 0.0);
        assert_eq!(t.value(100.0), 0.0);
        assert!(!t.active(100.0));
    }
}
