//! What the user sees while recording, and the video never shows: a blue outline around the
//! area being recorded (it shrinks and slides as the user zooms) and a small control strip at
//! the top of the screen. From `RecordingFrame.swift`, with the strip redone for Windows.
//!
//! # The outline
//!
//! Four thin layered windows, one per side, each `WS_EX_LAYERED | WS_EX_TRANSPARENT |
//! WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` and `WDA_EXCLUDEFROMCAPTURE` (the
//! overlay's window, `overlay::Overlay`). A window covering the whole display would hold a
//! monitor-sized bitmap (33 MB at 4K) that is almost all transparent and send it again on
//! every frame of the zoom's ease; the strips are a few hundred kilobytes. Top and bottom run
//! the full width with the rounded corners and the glow; left and right fill between them.
//! All are clipped to the recorded monitor. The outline is the accent blue at 90%, 2 DIPs
//! wide with a 10 DIP corner, inset 1 DIP from the area, with a soft glow over 6 DIPs.
//!
//! # The strip
//!
//! At rest: a red recording dot, the elapsed time `m:ss`, and three icon buttons (pen,
//! highlighter, stop). The tool in hand shows as selected (the card-size control's look).
//! Zoomed in, a `2.0x` badge follows the time; clicking it shows the whole screen again. No
//! sentence of text: the keys are taught by three quiet means, never all the time:
//!
//! - hovering a button for [`TOOLTIP_DELAY`] shows its name and its shortcut as key chips;
//! - for the first recordings ([`RecordingUiConfig::show_start_hint`]) one line,
//!   "Hold Alt and scroll to zoom", fades in under the strip and goes by itself;
//! - the first time a tool is picked up in a recording, its three keys show for
//!   [`TOOL_HINT_TIME`].
//!
//! These show in a second, click-through window under the strip (the "tip"), excluded from
//! capture like the rest.
//!
//! The strip is the one window here that takes the mouse. It is exactly the plate (its
//! rounded corners are alpha 0, which Windows hit-tests as holes), so it takes no click
//! beside itself. It never activates and never takes the focus: `WS_EX_NOACTIVATE` and
//! `WM_MOUSEACTIVATE` -> `MA_NOACTIVATE`; the recorded app stays the foreground window and its
//! clicks stop at the strip. A press captures the mouse only while a button is physically
//! down, so the release always comes back. The dot and the time are a grip: dragging them
//! moves the strip along the top edge (not remembered). The strip is topmost and is put back
//! on top whenever the ink window comes up, so it stays above the ink window, which is a
//! separate window: a press on the strip never reaches the ink.
//!
//! # Time
//!
//! The strip redraws once a second for the time, and otherwise only while something moves
//! (a hover fade, the appear and disappear fades, a hint fading). Hints and the tooltip delay
//! are deadlines the driver waits for, not a ticking clock. Reduced motion ("Animation
//! effects" off) makes every fade instant.
//!
//! [`RecordingUiConfig::show_start_hint`]: super::ink_overlay::RecordingUiConfig::show_start_hint

use std::cell::{Cell, RefCell};
use std::mem::size_of;

use fontdue::{Font, FontSettings};
use igorio_core::geom::Rect;
use igorio_core::ink::InkTool;
use igorio_kit::theme;
use tiny_skia::{FillRule, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT,
    TrackMouseEvent, VK_LBUTTON, VK_RBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOPMOST, IDC_ARROW, LoadCursorW,
    MA_NOACTIVATE, RegisterClassW, SPI_GETCLIENTAREAANIMATION, SWP_HIDEWINDOW, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowDisplayAffinity, SetWindowPos,
    SystemParametersInfoW, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WM_CAPTURECHANGED, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Error, PCWSTR, w};

use super::input::ZoomModifier;
use crate::overlay::{LayeredSurface, MonitorBounds, Overlay};

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// The outline line, DIPs (`outline.lineWidth`).
pub const OUTLINE_WIDTH: f64 = 2.0;
/// Corner radius, DIPs (`cornerWidth`).
pub const OUTLINE_RADIUS: f64 = 10.0;
/// The outline sits this far inside the area, DIPs (`insetBy(dx: 1, dy: 1)`).
pub const OUTLINE_INSET: f64 = 1.0;
/// How far the glow reaches from the line, DIPs (`shadowRadius`).
pub const GLOW: f64 = 6.0;
/// The line's opacity (`alpha: 0.9`).
const LINE_ALPHA: f32 = 0.9;
/// The glow's opacity next to the line. Swift's `shadowOpacity` 0.45 applies to a 6 pt blur
/// of a 2 pt line, whose peak is about a third of that.
const GLOW_ALPHA: f32 = 0.16;

/// The strip, DIPs: height, side padding, dot, the gap after the dot and after the time.
pub const PILL_HEIGHT: f64 = 32.0;
const PILL_PAD: f64 = 10.0;
const PILL_DOT: f64 = 8.0;
const DOT_GAP: f64 = 7.0;
const PILL_GAP: f64 = 8.0;
/// The zoom badge: height, side padding, corner radius.
const BADGE_HEIGHT: f64 = 20.0;
const BADGE_PAD: f64 = 6.0;
const CHIP_RADIUS: f64 = 5.0;
/// The icon buttons: size, gap between them, corner radius, the icon's box.
const BUTTON: (f64, f64) = (28.0, 24.0);
const BUTTON_GAP: f64 = 2.0;
const BUTTON_RADIUS: f64 = 6.0;
const ICON: f64 = 16.0;
/// The separator between the readout and the buttons: the gap before it, the gap after it
/// (the buttons carry their own inner space), its height.
const SEPARATOR_BEFORE: f64 = 10.0;
const SEPARATOR_AFTER: f64 = 4.0;
const SEPARATOR_HEIGHT: f64 = 16.0;
/// The strip's gap below the top of the work area, DIPs (`- 8`).
pub const PILL_TOP: f64 = 8.0;
/// The widest the strip gets at rest, DIPs.
pub const PILL_MAX_WIDTH: f64 = 220.0;
/// Text sizes, DIPs.
const TIME_SIZE: f32 = 13.0;
const BADGE_SIZE: f32 = 11.5;
const TIP_SIZE: f32 = 12.0;
const KEY_SIZE: f32 = 11.0;
/// The tip under the strip: gap below it, height, padding, chip height and padding.
const TIP_GAP: f64 = 6.0;
const TIP_HEIGHT: f64 = 28.0;
const TIP_PAD: f64 = 10.0;
const TIP_MARGIN: f64 = 6.0;
const KEY_HEIGHT: f64 = 18.0;
const KEY_PAD: f64 = 5.0;
const SEGMENT_GAP: f64 = 4.0;

/// Fade in and out of the frame, seconds.
pub const APPEAR: f64 = 0.2;
pub const DISAPPEAR: f64 = 0.15;
/// A button's hover highlight fades in and out over this long.
pub const HOVER_FADE: f64 = 0.12;
/// The tooltip shows after the pointer rests on a part this long.
pub const TOOLTIP_DELAY: f64 = 0.5;
/// The start hint fades in this long after the strip appears, and stays this long.
pub const START_HINT_DELAY: f64 = 0.8;
pub const START_HINT_TIME: f64 = 5.0;
/// The tool's keys show this long, once per recording.
pub const TOOL_HINT_TIME: f64 = 2.0;
/// The tip fades in and out.
pub const TIP_FADE_IN: f64 = 0.15;
pub const TIP_FADE_OUT: f64 = 0.25;
/// Below this zoom the badge is gone.
pub const BADGE_FROM: f64 = 1.05;

// ---------------------------------------------------------------------------------------
// Text helpers (pure)
// ---------------------------------------------------------------------------------------

/// The shortcut labels the pill shows (`None`: the user left the action without one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    pub toggle: Option<String>,
    pub pen: Option<String>,
    pub highlighter: Option<String>,
}

impl Default for Labels {
    /// The defaults of the ROADMAP table.
    fn default() -> Self {
        Labels {
            toggle: Some("Ctrl+Shift+6".into()),
            pen: Some("Ctrl+Shift+7".into()),
            highlighter: Some("Ctrl+Shift+8".into()),
        }
    }
}

/// `m:ss` of whole seconds (`String(format: "%d:%02d")`); minutes are not capped.
pub fn format_elapsed(seconds: f64) -> String {
    let whole = if seconds.is_finite() && seconds > 0.0 {
        seconds.floor() as u64
    } else {
        0
    };
    format!("{}:{:02}", whole / 60, whole % 60)
}

/// Seconds from `seconds` to the next whole second (the next text change).
pub fn until_next_second(seconds: f64) -> f64 {
    if !seconds.is_finite() || seconds < 0.0 {
        return 1.0;
    }
    (seconds.floor() + 1.0 - seconds).max(0.001)
}

// ---------------------------------------------------------------------------------------
// The outline's geometry (pure)
// ---------------------------------------------------------------------------------------

/// A window rectangle in physical pixels, virtual-screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strip {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Strip {
    fn clip(self, to: Strip) -> Option<Strip> {
        let left = self.x.max(to.x);
        let top = self.y.max(to.y);
        let right = (self.x + self.w).min(to.x + to.w);
        let bottom = (self.y + self.h).min(to.y + to.h);
        (right > left && bottom > top).then_some(Strip {
            x: left,
            y: top,
            w: right - left,
            h: bottom - top,
        })
    }
}

/// The outline's rectangle (where the line's centre runs) for a recorded `area`, both in
/// physical pixels: the area rounded to whole pixels and inset by [`OUTLINE_INSET`] plus half
/// the line. `None` for an empty area.
pub fn outline_rect(area: Rect, scale: f64) -> Option<Strip> {
    if !(area.width > 0.0 && area.height > 0.0) || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let inset = ((OUTLINE_INSET + OUTLINE_WIDTH / 2.0) * scale).round() as i32;
    let left = area.x.round() as i32;
    let top = area.y.round() as i32;
    let right = (area.x + area.width).round() as i32;
    let bottom = (area.y + area.height).round() as i32;
    let w = right - left - 2 * inset;
    let h = bottom - top - 2 * inset;
    (w > 0 && h > 0).then_some(Strip {
        x: left + inset,
        y: top + inset,
        w,
        h,
    })
}

/// The four strip windows (top, bottom, left, right) that show the outline of `area` on
/// `monitor` (both physical px). A side that would be empty after clipping to the monitor is
/// `None`.
pub fn outline_strips(area: Rect, monitor: Rect, scale: f64) -> [Option<Strip>; 4] {
    let Some(outline) = outline_rect(area, scale) else {
        return [None; 4];
    };
    let glow = (GLOW * scale).ceil() as i32 + 1;
    let half = (OUTLINE_WIDTH * scale / 2.0).ceil() as i32;
    // Deep enough for the corner arc, the line and the glow on both sides of it.
    let band = (OUTLINE_RADIUS * scale).ceil() as i32 + half + glow * 2;
    let outer = Strip {
        x: outline.x - half - glow,
        y: outline.y - half - glow,
        w: outline.w + 2 * (half + glow),
        h: outline.h + 2 * (half + glow),
    };
    let band = band.min(outer.h / 2).max(1);
    let top = Strip { h: band, ..outer };
    let bottom = Strip {
        y: outer.y + outer.h - band,
        h: band,
        ..outer
    };
    let side_h = outer.h - 2 * band;
    let side_w = band.min(outer.w / 2);
    let left = Strip {
        x: outer.x,
        y: outer.y + band,
        w: side_w,
        h: side_h,
    };
    let right = Strip {
        x: outer.x + outer.w - side_w,
        ..left
    };
    let screen = Strip {
        x: monitor.x.round() as i32,
        y: monitor.y.round() as i32,
        w: monitor.width.round() as i32,
        h: monitor.height.round() as i32,
    };
    let clip = |strip: Strip| {
        (strip.w > 0 && strip.h > 0)
            .then(|| strip.clip(screen))
            .flatten()
    };
    [clip(top), clip(bottom), clip(left), clip(right)]
}

// ---------------------------------------------------------------------------------------
// Drawing (pure)
// ---------------------------------------------------------------------------------------

fn rounded(x: f64, y: f64, w: f64, h: f64, r: f64) -> Option<Path> {
    let (x, y, w, h) = (x as f32, y as f32, w as f32, h as f32);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = (r as f32).clamp(0.0, w.min(h) / 2.0);
    let k = r * 0.552_284_8;
    let (l, t, rt, b) = (x, y, x + w, y + h);
    let mut p = PathBuilder::new();
    p.move_to(l + r, t);
    p.line_to(rt - r, t);
    p.cubic_to(rt - r + k, t, rt, t + r - k, rt, t + r);
    p.line_to(rt, b - r);
    p.cubic_to(rt, b - r + k, rt - r + k, b, rt - r, b);
    p.line_to(l + r, b);
    p.cubic_to(l + r - k, b, l, b - r + k, l, b - r);
    p.line_to(l, t + r);
    p.cubic_to(l, t + r - k, l + r - k, t, l + r, t);
    p.close();
    p.finish()
}

fn paint(rgb: [u8; 3], alpha: f32) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(
        rgb[0],
        rgb[1],
        rgb[2],
        (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
    );
    paint.anti_alias = true;
    paint
}

/// Draws the outline whose line runs along `outline` (physical px) into `pixmap`, whose
/// top-left is at `origin` in the same coordinates: the glow first, then the line.
pub fn draw_outline(pixmap: &mut Pixmap, outline: Strip, origin: (i32, i32), scale: f64) {
    let x = f64::from(outline.x - origin.0);
    let y = f64::from(outline.y - origin.1);
    let Some(path) = rounded(
        x,
        y,
        f64::from(outline.w),
        f64::from(outline.h),
        OUTLINE_RADIUS * scale,
    ) else {
        return;
    };
    let blue = [theme::ACCENT[0], theme::ACCENT[1], theme::ACCENT[2]];
    let line = OUTLINE_WIDTH * scale;
    // The glow: six rings centred on the line, each a little wider and all faint, so their
    // sum fades from `GLOW_ALPHA` at the line to nothing at `GLOW`.
    const RINGS: u32 = 6;
    let ring_alpha = 1.0 - (1.0 - GLOW_ALPHA).powf(1.0 / RINGS as f32);
    for i in 1..=RINGS {
        let width = line + 2.0 * GLOW * scale * f64::from(i) / f64::from(RINGS);
        let stroke = Stroke {
            width: width as f32,
            ..Stroke::default()
        };
        pixmap.stroke_path(
            &path,
            &paint(blue, ring_alpha),
            &stroke,
            Transform::identity(),
            None,
        );
    }
    let stroke = Stroke {
        width: line as f32,
        ..Stroke::default()
    };
    pixmap.stroke_path(
        &path,
        &paint(blue, LINE_ALPHA),
        &stroke,
        Transform::identity(),
        None,
    );
}

/// The pixmap of one strip window.
pub fn render_strip(strip: Strip, outline: Strip, scale: f64) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(strip.w.max(1) as u32, strip.h.max(1) as u32)?;
    draw_outline(&mut pixmap, outline, (strip.x, strip.y), scale);
    Some(pixmap)
}

/// The pill's two fonts.
pub struct Fonts {
    regular: Font,
    semibold: Font,
}

impl Fonts {
    pub fn new() -> Fonts {
        let load = |bytes: &'static [u8]| {
            Font::from_bytes(bytes, FontSettings::default()).expect("embedded Barlow font")
        };
        Fonts {
            regular: load(theme::FONT_REGULAR),
            semibold: load(theme::FONT_SEMIBOLD),
        }
    }
}

impl Default for Fonts {
    fn default() -> Self {
        Fonts::new()
    }
}

/// One run of text: font, size (physical px), colour.
struct Run<'a> {
    font: &'a Font,
    px: f32,
    text: &'a str,
    rgba: [u8; 4],
    /// Digits share one width, so the time does not wobble as it counts.
    tabular: bool,
}

impl Run<'_> {
    fn digit_width(&self) -> f32 {
        ('0'..='9')
            .map(|c| self.font.metrics(c, self.px).advance_width)
            .fold(0.0, f32::max)
    }

    fn width(&self) -> f32 {
        let digit = self.digit_width();
        self.text
            .chars()
            .map(|c| {
                if self.tabular && c.is_ascii_digit() {
                    digit
                } else {
                    self.font.metrics(c, self.px).advance_width
                }
            })
            .sum()
    }

    fn draw(&self, pixmap: &mut Pixmap, x: f32, baseline: f32, alpha: f32) {
        let digit = self.digit_width();
        let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
        let data = pixmap.data_mut();
        let mut pen = x;
        for c in self.text.chars() {
            let (metrics, bitmap) = self.font.rasterize(c, self.px);
            let advance = if self.tabular && c.is_ascii_digit() {
                digit
            } else {
                metrics.advance_width
            };
            let centre = (advance - metrics.advance_width) / 2.0;
            let gx = (pen + centre + metrics.xmin as f32).round() as i32;
            let gy = (baseline - (metrics.ymin + metrics.height as i32) as f32).round() as i32;
            for row in 0..metrics.height {
                let py = gy + row as i32;
                if !(0..height).contains(&py) {
                    continue;
                }
                for col in 0..metrics.width {
                    let px = gx + col as i32;
                    if !(0..width).contains(&px) {
                        continue;
                    }
                    let coverage = f32::from(bitmap[row * metrics.width + col]) / 255.0 * alpha;
                    blend(
                        data,
                        (py as usize * width as usize + px as usize) * 4,
                        self.rgba,
                        coverage,
                    );
                }
            }
            pen += advance;
        }
    }
}

/// Source-over of one straight-alpha colour at `coverage` onto a premultiplied pixel.
fn blend(data: &mut [u8], i: usize, rgba: [u8; 4], coverage: f32) {
    let a = (coverage * f32::from(rgba[3])).round().clamp(0.0, 255.0) as u32;
    if a == 0 {
        return;
    }
    let keep = 255 - a;
    for c in 0..3 {
        data[i + c] = (u32::from(rgba[c]) * a / 255 + u32::from(data[i + c]) * keep / 255) as u8;
    }
    data[i + 3] = (a + u32::from(data[i + 3]) * keep / 255) as u8;
}

// ---------------------------------------------------------------------------------------
// The strip's content (pure)
// ---------------------------------------------------------------------------------------

/// A part of the strip that answers the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Part {
    /// The dot and the time: drag to move the strip.
    Grip,
    /// The zoom badge: back to the whole screen.
    Zoom,
    Pen,
    Highlighter,
    Stop,
}

impl Part {
    pub const ALL: [Part; 5] = [
        Part::Grip,
        Part::Zoom,
        Part::Pen,
        Part::Highlighter,
        Part::Stop,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// The zoom badge's text, `None` at (about) the whole screen.
pub fn zoom_label(zoom: f64) -> Option<String> {
    (zoom.is_finite() && zoom >= BADGE_FROM).then(|| format!("{zoom:.1}x"))
}

/// What the strip shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PillModel {
    pub time: String,
    pub zoom: Option<String>,
    pub tool: Option<InkTool>,
}

pub fn pill_model(seconds: f64, zoom: f64, tool: Option<InkTool>) -> PillModel {
    PillModel {
        time: format_elapsed(seconds),
        zoom: zoom_label(zoom),
        tool,
    }
}

/// Where everything sits in the strip, in its window's physical pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct PillLayout {
    pub width: u32,
    pub height: u32,
    /// The parts that answer the mouse.
    pub parts: Vec<(Part, Strip)>,
    dot_x: f64,
    time_x: f64,
    separator_x: f64,
}

impl PillLayout {
    pub fn part(&self, part: Part) -> Option<Strip> {
        self.parts.iter().find(|(p, _)| *p == part).map(|(_, b)| *b)
    }
}

fn time_run<'a>(fonts: &'a Fonts, text: &'a str, scale: f64, rgba: [u8; 4]) -> Run<'a> {
    Run {
        font: &fonts.semibold,
        px: TIME_SIZE * scale as f32,
        text,
        rgba,
        tabular: true,
    }
}

fn badge_run<'a>(fonts: &'a Fonts, text: &'a str, scale: f64, rgba: [u8; 4]) -> Run<'a> {
    Run {
        font: &fonts.semibold,
        px: BADGE_SIZE * scale as f32,
        text,
        rgba,
        tabular: true,
    }
}

/// Lays the strip out at `scale`.
pub fn pill_layout(fonts: &Fonts, model: &PillModel, scale: f64) -> PillLayout {
    let px = |dip: f64| dip * scale;
    let height = px(PILL_HEIGHT).round();
    let mut parts = Vec::new();
    let mut x = px(PILL_PAD);
    let dot_x = x + px(PILL_DOT) / 2.0;
    x += px(PILL_DOT) + px(DOT_GAP);
    let time_x = x;
    x += f64::from(time_run(fonts, &model.time, scale, [0; 4]).width());
    let grip_end = x + px(PILL_GAP) / 2.0;
    parts.push((
        Part::Grip,
        Strip {
            x: 0,
            y: 0,
            w: grip_end.round() as i32,
            h: height as i32,
        },
    ));
    x += px(PILL_GAP);
    if let Some(zoom) = &model.zoom {
        let w = f64::from(badge_run(fonts, zoom, scale, [0; 4]).width()) + 2.0 * px(BADGE_PAD);
        let h = px(BADGE_HEIGHT);
        parts.push((
            Part::Zoom,
            Strip {
                x: x.round() as i32,
                y: ((height - h) / 2.0).round() as i32,
                w: w.round() as i32,
                h: h.round() as i32,
            },
        ));
        x += w + px(PILL_GAP);
    }
    x += px(SEPARATOR_BEFORE) - px(PILL_GAP);
    let separator_x = x.round();
    x = separator_x + 1.0 + px(SEPARATOR_AFTER);
    let (bw, bh) = (px(BUTTON.0), px(BUTTON.1));
    for (i, part) in [Part::Pen, Part::Highlighter, Part::Stop]
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            x += px(BUTTON_GAP);
        }
        parts.push((
            part,
            Strip {
                x: x.round() as i32,
                y: ((height - bh) / 2.0).round() as i32,
                w: bw.round() as i32,
                h: bh.round() as i32,
            },
        ));
        x += bw;
    }
    // The buttons carry their own padding: the edge after them is a little narrower.
    x += px(PILL_PAD) - px((PILL_HEIGHT - BUTTON.1) / 2.0);
    PillLayout {
        width: x.ceil().max(1.0) as u32,
        height: height.max(1.0) as u32,
        parts,
        dot_x,
        time_x,
        separator_x,
    }
}

/// The part under a point of the strip's window, if any.
pub fn hit(layout: &PillLayout, x: i32, y: i32) -> Option<Part> {
    layout
        .parts
        .iter()
        .find(|(_, b)| x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h)
        .map(|(p, _)| *p)
}

/// The strip's look beyond its content: hover highlights (0 to 1 per [`Part`]) and the part
/// pressed.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PillLook {
    pub hover: [f64; 5],
    pub pressed: Option<Part>,
}

/// The strip's palette.
struct Palette {
    plate: [u8; 4],
    line: [u8; 4],
    text: [u8; 4],
    muted: [u8; 4],
    chip: [u8; 3],
    chip_hover: [u8; 3],
    selected: [u8; 3],
    pressed: [u8; 3],
}

/// The red of the recording dot (the tool menu's Record screen).
const RECORD_RED: [u8; 3] = [0xe5, 0x48, 0x4d];

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            plate: [0x22, 0x27, 0x33, 250],
            line: [255, 255, 255, 40],
            text: [0xea, 0xf0, 0xfc, 255],
            muted: [0xaa, 0xb7, 0xcc, 255],
            chip: [0x31, 0x39, 0x48],
            chip_hover: [0x33, 0x38, 0x44],
            selected: [0x34, 0x4a, 0x6b],
            pressed: [0x47, 0x53, 0x68],
        }
    } else {
        Palette {
            plate: [0xff, 0xff, 0xff, 252],
            line: [theme::SLATE[0], theme::SLATE[1], theme::SLATE[2], 56],
            text: theme::TEXT,
            muted: theme::MUTED,
            chip: [0xe9, 0xee, 0xf6],
            chip_hover: [0xee, 0xf0, 0xf3],
            selected: [0xdc, 0xe9, 0xfc],
            pressed: [0xcd, 0xd8, 0xe8],
        }
    }
}

fn fill(pixmap: &mut Pixmap, path: &Path, rgb: [u8; 3], alpha: f32) {
    pixmap.fill_path(
        path,
        &paint(rgb, alpha),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn rgb(rgba: [u8; 4]) -> [u8; 3] {
    [rgba[0], rgba[1], rgba[2]]
}

/// The plate: a capsule with a hairline. The whole pixmap is the plate (its corners are
/// left transparent).
fn draw_plate(pixmap: &mut Pixmap, colors: &Palette, scale: f64) {
    let (w, h) = (f64::from(pixmap.width()), f64::from(pixmap.height()));
    if let Some(plate) = rounded(0.0, 0.0, w, h, h / 2.0) {
        fill(
            pixmap,
            &plate,
            rgb(colors.plate),
            f32::from(colors.plate[3]) / 255.0,
        );
    }
    let inset = 0.5 * scale;
    if let Some(border) = rounded(
        inset,
        inset,
        w - 2.0 * inset,
        h - 2.0 * inset,
        h / 2.0 - inset,
    ) {
        let stroke = Stroke {
            width: scale as f32,
            ..Stroke::default()
        };
        pixmap.stroke_path(
            &border,
            &paint(rgb(colors.line), f32::from(colors.line[3]) / 255.0),
            &stroke,
            Transform::identity(),
            None,
        );
    }
}

/// Draws an icon into the `ICON`-sized box at `(x, y)` (physical px).
fn draw_icon(pixmap: &mut Pixmap, part: Part, x: f64, y: f64, scale: f64, color: [u8; 3]) {
    let s = (ICON / 16.0 * scale) as f32;
    let at = Transform::from_row(s, 0.0, 0.0, s, x as f32, y as f32);
    let line = Stroke {
        width: 1.5,
        line_join: LineJoin::Round,
        line_cap: tiny_skia::LineCap::Round,
        ..Stroke::default()
    };
    let polygon = |points: &[(f32, f32)]| {
        let mut p = PathBuilder::new();
        p.move_to(points[0].0, points[0].1);
        for &(px, py) in &points[1..] {
            p.line_to(px, py);
        }
        p.close();
        p.finish()
    };
    match part {
        Part::Pen => {
            // A pencil, tip at the bottom left.
            if let Some(body) = polygon(&[
                (2.8, 13.2),
                (3.4, 10.4),
                (10.7, 3.1),
                (12.9, 5.3),
                (5.6, 12.6),
            ]) {
                pixmap.stroke_path(&body, &paint(color, 1.0), &line, at, None);
            }
            let mut p = PathBuilder::new();
            p.move_to(9.2, 4.6);
            p.line_to(11.4, 6.8);
            if let Some(band) = p.finish() {
                pixmap.stroke_path(&band, &paint(color, 1.0), &line, at, None);
            }
        }
        Part::Highlighter => {
            // A marker with a chisel nib, and a stroke of its yellow beside the nib.
            if let Some(body) = polygon(&[(5.0, 9.4), (10.6, 3.8), (13.4, 6.6), (7.8, 12.2)]) {
                pixmap.stroke_path(&body, &paint(color, 1.0), &line, at, None);
            }
            if let Some(nib) = polygon(&[(5.0, 9.4), (7.8, 12.2), (5.4, 13.6), (3.6, 11.8)]) {
                pixmap.fill_path(&nib, &paint(color, 1.0), FillRule::Winding, at, None);
            }
            if let Some(mark) = rounded(8.6, 13.0, 5.8, 2.0, 1.0) {
                pixmap.fill_path(
                    &mark,
                    &paint([0xf5, 0xc5, 0x18], 1.0),
                    FillRule::Winding,
                    at,
                    None,
                );
            }
        }
        Part::Stop => {
            if let Some(square) = rounded(4.5, 4.5, 7.0, 7.0, 1.5) {
                pixmap.fill_path(&square, &paint(color, 1.0), FillRule::Winding, at, None);
            }
        }
        Part::Grip | Part::Zoom => {}
    }
}

/// Renders the strip: plate, dot, time, badge, separator and buttons.
pub fn render_pill(
    fonts: &Fonts,
    model: &PillModel,
    layout: &PillLayout,
    look: &PillLook,
    scale: f64,
    dark: bool,
) -> Pixmap {
    let colors = palette(dark);
    let mut pixmap = Pixmap::new(layout.width, layout.height).expect("strip pixmap");
    draw_plate(&mut pixmap, &colors, scale);
    let mid = f64::from(layout.height) / 2.0;
    if let Some(dot) = PathBuilder::from_circle(
        layout.dot_x as f32,
        mid as f32,
        (PILL_DOT * scale / 2.0) as f32,
    ) {
        fill(&mut pixmap, &dot, RECORD_RED, 1.0);
    }
    // The digits' ink centred on the plate (ymin is the glyph's overshoot below the baseline).
    let baseline = |run: &Run| {
        let m = run.font.metrics('0', run.px);
        mid as f32 + m.height as f32 / 2.0 + m.ymin as f32
    };
    let time = time_run(fonts, &model.time, scale, colors.text);
    time.draw(
        &mut pixmap,
        layout.time_x as f32,
        baseline(&time).round(),
        1.0,
    );

    let chip_fill = |part: Part, selected: bool| -> ([u8; 3], f32) {
        if look.pressed == Some(part) {
            (colors.pressed, 1.0)
        } else if selected {
            let hover = look.hover[part.index()] as f32;
            (mix(colors.selected, colors.pressed, 0.35 * hover), 1.0)
        } else {
            let hover = look.hover[part.index()] as f32;
            (colors.chip_hover, hover)
        }
    };
    if let (Some(zoom), Some(b)) = (&model.zoom, layout.part(Part::Zoom)) {
        let hover = look.hover[Part::Zoom.index()] as f32;
        let base = if look.pressed == Some(Part::Zoom) {
            colors.pressed
        } else {
            mix(colors.chip, colors.pressed, 0.6 * hover)
        };
        if let Some(chip) = rounded(
            f64::from(b.x),
            f64::from(b.y),
            f64::from(b.w),
            f64::from(b.h),
            CHIP_RADIUS * scale,
        ) {
            fill(&mut pixmap, &chip, base, 1.0);
        }
        let run = badge_run(fonts, zoom, scale, colors.text);
        let x = f64::from(b.x) + (f64::from(b.w) - f64::from(run.width())) / 2.0;
        run.draw(&mut pixmap, x as f32, baseline(&run).round(), 1.0);
    }
    let sep_h = SEPARATOR_HEIGHT * scale;
    if let Some(sep) = tiny_skia::Rect::from_xywh(
        layout.separator_x as f32,
        (mid - sep_h / 2.0) as f32,
        scale.max(1.0).round() as f32,
        sep_h as f32,
    ) {
        pixmap.fill_rect(
            sep,
            &paint(rgb(colors.line), f32::from(colors.line[3]) / 255.0),
            Transform::identity(),
            None,
        );
    }
    for part in [Part::Pen, Part::Highlighter, Part::Stop] {
        let Some(b) = layout.part(part) else {
            continue;
        };
        let selected = match part {
            Part::Pen => model.tool == Some(InkTool::Pen),
            Part::Highlighter => model.tool == Some(InkTool::Highlighter),
            _ => false,
        };
        let (rgb_fill, alpha) = chip_fill(part, selected);
        if alpha > 0.0
            && let Some(back) = rounded(
                f64::from(b.x),
                f64::from(b.y),
                f64::from(b.w),
                f64::from(b.h),
                BUTTON_RADIUS * scale,
            )
        {
            fill(&mut pixmap, &back, rgb_fill, alpha);
        }
        let icon = ICON * scale;
        let x = (f64::from(b.x) + (f64::from(b.w) - icon) / 2.0).round();
        let y = (f64::from(b.y) + (f64::from(b.h) - icon) / 2.0).round();
        draw_icon(&mut pixmap, part, x, y, scale, rgb(colors.text));
    }
    pixmap
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

/// Where the strip's window goes: centred on the monitor plus the dragged `offset`, kept on
/// the monitor, [`PILL_TOP`] below the top of the work area (under a taskbar at the top).
pub fn pill_origin(monitor: &MonitorBounds, width: u32, offset: i32) -> (i32, i32) {
    let b = monitor.bounds;
    let centred = (b.x + (b.width - f64::from(width)) / 2.0).round() as i32;
    let left = b.x.round() as i32;
    let right = (b.x + b.width).round() as i32 - width as i32;
    let x = (centred + offset).clamp(left, right.max(left));
    let y = (monitor.work_area.y + PILL_TOP * monitor.scale).round() as i32;
    (x, y)
}

// ---------------------------------------------------------------------------------------
// The tip: tooltips and hints (pure)
// ---------------------------------------------------------------------------------------

/// What the tip under the strip says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tip {
    /// A hovered part's name and shortcut.
    Tooltip(Part),
    /// "Hold Alt and scroll to zoom", at the start of the first recordings.
    StartHint,
    /// The drawing keys, the first time a tool is picked up in a recording.
    ToolHint,
}

/// A run of the tip: words, or a key chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Key(String),
}

fn text(s: &str) -> Segment {
    Segment::Text(s.into())
}

/// One chip per key of a shortcut label ("Ctrl+Shift+7").
fn keys(label: &Option<String>) -> Vec<Segment> {
    label
        .iter()
        .flat_map(|l| l.split('+'))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| Segment::Key(k.into()))
        .collect()
}

fn modifier_keys(modifier: ZoomModifier) -> Vec<Segment> {
    match modifier {
        ZoomModifier::Alt => vec![Segment::Key("Alt".into())],
        ZoomModifier::CtrlAlt => vec![Segment::Key("Ctrl".into()), Segment::Key("Alt".into())],
    }
}

/// The tip's content.
pub fn tip_segments(tip: Tip, labels: &Labels, modifier: ZoomModifier) -> Vec<Segment> {
    let mut out = Vec::new();
    match tip {
        Tip::Tooltip(Part::Pen) => {
            out.push(text("Pen"));
            out.extend(keys(&labels.pen));
        }
        Tip::Tooltip(Part::Highlighter) => {
            out.push(text("Highlighter"));
            out.extend(keys(&labels.highlighter));
        }
        Tip::Tooltip(Part::Stop) => {
            out.push(text("Stop recording"));
            out.extend(keys(&labels.toggle));
        }
        Tip::Tooltip(Part::Zoom) => {
            out.push(text("Whole screen · tap"));
            out.extend(modifier_keys(modifier));
            out.push(text("twice"));
        }
        Tip::Tooltip(Part::Grip) => out.push(text("Drag to move")),
        Tip::StartHint => {
            out.push(text("Hold"));
            out.extend(modifier_keys(modifier));
            out.push(text("and scroll to zoom"));
        }
        Tip::ToolHint => {
            out.extend([
                Segment::Key("Ctrl".into()),
                Segment::Key("Z".into()),
                text("undo ·"),
            ]);
            out.extend([Segment::Key("Backspace".into()), text("clear ·")]);
            out.extend([Segment::Key("Esc".into()), text("done")]);
        }
    }
    out
}

/// Renders the tip: a small plate with words and key chips, with room for a soft shadow.
pub fn render_tip(fonts: &Fonts, segments: &[Segment], scale: f64, dark: bool) -> Pixmap {
    let colors = palette(dark);
    let px = |dip: f64| dip * scale;
    let word = |s: &'_ str| -> f64 {
        f64::from(
            Run {
                font: &fonts.regular,
                px: TIP_SIZE * scale as f32,
                text: s,
                rgba: [0; 4],
                tabular: false,
            }
            .width(),
        )
    };
    let key = |s: &'_ str| -> f64 {
        f64::from(
            Run {
                font: &fonts.semibold,
                px: KEY_SIZE * scale as f32,
                text: s,
                rgba: [0; 4],
                tabular: false,
            }
            .width(),
        ) + 2.0 * px(KEY_PAD)
    };
    let widths: Vec<f64> = segments
        .iter()
        .map(|s| match s {
            Segment::Text(t) => word(t),
            Segment::Key(k) => key(k),
        })
        .collect();
    let content = widths.iter().sum::<f64>() + px(SEGMENT_GAP) * (widths.len().max(1) - 1) as f64;
    let margin = px(TIP_MARGIN).ceil();
    let plate_w = (content + 2.0 * px(TIP_PAD)).ceil();
    let plate_h = px(TIP_HEIGHT).round();
    let mut pixmap = Pixmap::new(
        (plate_w + 2.0 * margin) as u32,
        (plate_h + 2.0 * margin) as u32,
    )
    .expect("tip pixmap");
    for i in (1..=3).rev() {
        let spread = f64::from(i) * scale;
        if let Some(path) = rounded(
            margin - spread,
            margin - spread + scale,
            plate_w + 2.0 * spread,
            plate_h + 2.0 * spread,
            px(8.0) + spread,
        ) {
            let shade = if dark { [0, 0, 0] } else { theme::SLATE };
            fill(&mut pixmap, &path, shade, if dark { 0.06 } else { 0.025 } + 0.012 * (3 - i) as f32);
        }
    }
    if let Some(plate) = rounded(margin, margin, plate_w, plate_h, px(8.0)) {
        fill(
            &mut pixmap,
            &plate,
            rgb(colors.plate),
            f32::from(colors.plate[3]) / 255.0,
        );
        let stroke = Stroke {
            width: scale as f32,
            ..Stroke::default()
        };
        if let Some(border) = rounded(
            margin + 0.5 * scale,
            margin + 0.5 * scale,
            plate_w - scale,
            plate_h - scale,
            px(8.0) - 0.5 * scale,
        ) {
            pixmap.stroke_path(
                &border,
                &paint(rgb(colors.line), f32::from(colors.line[3]) / 255.0),
                &stroke,
                Transform::identity(),
                None,
            );
        }
    }
    let mid = margin + plate_h / 2.0;
    let mut x = margin + px(TIP_PAD);
    for (segment, width) in segments.iter().zip(&widths) {
        match segment {
            Segment::Text(t) => {
                let run = Run {
                    font: &fonts.regular,
                    px: TIP_SIZE * scale as f32,
                    text: t,
                    rgba: colors.muted,
                    tabular: false,
                };
                let base = mid as f32 + run.font.metrics('x', run.px).height as f32 / 2.0;
                run.draw(&mut pixmap, x as f32, base.round(), 1.0);
            }
            Segment::Key(k) => {
                let h = px(KEY_HEIGHT);
                if let Some(chip) = rounded(x, mid - h / 2.0, *width, h, CHIP_RADIUS * scale) {
                    fill(&mut pixmap, &chip, colors.chip, 1.0);
                }
                let run = Run {
                    font: &fonts.semibold,
                    px: KEY_SIZE * scale as f32,
                    text: k,
                    rgba: colors.text,
                    tabular: false,
                };
                let cap = run.font.metrics('H', run.px).height as f32;
                run.draw(
                    &mut pixmap,
                    (x + px(KEY_PAD)) as f32,
                    (mid as f32 + cap / 2.0).round(),
                    1.0,
                );
            }
        }
        x += width + px(SEGMENT_GAP);
    }
    pixmap
}

/// Where the tip's window goes: under the strip, centred on `anchor_x` (the hovered part's
/// centre, or the strip's), kept on the monitor.
pub fn tip_origin(
    monitor: &MonitorBounds,
    pill: Strip,
    anchor_x: i32,
    size: (u32, u32),
) -> (i32, i32) {
    let margin = (TIP_MARGIN * monitor.scale).ceil() as i32;
    let b = monitor.bounds;
    let left = b.x.round() as i32;
    let right = (b.x + b.width).round() as i32 - size.0 as i32;
    let x = (anchor_x - size.0 as i32 / 2).clamp(left, right.max(left));
    let y = pill.y + pill.h + (TIP_GAP * monitor.scale).round() as i32 - margin;
    (x, y)
}

/// When each tip shows: a timetable driven by the pointer, the tool and the clock.
#[derive(Debug, Clone, PartialEq)]
pub struct TipSchedule {
    start_hint: Option<(f64, f64)>,
    tool_hint: Option<(f64, f64)>,
    tool_hint_used: bool,
    /// The part under the pointer and since when.
    hover: Option<(Part, f64)>,
    pressed: bool,
}

impl TipSchedule {
    /// `start`: when the strip appeared. `start_hint`: show the zoom hint this time.
    pub fn new(start: f64, start_hint: bool) -> TipSchedule {
        TipSchedule {
            start_hint: start_hint.then_some((
                start + START_HINT_DELAY,
                start + START_HINT_DELAY + START_HINT_TIME,
            )),
            tool_hint: None,
            tool_hint_used: false,
            hover: None,
            pressed: false,
        }
    }

    /// A tool was picked up: the first time in this recording, its keys show (and end the
    /// start hint, which has done its job by then).
    pub fn tool_picked(&mut self, now: f64) {
        if self.tool_hint_used {
            return;
        }
        self.tool_hint_used = true;
        self.tool_hint = Some((now, now + TOOL_HINT_TIME));
        if let Some((from, until)) = self.start_hint {
            self.start_hint = Some((from, until.min(now)));
        }
    }

    /// The part under the pointer changed.
    pub fn hover(&mut self, part: Option<Part>, now: f64) {
        if self.hover.map(|(p, _)| p) != part {
            self.hover = part.map(|p| (p, now));
        }
    }

    /// A button is held down: no tooltip meanwhile.
    pub fn pressed(&mut self, pressed: bool) {
        self.pressed = pressed;
    }

    /// The tip to show at `now`, if any. A tooltip wins over a hint, the tool hint over the
    /// start hint.
    pub fn current(&self, now: f64) -> Option<Tip> {
        let within = |window: Option<(f64, f64)>| window.is_some_and(|(a, b)| now >= a && now < b);
        if let Some((part, since)) = self.hover
            && !self.pressed
            && now >= since + TOOLTIP_DELAY
        {
            return Some(Tip::Tooltip(part));
        }
        if within(self.tool_hint) {
            return Some(Tip::ToolHint);
        }
        if within(self.start_hint) {
            return Some(Tip::StartHint);
        }
        None
    }

    /// The next time [`current`](Self::current) may change, after `now`.
    pub fn next_change(&self, now: f64) -> Option<f64> {
        let mut times = Vec::new();
        if let Some((_, since)) = self.hover
            && !self.pressed
        {
            times.push(since + TOOLTIP_DELAY);
        }
        for (a, b) in [self.tool_hint, self.start_hint].into_iter().flatten() {
            times.push(a);
            times.push(b);
        }
        times.into_iter().filter(|&t| t > now).reduce(f64::min)
    }
}

// ---------------------------------------------------------------------------------------
// Motion (pure)
// ---------------------------------------------------------------------------------------

/// Core Animation's `easeOut` (cubic Bézier 0, 0, 0.58, 1) at `t` in 0...1.
pub fn ease_out(t: f64) -> f64 {
    if t.is_nan() || t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    // x(s) = 3 (1 - s) s² 0.58 + s³ is increasing: bisect for x(s) = t.
    let x = |s: f64| 3.0 * (1.0 - s) * s * s * 0.58 + s * s * s;
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..30 {
        let mid = (lo + hi) / 2.0;
        if x(mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let s = (lo + hi) / 2.0;
    3.0 * (1.0 - s) * s * s + s * s * s
}

/// An opacity going from one value to another over a time, eased out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fade {
    from: f64,
    to: f64,
    start: f64,
    duration: f64,
}

impl Fade {
    pub fn at(value: f64) -> Fade {
        Fade {
            from: value,
            to: value,
            start: 0.0,
            duration: 0.0,
        }
    }

    /// From the current value at `now` to `to` over `duration` (0: at once).
    pub fn to(&mut self, to: f64, now: f64, duration: f64) {
        let from = self.value(now);
        *self = Fade {
            from,
            to,
            start: now,
            duration: if from == to { 0.0 } else { duration.max(0.0) },
        };
    }

    pub fn value(&self, now: f64) -> f64 {
        if self.duration <= 0.0 || now >= self.start + self.duration {
            return self.to;
        }
        let t = ((now - self.start) / self.duration).max(0.0);
        self.from + (self.to - self.from) * ease_out(t)
    }

    pub fn target(&self) -> f64 {
        self.to
    }

    /// Still changing at `now`.
    pub fn moving(&self, now: f64) -> bool {
        self.duration > 0.0 && now < self.start + self.duration
    }
}

/// Opacity as a layered window's constant alpha.
pub fn alpha_byte(opacity: f64) -> u8 {
    (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
}

// ---------------------------------------------------------------------------------------
// System settings
// ---------------------------------------------------------------------------------------

/// The system's app mode is dark.
pub fn system_dark() -> bool {
    let mut light: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: the value is restricted to a DWORD and both out buffers are locals.
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

/// False when the user turned off "Animation effects" (reduced motion).
pub fn animation_effects() -> bool {
    let mut enabled = windows::core::BOOL(0);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes exactly one BOOL into a local.
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

// ---------------------------------------------------------------------------------------
// The windows
// ---------------------------------------------------------------------------------------

/// One click-through layered window (an outline strip, the tip) with its surface.
struct Piece {
    window: Overlay,
    surface: LayeredSurface,
    /// Where it is and what was drawn into it (strip rect, outline rect relative to it).
    drawn: Option<(Strip, (i32, i32, i32, i32))>,
    shown: bool,
}

impl Piece {
    fn new(capturable: bool) -> windows::core::Result<Piece> {
        let mut window = Overlay::new()?;
        window.set_capturable(capturable)?;
        Ok(Piece {
            window,
            surface: LayeredSurface::new()?,
            drawn: None,
            shown: false,
        })
    }

    fn hide(&mut self) {
        if self.shown {
            self.window.hide();
            self.shown = false;
        }
    }

    fn show(&mut self) {
        if !self.shown {
            self.window.show();
            self.shown = true;
        }
    }

    /// Applies a constant alpha (0 hides).
    fn alpha(&mut self, alpha: u8) {
        let Some((at, _)) = self.drawn else {
            return;
        };
        if alpha == 0 {
            self.hide();
            return;
        }
        let _ = self
            .surface
            .set_alpha(self.window.hwnd(), (at.x, at.y), alpha);
        self.show();
    }
}

/// A mouse event on the strip, in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PillMouse {
    Move(i32, i32),
    Down(i32, i32),
    Up(i32, i32),
    Leave,
    /// The capture went elsewhere.
    Lost,
}

thread_local! {
    static PILL_QUEUE: RefCell<Vec<PillMouse>> = const { RefCell::new(Vec::new()) };
    /// The strip window's rectangle on screen: origin and size.
    static PILL_RECT: Cell<(i32, i32, i32, i32)> = const { Cell::new((0, 0, 0, 0)) };
    /// `TrackMouseEvent` asked for a `WM_MOUSELEAVE`.
    static PILL_TRACKING: Cell<bool> = const { Cell::new(false) };
}

const PILL_CLASS: PCWSTR = w!("IgorioRecordingStrip");

fn pill_push(event: PillMouse) {
    PILL_QUEUE.with(|queue| {
        if let Ok(mut queue) = queue.try_borrow_mut() {
            queue.push(event);
        }
    });
}

/// A client point of the strip in screen pixels.
fn pill_screen(l: LPARAM) -> (i32, i32) {
    let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    let (ox, oy, _, _) = PILL_RECT.get();
    (x + ox, y + oy)
}

fn button_down() -> bool {
    // SAFETY: plain value arguments.
    unsafe {
        GetAsyncKeyState(i32::from(VK_LBUTTON.0)) < 0
            || GetAsyncKeyState(i32::from(VK_RBUTTON.0)) < 0
    }
}

/// Touches only thread-locals, so it is safe to re-enter.
unsafe extern "system" fn pill_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        // Never activate: the recorded app keeps the foreground and the focus.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let (x, y) = pill_screen(l);
            // Ask for the leave only while the cursor is really over the strip (a move can
            // also be a posted message, and then the leave would come at once).
            let (ox, oy, ow, oh) = PILL_RECT.get();
            let mut cursor = windows::Win32::Foundation::POINT::default();
            // SAFETY: writes into a local.
            let over =
                unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor) }
                    .is_ok()
                    && cursor.x >= ox
                    && cursor.x < ox + ow
                    && cursor.y >= oy
                    && cursor.y < oy + oh;
            if over && !PILL_TRACKING.get() {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: `track` is a local naming our own window.
                if unsafe { TrackMouseEvent(&mut track) }.is_ok() {
                    PILL_TRACKING.set(true);
                }
            }
            pill_push(PillMouse::Move(x, y));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            PILL_TRACKING.set(false);
            pill_push(PillMouse::Leave);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Capture only a real press, so the release comes back here.
            if button_down() {
                // SAFETY: our own window, on this thread.
                unsafe { SetCapture(hwnd) };
            }
            pill_push(PillMouse::Down(pill_screen(l).0, pill_screen(l).1));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            pill_push(PillMouse::Up(pill_screen(l).0, pill_screen(l).1));
            // SAFETY: releases only our own capture.
            unsafe {
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            if l.0 != hwnd.0 as isize {
                pill_push(PillMouse::Lost);
            }
            LRESULT(0)
        }
        // SAFETY: the arguments come unchanged from the system.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// The strip's window: layered, topmost, never activated, excluded from capture, and the
/// only one here that takes the mouse.
struct PillWindow {
    hwnd: HWND,
    surface: LayeredSurface,
    shown: bool,
    at: Option<Strip>,
}

impl PillWindow {
    fn new(capturable: bool) -> windows::core::Result<PillWindow> {
        // SAFETY: a null module name returns the exe's own module.
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        // SAFETY: a system cursor, shared, never freed.
        let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(pill_proc),
            hInstance: instance,
            hCursor: arrow,
            lpszClassName: PILL_CLASS,
            ..Default::default()
        };
        // SAFETY: `class` points only to static data and a `'static` function.
        if unsafe { RegisterClassW(&class) } == 0 {
            let error = Error::from_thread();
            if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
                return Err(error);
            }
        }
        // SAFETY: the class is registered above; strings are static; no creation parameter.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                PILL_CLASS,
                w!("Igorio recording"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )?
        };
        let mut window = PillWindow {
            hwnd,
            surface: LayeredSurface::new()?,
            shown: false,
            at: None,
        };
        window.set_capturable(capturable);
        Ok(window)
    }

    fn set_capturable(&mut self, capturable: bool) {
        let affinity = if capturable {
            WDA_NONE
        } else {
            WDA_EXCLUDEFROMCAPTURE
        };
        // SAFETY: our own window, made on this thread.
        let _ = unsafe { SetWindowDisplayAffinity(self.hwnd, affinity) };
    }

    fn present(&mut self, pixmap: &Pixmap, origin: (i32, i32), alpha: u8) {
        let _ = self
            .surface
            .present(self.hwnd, pixmap, origin, None, false, alpha);
        let at = Strip {
            x: origin.0,
            y: origin.1,
            w: pixmap.width() as i32,
            h: pixmap.height() as i32,
        };
        self.at = Some(at);
        PILL_RECT.set((at.x, at.y, at.w, at.h));
    }

    fn alpha(&mut self, alpha: u8) {
        let Some(at) = self.at else {
            return;
        };
        if alpha == 0 {
            self.place(false);
            return;
        }
        let _ = self.surface.set_alpha(self.hwnd, (at.x, at.y), alpha);
        if !self.shown {
            self.place(true);
        }
    }

    fn place(&mut self, visible: bool) {
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        // SAFETY: our own window; `SWP_NOACTIVATE` keeps it from taking focus.
        unsafe {
            let _ = if visible {
                SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    flags | SWP_SHOWWINDOW,
                )
            } else {
                SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    flags | SWP_NOZORDER | SWP_HIDEWINDOW,
                )
            };
        }
        self.shown = visible;
    }
}

impl Drop for PillWindow {
    fn drop(&mut self) {
        PILL_RECT.set((0, 0, 0, 0));
        // SAFETY: made on this thread, destroyed once.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

/// What a click on the strip asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillAction {
    /// Pick the tool up, switch to it, or put it down (the shortcut's path).
    Tool(InkTool),
    /// Stop the recording.
    Stop,
    /// Back to the whole screen.
    ResetZoom,
}

/// The strip as of the last frame, for tests: its window's origin, its parts (screen px)
/// and the tip showing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PillSnapshot {
    pub origin: (i32, i32),
    pub parts: Vec<(Part, Strip)>,
    pub tip: Option<Tip>,
    pub shown: bool,
}

/// The outline, the strip and the tip. Belongs to the thread that made it, which pumps its
/// messages.
pub struct FrameWindows {
    strips: Vec<Piece>,
    tip: Piece,
    pill: PillWindow,
    fonts: Fonts,
    monitor: MonitorBounds,
    labels: Labels,
    modifier: ZoomModifier,
    area: Option<Rect>,
    zoom: f64,
    seconds: f64,
    tool: Option<InkTool>,
    model: PillModel,
    layout: PillLayout,
    hover_fades: [Fade; 5],
    hover: Option<Part>,
    pressed: Option<Part>,
    /// A drag of the grip: the cursor's x and the offset when it began.
    drag: Option<(i32, i32)>,
    offset: i32,
    schedule: TipSchedule,
    /// The tip asked for, and the one drawn (kept while it fades out).
    tip_target: Option<Tip>,
    tip_drawn: Option<Tip>,
    tip_fade: Fade,
    dark: bool,
    animate: bool,
    opacity: Fade,
    /// The frame's alpha last given to the windows.
    alpha: u8,
    tip_alpha: u8,
    pill_dirty: bool,
}

impl FrameWindows {
    /// Creates the windows, hidden. `capturable`: let screenshots see them (debug).
    /// `start_hint`: show the zoom hint after the strip appears at `now`.
    pub fn new(
        monitor: MonitorBounds,
        labels: Labels,
        modifier: ZoomModifier,
        capturable: bool,
        start_hint: bool,
        now: f64,
    ) -> windows::core::Result<FrameWindows> {
        let mut strips = Vec::with_capacity(4);
        for _ in 0..4 {
            strips.push(Piece::new(capturable)?);
        }
        let fonts = Fonts::new();
        let model = pill_model(0.0, 1.0, None);
        let layout = pill_layout(&fonts, &model, monitor.scale);
        Ok(FrameWindows {
            strips,
            tip: Piece::new(capturable)?,
            pill: PillWindow::new(capturable)?,
            fonts,
            monitor,
            labels,
            modifier,
            area: None,
            zoom: 1.0,
            seconds: 0.0,
            tool: None,
            model,
            layout,
            hover_fades: [Fade::at(0.0); 5],
            hover: None,
            pressed: None,
            drag: None,
            offset: 0,
            schedule: TipSchedule::new(now, start_hint),
            tip_target: None,
            tip_drawn: None,
            tip_fade: Fade::at(0.0),
            dark: system_dark(),
            animate: animation_effects(),
            opacity: Fade::at(0.0),
            alpha: 0,
            tip_alpha: 0,
            pill_dirty: true,
        })
    }

    /// The window handles: four strips, the strip with the buttons, the tip.
    pub fn hwnds(&self) -> Vec<HWND> {
        self.strips
            .iter()
            .map(|piece| piece.window.hwnd())
            .chain([self.pill.hwnd, self.tip.window.hwnd()])
            .collect()
    }

    /// Debug and the self-test's control run: let screenshots see the windows.
    pub fn set_capturable(&mut self, capturable: bool) {
        for piece in self.strips.iter_mut().chain(std::iter::once(&mut self.tip)) {
            let _ = piece.window.set_capturable(capturable);
        }
        self.pill.set_capturable(capturable);
    }

    pub fn appear(&mut self, now: f64) {
        self.dark = system_dark();
        self.animate = animation_effects();
        self.opacity
            .to(1.0, now, if self.animate { APPEAR } else { 0.0 });
        self.pill_dirty = true;
    }

    pub fn disappear(&mut self, now: f64) {
        self.opacity
            .to(0.0, now, if self.animate { DISAPPEAR } else { 0.0 });
    }

    /// Fully hidden and not fading.
    pub fn is_hidden(&self, now: f64) -> bool {
        self.opacity.target() == 0.0 && !self.opacity.moving(now)
    }

    /// The area being recorded (physical px). Draws the outline again if it changed, and
    /// the zoom badge if its text changed.
    pub fn set_area(&mut self, area: Rect) {
        if self.area == Some(area) {
            return;
        }
        self.area = Some(area);
        if area.width > 0.0 {
            self.zoom = self.monitor.bounds.width / area.width;
            self.update_model();
        }
        let scale = self.monitor.scale;
        let strips = outline_strips(area, self.monitor.bounds, scale);
        let outline = outline_rect(area, scale);
        let alpha = self.alpha;
        for (piece, strip) in self.strips.iter_mut().zip(strips) {
            let (Some(strip), Some(outline)) = (strip, outline) else {
                piece.hide();
                piece.drawn = None;
                continue;
            };
            let key = (
                outline.x - strip.x,
                outline.y - strip.y,
                outline.w,
                outline.h,
            );
            let hwnd = piece.window.hwnd();
            let same = piece.drawn.is_some_and(|(old, old_key)| {
                old.w == strip.w && old.h == strip.h && old_key == key
            });
            if same {
                // Only moved: the content is the same.
                let _ = piece.surface.set_alpha(hwnd, (strip.x, strip.y), alpha);
            } else if let Some(pixmap) = render_strip(strip, outline, scale) {
                let _ =
                    piece
                        .surface
                        .present(hwnd, &pixmap, (strip.x, strip.y), None, false, alpha);
            }
            piece.drawn = Some((strip, key));
            if alpha > 0 {
                piece.show();
            }
        }
    }

    /// The elapsed time and the tool in hand. The strip is drawn again only if what it shows
    /// changed. The first tool picked up shows its keys under the strip.
    pub fn set_pill(&mut self, seconds: f64, tool: Option<InkTool>, now: f64) {
        if tool.is_some() && tool != self.tool {
            self.schedule.tool_picked(now);
        }
        self.seconds = seconds;
        self.tool = tool;
        self.update_model();
    }

    fn update_model(&mut self) {
        let model = pill_model(self.seconds, self.zoom, self.tool);
        if model != self.model {
            self.model = model;
            self.pill_dirty = true;
        }
    }

    /// The strip's screen rectangle as last presented.
    fn pill_rect(&self) -> Strip {
        let origin = pill_origin(&self.monitor, self.layout.width, self.offset);
        Strip {
            x: origin.0,
            y: origin.1,
            w: self.layout.width as i32,
            h: self.layout.height as i32,
        }
    }

    fn part_at(&self, x: i32, y: i32) -> Option<Part> {
        let at = self.pill_rect();
        hit(&self.layout, x - at.x, y - at.y)
    }

    fn set_hover(&mut self, part: Option<Part>, now: f64) {
        if part == self.hover {
            return;
        }
        let duration = if self.animate { HOVER_FADE } else { 0.0 };
        if let Some(old) = self.hover {
            self.hover_fades[old.index()].to(0.0, now, duration);
        }
        if let Some(new) = part {
            self.hover_fades[new.index()].to(1.0, now, duration);
        }
        self.hover = part;
        self.schedule.hover(part, now);
        self.pill_dirty = true;
    }

    /// Handles the mouse on the strip since the last call. Returns what clicks asked for.
    /// Nothing is asked once the frame is going away.
    pub fn input(&mut self, now: f64) -> Vec<PillAction> {
        let events: Vec<PillMouse> = PILL_QUEUE.with(|queue| {
            queue
                .try_borrow_mut()
                .map(|mut queue| std::mem::take(&mut *queue))
                .unwrap_or_default()
        });
        let mut actions = Vec::new();
        let live = self.opacity.target() > 0.0;
        for event in events {
            match event {
                PillMouse::Move(x, _) if self.drag.is_some() => {
                    let (start_x, start_offset) = self.drag.unwrap_or_default();
                    let wanted = start_offset + (x - start_x);
                    let centred = pill_origin(&self.monitor, self.layout.width, 0).0;
                    let placed = pill_origin(&self.monitor, self.layout.width, wanted).0;
                    let offset = placed - centred;
                    if offset != self.offset {
                        self.offset = offset;
                        self.pill_dirty = true;
                    }
                }
                PillMouse::Move(x, y) => {
                    let part = self.part_at(x, y);
                    self.set_hover(part, now);
                }
                PillMouse::Down(x, y) => match self.part_at(x, y) {
                    Some(Part::Grip) => {
                        self.drag = Some((x, self.offset));
                        self.schedule.pressed(true);
                    }
                    Some(part) => {
                        self.pressed = Some(part);
                        self.schedule.pressed(true);
                        self.pill_dirty = true;
                    }
                    None => {}
                },
                PillMouse::Up(x, y) => {
                    self.drag = None;
                    self.schedule.pressed(false);
                    if let Some(part) = self.pressed.take() {
                        self.pill_dirty = true;
                        if live && self.part_at(x, y) == Some(part) {
                            actions.extend(match part {
                                Part::Pen => Some(PillAction::Tool(InkTool::Pen)),
                                Part::Highlighter => Some(PillAction::Tool(InkTool::Highlighter)),
                                Part::Stop => Some(PillAction::Stop),
                                Part::Zoom => Some(PillAction::ResetZoom),
                                Part::Grip => None,
                            });
                        }
                    }
                }
                PillMouse::Leave => {
                    if self.drag.is_none() && self.pressed.is_none() {
                        self.set_hover(None, now);
                    }
                }
                PillMouse::Lost => {
                    self.drag = None;
                    if self.pressed.take().is_some() {
                        self.pill_dirty = true;
                    }
                    self.schedule.pressed(false);
                }
            }
        }
        actions
    }

    /// When the tip may change next (the tooltip delay, a hint's start or end).
    pub fn next_deadline(&self, now: f64) -> Option<f64> {
        self.schedule.next_change(now)
    }

    /// Applies the fades and redraws what changed at `now`. Returns whether something still
    /// moves (the caller keeps the animation clock running only then).
    pub fn frame(&mut self, now: f64) -> bool {
        let opacity = self.opacity.value(now);
        let alpha = alpha_byte(opacity);
        let mut moving = self.opacity.moving(now);
        if alpha != self.alpha {
            self.alpha = alpha;
            for piece in &mut self.strips {
                piece.alpha(alpha);
            }
            if !self.pill_dirty {
                self.pill.alpha(alpha);
            }
        }
        // The strip.
        let hovering = self.hover_fades.iter().any(|f| f.moving(now));
        moving |= hovering;
        if (self.pill_dirty || hovering) && alpha > 0 {
            self.pill_dirty = false;
            let scale = self.monitor.scale;
            self.layout = pill_layout(&self.fonts, &self.model, scale);
            let mut look = PillLook {
                pressed: self.pressed,
                ..PillLook::default()
            };
            for part in Part::ALL {
                look.hover[part.index()] = self.hover_fades[part.index()].value(now);
            }
            let pixmap = render_pill(
                &self.fonts,
                &self.model,
                &self.layout,
                &look,
                scale,
                self.dark,
            );
            let origin = pill_origin(&self.monitor, self.layout.width, self.offset);
            self.pill.present(&pixmap, origin, alpha);
            if !self.pill.shown {
                self.pill.place(true);
            }
        } else if alpha == 0 && self.pill.shown {
            self.pill.place(false);
            self.set_hover(None, now);
        }
        // The tip.
        let wanted = if self.opacity.target() > 0.0 {
            self.schedule.current(now)
        } else {
            None
        };
        if wanted != self.tip_target {
            self.tip_target = wanted;
            match wanted {
                Some(tip) => {
                    self.draw_tip(tip);
                    let duration = if self.animate { TIP_FADE_IN } else { 0.0 };
                    self.tip_fade.to(1.0, now, duration);
                }
                None => {
                    let duration = if self.animate { TIP_FADE_OUT } else { 0.0 };
                    self.tip_fade.to(0.0, now, duration);
                }
            }
        }
        moving |= self.tip_fade.moving(now);
        let tip_alpha = alpha_byte(self.tip_fade.value(now) * opacity);
        if tip_alpha != self.tip_alpha {
            self.tip_alpha = tip_alpha;
            self.tip.alpha(tip_alpha);
            if tip_alpha == 0 {
                self.tip_drawn = None;
            }
        }
        moving
    }

    fn draw_tip(&mut self, tip: Tip) {
        let segments = tip_segments(tip, &self.labels, self.modifier);
        let scale = self.monitor.scale;
        let pixmap = render_tip(&self.fonts, &segments, scale, self.dark);
        let pill = self.pill_rect();
        let anchor = match tip {
            Tip::Tooltip(part) => self
                .layout
                .part(part)
                .map_or(pill.x + pill.w / 2, |b| pill.x + b.x + b.w / 2),
            _ => pill.x + pill.w / 2,
        };
        let origin = tip_origin(
            &self.monitor,
            pill,
            anchor,
            (pixmap.width(), pixmap.height()),
        );
        let hwnd = self.tip.window.hwnd();
        let _ = self
            .tip
            .surface
            .present(hwnd, &pixmap, origin, None, false, self.tip_alpha);
        self.tip.drawn = Some((
            Strip {
                x: origin.0,
                y: origin.1,
                w: pixmap.width() as i32,
                h: pixmap.height() as i32,
            },
            (0, 0, 0, 0),
        ));
        self.tip_drawn = Some(tip);
    }

    /// The strip as of the last frame.
    pub fn snapshot(&self) -> PillSnapshot {
        let at = self.pill.at.unwrap_or(self.pill_rect());
        PillSnapshot {
            origin: (at.x, at.y),
            parts: self
                .layout
                .parts
                .iter()
                .map(|(p, b)| {
                    (
                        *p,
                        Strip {
                            x: b.x + at.x,
                            y: b.y + at.y,
                            ..*b
                        },
                    )
                })
                .collect(),
            tip: if self.tip_alpha > 0 {
                self.tip_drawn
            } else {
                None
            },
            shown: self.pill.shown,
        }
    }

    /// Puts the windows back on top of the topmost band (the ink window came up, or another
    /// topmost window came above them). The strip ends on top.
    pub fn reassert(&mut self) {
        for piece in self.strips.iter_mut().chain(std::iter::once(&mut self.tip)) {
            piece.window.reassert();
        }
        if self.pill.shown {
            self.pill.place(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_is_minutes_and_two_digit_seconds() {
        assert_eq!(format_elapsed(0.0), "0:00");
        assert_eq!(format_elapsed(42.9), "0:42");
        assert_eq!(format_elapsed(60.0), "1:00");
        assert_eq!(format_elapsed(4523.0), "75:23");
        assert_eq!(format_elapsed(-3.0), "0:00");
        assert_eq!(format_elapsed(f64::NAN), "0:00");
    }

    #[test]
    fn the_zoom_badge_shows_only_when_zoomed_in() {
        assert_eq!(zoom_label(1.0), None);
        assert_eq!(zoom_label(1.04), None);
        assert_eq!(zoom_label(2.0).as_deref(), Some("2.0x"));
        assert_eq!(zoom_label(3.96).as_deref(), Some("4.0x"));
        assert_eq!(zoom_label(f64::NAN), None);
    }

    #[test]
    fn the_strip_shows_time_tool_and_badge_and_no_sentence() {
        let rest = pill_model(42.0, 1.0, None);
        assert_eq!(
            (rest.time.as_str(), rest.zoom.as_deref(), rest.tool),
            ("0:42", None, None)
        );
        let pen = pill_model(75.0, 1.0, Some(InkTool::Pen));
        assert_eq!(pen.tool, Some(InkTool::Pen));
        let zoomed = pill_model(5.0, 2.0, Some(InkTool::Highlighter));
        assert_eq!(zoomed.zoom.as_deref(), Some("2.0x"));
        let fonts = Fonts::new();
        let parts = |m: &PillModel| -> Vec<Part> {
            pill_layout(&fonts, m, 1.0)
                .parts
                .iter()
                .map(|(p, _)| *p)
                .collect()
        };
        assert_eq!(
            parts(&rest),
            vec![Part::Grip, Part::Pen, Part::Highlighter, Part::Stop]
        );
        assert_eq!(
            parts(&zoomed),
            vec![
                Part::Grip,
                Part::Zoom,
                Part::Pen,
                Part::Highlighter,
                Part::Stop
            ]
        );
    }

    #[test]
    fn the_strip_is_small_at_every_dpi() {
        let fonts = Fonts::new();
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for (model, limit) in [
                (pill_model(42.0, 1.0, None), PILL_MAX_WIDTH),
                (pill_model(3599.0, 1.0, Some(InkTool::Pen)), PILL_MAX_WIDTH),
                (pill_model(3599.0, 4.0, None), PILL_MAX_WIDTH + 50.0),
            ] {
                let layout = pill_layout(&fonts, &model, scale);
                let width = f64::from(layout.width) / scale;
                assert!(width <= limit, "{scale}: {width} DIP for {model:?}");
                assert_eq!(layout.height, (PILL_HEIGHT * scale).round() as u32);
                for (_, b) in &layout.parts {
                    assert!(
                        b.x >= 0 && b.x + b.w <= layout.width as i32,
                        "{scale}: {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn buttons_are_hit_at_their_centre_and_not_between_at_several_dpis() {
        let fonts = Fonts::new();
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let model = pill_model(42.0, 2.5, None);
            let layout = pill_layout(&fonts, &model, scale);
            for (part, b) in &layout.parts {
                assert_eq!(
                    hit(&layout, b.x + b.w / 2, b.y + b.h / 2),
                    Some(*part),
                    "{scale}"
                );
                // The edges belong to the part, one pixel out does not.
                assert_eq!(hit(&layout, b.x, b.y), Some(*part));
                assert_ne!(hit(&layout, b.x + b.w, b.y + b.h / 2), Some(*part));
            }
            // Between two buttons: nothing.
            let pen = layout.part(Part::Pen).unwrap();
            let gap_x = pen.x + pen.w;
            assert_eq!(hit(&layout, gap_x, pen.y + pen.h / 2), None, "{scale}");
            assert_eq!(hit(&layout, -1, 0), None);
            assert_eq!(hit(&layout, layout.width as i32, 0), None);
            // The dot is the grip.
            assert_eq!(
                hit(
                    &layout,
                    (PILL_PAD * scale) as i32 + 2,
                    layout.height as i32 / 2
                ),
                Some(Part::Grip)
            );
            // Buttons are at least 24 DIPs tall: easy to hit.
            assert!(f64::from(pen.h) >= 23.0 * scale);
        }
    }

    #[test]
    fn the_selected_tool_looks_selected() {
        let fonts = Fonts::new();
        let none = pill_model(1.0, 1.0, None);
        let pen = pill_model(1.0, 1.0, Some(InkTool::Pen));
        let layout = pill_layout(&fonts, &none, 1.0);
        let b = layout.part(Part::Pen).unwrap();
        let corner = |m: &PillModel| {
            let pixmap = render_pill(&fonts, m, &layout, &PillLook::default(), 1.0, false);
            pixmap.pixel((b.x + 3) as u32, (b.y + 3) as u32).unwrap()
        };
        let (plain, selected) = (corner(&none), corner(&pen));
        assert!(
            plain.red() == plain.blue() && plain.red() > 245,
            "{plain:?}"
        );
        assert_eq!(
            (selected.red(), selected.green(), selected.blue()),
            (0xdc, 0xe9, 0xfc)
        );
        // Hovered, not selected: the chip shade.
        let look = PillLook {
            hover: [0.0, 0.0, 1.0, 0.0, 0.0],
            pressed: None,
        };
        let hovered = render_pill(&fonts, &none, &layout, &look, 1.0, false)
            .pixel((b.x + 3) as u32, (b.y + 3) as u32)
            .unwrap();
        assert!(hovered.red() < 250 && hovered.blue() > hovered.red());
    }

    #[test]
    fn tooltips_name_the_action_and_its_keys() {
        let labels = Labels::default();
        let keys =
            |v: &[&str]| -> Vec<Segment> { v.iter().map(|k| Segment::Key((*k).into())).collect() };
        let mut pen = vec![Segment::Text("Pen".into())];
        pen.extend(keys(&["Ctrl", "Shift", "7"]));
        assert_eq!(
            tip_segments(Tip::Tooltip(Part::Pen), &labels, ZoomModifier::Alt),
            pen
        );
        let stop = tip_segments(Tip::Tooltip(Part::Stop), &labels, ZoomModifier::Alt);
        assert_eq!(stop[0], Segment::Text("Stop recording".into()));
        assert_eq!(stop.last(), Some(&Segment::Key("6".into())));
        // A cleared shortcut: only the name.
        let none = Labels {
            highlighter: None,
            ..Labels::default()
        };
        assert_eq!(
            tip_segments(Tip::Tooltip(Part::Highlighter), &none, ZoomModifier::Alt),
            vec![Segment::Text("Highlighter".into())]
        );
        // The start hint names the real modifier.
        let mut start = vec![Segment::Text("Hold".into())];
        start.extend(keys(&["Ctrl", "Alt"]));
        start.push(Segment::Text("and scroll to zoom".into()));
        assert_eq!(
            tip_segments(Tip::StartHint, &labels, ZoomModifier::CtrlAlt),
            start
        );
        let tool = tip_segments(Tip::ToolHint, &labels, ZoomModifier::Alt);
        assert!(
            tool.contains(&Segment::Key("Esc".into()))
                && tool.contains(&Segment::Key("Backspace".into()))
        );
    }

    #[test]
    fn the_start_hint_comes_once_after_a_beat_and_goes_by_itself() {
        let s = TipSchedule::new(10.0, true);
        assert_eq!(s.current(10.0), None);
        assert_eq!(s.next_change(10.0), Some(10.0 + START_HINT_DELAY));
        assert_eq!(s.current(10.0 + START_HINT_DELAY), Some(Tip::StartHint));
        let end = 10.0 + START_HINT_DELAY + START_HINT_TIME;
        assert_eq!(s.next_change(11.0), Some(end));
        assert_eq!(s.current(end), None);
        assert_eq!(s.next_change(end), None, "nothing more to wait for");
        // Off: never.
        let off = TipSchedule::new(10.0, false);
        assert!((0..100).all(|i| off.current(10.0 + f64::from(i) * 0.1).is_none()));
        assert_eq!(off.next_change(10.0), None);
    }

    #[test]
    fn the_tool_hint_shows_once_per_recording_and_ends_the_start_hint() {
        let mut s = TipSchedule::new(0.0, true);
        s.tool_picked(2.0);
        assert_eq!(s.current(2.0), Some(Tip::ToolHint));
        assert_eq!(s.current(2.0 + TOOL_HINT_TIME - 0.01), Some(Tip::ToolHint));
        assert_eq!(
            s.current(2.0 + TOOL_HINT_TIME),
            None,
            "the start hint is over too"
        );
        s.tool_picked(10.0);
        assert_eq!(s.current(10.0), None, "only once");
    }

    #[test]
    fn a_tooltip_waits_for_the_pointer_to_rest_and_never_shows_while_pressed() {
        let mut s = TipSchedule::new(0.0, false);
        s.hover(Some(Part::Pen), 5.0);
        assert_eq!(s.current(5.0 + TOOLTIP_DELAY - 0.01), None);
        assert_eq!(s.next_change(5.1), Some(5.0 + TOOLTIP_DELAY));
        assert_eq!(
            s.current(5.0 + TOOLTIP_DELAY),
            Some(Tip::Tooltip(Part::Pen))
        );
        // Moving within the same part keeps it; another part starts over.
        s.hover(Some(Part::Pen), 6.0);
        assert_eq!(s.current(6.0), Some(Tip::Tooltip(Part::Pen)));
        s.hover(Some(Part::Stop), 6.0);
        assert_eq!(s.current(6.1), None);
        s.pressed(true);
        assert_eq!(s.current(7.0), None);
        s.pressed(false);
        assert_eq!(s.current(7.0), Some(Tip::Tooltip(Part::Stop)));
        // A tooltip wins over a hint; leaving brings the hint back.
        s.tool_picked(7.0);
        assert_eq!(s.current(7.5), Some(Tip::Tooltip(Part::Stop)));
        s.hover(None, 7.5);
        assert_eq!(s.current(7.5), Some(Tip::ToolHint));
    }

    #[test]
    fn the_strip_sits_top_centre_and_drags_along_the_top_edge_only_on_its_monitor() {
        let monitor = MonitorBounds {
            bounds: Rect::new(0.0, 0.0, 2560.0, 1440.0),
            work_area: Rect::new(0.0, 48.0, 2560.0, 1392.0),
            scale: 1.25,
        };
        assert_eq!(pill_origin(&monitor, 200, 0), (1180, 58));
        assert_eq!(pill_origin(&monitor, 200, -500), (680, 58));
        assert_eq!(pill_origin(&monitor, 200, -5000), (0, 58));
        assert_eq!(pill_origin(&monitor, 200, 5000), (2360, 58));
        // A monitor left of the primary.
        let left = MonitorBounds {
            bounds: Rect::new(-1920.0, 0.0, 1920.0, 1080.0),
            work_area: Rect::new(-1920.0, 0.0, 1920.0, 1040.0),
            scale: 1.0,
        };
        assert_eq!(pill_origin(&left, 180, 0), (-1050, 8));
        // The tip goes under the strip, kept on the monitor.
        let pill = Strip {
            x: 1180,
            y: 58,
            w: 200,
            h: 40,
        };
        let (x, y) = tip_origin(&monitor, pill, 1280, (300, 50));
        assert_eq!((x, y), (1130, 58 + 40 + 8 - 8));
        assert_eq!(tip_origin(&monitor, pill, 10, (300, 50)).0, 0);
    }

    #[test]
    fn the_next_tick_is_the_next_whole_second() {
        assert!((until_next_second(3.25) - 0.75).abs() < 1e-9);
        assert!((until_next_second(3.0) - 1.0).abs() < 1e-9);
        assert_eq!(until_next_second(f64::NAN), 1.0);
    }

    fn covers(strips: &[Option<Strip>; 4], x: i32, y: i32) -> bool {
        strips
            .iter()
            .flatten()
            .any(|s| x >= s.x && x < s.x + s.w && y >= s.y && y < s.y + s.h)
    }

    #[test]
    fn strips_cover_the_line_and_never_the_middle_at_several_dpis() {
        let monitor = Rect::new(0.0, 0.0, 3840.0, 2160.0);
        for scale in [1.0, 1.25, 1.5, 2.0] {
            // A 2x zoom in the middle of the screen.
            let area = Rect::new(960.0, 540.0, 1920.0, 1080.0);
            let strips = outline_strips(area, monitor, scale);
            let outline = outline_rect(area, scale).unwrap();
            assert!(strips.iter().all(Option::is_some), "{scale}");
            // The line's middle of every side, and the corners, are covered.
            for (x, y) in [
                (outline.x + outline.w / 2, outline.y),
                (outline.x + outline.w / 2, outline.y + outline.h - 1),
                (outline.x, outline.y + outline.h / 2),
                (outline.x + outline.w - 1, outline.y + outline.h / 2),
                (outline.x, outline.y),
                (outline.x + outline.w - 1, outline.y + outline.h - 1),
            ] {
                assert!(covers(&strips, x, y), "{scale}: ({x}, {y}) not covered");
            }
            // The glow is covered too, and the middle of the area is not.
            let glow = (GLOW * scale) as i32;
            assert!(covers(&strips, outline.x - glow, outline.y + outline.h / 2));
            assert!(covers(&strips, outline.x + glow, outline.y + outline.h / 2));
            assert!(!covers(&strips, 1920, 1080));
            // The strips do not overlap.
            let s: Vec<Strip> = strips.iter().flatten().copied().collect();
            for (i, a) in s.iter().enumerate() {
                for b in &s[i + 1..] {
                    assert!(a.clip(*b).is_none(), "{scale}: {a:?} overlaps {b:?}");
                }
            }
            // Thin: all four together are a small part of the screen.
            let pixels: i64 = s.iter().map(|s| i64::from(s.w) * i64::from(s.h)).sum();
            assert!(pixels < 3840 * 2160 / 10, "{scale}: {pixels} px");
        }
    }

    #[test]
    fn the_outline_is_inset_and_rounded_to_pixels() {
        let area = Rect::new(100.4, 50.6, 800.0, 600.0);
        assert_eq!(
            outline_rect(area, 1.0),
            Some(Strip {
                x: 102,
                y: 53,
                w: 796,
                h: 596
            })
        );
        assert_eq!(
            outline_rect(area, 2.0),
            Some(Strip {
                x: 104,
                y: 55,
                w: 792,
                h: 592
            })
        );
        assert_eq!(outline_rect(Rect::new(0.0, 0.0, 0.0, 10.0), 1.0), None);
    }

    #[test]
    fn a_whole_screen_outline_stays_on_its_monitor() {
        // The second monitor, right of a 2560 px primary.
        let monitor = Rect::new(2560.0, 0.0, 1920.0, 1080.0);
        let strips = outline_strips(monitor, monitor, 1.0);
        for strip in strips.iter().flatten() {
            assert!(strip.x >= 2560 && strip.x + strip.w <= 4480, "{strip:?}");
            assert!(strip.y >= 0 && strip.y + strip.h <= 1080, "{strip:?}");
        }
        assert!(strips.iter().all(Option::is_some));
        // A monitor with negative coordinates (left of the primary), at 150%.
        let left = Rect::new(-2880.0, -200.0, 2880.0, 1620.0);
        let strips = outline_strips(left, left, 1.5);
        for strip in strips.iter().flatten() {
            assert!(strip.x >= -2880 && strip.x + strip.w <= 0, "{strip:?}");
        }
    }

    #[test]
    fn the_rendered_strip_has_the_line_where_the_geometry_says() {
        let area = Rect::new(0.0, 0.0, 400.0, 300.0);
        let monitor = Rect::new(-100.0, -100.0, 800.0, 600.0);
        let strips = outline_strips(area, monitor, 1.0);
        let outline = outline_rect(area, 1.0).unwrap();
        let top = strips[0].unwrap();
        let pixmap = render_strip(top, outline, 1.0).unwrap();
        let on_line = pixmap
            .pixel((200 - top.x) as u32, (outline.y - top.y) as u32)
            .unwrap();
        assert!(on_line.alpha() > 200, "{on_line:?}");
        // Blue.
        assert!(on_line.blue() > on_line.red());
        let far = pixmap
            .pixel((200 - top.x) as u32, (top.h - 1) as u32)
            .unwrap();
        assert_eq!(far.alpha(), 0);
    }

    #[test]
    fn fades_ease_out_and_end_on_time() {
        assert_eq!(ease_out(0.0), 0.0);
        assert!((ease_out(1.0) - 1.0).abs() < 1e-6);
        assert!(ease_out(0.5) > 0.5, "ease-out is ahead of linear");
        let mut fade = Fade::at(0.0);
        fade.to(1.0, 10.0, APPEAR);
        assert!(fade.moving(10.1));
        assert!(fade.value(10.1) > 0.5);
        assert!(!fade.moving(10.0 + APPEAR));
        assert_eq!(fade.value(10.0 + APPEAR), 1.0);
        // Reversing mid-way starts from where it is.
        fade.to(1.0, 0.0, APPEAR);
        fade.to(0.0, 0.1, DISAPPEAR);
        assert!(fade.value(0.1) > 0.5);
        assert_eq!(fade.value(0.1 + DISAPPEAR), 0.0);
        // Reduced motion: at once.
        fade.to(1.0, 5.0, 0.0);
        assert!(!fade.moving(5.0));
        assert_eq!(fade.value(5.0), 1.0);
        assert_eq!(alpha_byte(0.5), 128);
    }
}
