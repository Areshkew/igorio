//! What the user sees while recording, and the video never shows: a blue outline around the
//! area being recorded (it shrinks and slides as the user zooms) and a small pill at the top
//! of the screen with the elapsed time and the hint for the keys. From `RecordingFrame.swift`.
//!
//! # Windows
//!
//! Five small layered windows, each `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST |
//! WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` and `WDA_EXCLUDEFROMCAPTURE` (the overlay's window,
//! `overlay::Overlay`): four strips for the four sides of the outline and one for the pill.
//! The Swift panel covers the whole display; a layered window that size would hold a
//! monitor-sized bitmap (33 MB at 4K) that is almost all transparent, and every change of the
//! area while it eases would send all of it to the compositor again. The strips together are
//! a few hundred kilobytes, and a frame of the ease re-renders and sends only them. A strip
//! whose size did not change only moves.
//!
//! Strip layout: the top and bottom strips run the full width of the outline including its
//! rounded corners and glow; the left and right strips fill the height between them. All are
//! clipped to the recorded monitor, so the glow of an outline at the screen's edge never
//! reaches a second monitor.
//!
//! The windows are excluded from capture, so the outline never sits on the recorded pixels in
//! the video (`recording_ui_selftest` checks this with a screen capture, against a control run
//! with the exclusion off).
//!
//! # Look
//!
//! The outline is the accent blue (`theme::ACCENT`) at 90%, 2 DIPs wide with a 10 DIP corner,
//! inset by 1 DIP from the area, with a soft glow of the same blue that fades out over 6 DIPs
//! (`outline.shadowRadius` 6, opacity 0.45 of a blurred 2 pt line: about 0.16 at the line). The pill is a plate in this app's flat style
//! (as the summon pill, light or dark with the system's app mode): a blue dot, the elapsed
//! time `m:ss`, and the hint, `"Alt + wheel to zoom · Ctrl+Shift+7 pen · Ctrl+Shift+8
//! highlighter · Ctrl+Shift+6 to stop"`, or with a tool in hand its name and the keys that go
//! with it. The Swift dot breathes; here it is steady, because a breathing dot needs a clock
//! ticking all the time.
//!
//! # Time
//!
//! Nothing ticks except the pill's text, once a second while recording (the driver waits for
//! the next whole second of elapsed time). The outline moves only when the caller hands in a
//! new area. The appear and disappear fades (0.2 s and 0.15 s, ease-out) run the animation
//! clock only while they change; with reduced motion ("Animation effects" off) they are
//! instant.

use std::mem::size_of;

use fontdue::{Font, FontSettings};
use igorio_core::geom::Rect;
use igorio_core::ink::InkTool;
use igorio_kit::theme;
use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::w;

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

/// The pill's plate height and paddings, DIPs (`.padding(.horizontal, 14)`, `.vertical, 7`).
pub const PILL_HEIGHT: f64 = 30.0;
const PILL_PAD_X: f64 = 14.0;
const PILL_GAP: f64 = 8.0;
const PILL_DOT: f64 = 7.0;
/// Room around the plate for its shadow, DIPs.
const PILL_MARGIN: f64 = 8.0;
/// The plate's gap below the top of the work area, DIPs (`- 8`).
pub const PILL_TOP: f64 = 8.0;
/// Text sizes, DIPs (`Theme.font(12.5, .semibold)`, 12, 11.5).
const TIME_SIZE: f32 = 12.5;
const TITLE_SIZE: f32 = 12.0;
const HINT_SIZE: f32 = 11.5;

/// Fade in and out of the frame, seconds.
pub const APPEAR: f64 = 0.2;
pub const DISAPPEAR: f64 = 0.15;

// ---------------------------------------------------------------------------------------
// The pill's text (pure)
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

/// The zoom part of the hint, for the chosen gesture (`"⌥ scroll to zoom"`).
pub fn zoom_hint(modifier: ZoomModifier) -> String {
    format!("{} + wheel to zoom", modifier.label())
}

/// What the pill says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PillText {
    pub time: String,
    /// The tool in hand, if any.
    pub title: Option<&'static str>,
    pub hint: String,
}

/// The pill's text (`RecordingPill.body`).
pub fn pill_text(
    seconds: f64,
    tool: Option<InkTool>,
    labels: &Labels,
    modifier: ZoomModifier,
) -> PillText {
    let mut parts: Vec<String> = Vec::new();
    let title = match tool {
        None => {
            parts.push(zoom_hint(modifier));
            if let Some(pen) = &labels.pen {
                parts.push(format!("{pen} pen"));
            }
            if let Some(highlighter) = &labels.highlighter {
                parts.push(format!("{highlighter} highlighter"));
            }
            if let Some(toggle) = &labels.toggle {
                parts.push(format!("{toggle} to stop"));
            }
            None
        }
        Some(tool) => {
            let other = match tool {
                InkTool::Pen => labels
                    .highlighter
                    .as_ref()
                    .map(|l| format!("{l} highlighter")),
                InkTool::Highlighter => labels.pen.as_ref().map(|l| format!("{l} pen")),
            };
            parts.extend(other);
            parts.push("Ctrl+Z undo".into());
            parts.push("Backspace clear".into());
            parts.push("Esc done".into());
            Some(tool.as_str())
        }
    };
    PillText {
        time: format_elapsed(seconds),
        title,
        hint: parts.join(" · "),
    }
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

/// The pill's palette: plate, hairline, shadow, text, muted text.
struct PillColors {
    plate: [u8; 4],
    line: [u8; 4],
    shadow: [u8; 4],
    text: [u8; 4],
    muted: [u8; 4],
}

fn pill_colors(dark: bool) -> PillColors {
    if dark {
        PillColors {
            plate: [0x22, 0x27, 0x33, 240],
            line: [255, 255, 255, 40],
            shadow: [0, 0, 0, 90],
            text: [0xea, 0xf0, 0xfc, 255],
            muted: [0xaa, 0xb7, 0xcc, 255],
        }
    } else {
        PillColors {
            plate: [0xff, 0xff, 0xff, 245],
            line: [theme::SLATE[0], theme::SLATE[1], theme::SLATE[2], 64],
            shadow: [theme::SLATE[0], theme::SLATE[1], theme::SLATE[2], 44],
            text: theme::TEXT,
            muted: theme::MUTED,
        }
    }
}

/// Renders the pill (plate, shadow and text, with room for the shadow around it) at
/// `scale`. The plate is centred in the pixmap.
pub fn render_pill(fonts: &Fonts, text: &PillText, scale: f64, dark: bool) -> Pixmap {
    let s = scale as f32;
    let colors = pill_colors(dark);
    let time = Run {
        font: &fonts.semibold,
        px: TIME_SIZE * s,
        text: &text.time,
        rgba: colors.text,
        tabular: true,
    };
    let title = text.title.map(|title| Run {
        font: &fonts.semibold,
        px: TITLE_SIZE * s,
        text: title,
        rgba: colors.text,
        tabular: false,
    });
    let hint = Run {
        font: &fonts.regular,
        px: HINT_SIZE * s,
        text: &text.hint,
        rgba: colors.muted,
        tabular: false,
    };
    let gap = (PILL_GAP * scale) as f32;
    let dot = (PILL_DOT * scale) as f32;
    let mut content = dot + gap + time.width() + gap + hint.width();
    if let Some(title) = &title {
        content += title.width() + gap;
    }
    let plate_w = (f64::from(content) + 2.0 * PILL_PAD_X * scale).ceil();
    let plate_h = (PILL_HEIGHT * scale).round();
    let margin = (PILL_MARGIN * scale).ceil();
    let width = (plate_w + 2.0 * margin) as u32;
    let height = (plate_h + 2.0 * margin) as u32;
    let mut pixmap = Pixmap::new(width.max(1), height.max(1)).expect("pill pixmap");
    let (px, py) = (margin, margin);
    let radius = plate_h / 2.0;

    // A soft shadow: rings that grow and fade, a little below the plate.
    for i in (1..=4).rev() {
        let spread = f64::from(i) * 1.2 * scale;
        if let Some(path) = rounded(
            px - spread,
            py - spread + scale,
            plate_w + 2.0 * spread,
            plate_h + 2.0 * spread,
            radius + spread,
        ) {
            let a = f32::from(colors.shadow[3]) / 255.0 * (0.35 - 0.07 * i as f32);
            let rgb = [colors.shadow[0], colors.shadow[1], colors.shadow[2]];
            pixmap.fill_path(
                &path,
                &paint(rgb, a),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
    if let Some(plate) = rounded(px, py, plate_w, plate_h, radius) {
        let rgb = [colors.plate[0], colors.plate[1], colors.plate[2]];
        let a = f32::from(colors.plate[3]) / 255.0;
        pixmap.fill_path(
            &plate,
            &paint(rgb, a),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let inset = 0.5 * scale;
    if let Some(border) = rounded(
        px + inset,
        py + inset,
        plate_w - 2.0 * inset,
        plate_h - 2.0 * inset,
        radius - inset,
    ) {
        let rgb = [colors.line[0], colors.line[1], colors.line[2]];
        let stroke = Stroke {
            width: s,
            ..Stroke::default()
        };
        let a = f32::from(colors.line[3]) / 255.0;
        pixmap.stroke_path(
            &border,
            &paint(rgb, a),
            &stroke,
            Transform::identity(),
            None,
        );
    }

    // The content, centred vertically on the plate.
    let mid = (py + plate_h / 2.0) as f32;
    let mut x = (px + PILL_PAD_X * scale) as f32;
    let blue = [theme::ACCENT[0], theme::ACCENT[1], theme::ACCENT[2]];
    if let Some(circle) = PathBuilder::from_circle(x + dot / 2.0, mid, dot / 2.0) {
        pixmap.fill_path(
            &circle,
            &paint(blue, 1.0),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    x += dot + gap;
    // Baselines: the cap height of each run centred on the plate.
    let baseline = |run: &Run| {
        let cap = run.font.metrics('H', run.px).height as f32;
        mid + cap / 2.0
    };
    time.draw(&mut pixmap, x, baseline(&time), 1.0);
    x += time.width() + gap;
    if let Some(title) = &title {
        title.draw(&mut pixmap, x, baseline(title), 1.0);
        x += title.width() + gap;
    }
    hint.draw(&mut pixmap, x, baseline(&hint), 1.0);
    pixmap
}

/// Where the pill window goes: centred on the monitor, its plate [`PILL_TOP`] below the top
/// of the work area (under a taskbar docked at the top).
pub fn pill_origin(monitor: &MonitorBounds, size: (u32, u32)) -> (i32, i32) {
    let margin = (PILL_MARGIN * monitor.scale).ceil();
    let x = monitor.bounds.x + (monitor.bounds.width - f64::from(size.0)) / 2.0;
    let y = monitor.work_area.y + PILL_TOP * monitor.scale - margin;
    (x.round() as i32, y.round() as i32)
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

/// One layered window with its surface and what it shows.
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
}

/// The outline and the pill. Belongs to the thread that made it, which pumps its messages.
pub struct FrameWindows {
    strips: Vec<Piece>,
    pill: Piece,
    fonts: Fonts,
    monitor: MonitorBounds,
    labels: Labels,
    modifier: ZoomModifier,
    area: Option<Rect>,
    pill_text: Option<PillText>,
    pill_size: (u32, u32),
    dark: bool,
    opacity: Fade,
    /// The alpha last given to the windows.
    alpha: u8,
}

impl FrameWindows {
    /// Creates the five windows, hidden. `capturable`: let screenshots see them (debug).
    pub fn new(
        monitor: MonitorBounds,
        labels: Labels,
        modifier: ZoomModifier,
        capturable: bool,
    ) -> windows::core::Result<FrameWindows> {
        let mut strips = Vec::with_capacity(4);
        for _ in 0..4 {
            strips.push(Piece::new(capturable)?);
        }
        Ok(FrameWindows {
            strips,
            pill: Piece::new(capturable)?,
            fonts: Fonts::new(),
            monitor,
            labels,
            modifier,
            area: None,
            pill_text: None,
            pill_size: (0, 0),
            dark: system_dark(),
            opacity: Fade::at(0.0),
            alpha: 0,
        })
    }

    /// The window handles: four strips, then the pill.
    pub fn hwnds(&self) -> Vec<HWND> {
        self.strips
            .iter()
            .chain(std::iter::once(&self.pill))
            .map(|piece| piece.window.hwnd())
            .collect()
    }

    /// Debug and the self-test's control run: let screenshots see the windows.
    pub fn set_capturable(&mut self, capturable: bool) {
        for piece in self
            .strips
            .iter_mut()
            .chain(std::iter::once(&mut self.pill))
        {
            let _ = piece.window.set_capturable(capturable);
        }
    }

    /// Fades in (or out) from the current opacity.
    pub fn appear(&mut self, now: f64) {
        self.dark = system_dark();
        let duration = if animation_effects() { APPEAR } else { 0.0 };
        self.opacity.to(1.0, now, duration);
    }

    pub fn disappear(&mut self, now: f64) {
        let duration = if animation_effects() { DISAPPEAR } else { 0.0 };
        self.opacity.to(0.0, now, duration);
    }

    /// Fully hidden and not fading.
    pub fn is_hidden(&self, now: f64) -> bool {
        self.opacity.target() == 0.0 && !self.opacity.moving(now)
    }

    /// The area being recorded (physical px). Draws the outline again if it changed.
    pub fn set_area(&mut self, area: Rect) {
        if self.area == Some(area) {
            return;
        }
        self.area = Some(area);
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
            if alpha > 0 && !piece.shown {
                piece.window.show();
                piece.shown = true;
            }
        }
    }

    /// The pill's time and tool. Draws it again only if its text changed.
    pub fn set_pill(&mut self, seconds: f64, tool: Option<InkTool>) {
        let text = pill_text(seconds, tool, &self.labels, self.modifier);
        if self.pill_text.as_ref() == Some(&text) {
            return;
        }
        let pixmap = render_pill(&self.fonts, &text, self.monitor.scale, self.dark);
        self.pill_size = (pixmap.width(), pixmap.height());
        let origin = pill_origin(&self.monitor, self.pill_size);
        let hwnd = self.pill.window.hwnd();
        let _ = self
            .pill
            .surface
            .present(hwnd, &pixmap, origin, None, false, self.alpha);
        self.pill.drawn = Some((
            Strip {
                x: origin.0,
                y: origin.1,
                w: self.pill_size.0 as i32,
                h: self.pill_size.1 as i32,
            },
            (0, 0, 0, 0),
        ));
        self.pill_text = Some(text);
        if self.alpha > 0 && !self.pill.shown {
            self.pill.window.show();
            self.pill.shown = true;
        }
    }

    /// Applies the fade at `now`. Returns whether it still changes (the caller keeps the
    /// animation clock running only then).
    pub fn frame(&mut self, now: f64) -> bool {
        let alpha = alpha_byte(self.opacity.value(now));
        if alpha != self.alpha {
            self.alpha = alpha;
            for piece in self
                .strips
                .iter_mut()
                .chain(std::iter::once(&mut self.pill))
            {
                let Some((at, _)) = piece.drawn else {
                    continue;
                };
                if alpha == 0 {
                    piece.hide();
                    continue;
                }
                let _ = piece
                    .surface
                    .set_alpha(piece.window.hwnd(), (at.x, at.y), alpha);
                if !piece.shown {
                    piece.window.show();
                    piece.shown = true;
                }
            }
        }
        self.opacity.moving(now)
    }

    /// Puts the windows back on top of the topmost band (another topmost window may have come
    /// above them).
    pub fn reassert(&mut self) {
        for piece in self
            .strips
            .iter_mut()
            .chain(std::iter::once(&mut self.pill))
        {
            piece.window.reassert();
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
    fn the_pill_says_the_gesture_and_the_shortcuts() {
        let labels = Labels::default();
        let idle = pill_text(42.0, None, &labels, ZoomModifier::Alt);
        assert_eq!(idle.time, "0:42");
        assert_eq!(idle.title, None);
        assert_eq!(
            idle.hint,
            "Alt + wheel to zoom · Ctrl+Shift+7 pen · Ctrl+Shift+8 highlighter · Ctrl+Shift+6 to stop"
        );
        let pen = pill_text(61.0, Some(InkTool::Pen), &labels, ZoomModifier::CtrlAlt);
        assert_eq!(pen.title, Some("pen"));
        assert_eq!(
            pen.hint,
            "Ctrl+Shift+8 highlighter · Ctrl+Z undo · Backspace clear · Esc done"
        );
        let highlighter = pill_text(0.0, Some(InkTool::Highlighter), &labels, ZoomModifier::Alt);
        assert!(highlighter.hint.starts_with("Ctrl+Shift+7 pen · "));
        assert_eq!(zoom_hint(ZoomModifier::CtrlAlt), "Ctrl+Alt + wheel to zoom");
        // A cleared shortcut is left out.
        let none = Labels {
            pen: None,
            ..Labels::default()
        };
        let text = pill_text(0.0, None, &none, ZoomModifier::Alt);
        assert!(!text.hint.contains("pen ·") && text.hint.contains("highlighter"));
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

    #[test]
    fn the_pill_is_centred_under_the_top_of_the_work_area() {
        let monitor = MonitorBounds {
            bounds: Rect::new(0.0, 0.0, 2560.0, 1440.0),
            work_area: Rect::new(0.0, 48.0, 2560.0, 1392.0),
            scale: 1.25,
        };
        let (x, y) = pill_origin(&monitor, (600, 58));
        assert_eq!(x, 980);
        assert_eq!(y, 48 + 10 - 10);
        let fonts = Fonts::new();
        let text = pill_text(5.0, None, &Labels::default(), ZoomModifier::Alt);
        let small = render_pill(&fonts, &text, 1.0, false);
        let large = render_pill(&fonts, &text, 2.0, true);
        assert!(large.width() > small.width() * 19 / 10 && large.width() < small.width() * 21 / 10);
        assert_eq!(small.height(), (PILL_HEIGHT + 2.0 * PILL_MARGIN) as u32);
    }
}
