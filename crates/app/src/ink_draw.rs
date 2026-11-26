//! Software renderer for the pen and the highlighter: turns the pure stroke geometry of
//! `igorio_core::ink` into premultiplied pixels, the way `InkOverlay.swift` draws it with
//! Core Animation layers. Everything is in physical pixels; points arrive in logical pixels.
//!
//! One layer per stroke is what makes the Swift drawing look the way it does: a highlighter
//! fill is a single non-zero-winding path, so a stroke crossing itself does not darken, while
//! two highlighter *strokes* are two layers and therefore do darken where they cross (alpha
//! `0.38` over `0.38`, about `0.62`). The same holds here: every stroke is filled once.
//!
//! Finished strokes live in a committed layer, rendered at full opacity. A normal frame
//! copies only the damage rectangle of that layer and paints the current stroke on top, so the
//! cost follows the current stroke, not the number of finished ones. Undo, clear, a resize, a
//! scale change or a new highlighter stroke (which must stay under every pen stroke) rebuild
//! the layer.

use igorio_core::freehand::PathCmd;
use igorio_core::geom::Point;
use igorio_core::ink::{Ink, InkTool, Stroke as InkStroke};
use tiny_skia::{
    Color, FillRule, Mask, Paint, Path, PathBuilder, Pixmap, Shader, Stroke as SkiaStroke,
    Transform,
};

/// `InkStyle.penColor`: calibrated hue 212/360, saturation 0.72, brightness 0.88, alpha 1.
const PEN_COLOR: [u8; 3] = [63, 138, 224];
/// `InkStyle.highlighterColor`: hue 52/360, saturation 0.75, brightness 1, alpha 0.38.
const HIGHLIGHTER_COLOR: [u8; 3] = [255, 230, 64];
const HIGHLIGHTER_ALPHA: f64 = 0.38;
/// `layer(for:)` pen shadow: black at 0.18, radius 1.5, offset (0, 0.5) points.
const SHADOW_ALPHA: f64 = 0.18;
const SHADOW_RADIUS: f64 = 1.5;
const SHADOW_OFFSET_Y: f64 = 0.5;
/// `InkStyle.penWidth` / `highlighterWidth`, in points.
const PEN_WIDTH: f64 = 5.0;
const HIGHLIGHTER_WIDTH: f64 = 22.0;

/// A tiny-skia path plus its (physical) bounding box.
struct Shape {
    path: Path,
    bounds: Bounds,
}

/// An axis-aligned box in physical pixels; `f64` because it is derived from `f64` geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Bounds {
    fn empty() -> Self {
        Self {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
        }
    }

    fn add(&mut self, x: f64, y: f64) {
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    fn is_empty(&self) -> bool {
        !(self.max_x >= self.min_x && self.max_y >= self.min_y)
    }

    fn union(self, other: Bounds) -> Bounds {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Bounds {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    fn expand(self, by: f64) -> Bounds {
        Bounds {
            min_x: self.min_x - by,
            min_y: self.min_y - by,
            max_x: self.max_x + by,
            max_y: self.max_y + by,
        }
    }

    /// Rounds out to whole pixels and clips to the pixmap.
    fn clip(self, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
        if self.is_empty() {
            return None;
        }
        let left = self.min_x.floor().max(0.0);
        let top = self.min_y.floor().max(0.0);
        let right = self.max_x.ceil().min(width as f64);
        let bottom = self.max_y.ceil().min(height as f64);
        if right <= left || bottom <= top {
            return None;
        }
        Some((
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        ))
    }
}

/// What identifies a stroke for change detection; comparing point *counts* plus the two ends
/// is enough because `Ink` only ever appends points or drops the stroke.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StrokeSig {
    tool: InkTool,
    points: usize,
    first: (u64, u64),
    last: (u64, u64),
}

fn signature(stroke: &InkStroke) -> StrokeSig {
    let points = stroke.points();
    let first = points.first().copied().unwrap_or(Point::ZERO);
    let last = points.last().copied().unwrap_or(Point::ZERO);
    StrokeSig {
        tool: stroke.tool(),
        points: points.len(),
        first: (first.x.to_bits(), first.y.to_bits()),
        last: (last.x.to_bits(), last.y.to_bits()),
    }
}

/// Draws pen and highlighter strokes into a premultiplied RGBA pixmap.
pub struct InkRenderer {
    /// Every finished stroke, flattened at full opacity (highlighter strokes below pen ones).
    committed: Pixmap,
    committed_width: u32,
    committed_height: u32,
    committed_scale: f64,
    committed_finished: usize,
    committed_last: Option<StrokeSig>,
    /// Whether `committed` matches the last finished set it was built from.
    has_layer: bool,
    /// Whether the output pixmap may hold any ink (decides damage when clearing).
    has_content: bool,
    /// The current stroke as of the last frame.
    current: Option<StrokeSig>,
    /// The current stroke's damage bounds as of the last frame.
    current_bounds: Option<Bounds>,
    opacity: f64,
    damage: Option<(u32, u32, u32, u32)>,
    /// Identity (data pointer, byte length) of the pixmap last painted, so a different surface
    /// is filled completely instead of trusting the previous frame's pixels.
    surface: Option<(usize, usize)>,
    /// Scratch alpha mask and blur buffer, both sized like the output pixmap.
    shadow: Mask,
    blur: Vec<u8>,
    /// Running column sums for the vertical box-blur pass, reused every frame.
    column_sums: Vec<u32>,
}

impl InkRenderer {
    pub fn new() -> Self {
        Self {
            committed: Pixmap::new(1, 1).expect("1x1 ink layer"),
            committed_width: 0,
            committed_height: 0,
            committed_scale: 0.0,
            committed_finished: 0,
            committed_last: None,
            has_layer: false,
            has_content: false,
            current: None,
            current_bounds: None,
            opacity: -1.0,
            damage: None,
            surface: None,
            shadow: Mask::new(1, 1).expect("1x1 ink mask"),
            blur: Vec::new(),
            column_sums: Vec::new(),
        }
    }

    /// Paints all strokes of `ink` into `pixmap`, which covers `pixmap.width()` by
    /// `pixmap.height()` physical pixels at `scale` physical pixels per logical pixel.
    /// Stroke points are logical, relative to the pixmap's top-left. `opacity` fades all ink.
    pub fn render(&mut self, ink: &Ink, scale: f64, opacity: f64, pixmap: &mut Pixmap) {
        self.damage = None;
        let width = pixmap.width();
        let height = pixmap.height();
        if width == 0 || height == 0 {
            return;
        }
        let opacity = if opacity.is_nan() {
            0.0
        } else {
            opacity.clamp(0.0, 1.0)
        };

        // No meaningful scale: nothing can be drawn.
        if !scale.is_finite() || scale <= 0.0 {
            self.clear_output(pixmap);
            return;
        }

        let finished = ink.strokes();
        let current = ink.current();
        if (finished.is_empty() && current.is_none()) || opacity <= 0.0 {
            self.clear_output(pixmap);
            self.current = None;
            self.current_bounds = None;
            self.opacity = opacity;
            return;
        }

        self.ensure_scratch(width, height);

        let surface = (pixmap.data().as_ptr() as usize, pixmap.data().len());
        let surface_changed = self.surface != Some(surface);
        let current_sig = current.map(signature);
        let last_finished = finished.last().map(signature);
        let layer_ok = self.has_layer
            && self.committed_width == width
            && self.committed_height == height
            && self.committed_scale.to_bits() == scale.to_bits();

        let margin = damage_margin(scale);
        let mut full = self.opacity.to_bits() != opacity.to_bits() || surface_changed;
        let mut layer_event = false;
        let mut damage_bounds: Option<Bounds> = None;

        if !layer_ok
            || self.committed_finished != finished.len()
            || self.committed_last != last_finished
        {
            // The common end-of-stroke case: the previous current stroke is now finished and
            // only needs to be added on top. A highlighter would have to go below the pens,
            // so that case (and undo/clear/resize) rebuilds instead.
            let append_pen = layer_ok
                && self.committed_finished + 1 == finished.len()
                && self.current.is_some()
                && self.current == last_finished
                && last_finished.map(|sig| sig.tool) == Some(InkTool::Pen);
            if append_pen {
                let stroke = finished.last().expect("the stroke that just finished");
                damage_bounds = self.current_bounds;
                if let Some(shape) = build_shape(stroke, true, scale) {
                    paint(
                        &mut self.committed,
                        &shape,
                        InkTool::Pen,
                        scale,
                        1.0,
                        &mut self.shadow,
                        &mut self.blur,
                        &mut self.column_sums,
                        true,
                    );
                    let bounds = shape.bounds.expand(margin);
                    damage_bounds = Some(match damage_bounds {
                        Some(old) => old.union(bounds),
                        None => bounds,
                    });
                    self.has_content = true;
                }
                self.committed_finished = finished.len();
                self.committed_last = last_finished;
            } else {
                self.rebuild(finished, scale, width, height);
                full = true;
            }
            layer_event = true;
            self.current = None;
            self.current_bounds = None;
        }

        if !full && current_sig == self.current && !layer_event {
            // Nothing visible changed.
            return;
        }

        // The current stroke: its whole shape is repainted, never just the new segment, so the
        // outline stays correct when the last points ease in.
        let current_shape = current.and_then(|stroke| build_shape(stroke, false, scale));
        if let Some(shape) = &current_shape {
            damage_bounds = Some(match damage_bounds {
                Some(bounds) => bounds.union(shape.bounds.expand(margin)),
                None => shape.bounds.expand(margin),
            });
        }
        if !full && let Some(old) = self.current_bounds {
            damage_bounds = Some(match damage_bounds {
                Some(bounds) => bounds.union(old),
                None => old,
            });
        }

        let region = if full {
            Some((0, 0, width, height))
        } else {
            damage_bounds.and_then(|bounds| bounds.clip(width, height))
        };
        let Some(rect) = region else {
            self.current = current_sig;
            self.current_bounds = current_shape
                .as_ref()
                .map(|shape| shape.bounds.expand(margin));
            self.opacity = opacity;
            self.surface = Some(surface);
            return;
        };

        let faded = opacity < 1.0;
        let units = alpha_units(opacity);
        match current {
            // The highlighter's container is below the pen one, so the current highlighter is
            // painted first and the finished layer is composited over it.
            Some(stroke) if stroke.tool() == InkTool::Highlighter => {
                if let Some(shape) = &current_shape {
                    clear_region(pixmap, rect);
                    paint(
                        pixmap,
                        shape,
                        InkTool::Highlighter,
                        scale,
                        opacity,
                        &mut self.shadow,
                        &mut self.blur,
                        &mut self.column_sums,
                        false,
                    );
                    composite_layer_over(&self.committed, pixmap, rect, units);
                } else {
                    copy_region(&self.committed, pixmap, rect);
                    if faded {
                        fade_region(pixmap, rect, units);
                    }
                }
            }
            _ => {
                copy_region(&self.committed, pixmap, rect);
                if faded {
                    fade_region(pixmap, rect, units);
                }
                if let (Some(shape), Some(stroke)) = (&current_shape, current) {
                    paint(
                        pixmap,
                        shape,
                        stroke.tool(),
                        scale,
                        opacity,
                        &mut self.shadow,
                        &mut self.blur,
                        &mut self.column_sums,
                        false,
                    );
                }
            }
        }

        self.damage = Some(rect);
        self.has_content = self.has_content || current_shape.is_some();
        self.current = current_sig;
        self.current_bounds = current_shape
            .as_ref()
            .map(|shape| shape.bounds.expand(margin));
        self.opacity = opacity;
        self.surface = Some(surface);
    }

    /// The rectangle that changed since the last `render`: `(x, y, width, height)` in physical
    /// pixels, clipped to the pixmap. `None` when nothing changed.
    pub fn damage(&self) -> Option<(u32, u32, u32, u32)> {
        self.damage
    }

    /// Clears the visible pixmap; the caller gets full damage when ink used to be there.
    fn clear_output(&mut self, pixmap: &mut Pixmap) {
        let had = self.has_content;
        pixmap.fill(Color::TRANSPARENT);
        self.has_layer = false;
        self.committed_finished = 0;
        self.committed_last = None;
        self.current = None;
        self.current_bounds = None;
        self.has_content = false;
        self.surface = Some((pixmap.data().as_ptr() as usize, pixmap.data().len()));
        if had {
            self.damage = Some((0, 0, pixmap.width(), pixmap.height()));
        }
    }

    fn ensure_scratch(&mut self, width: u32, height: u32) {
        if (self.shadow.width() != width || self.shadow.height() != height)
            && let Some(mask) = Mask::new(width, height)
        {
            self.shadow = mask;
        }
        let len = width as usize * height as usize;
        if self.blur.len() != len {
            self.blur.resize(len, 0);
        }
        if self.column_sums.len() < width as usize {
            self.column_sums.resize(width as usize, 0);
        }
    }

    fn rebuild(&mut self, strokes: &[InkStroke], scale: f64, width: u32, height: u32) {
        if self.committed.width() != width || self.committed.height() != height {
            self.committed = Pixmap::new(width, height).expect("ink layer");
        }
        self.committed.fill(Color::TRANSPARENT);
        let mut has_content = false;
        // Highlights sit under lines, as the two container layers do in InkOverlay.swift.
        for tool in [InkTool::Highlighter, InkTool::Pen] {
            for stroke in strokes.iter().filter(|stroke| stroke.tool() == tool) {
                if let Some(shape) = build_shape(stroke, true, scale) {
                    paint(
                        &mut self.committed,
                        &shape,
                        tool,
                        scale,
                        1.0,
                        &mut self.shadow,
                        &mut self.blur,
                        &mut self.column_sums,
                        true,
                    );
                    has_content = true;
                }
            }
        }
        self.committed_width = width;
        self.committed_height = height;
        self.committed_scale = scale;
        self.committed_finished = strokes.len();
        self.committed_last = strokes.last().map(signature);
        self.has_layer = true;
        self.has_content = has_content;
        self.current = None;
        self.current_bounds = None;
    }
}

impl Default for InkRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the filled outline of a stroke in physical pixels.
fn build_shape(stroke: &InkStroke, complete: bool, scale: f64) -> Option<Shape> {
    let cmds = Ink::shape(stroke, complete);
    if cmds.is_empty() {
        return None;
    }
    let mut builder = PathBuilder::new();
    let mut bounds = Bounds::empty();
    for cmd in cmds {
        match cmd {
            PathCmd::MoveTo(p) => {
                let (x, y) = physical(p, scale)?;
                builder.move_to(x, y);
                bounds.add(x as f64, y as f64);
            }
            PathCmd::LineTo(p) => {
                let (x, y) = physical(p, scale)?;
                builder.line_to(x, y);
                bounds.add(x as f64, y as f64);
            }
            PathCmd::QuadTo { ctrl, to } => {
                let (cx, cy) = physical(ctrl, scale)?;
                let (x, y) = physical(to, scale)?;
                builder.quad_to(cx, cy, x, y);
                bounds.add(cx as f64, cy as f64);
                bounds.add(x as f64, y as f64);
            }
            PathCmd::Close => builder.close(),
        }
    }
    if bounds.is_empty() {
        return None;
    }
    Some(Shape {
        path: builder.finish()?,
        bounds,
    })
}

/// A point in physical pixels, rejected when it is not representable.
fn physical(point: Point, scale: f64) -> Option<(f32, f32)> {
    let x = point.x * scale;
    let y = point.y * scale;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    // Keep f32 conversions finite; such points are far outside any pixmap anyway.
    Some((x.clamp(-1.0e7, 1.0e7) as f32, y.clamp(-1.0e7, 1.0e7) as f32))
}

/// Fills a shape (and, for the pen, its shadow) with the tool's colour.
#[allow(clippy::too_many_arguments)]
fn paint(
    pixmap: &mut Pixmap,
    shape: &Shape,
    tool: InkTool,
    scale: f64,
    opacity: f64,
    shadow: &mut Mask,
    blur: &mut [u8],
    sums: &mut [u32],
    blurred_shadow: bool,
) {
    if opacity <= 0.0 || shape.bounds.is_empty() {
        return;
    }
    if tool == InkTool::Pen {
        if blurred_shadow {
            paint_shadow(pixmap, shape, scale, opacity, shadow, blur, sums);
        } else {
            paint_flat_shadow(pixmap, shape, scale, opacity);
        }
    }
    let (rgb, base_alpha) = match tool {
        InkTool::Pen => (PEN_COLOR, 1.0),
        InkTool::Highlighter => (HIGHLIGHTER_COLOR, HIGHLIGHTER_ALPHA),
    };
    let alpha = color_alpha(base_alpha, opacity);
    if alpha == 0 {
        return;
    }
    let paint = Paint {
        shader: Shader::SolidColor(Color::from_rgba8(rgb[0], rgb[1], rgb[2], alpha)),
        anti_alias: true,
        ..Paint::default()
    };
    // `CAShapeLayer.fillRule` is `.nonZero`, which tiny-skia calls `Winding`.
    pixmap.fill_path(
        &shape.path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// The pen shadow for a stroke that is still being drawn: the same 18% black, offset by the
/// same half point, but without the blur. Blurring the silhouette of a long live stroke on
/// every mouse move is what would blow the frame budget; the finished stroke gets the blurred
/// shadow when it is committed.
fn paint_flat_shadow(pixmap: &mut Pixmap, shape: &Shape, scale: f64, opacity: f64) {
    let alpha = color_alpha(SHADOW_ALPHA, opacity);
    if alpha == 0 {
        return;
    }
    let paint = Paint {
        shader: Shader::SolidColor(Color::from_rgba8(0, 0, 0, alpha)),
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_path(
        &shape.path,
        &paint,
        FillRule::Winding,
        Transform::from_translate(0.0, (SHADOW_OFFSET_Y * scale) as f32),
        None,
    );
}

/// `layer.shadowColor` black, `shadowOpacity` 0.18, `shadowRadius` 1.5, `shadowOffset` (0, 0.5).
fn paint_shadow(
    pixmap: &mut Pixmap,
    shape: &Shape,
    scale: f64,
    opacity: f64,
    mask: &mut Mask,
    blur: &mut [u8],
    sums: &mut [u32],
) {
    if mask.width() != pixmap.width() || mask.height() != pixmap.height() {
        return;
    }
    let strength = (SHADOW_ALPHA * opacity * 255.0).round() as u32;
    if strength == 0 {
        return;
    }
    let radius = ((SHADOW_RADIUS * scale) * 0.5).round().max(1.0) as usize;
    let support = (3 * radius) as f64 + 1.0;
    let offset = SHADOW_OFFSET_Y * scale;
    let shadow_bounds = Bounds {
        min_x: shape.bounds.min_x - support,
        min_y: shape.bounds.min_y - support + offset,
        max_x: shape.bounds.max_x + support,
        max_y: shape.bounds.max_y + support + offset,
    };
    let Some((x, y, w, h)) = shadow_bounds.clip(pixmap.width(), pixmap.height()) else {
        return;
    };

    // A stroke can leave the pixmap; keep the mask operation inside the pixmap too.
    let width = pixmap.width() as usize;
    {
        let data = mask.data_mut();
        for row in y as usize..(y + h) as usize {
            let start = row * width + x as usize;
            data[start..start + w as usize].fill(0);
        }
    }
    mask.fill_path(
        &shape.path,
        FillRule::Winding,
        true,
        Transform::from_translate(0.0, offset as f32),
    );
    blur_region(
        mask, blur, sums, width, x as usize, y as usize, w as usize, h as usize, radius,
    );
    blend_shadow(pixmap, mask, (x, y, w, h), strength);
}

/// Three box-blur passes over one rectangle of a full-pixmap mask.
///
/// The rectangle was chosen with a `3 * radius + 1` margin around the ink, so its outer
/// `radius` pixels are known zero: the passes can skip them and avoid clamping the window at
/// the edges.
#[allow(clippy::too_many_arguments)]
fn blur_region(
    mask: &mut Mask,
    temp: &mut [u8],
    sums: &mut [u32],
    width: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    radius: usize,
) {
    if w <= 2 * radius || h <= 2 * radius || temp.len() < width * mask.height() as usize {
        return;
    }
    let denominator = (2 * radius + 1) as u32;
    // Division by the window size dominates a naive box blur; a reciprocal multiply is exact
    // enough for a 0..=255 alpha and much cheaper.
    let reciprocal = ((1u64 << 24) + u64::from(denominator) / 2) / u64::from(denominator);
    let left = x + radius;
    let right = x + w - radius;
    let top = y + radius;
    let bottom = y + h - radius;
    let span = right - left;
    if sums.len() < span {
        return;
    }
    let sums = &mut sums[..span];
    for _ in 0..3 {
        {
            let src = mask.data();
            for row in top..bottom {
                let base = row * width;
                let mut sum = 0u32;
                for column in left - radius..=left + radius {
                    sum += src[base + column] as u32;
                }
                for column in left..right {
                    temp[base + column] = box_value(sum, reciprocal);
                    if column + 1 < right {
                        sum += src[base + column + radius + 1] as u32;
                        sum -= src[base + column - radius] as u32;
                    }
                }
            }
            // The rows the horizontal pass skipped are zero in the mask (and so is their
            // horizontal blur); make the scratch match before the vertical pass reads them.
            for row in (y..top).chain(bottom..y + h) {
                let base = row * width;
                temp[base + left..base + right].fill(0);
            }
        }
        {
            // Row-major sliding window: each scratch row is read once, instead of walking
            // columns with the pixmap's full stride.
            sums.fill(0);
            for row in top - radius..=top + radius {
                let base = row * width;
                for (index, sum) in sums.iter_mut().enumerate() {
                    *sum += temp[base + left + index] as u32;
                }
            }
            let dst = mask.data_mut();
            for row in top..bottom {
                let base = row * width;
                for (index, sum) in sums.iter().enumerate() {
                    dst[base + left + index] = box_value(*sum, reciprocal);
                }
                if row + 1 < bottom {
                    let add = (row + radius + 1) * width;
                    let remove = (row - radius) * width;
                    for (index, sum) in sums.iter_mut().enumerate() {
                        *sum += temp[add + left + index] as u32;
                        *sum -= temp[remove + left + index] as u32;
                    }
                }
            }
        }
    }
}

#[inline]
fn box_value(sum: u32, reciprocal: u64) -> u8 {
    ((u64::from(sum) * reciprocal) >> 24).min(255) as u8
}

/// Composites a black shadow with per-pixel alpha over the pixmap.
fn blend_shadow(pixmap: &mut Pixmap, mask: &Mask, rect: (u32, u32, u32, u32), strength: u32) {
    let (x, y, w, h) = rect;
    let width = pixmap.width() as usize;
    let stride = width * 4;
    let mask_data = mask.data();
    let data = pixmap.data_mut();
    for row in 0..h as usize {
        let mask_row = (y as usize + row) * width + x as usize;
        let data_row = (y as usize + row) * stride + x as usize * 4;
        for column in 0..w as usize {
            let alpha = mask_data[mask_row + column] as u32 * strength / 255;
            if alpha == 0 {
                continue;
            }
            let keep = 255 - alpha;
            let i = data_row + column * 4;
            let below_alpha = data[i + 3] as u32;
            let out_alpha = alpha + (below_alpha * keep + 127) / 255;
            let red = (data[i] as u32 * keep + 127) / 255;
            let green = (data[i + 1] as u32 * keep + 127) / 255;
            let blue = (data[i + 2] as u32 * keep + 127) / 255;
            data[i] = red.min(out_alpha) as u8;
            data[i + 1] = green.min(out_alpha) as u8;
            data[i + 2] = blue.min(out_alpha) as u8;
            data[i + 3] = out_alpha as u8;
        }
    }
}

fn copy_region(src: &Pixmap, dst: &mut Pixmap, rect: (u32, u32, u32, u32)) {
    let (x, y, w, h) = rect;
    let src_stride = src.width() as usize * 4;
    let dst_stride = dst.width() as usize * 4;
    let start = x as usize * 4;
    let len = w as usize * 4;
    let src_data = src.data();
    let dst_data = dst.data_mut();
    for row in 0..h as usize {
        let src_index = (y as usize + row) * src_stride + start;
        let dst_index = (y as usize + row) * dst_stride + start;
        dst_data[dst_index..dst_index + len].copy_from_slice(&src_data[src_index..src_index + len]);
    }
}

/// Writes the finished layer (faded by `units`) over the current highlighter in the region.
fn composite_layer_over(src: &Pixmap, dst: &mut Pixmap, rect: (u32, u32, u32, u32), units: u32) {
    let (x, y, w, h) = rect;
    let width = dst.width() as usize;
    let stride = width * 4;
    let src_data = src.data();
    let dst_data = dst.data_mut();
    let units = units.min(256);
    for row in 0..h as usize {
        let row_index = (y as usize + row) * stride;
        for column in 0..w as usize {
            let i = row_index + (x as usize + column) * 4;
            let alpha = (src_data[i + 3] as u32 * units + 128) >> 8;
            if alpha == 0 {
                continue;
            }
            let red = ((src_data[i] as u32 * units + 128) >> 8).min(alpha);
            let green = ((src_data[i + 1] as u32 * units + 128) >> 8).min(alpha);
            let blue = ((src_data[i + 2] as u32 * units + 128) >> 8).min(alpha);
            let keep = 255 - alpha;
            let below_alpha = dst_data[i + 3] as u32;
            let out_alpha = alpha + (below_alpha * keep + 127) / 255;
            let out_red = red + (dst_data[i] as u32 * keep + 127) / 255;
            let out_green = green + (dst_data[i + 1] as u32 * keep + 127) / 255;
            let out_blue = blue + (dst_data[i + 2] as u32 * keep + 127) / 255;
            dst_data[i] = out_red.min(out_alpha) as u8;
            dst_data[i + 1] = out_green.min(out_alpha) as u8;
            dst_data[i + 2] = out_blue.min(out_alpha) as u8;
            dst_data[i + 3] = out_alpha as u8;
        }
    }
}

fn clear_region(pixmap: &mut Pixmap, rect: (u32, u32, u32, u32)) {
    let (x, y, w, h) = rect;
    let stride = pixmap.width() as usize * 4;
    let start = x as usize * 4;
    let len = w as usize * 4;
    let data = pixmap.data_mut();
    for row in 0..h as usize {
        let index = (y as usize + row) * stride + start;
        data[index..index + len].fill(0);
    }
}

/// Multiplies the region's premultiplied colour and alpha by `units / 256`.
fn fade_region(pixmap: &mut Pixmap, rect: (u32, u32, u32, u32), units: u32) {
    let units = units.min(256);
    if units >= 256 {
        return;
    }
    let (x, y, w, h) = rect;
    let stride = pixmap.width() as usize * 4;
    let data = pixmap.data_mut();
    for row in 0..h as usize {
        let row_index = (y as usize + row) * stride;
        for column in 0..w as usize {
            let i = row_index + (x as usize + column) * 4;
            let alpha = (data[i + 3] as u32 * units + 128) >> 8;
            let red = ((data[i] as u32 * units + 128) >> 8).min(alpha);
            let green = ((data[i + 1] as u32 * units + 128) >> 8).min(alpha);
            let blue = ((data[i + 2] as u32 * units + 128) >> 8).min(alpha);
            data[i] = red as u8;
            data[i + 1] = green as u8;
            data[i + 2] = blue as u8;
            data[i + 3] = alpha as u8;
        }
    }
}

fn alpha_units(opacity: f64) -> u32 {
    (opacity.clamp(0.0, 1.0) * 256.0).round() as u32
}

fn color_alpha(base: f64, opacity: f64) -> u8 {
    (base * opacity).clamp(0.0, 1.0).mul_add(255.0, 0.5) as u8
}

/// Enough margin for anti-aliasing and the pen's blurred shadow.
fn damage_margin(scale: f64) -> f64 {
    let radius = ((SHADOW_RADIUS * scale) * 0.5).round().max(1.0);
    2.0 + 3.0 * radius + SHADOW_OFFSET_Y * scale
}

/// A small image of the tool to use as the mouse cursor, with its hot spot, drawn as
/// `InkStyle.cursor(for:)` does. Premultiplied RGBA; the hot spot is the image centre.
///
/// `scale` is the display's physical pixels per point. `CursorScale.enlargement` has no
/// Windows equivalent and is taken as 1, so the nib keeps the ink's width.
pub fn tool_cursor(tool: InkTool, scale: f64) -> (Pixmap, (u32, u32)) {
    let s = if scale.is_finite() && scale > 0.0 {
        scale.min(64.0)
    } else {
        1.0
    };
    // `nib` in points: the pen's dot is just wider than the line, the highlighter a flat nib.
    let (nib_width, nib_height) = match tool {
        InkTool::Pen => (PEN_WIDTH + 4.0, PEN_WIDTH + 4.0),
        InkTool::Highlighter => (8.0, HIGHLIGHTER_WIDTH),
    };
    let body_width = (nib_width.max(4.0) * s).max(1.0);
    let body_height = (nib_height.max(4.0) * s).max(1.0);
    let pad = 2.0 * s;
    let width = (body_width + 2.0 * pad).ceil().max(1.0) as u32;
    let height = (body_height + 2.0 * pad).ceil().max(1.0) as u32;
    let mut pixmap = Pixmap::new(width, height).expect("cursor pixmap");

    let body = (
        pad as f32,
        pad as f32,
        (width as f64 - 2.0 * pad).max(1.0) as f32,
        (height as f64 - 2.0 * pad).max(1.0) as f32,
    );
    let path = match tool {
        InkTool::Pen => PathBuilder::from_circle(
            body.0 + body.2 / 2.0,
            body.1 + body.3 / 2.0,
            body.2.min(body.3) / 2.0,
        ),
        InkTool::Highlighter => rounded_rect(body.0, body.1, body.2, body.3, (2.0 * s) as f32),
    };
    let Some(path) = path else {
        return (pixmap, (width / 2, height / 2));
    };

    let (fill, fill_alpha) = match tool {
        InkTool::Pen => (PEN_COLOR, 1.0),
        // `highlighterColor.withAlphaComponent(0.85)`.
        InkTool::Highlighter => (HIGHLIGHTER_COLOR, 0.85),
    };
    let fill_paint = Paint {
        shader: Shader::SolidColor(Color::from_rgba8(
            fill[0],
            fill[1],
            fill[2],
            color_alpha(fill_alpha, 1.0),
        )),
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_path(
        &path,
        &fill_paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    // A white rim, so the tip shows on dark and light alike.
    let rim = Paint {
        shader: Shader::SolidColor(Color::from_rgba8(255, 255, 255, color_alpha(0.95, 1.0))),
        anti_alias: true,
        ..Paint::default()
    };
    let rim_stroke = SkiaStroke {
        width: (1.5 * s) as f32,
        ..SkiaStroke::default()
    };
    pixmap.stroke_path(&path, &rim, &rim_stroke, Transform::identity(), None);

    (pixmap, (width / 2, height / 2))
}

fn rounded_rect(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<Path> {
    let rect = tiny_skia::Rect::from_xywh(x, y, width, height)?;
    let r = radius.clamp(0.0, width.min(height) / 2.0);
    let k = r * 0.552_284_8;
    let (left, top, right, bottom) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let mut builder = PathBuilder::new();
    builder.move_to(left + r, top);
    builder.line_to(right - r, top);
    builder.cubic_to(right - r + k, top, right, top + r - k, right, top + r);
    builder.line_to(right, bottom - r);
    builder.cubic_to(
        right,
        bottom - r + k,
        right - r + k,
        bottom,
        right - r,
        bottom,
    );
    builder.line_to(left + r, bottom);
    builder.cubic_to(left + r - k, bottom, left, bottom - r + k, left, bottom - r);
    builder.line_to(left, top + r);
    builder.cubic_to(left, top + r - k, left + r - k, top, left + r, top);
    builder.close();
    builder.finish()
}
