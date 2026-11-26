//! Software renderer for the overlay panel. Ports `FairyView.swift` to tiny-skia:
//! a tilted pile of screenshots with a hairline, a soft slate shadow and an optional
//! hint, plus the recording badge. Everything is drawn in physical pixels.

use std::collections::HashMap;
use std::sync::Arc;

use fontdue::{Font, FontSettings};
use igorio_kit::{Image, theme};
use tiny_skia::{
    Color, FillRule, FilterQuality, Mask, Paint, Path, PathBuilder, Pattern, Pixmap, PixmapPaint,
    Rect, Shader, SpreadMode, Stroke, Transform,
};

use crate::layout;
use crate::scene::{Leaving, Scene, SceneCard};

/// How many reduced copies of source images are kept between frames.
const CACHE_LIMIT: usize = 16;

struct CachedImage {
    /// Kept alive so `Arc::as_ptr` cannot be reused by another image while cached.
    source: Arc<Image>,
    reduced: Arc<Pixmap>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ShadowKey {
    width: u32,
    height: u32,
    radius: u32,
    tilt: u32,
    scale: u32,
    top: bool,
}

struct ShadowSprite {
    pixmap: Arc<Pixmap>,
    origin_x: f32,
    origin_y: f32,
}

pub struct Renderer {
    medium: Font,
    semibold: Font,
    /// Whole panel, composited onto the caller's pixmap with `scene.opacity`.
    scratch: Pixmap,
    /// One card, drawn unrotated and then composited rotated.
    layer: Pixmap,
    /// Reused alpha mask for shadows and the hint glow.
    mask: Mask,
    /// Ping-pong buffer for the box blur.
    blur: Vec<u8>,
    /// Rasterized hint coverage, kept for the text pass after the glow is blurred.
    hint_alpha: Vec<u8>,
    images: HashMap<(usize, u64, u32, u32), CachedImage>,
    shadows: HashMap<ShadowKey, Arc<ShadowSprite>>,
}

impl Renderer {
    pub fn new() -> Self {
        let load = |bytes: &'static [u8]| {
            Font::from_bytes(bytes, FontSettings::default()).expect("embedded Barlow font")
        };
        Self {
            medium: load(theme::FONT_MEDIUM),
            semibold: load(theme::FONT_SEMIBOLD),
            scratch: Pixmap::new(1, 1).expect("1x1 pixmap"),
            layer: Pixmap::new(1, 1).expect("1x1 pixmap"),
            mask: Mask::new(1, 1).expect("1x1 mask"),
            blur: Vec::new(),
            hint_alpha: Vec::new(),
            images: HashMap::new(),
            shadows: HashMap::new(),
        }
    }

    /// The pixmap size for a scale: layout::PANEL_SIZE * scale, rounded up.
    pub fn pixmap_size(scale: f64) -> (u32, u32) {
        let width = (layout::PANEL_SIZE.width * scale).ceil().max(1.0);
        let height = (layout::PANEL_SIZE.height * scale).ceil().max(1.0);
        (
            width.min(u32::MAX as f64) as u32,
            height.min(u32::MAX as f64) as u32,
        )
    }

    /// Clears `pixmap` to transparent and paints `scene` into it.
    pub fn render(&mut self, scene: &Scene, pixmap: &mut Pixmap) {
        pixmap.fill(Color::TRANSPARENT);

        let scale = scene.scale;
        if !scale.is_finite() || scale <= 0.0 || scene.cards.is_empty() || scene.opacity <= 0.0 {
            return;
        }

        let s = scale as f32;
        let (width, height) = (pixmap.width(), pixmap.height());
        ensure_pixmap(&mut self.scratch, width, height);
        ensure_mask(&mut self.mask, width, height);
        self.scratch.fill(Color::TRANSPARENT);

        for card in &scene.cards {
            self.draw_card(scene, card, s);
        }
        if let Some(hint) = &scene.hint {
            self.draw_hint(scene, hint, s);
        }

        let opacity = scene.opacity.clamp(0.0, 1.0) as f32;
        if opacity >= 1.0 {
            pixmap.data_mut().copy_from_slice(self.scratch.data());
        } else {
            let paint = PixmapPaint {
                opacity,
                ..PixmapPaint::default()
            };
            pixmap.draw_pixmap(
                0,
                0,
                self.scratch.as_ref(),
                &paint,
                Transform::identity(),
                None,
            );
        }
    }

    fn draw_card(&mut self, scene: &Scene, card: &SceneCard, s: f32) {
        let Some((x, y, cw, ch)) = card_geometry(card, scene.scale) else {
            return;
        };
        let (x, y, cw, ch) = (x as f32, y as f32, cw as f32, ch as f32);

        let (leave_scale, card_opacity, anchor_top_left) = match card.leaving {
            None => (1.0, 1.0, false),
            Some(Leaving::Dissolve(p)) => {
                let ease = p.clamp(0.0, 1.0).powi(2);
                (1.0 - 0.28 * ease, 1.0 - ease, false)
            }
            Some(Leaving::Drop(p)) => {
                let ease = p.clamp(0.0, 1.0).powi(2);
                (1.0 - 0.65 * ease, 1.0 - ease, true)
            }
        };
        if card_opacity <= 0.0 {
            return;
        }

        let (center_x, center_y) = (x + cw / 2.0, y + ch / 2.0);
        let (pivot_x, pivot_y) = if anchor_top_left {
            (x, y)
        } else {
            (center_x, center_y)
        };
        let mut card_transform = Transform::from_translate(x, y);
        card_transform = Transform::from_rotate_at(card.tilt as f32, center_x, center_y)
            .pre_concat(card_transform);
        let scale_about = Transform::from_translate(pivot_x, pivot_y)
            .pre_concat(Transform::from_scale(
                leave_scale as f32,
                leave_scale as f32,
            ))
            .pre_concat(Transform::from_translate(-pivot_x, -pivot_y));
        card_transform = scale_about.pre_concat(card_transform);

        let radius = (theme::RADIUS_SM * scene.scale).clamp(0.0, (cw.min(ch) / 2.0) as f64) as f32;
        let Some(local_path) = rounded_rect(cw, ch, radius) else {
            return;
        };

        let sprite = self.shadow_sprite(cw, ch, radius, card.tilt as f32, s, card.depth == 0);
        let (local_pivot_x, local_pivot_y) = if anchor_top_left {
            (0.0, 0.0)
        } else {
            (cw / 2.0, ch / 2.0)
        };
        let leave_local = Transform::from_translate(local_pivot_x, local_pivot_y)
            .pre_concat(Transform::from_scale(
                leave_scale as f32,
                leave_scale as f32,
            ))
            .pre_concat(Transform::from_translate(-local_pivot_x, -local_pivot_y));
        let shadow_transform = Transform::from_translate(x, y)
            .pre_concat(leave_local)
            .pre_concat(Transform::from_translate(sprite.origin_x, sprite.origin_y));
        if card.leaving.is_none() && x.fract() == 0.0 && y.fract() == 0.0 {
            blit_sprite(
                &sprite.pixmap,
                &mut self.scratch,
                (x + sprite.origin_x) as i32,
                (y + sprite.origin_y) as i32,
            );
        } else {
            let shadow_paint = PixmapPaint {
                opacity: card_opacity as f32,
                quality: FilterQuality::Nearest,
                ..PixmapPaint::default()
            };
            self.scratch.draw_pixmap(
                0,
                0,
                (*sprite.pixmap).as_ref(),
                &shadow_paint,
                shadow_transform,
                None,
            );
        }
        let reduced = self.reduced_image(&card.image, scene.scale, cw as u32, ch as u32);
        let reduced_w = reduced.width() as f32;
        let reduced_h = reduced.height() as f32;
        let image_to_local = Transform::from_scale(cw / reduced_w, ch / reduced_h);
        let Some(final_path) = local_path.clone().transform(card_transform) else {
            return;
        };

        if let Some(duration) = card.movie {
            ensure_pixmap(&mut self.layer, cw as u32, ch as u32);
            self.layer.fill(Color::TRANSPARENT);
            let pattern = Pattern::new(
                (*reduced).as_ref(),
                SpreadMode::Pad,
                FilterQuality::Bilinear,
                1.0,
                image_to_local,
            );
            let image_paint = Paint {
                shader: pattern,
                ..Paint::default()
            };
            self.layer.fill_path(
                &local_path,
                &image_paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            draw_badge(&self.semibold, &mut self.layer, duration, s);

            let composite = PixmapPaint {
                opacity: card_opacity as f32,
                quality: FilterQuality::Bilinear,
                ..PixmapPaint::default()
            };
            self.scratch
                .draw_pixmap(0, 0, self.layer.as_ref(), &composite, card_transform, None);
        } else {
            let pattern = Pattern::new(
                (*reduced).as_ref(),
                SpreadMode::Pad,
                FilterQuality::Bilinear,
                1.0,
                card_transform.pre_concat(image_to_local),
            );
            let image_paint = Paint {
                shader: pattern,
                ..Paint::default()
            };
            self.scratch.fill_path(
                &final_path,
                &image_paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        let alpha = (theme::HAIRLINE_STRONG[3] as f32 * card_opacity as f32).round() as u8;
        let paint = Paint {
            shader: Shader::SolidColor(Color::from_rgba8(
                theme::HAIRLINE_STRONG[0],
                theme::HAIRLINE_STRONG[1],
                theme::HAIRLINE_STRONG[2],
                alpha,
            )),
            ..Paint::default()
        };
        let stroke = Stroke {
            width: s.max(0.5),
            ..Stroke::default()
        };
        self.scratch
            .stroke_path(&final_path, &paint, &stroke, Transform::identity(), None);
    }

    fn shadow_sprite(
        &mut self,
        width: f32,
        height: f32,
        radius: f32,
        tilt: f32,
        s: f32,
        is_top: bool,
    ) -> Arc<ShadowSprite> {
        let key = ShadowKey {
            width: width.to_bits(),
            height: height.to_bits(),
            radius: radius.to_bits(),
            tilt: tilt.to_bits(),
            scale: s.to_bits(),
            top: is_top,
        };
        if let Some(sprite) = self.shadows.get(&key) {
            return sprite.clone();
        }

        let sprite = Arc::new(self.build_shadow_sprite(width, height, radius, tilt, s, is_top));
        if self.shadows.len() >= 16 {
            self.shadows.clear();
        }
        self.shadows.insert(key, sprite.clone());
        sprite
    }

    fn build_shadow_sprite(
        &mut self,
        width: f32,
        height: f32,
        radius: f32,
        tilt: f32,
        s: f32,
        is_top: bool,
    ) -> ShadowSprite {
        let strength = if is_top { 1.0 } else { 0.6 };
        let angle = tilt.to_radians();
        let (sin, cos) = (angle.sin(), angle.cos());
        let center_x = width / 2.0;
        let center_y = height / 2.0;

        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for (x, y) in [(0.0, 0.0), (width, 0.0), (width, height), (0.0, height)] {
            let dx = x - center_x;
            let dy = y - center_y;
            let rx = center_x + dx * cos - dy * sin;
            let ry = center_y + dx * sin + dy * cos;
            min_x = min_x.min(rx);
            min_y = min_y.min(ry);
            max_x = max_x.max(rx);
            max_y = max_y.max(ry);
        }

        let shadows = [
            (16.0f32 * s, 10.0f32 * s, 0.09f32),
            (4.0f32 * s, 2.0f32 * s, 0.06f32),
        ];
        let mut sprite_min_x = min_x;
        let mut sprite_min_y = min_y;
        let mut sprite_max_x = max_x;
        let mut sprite_max_y = max_y;
        for (blur, offset_y, _) in shadows {
            let blur_radius = shadow_blur_radius(blur);
            let margin = 3.0 * blur_radius as f32 + 2.0;
            sprite_min_x = sprite_min_x.min(min_x - margin);
            sprite_max_x = sprite_max_x.max(max_x + margin);
            sprite_min_y = sprite_min_y.min(min_y + offset_y - margin);
            sprite_max_y = sprite_max_y.max(max_y + offset_y + margin);
        }

        let origin_x = sprite_min_x.floor();
        let origin_y = sprite_min_y.floor();
        let sprite_w = (sprite_max_x - origin_x).ceil().max(1.0) as u32;
        let sprite_h = (sprite_max_y - origin_y).ceil().max(1.0) as u32;

        let mut pixmap = Pixmap::new(sprite_w, sprite_h).expect("shadow pixmap");
        let rect =
            Rect::from_xywh(0.0, 0.0, sprite_w as f32, sprite_h as f32).expect("shadow rect");
        let Some(path) = rounded_rect(width, height, radius) else {
            return ShadowSprite {
                pixmap: Arc::new(pixmap),
                origin_x,
                origin_y,
            };
        };
        ensure_mask(&mut self.mask, sprite_w, sprite_h);

        for (blur, offset_y, alpha) in shadows {
            self.mask.clear();
            let transform = Transform::from_translate(-origin_x, -origin_y)
                .pre_concat(Transform::from_translate(0.0, offset_y))
                .pre_concat(Transform::from_rotate_at(tilt, center_x, center_y));
            self.mask
                .fill_path(&path, FillRule::Winding, true, transform);
            box_blur_3(&mut self.mask, &mut self.blur, shadow_blur_radius(blur));

            let a = (alpha * strength * 255.0).round().clamp(0.0, 255.0) as u8;
            let paint = Paint {
                shader: Shader::SolidColor(Color::from_rgba8(
                    theme::SLATE[0],
                    theme::SLATE[1],
                    theme::SLATE[2],
                    a,
                )),
                ..Paint::default()
            };
            pixmap.fill_rect(rect, &paint, Transform::identity(), Some(&self.mask));
        }

        ShadowSprite {
            pixmap: Arc::new(pixmap),
            origin_x,
            origin_y,
        }
    }

    fn draw_hint(&mut self, scene: &Scene, hint: &str, s: f32) {
        let Some(top) = scene.cards.last() else {
            return;
        };
        let Some((_, top_y, _, top_h)) = card_geometry(top, scene.scale) else {
            return;
        };

        let x = (layout::INSET + 2.0) * scene.scale;
        let top = top_y + top_h + 7.0 * scene.scale;
        let px = 11.0 * s;
        let Some(metrics) = self.medium.horizontal_line_metrics(px) else {
            return;
        };
        let baseline = top as f32 + metrics.ascent;
        let text_width = text_advance(&self.medium, hint, px);
        let text_height = metrics.ascent - metrics.descent;

        let opacity = scene.hint_opacity.clamp(0.0, 1.0) as f32;
        if opacity <= 0.0 {
            return;
        }

        let blur_radius = shadow_blur_radius(2.0 * s);
        let pad = 3.0 * blur_radius as f32 + 2.0;
        let width = self.scratch.width() as f32;
        let height = self.scratch.height() as f32;
        let left = (x as f32 - pad).floor().max(0.0);
        let top_px = (top as f32 - pad).floor().max(0.0);
        let right = (x as f32 + text_width + pad).ceil().min(width);
        let bottom = (top as f32 + text_height + pad).ceil().min(height);
        if right <= left || bottom <= top_px {
            return;
        }
        let box_w = (right - left) as u32;
        let box_h = (bottom - top_px) as u32;

        self.hint_alpha.clear();
        self.hint_alpha.resize(box_w as usize * box_h as usize, 0);
        rasterize_text(
            &self.medium,
            hint,
            px,
            x as f32 - left,
            baseline - top_px,
            box_w,
            &mut self.hint_alpha,
        );

        // White glow behind the text, like the Swift white shadow at 90%, radius 2.
        ensure_mask(&mut self.mask, box_w, box_h);
        self.mask.clear();
        self.mask.data_mut().copy_from_slice(&self.hint_alpha);
        box_blur_3(&mut self.mask, &mut self.blur, blur_radius);
        let glow = Paint {
            shader: Shader::SolidColor(Color::from_rgba8(
                255,
                255,
                255,
                (0.9 * opacity * 255.0).round().clamp(0.0, 255.0) as u8,
            )),
            ..Paint::default()
        };
        let rect = Rect::from_xywh(0.0, 0.0, box_w as f32, box_h as f32).expect("hint rect");
        ensure_pixmap(&mut self.layer, box_w, box_h);
        self.layer.fill(Color::TRANSPARENT);
        self.layer
            .fill_rect(rect, &glow, Transform::identity(), Some(&self.mask));
        self.scratch.draw_pixmap(
            left as i32,
            top_px as i32,
            self.layer.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );

        draw_text(
            &self.medium,
            &mut self.scratch,
            hint,
            px,
            x as f32,
            baseline,
            [
                theme::MUTED[0],
                theme::MUTED[1],
                theme::MUTED[2],
                (opacity * 255.0) as u8,
            ],
        );
    }

    fn reduced_image(
        &mut self,
        image: &Arc<Image>,
        scale: f64,
        width: u32,
        height: u32,
    ) -> Arc<Pixmap> {
        let key = (Arc::as_ptr(image) as usize, scale.to_bits(), width, height);
        if let Some(cached) = self.images.get(&key)
            && Arc::ptr_eq(&cached.source, image)
        {
            return cached.reduced.clone();
        }

        let reduced = Arc::new(reduce_image(image, width, height));

        if self.images.len() >= CACHE_LIMIT {
            self.images.clear();
        }
        self.images.insert(
            key,
            CachedImage {
                source: image.clone(),
                reduced: reduced.clone(),
            },
        );
        reduced
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// "m:ss" for a recording length; seconds are rounded (75.4 -> "1:15").
pub fn movie_label(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Fits `THUMB_MAX * shrink` and adds the stack offset and wobble, in physical pixels.
fn card_geometry(card: &SceneCard, scale: f64) -> Option<(f64, f64, f64, f64)> {
    let image = &card.image;
    let image_w = image.width as f64;
    let image_h = image.height as f64;
    if image_w <= 0.0 || image_h <= 0.0 {
        return None;
    }

    let shrink = 1.0 - card.depth as f64 * 0.05;
    let max_w = (layout::THUMB_MAX.width * shrink).max(1.0);
    let max_h = (layout::THUMB_MAX.height * shrink).max(1.0);
    let fit = (max_w / image_w).min(max_h / image_h);
    let width = (image_w * fit * scale).round().max(1.0);
    let height = (image_h * fit * scale).round().max(1.0);

    let wobble = card.wobble.clamp(0.0, 1.0);
    let wobble_x = if wobble > 0.0 {
        (wobble * std::f64::consts::PI * 6.0).sin() * 7.0 * (1.0 - wobble)
    } else {
        0.0
    };
    let x = (layout::INSET + card.depth as f64 * 3.0 + wobble_x) * scale;
    let y = (layout::INSET - card.depth as f64 * 4.0) * scale;
    Some((x, y, width, height))
}

fn ensure_pixmap(pixmap: &mut Pixmap, width: u32, height: u32) {
    if pixmap.width() != width || pixmap.height() != height {
        *pixmap = Pixmap::new(width, height).expect("non-empty pixmap");
    }
}

fn ensure_mask(mask: &mut Mask, width: u32, height: u32) {
    if mask.width() != width || mask.height() != height {
        *mask = Mask::new(width, height).expect("non-empty mask");
    }
}

fn rounded_rect(width: f32, height: f32, radius: f32) -> Option<Path> {
    let rect = Rect::from_xywh(0.0, 0.0, width, height)?;
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

fn draw_badge(font: &Font, layer: &mut Pixmap, duration: Option<f64>, s: f32) {
    let layer_w = layer.width() as f32;
    let layer_h = layer.height() as f32;
    let pad_x = 5.0 * s;
    let pad_y = 2.5 * s;
    let gap = 3.0 * s;
    let triangle_w = 5.0 * s;
    let triangle_h = 6.5 * s;
    let px = 9.0 * s;
    let Some(metrics) = font.horizontal_line_metrics(px) else {
        return;
    };
    let text_h = metrics.ascent - metrics.descent;
    let label = duration.map(movie_label);
    let text_w = label
        .as_deref()
        .map(|text| text_advance(font, text, px))
        .unwrap_or(0.0);

    let content_h = triangle_h.max(text_h);
    let badge_h = content_h + 2.0 * pad_y;
    let badge_w = 2.0 * pad_x + triangle_w + if label.is_some() { gap + text_w } else { 0.0 };
    if badge_w > layer_w || badge_h > layer_h {
        return;
    }
    let badge_x = 5.0 * s;
    let badge_y = layer_h - 5.0 * s - badge_h;

    if let Some(capsule) = rounded_rect(badge_w, badge_h, badge_h / 2.0) {
        let shifted = capsule
            .clone()
            .transform(Transform::from_translate(badge_x, badge_y))
            .unwrap_or(capsule);
        let paint = Paint {
            shader: Shader::SolidColor(Color::from_rgba8(0, 0, 0, 140)),
            ..Paint::default()
        };
        layer.fill_path(
            &shifted,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    let triangle_x = badge_x + pad_x;
    let triangle_y = badge_y + (badge_h - triangle_h) / 2.0;
    let mut builder = PathBuilder::new();
    builder.move_to(triangle_x, triangle_y);
    builder.line_to(triangle_x, triangle_y + triangle_h);
    builder.line_to(triangle_x + triangle_w, triangle_y + triangle_h / 2.0);
    builder.close();
    if let Some(triangle) = builder.finish() {
        let paint = Paint {
            shader: Shader::SolidColor(Color::WHITE),
            ..Paint::default()
        };
        layer.fill_path(
            &triangle,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    if let Some(label) = label {
        let text_x = badge_x + pad_x + triangle_w + gap;
        let text_top = badge_y + (badge_h - text_h) / 2.0;
        let baseline = text_top + metrics.ascent;
        draw_text(
            font,
            layer,
            &label,
            px,
            text_x,
            baseline,
            [255, 255, 255, 255],
        );
    }
}

fn text_advance(font: &Font, text: &str, px: f32) -> f32 {
    text.chars()
        .map(|ch| font.metrics(ch, px).advance_width)
        .sum()
}

fn draw_text(
    font: &Font,
    pixmap: &mut Pixmap,
    text: &str,
    px: f32,
    x: f32,
    baseline: f32,
    color: [u8; 4],
) {
    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    let data = pixmap.data_mut();
    let mut pen = x;

    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, px);
        if metrics.width > 0 && metrics.height > 0 {
            let glyph_x = (pen + metrics.xmin as f32).round() as i32;
            let glyph_y = (baseline - (metrics.ymin + metrics.height as i32) as f32).round() as i32;

            for row in 0..metrics.height {
                let y = glyph_y + row as i32;
                if y < 0 || y >= height {
                    continue;
                }
                for col in 0..metrics.width {
                    let x = glyph_x + col as i32;
                    if x < 0 || x >= width {
                        continue;
                    }
                    let coverage = bitmap[row * metrics.width + col] as f32 / 255.0;
                    if coverage > 0.0 {
                        let index = (y as usize * width as usize + x as usize) * 4;
                        blend_pixel(data, index, color, coverage);
                    }
                }
            }
        }
        pen += metrics.advance_width;
    }
}

fn rasterize_text(
    font: &Font,
    text: &str,
    px: f32,
    x: f32,
    baseline: f32,
    width: u32,
    out: &mut [u8],
) {
    let height = (out.len() / width.max(1) as usize) as u32;
    let mut pen = x;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, px);
        if metrics.width > 0 && metrics.height > 0 {
            let glyph_x = (pen + metrics.xmin as f32).round() as i32;
            let glyph_y = (baseline - (metrics.ymin + metrics.height as i32) as f32).round() as i32;

            for row in 0..metrics.height {
                let y = glyph_y + row as i32;
                if y < 0 || y >= height as i32 {
                    continue;
                }
                for col in 0..metrics.width {
                    let x = glyph_x + col as i32;
                    if x < 0 || x >= width as i32 {
                        continue;
                    }
                    let coverage = bitmap[row * metrics.width + col];
                    let index = y as usize * width as usize + x as usize;
                    if coverage > out[index] {
                        out[index] = coverage;
                    }
                }
            }
        }
        pen += metrics.advance_width;
    }
}

/// Composites a premultiplied sprite at an integer position, source-over. Used for shadows at
/// rest, where avoiding tiny-skia's general draw pipeline matters more than filtering.
fn blit_sprite(src: &Pixmap, dst: &mut Pixmap, dx: i32, dy: i32) {
    let src_w = src.width() as i32;
    let src_h = src.height() as i32;
    let dst_w = dst.width() as i32;
    let dst_h = dst.height() as i32;
    let left = dx.max(0);
    let top = dy.max(0);
    let right = (dx + src_w).min(dst_w);
    let bottom = (dy + src_h).min(dst_h);
    if right <= left || bottom <= top {
        return;
    }

    let src_stride = src_w as usize * 4;
    let src_data = src.data();
    let dst_stride = dst_w as usize * 4;
    let data = dst.data_mut();

    for y in top..bottom {
        let src_row = (y - dy) as usize * src_stride;
        let dst_row = y as usize * dst_stride;
        for x in left..right {
            let si = src_row + (x - dx) as usize * 4;
            let alpha = src_data[si + 3] as u32;
            if alpha == 0 {
                continue;
            }
            let di = dst_row + x as usize * 4;
            let keep = 255 - alpha;
            data[di] = (src_data[si] as u32 + data[di] as u32 * keep / 255) as u8;
            data[di + 1] = (src_data[si + 1] as u32 + data[di + 1] as u32 * keep / 255) as u8;
            data[di + 2] = (src_data[si + 2] as u32 + data[di + 2] as u32 * keep / 255) as u8;
            data[di + 3] = (alpha + data[di + 3] as u32 * keep / 255) as u8;
        }
    }
}

fn blend_pixel(data: &mut [u8], index: usize, color: [u8; 4], alpha: f32) {
    if alpha <= 0.0 || index + 4 > data.len() {
        return;
    }
    let sa = (alpha * 255.0).round().clamp(0.0, 255.0) as u32;
    if sa == 0 {
        return;
    }
    let keep = 255 - sa;
    let r = color[0] as u32 * sa / 255;
    let g = color[1] as u32 * sa / 255;
    let b = color[2] as u32 * sa / 255;
    data[index] = (r + data[index] as u32 * keep / 255) as u8;
    data[index + 1] = (g + data[index + 1] as u32 * keep / 255) as u8;
    data[index + 2] = (b + data[index + 2] as u32 * keep / 255) as u8;
    data[index + 3] = (sa + data[index + 3] as u32 * keep / 255) as u8;
}

fn shadow_blur_radius(blur: f32) -> usize {
    ((blur * 0.5).round() as usize).max(1)
}

/// Three-pass box blur over the whole mask; `temp` is scratch of the same size.
fn box_blur_3(mask: &mut Mask, temp: &mut Vec<u8>, radius: usize) {
    let width = mask.width() as usize;
    let height = mask.height() as usize;
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    temp.clear();
    temp.resize(width * height, 0);

    for _ in 0..3 {
        blur_rows(mask.data(), temp.as_mut_slice(), width, height, radius);
        blur_columns(temp.as_slice(), mask.data_mut(), width, height, radius);
    }
}

#[allow(clippy::needless_range_loop)]
fn blur_rows(src: &[u8], dst: &mut [u8], width: usize, height: usize, radius: usize) {
    let denominator = (2 * radius + 1) as u32;
    for y in 0..height {
        let row = y * width;
        let mut sum = src[row] as u32 * radius as u32;
        for x in 0..=radius {
            sum += src[row + x.min(width - 1)] as u32;
        }
        for x in 0..width {
            dst[row + x] = (sum / denominator) as u8;
            let add = src[row + (x + radius + 1).min(width - 1)] as u32;
            let sub = src[row + x.saturating_sub(radius)] as u32;
            sum = sum + add - sub;
        }
    }
}

#[allow(clippy::needless_range_loop)]
fn blur_columns(src: &[u8], dst: &mut [u8], width: usize, height: usize, radius: usize) {
    let denominator = (2 * radius + 1) as u32;
    for x in 0..width {
        let mut sum = src[x] as u32 * radius as u32;
        for y in 0..=radius {
            sum += src[y.min(height - 1) * width + x] as u32;
        }
        for y in 0..height {
            dst[y * width + x] = (sum / denominator) as u8;
            let add = src[(y + radius + 1).min(height - 1) * width + x] as u32;
            let sub = src[y.saturating_sub(radius) * width + x] as u32;
            sum = sum + add - sub;
        }
    }
}

/// Box-filters `image` down to exactly `out_width` x `out_height`, keeping alpha weighted.
/// The result is premultiplied, ready to use as a pattern.
fn reduce_image(image: &Image, out_width: u32, out_height: u32) -> Pixmap {
    let src_w = image.width as usize;
    let src_h = image.height as usize;
    let out_w = out_width.clamp(1, image.width) as usize;
    let out_h = out_height.clamp(1, image.height) as usize;

    let x_bounds: Vec<(usize, usize)> = (0..out_w)
        .map(|x| {
            let start = ((x as f64 * src_w as f64 / out_w as f64).floor() as usize).min(src_w - 1);
            let end = (((x + 1) as f64 * src_w as f64 / out_w as f64).ceil() as usize)
                .clamp(start + 1, src_w);
            (start, end)
        })
        .collect();
    let y_bounds: Vec<(usize, usize)> = (0..out_h)
        .map(|y| {
            let start = ((y as f64 * src_h as f64 / out_h as f64).floor() as usize).min(src_h - 1);
            let end = (((y + 1) as f64 * src_h as f64 / out_h as f64).ceil() as usize)
                .clamp(start + 1, src_h);
            (start, end)
        })
        .collect();

    let mut out = Pixmap::new(out_w as u32, out_h as u32).expect("reduced image pixmap");
    let data = out.data_mut();
    for (oy, &(y0, y1)) in y_bounds.iter().enumerate() {
        for (ox, &(x0, x1)) in x_bounds.iter().enumerate() {
            let mut sum = [0u64; 4];
            let mut count = 0u64;
            for y in y0..y1 {
                let row = y * src_w * 4;
                for x in x0..x1 {
                    let index = row + x * 4;
                    let alpha = image.rgba[index + 3] as u64;
                    sum[0] += image.rgba[index] as u64 * alpha;
                    sum[1] += image.rgba[index + 1] as u64 * alpha;
                    sum[2] += image.rgba[index + 2] as u64 * alpha;
                    sum[3] += alpha;
                    count += 1;
                }
            }
            let count = count.max(1);
            let out_index = (oy * out_w + ox) * 4;
            let scale = count * 255;
            data[out_index] = (sum[0] / scale) as u8;
            data[out_index + 1] = (sum[1] / scale) as u8;
            data[out_index + 2] = (sum[2] / scale) as u8;
            data[out_index + 3] = (sum[3] / count) as u8;
        }
    }
    out
}
