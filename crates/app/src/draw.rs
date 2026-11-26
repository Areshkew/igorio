//! Software renderer for the overlay panel. Ports `FairyView.swift` to tiny-skia:
//! a tilted pile of screenshots with a hairline, a soft slate shadow and an optional
//! hint, plus the recording badge. Everything is drawn in physical pixels.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

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
/// Opacity of the dark pill behind the hint text, out of 255.
const HINT_PILL_ALPHA: u32 = 150;
/// Transparent padding around the movie-card layer, so its rotated edge can be filtered.
const LAYER_BORDER: f32 = 2.0;

struct CachedImage {
    /// Weak so a discarded screenshot's pixels are freed even while the cache entry exists.
    source: Weak<Image>,
    reduced: Arc<Pixmap>,
    used: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CardKey {
    image: usize,
    width: u32,
    height: u32,
    tilt: u32,
    scale: u64,
    top: bool,
    badge: Option<Option<u64>>,
}

struct CardSprite {
    pixmap: Arc<Pixmap>,
    origin_x: i32,
    origin_y: i32,
}

struct CachedCard {
    source: Weak<Image>,
    sprite: Arc<CardSprite>,
    used: bool,
}

struct HintSprite {
    text: String,
    scale: u32,
    pixmap: Arc<Pixmap>,
    origin_x: i32,
    origin_y: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct LayerCard {
    key: CardKey,
    x: i32,
    y: i32,
}

struct StaticLayer {
    scale: u64,
    cards: Vec<LayerCard>,
    pixmap: Pixmap,
}

pub struct Renderer {
    semibold: Font,
    /// Card content (image plus badge) drawn unrotated before it is placed in the sprite.
    layer: Pixmap,
    /// Reused alpha mask for shadows.
    mask: Mask,
    /// Ping-pong buffer for the box blur.
    blur: Vec<u8>,
    images: HashMap<(usize, u64), CachedImage>,
    cards: HashMap<CardKey, CachedCard>,
    hint: Option<Arc<HintSprite>>,
    base: Option<StaticLayer>,
    #[cfg(test)]
    reductions: usize,
}

impl Renderer {
    pub fn new() -> Self {
        let load = |bytes: &'static [u8]| {
            Font::from_bytes(bytes, FontSettings::default()).expect("embedded Barlow font")
        };
        Self {
            semibold: load(theme::FONT_SEMIBOLD),
            layer: Pixmap::new(1, 1).expect("1x1 pixmap"),
            mask: Mask::new(1, 1).expect("1x1 mask"),
            blur: Vec::new(),
            images: HashMap::new(),
            cards: HashMap::new(),
            hint: None,
            base: None,
            #[cfg(test)]
            reductions: 0,
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
        self.evict_caches();

        let scale = scene.scale;
        if !scale.is_finite() || scale <= 0.0 {
            return;
        }
        let expected = Self::pixmap_size(scale);
        if pixmap.width() != expected.0 || pixmap.height() != expected.1 {
            return;
        }
        if scene.cards.is_empty() || scene.opacity <= 0.0 {
            return;
        }

        let s = scale as f32;
        let scene_opacity = opacity_units(scene.opacity);
        let static_cards = self.static_prefix(scene);
        let prefix = static_cards.len();
        if prefix > 0 {
            self.ensure_base(scene, s, pixmap.width(), pixmap.height(), &static_cards);
            self.touch_static(&static_cards);
            if let Some(base) = &self.base {
                if scene_opacity >= 256 {
                    pixmap.data_mut().copy_from_slice(base.pixmap.data());
                } else {
                    blit_sprite(&base.pixmap, pixmap, 0, 0, scene_opacity);
                }
            }
        }
        for card in &scene.cards[prefix..] {
            self.draw_card(scene, card, s, scene_opacity, pixmap);
        }
        if let Some(hint) = &scene.hint {
            self.draw_hint(scene, hint, s, scene_opacity, pixmap);
        }
    }

    fn static_prefix(&self, scene: &Scene) -> Vec<LayerCard> {
        let mut cards = Vec::new();
        for card in &scene.cards {
            if card.leaving.is_some() || card.wobble > 0.0 || card.wobble.is_nan() {
                break;
            }
            let Some((x, y, width, height)) = card_geometry(card, scene.scale) else {
                break;
            };
            let tilt = finite_tilt(card.tilt);
            cards.push(LayerCard {
                key: CardKey {
                    image: Arc::as_ptr(&card.image) as usize,
                    width: (width as f32).to_bits(),
                    height: (height as f32).to_bits(),
                    tilt: tilt.to_bits(),
                    scale: scene.scale.to_bits(),
                    top: card.depth == 0,
                    badge: badge_key(card.movie),
                },
                x: (x as f32).round() as i32,
                y: (y as f32).round() as i32,
            });
        }
        cards
    }

    fn touch_static(&mut self, cards: &[LayerCard]) {
        for card in cards {
            if let Some(entry) = self.cards.get_mut(&card.key) {
                entry.used = true;
            }
            if let Some(entry) = self.images.get_mut(&(card.key.image, card.key.scale)) {
                entry.used = true;
            }
        }
    }

    fn ensure_base(&mut self, scene: &Scene, s: f32, width: u32, height: u32, cards: &[LayerCard]) {
        let scale = scene.scale.to_bits();
        if let Some(base) = &self.base
            && base.scale == scale
            && base.cards == cards
        {
            return;
        }

        let mut pixmap = Pixmap::new(width, height).expect("base layer");
        for (card, layer_card) in scene.cards.iter().zip(cards) {
            let Some((_, _, cw, ch)) = card_geometry(card, scene.scale) else {
                continue;
            };
            let radius = (theme::RADIUS_SM * scene.scale).clamp(0.0, cw.min(ch) / 2.0) as f32;
            let tilt = finite_tilt(card.tilt);
            let sprite = self.card_sprite(
                &card.image,
                scene.scale,
                cw as f32,
                ch as f32,
                radius,
                tilt,
                s,
                card.depth == 0,
                badge_key(card.movie),
            );
            blit_sprite(
                &sprite.pixmap,
                &mut pixmap,
                layer_card.x + sprite.origin_x,
                layer_card.y + sprite.origin_y,
                256,
            );
        }
        self.base = Some(StaticLayer {
            scale,
            cards: cards.to_vec(),
            pixmap,
        });
    }

    fn evict_caches(&mut self) {
        self.images
            .retain(|_, entry| entry.used && entry.source.strong_count() > 0);
        for entry in self.images.values_mut() {
            entry.used = false;
        }
        self.cards
            .retain(|_, entry| entry.used && entry.source.strong_count() > 0);
        for entry in self.cards.values_mut() {
            entry.used = false;
        }
    }

    fn draw_card(
        &mut self,
        scene: &Scene,
        card: &SceneCard,
        s: f32,
        scene_opacity: u32,
        pixmap: &mut Pixmap,
    ) {
        let Some((x, y, cw, ch)) = card_geometry(card, scene.scale) else {
            return;
        };
        let (x, y, cw, ch) = (x as f32, y as f32, cw as f32, ch as f32);

        let (leave_scale, card_opacity, anchor_top_left) = match card.leaving {
            None => (1.0, 1.0, false),
            Some(Leaving::Dissolve(progress)) => {
                let ease = leave_progress(progress).powi(2);
                (1.0 - 0.28 * ease, 1.0 - ease, false)
            }
            Some(Leaving::Drop(progress)) => {
                let ease = leave_progress(progress).powi(2);
                (1.0 - 0.65 * ease, 1.0 - ease, true)
            }
        };
        if card_opacity <= 0.0 {
            return;
        }

        let tilt = finite_tilt(card.tilt);
        let radius = (theme::RADIUS_SM * scene.scale).clamp(0.0, (cw.min(ch) / 2.0) as f64) as f32;
        let badge = badge_key(card.movie);
        let sprite = self.card_sprite(
            &card.image,
            scene.scale,
            cw,
            ch,
            radius,
            tilt,
            s,
            card.depth == 0,
            badge,
        );

        let opacity = (scene_opacity as f64 * card_opacity).round() as u32;
        if opacity == 0 {
            return;
        }
        let x = x.round();
        let y = y.round();
        if card.leaving.is_none() {
            blit_sprite(
                &sprite.pixmap,
                pixmap,
                x as i32 + sprite.origin_x,
                y as i32 + sprite.origin_y,
                opacity,
            );
        } else {
            let pivot = if anchor_top_left {
                (0.0, 0.0)
            } else {
                (cw / 2.0, ch / 2.0)
            };
            blit_scaled(
                &sprite.pixmap,
                pixmap,
                x,
                y,
                pivot,
                leave_scale as f32,
                sprite.origin_x,
                sprite.origin_y,
                opacity,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn card_sprite(
        &mut self,
        image: &Arc<Image>,
        scale: f64,
        width: f32,
        height: f32,
        radius: f32,
        tilt: f32,
        s: f32,
        is_top: bool,
        badge: Option<Option<u64>>,
    ) -> Arc<CardSprite> {
        let key = CardKey {
            image: Arc::as_ptr(image) as usize,
            width: width.to_bits(),
            height: height.to_bits(),
            tilt: tilt.to_bits(),
            scale: scale.to_bits(),
            top: is_top,
            badge,
        };
        let cached = self.cards.get_mut(&key).and_then(|cached| {
            (cached.source.as_ptr() == Arc::as_ptr(image) && cached.source.strong_count() > 0).then(
                || {
                    cached.used = true;
                    cached.sprite.clone()
                },
            )
        });
        if let Some(sprite) = cached {
            self.touch_image(image, scale);
            return sprite;
        }

        let sprite = Arc::new(
            self.build_card_sprite(image, scale, width, height, radius, tilt, s, is_top, badge),
        );
        if self.cards.len() >= CACHE_LIMIT {
            self.cards.clear();
        }
        self.cards.insert(
            key,
            CachedCard {
                source: Arc::downgrade(image),
                sprite: sprite.clone(),
                used: true,
            },
        );
        sprite
    }

    fn touch_image(&mut self, image: &Arc<Image>, scale: f64) {
        let key = (Arc::as_ptr(image) as usize, scale.to_bits());
        if let Some(entry) = self.images.get_mut(&key)
            && entry.source.as_ptr() == Arc::as_ptr(image)
        {
            entry.used = true;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_card_sprite(
        &mut self,
        image: &Arc<Image>,
        scale: f64,
        width: f32,
        height: f32,
        radius: f32,
        tilt: f32,
        s: f32,
        is_top: bool,
        badge: Option<Option<u64>>,
    ) -> CardSprite {
        let strength = if is_top { 1.0 } else { 0.6 };
        let angle = tilt.to_radians();
        let (sin, cos) = (angle.sin(), angle.cos());
        let center_x = width / 2.0;
        let center_y = height / 2.0;

        let mut rot_min_x = f32::MAX;
        let mut rot_min_y = f32::MAX;
        let mut rot_max_x = f32::MIN;
        let mut rot_max_y = f32::MIN;
        for (x, y) in [(0.0, 0.0), (width, 0.0), (width, height), (0.0, height)] {
            let dx = x - center_x;
            let dy = y - center_y;
            let rx = center_x + dx * cos - dy * sin;
            let ry = center_y + dx * sin + dy * cos;
            rot_min_x = rot_min_x.min(rx);
            rot_min_y = rot_min_y.min(ry);
            rot_max_x = rot_max_x.max(rx);
            rot_max_y = rot_max_y.max(ry);
        }

        let stroke_margin = s * 0.5 + 2.0;
        let mut min_x = rot_min_x - stroke_margin;
        let mut min_y = rot_min_y - stroke_margin;
        let mut max_x = rot_max_x + stroke_margin;
        let mut max_y = rot_max_y + stroke_margin;
        let shadows = [
            (16.0f32 * s, 10.0f32 * s, 0.09f32),
            (4.0f32 * s, 2.0f32 * s, 0.06f32),
        ];
        for (blur, offset_y, _) in shadows {
            let blur_radius = shadow_blur_radius(blur);
            let margin = 3.0 * blur_radius as f32 + 2.0;
            min_x = min_x.min(rot_min_x - margin);
            max_x = max_x.max(rot_max_x + margin);
            min_y = min_y.min(rot_min_y + offset_y - margin);
            max_y = max_y.max(rot_max_y + offset_y + margin);
        }

        let origin_x = min_x.floor();
        let origin_y = min_y.floor();
        let sprite_w = (max_x - origin_x).ceil().max(1.0) as u32;
        let sprite_h = (max_y - origin_y).ceil().max(1.0) as u32;

        let mut pixmap = Pixmap::new(sprite_w, sprite_h).expect("card sprite");
        let rect =
            Rect::from_xywh(0.0, 0.0, sprite_w as f32, sprite_h as f32).expect("sprite rect");
        let Some(local_path) = rounded_rect(width, height, radius) else {
            return CardSprite {
                pixmap: Arc::new(pixmap),
                origin_x: origin_x as i32,
                origin_y: origin_y as i32,
            };
        };
        let rotation = Transform::from_rotate_at(tilt, center_x, center_y);
        ensure_mask(&mut self.mask, sprite_w, sprite_h);

        for (blur, offset_y, alpha) in shadows {
            self.mask.clear();
            let transform = Transform::from_translate(-origin_x, -origin_y)
                .pre_concat(Transform::from_translate(0.0, offset_y))
                .pre_concat(rotation);
            self.mask
                .fill_path(&local_path, FillRule::Winding, true, transform);
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

        let pad = LAYER_BORDER;
        ensure_pixmap(
            &mut self.layer,
            (width + 2.0 * pad) as u32,
            (height + 2.0 * pad) as u32,
        );
        self.layer.fill(Color::TRANSPARENT);
        let layer_path = local_path
            .clone()
            .transform(Transform::from_translate(pad, pad));
        let reduced = self.reduced_image(image, scale);
        let reduced_w = reduced.width() as f32;
        let reduced_h = reduced.height() as f32;
        let image_to_layer =
            Transform::from_scale(width / reduced_w, height / reduced_h).post_translate(pad, pad);
        let pattern = Pattern::new(
            (*reduced).as_ref(),
            SpreadMode::Pad,
            FilterQuality::Bilinear,
            1.0,
            image_to_layer,
        );
        let image_paint = Paint {
            shader: pattern,
            ..Paint::default()
        };
        if let Some(layer_path) = layer_path {
            self.layer.fill_path(
                &layer_path,
                &image_paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        draw_badge(
            &self.semibold,
            &mut self.layer,
            badge,
            s,
            (pad, pad),
            (width, height),
        );

        let layer_to_sprite = Transform::from_translate(-origin_x, -origin_y)
            .pre_concat(rotation)
            .pre_concat(Transform::from_translate(-pad, -pad));
        let composite = PixmapPaint {
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        pixmap.draw_pixmap(0, 0, self.layer.as_ref(), &composite, layer_to_sprite, None);

        if let Some(stroke_path) = local_path
            .transform(Transform::from_translate(-origin_x, -origin_y).pre_concat(rotation))
        {
            let paint = Paint {
                shader: Shader::SolidColor(Color::from_rgba8(
                    theme::HAIRLINE_STRONG[0],
                    theme::HAIRLINE_STRONG[1],
                    theme::HAIRLINE_STRONG[2],
                    theme::HAIRLINE_STRONG[3],
                )),
                ..Paint::default()
            };
            let stroke = Stroke {
                width: s.max(0.5),
                ..Stroke::default()
            };
            pixmap.stroke_path(&stroke_path, &paint, &stroke, Transform::identity(), None);
        }

        CardSprite {
            pixmap: Arc::new(pixmap),
            origin_x: origin_x as i32,
            origin_y: origin_y as i32,
        }
    }

    fn draw_hint(
        &mut self,
        scene: &Scene,
        hint: &str,
        s: f32,
        scene_opacity: u32,
        pixmap: &mut Pixmap,
    ) {
        let scale = scene.scale;
        let opacity = (scene_opacity as f64 * scene.hint_opacity.clamp(0.0, 1.0)).round() as u32;
        if opacity == 0 {
            return;
        }
        let tallest = scene
            .cards
            .iter()
            .filter_map(|card| {
                let shrink = 1.0 - card.depth as f64 * 0.05;
                fitted_card_size(&card.image, shrink, scale).map(|(_, height)| height)
            })
            .fold(0.0f64, f64::max);
        if tallest <= 0.0 {
            return;
        }

        let sprite = self.hint_sprite(hint, s);
        let anchor_x = ((layout::INSET + 2.0) * scale) as f32;
        let anchor_y = ((layout::INSET + tallest + 7.0) * scale) as f32;
        let x = (anchor_x + sprite.origin_x as f32).round() as i32;
        let y = (anchor_y + sprite.origin_y as f32).round() as i32;
        blit_sprite(&sprite.pixmap, pixmap, x, y, opacity);
    }

    fn hint_sprite(&mut self, text: &str, s: f32) -> Arc<HintSprite> {
        let scale = s.to_bits();
        if let Some(sprite) = &self.hint
            && sprite.text == text
            && sprite.scale == scale
        {
            return sprite.clone();
        }
        let sprite = Arc::new(self.build_hint_sprite(text, s));
        self.hint = Some(sprite.clone());
        sprite
    }

    fn build_hint_sprite(&mut self, text: &str, s: f32) -> HintSprite {
        // White text on a dark pill. A glow behind the text (the Swift look) reads as
        // blur whenever its colour contrasts with the desktop; a hard-edged pill does not.
        let font = &self.semibold;
        let px = 11.0 * s;
        let Some(metrics) = font.horizontal_line_metrics(px) else {
            return HintSprite {
                text: text.to_string(),
                scale: s.to_bits(),
                pixmap: Arc::new(Pixmap::new(1, 1).expect("hint pixmap")),
                origin_x: 0,
                origin_y: 0,
            };
        };
        let text_width = text_advance(font, text, px);
        let text_height = metrics.ascent - metrics.descent;
        // The pill reaches past the text on every side; one more pixel for its soft edge.
        let (pad_x, pad_y) = (6.0 * s, 2.5 * s);
        let left = -(pad_x + 1.0).ceil() as i32;
        let top = -(pad_y + 1.0).ceil() as i32;
        let right = (text_width + pad_x + 1.0).ceil() as i32;
        let bottom = (text_height + pad_y + 1.0).ceil() as i32;
        let box_w = (right - left).max(1) as u32;
        let box_h = (bottom - top).max(1) as u32;

        let mut pixmap = Pixmap::new(box_w, box_h).expect("hint pixmap");
        let (pill_x, pill_y) = (-left as f32 - pad_x, -top as f32 - pad_y);
        let (pill_w, pill_h) = (text_width + 2.0 * pad_x, text_height + 2.0 * pad_y);
        let r = pill_h / 2.0;
        let opaque = Paint {
            shader: Shader::SolidColor(Color::BLACK),
            anti_alias: true,
            ..Paint::default()
        };
        // Drawn opaque so the overlapping parts do not add up, then faded as a whole.
        let body = Rect::from_xywh(pill_x + r, pill_y, (pill_w - 2.0 * r).max(1.0), pill_h);
        for path in [
            body.map(PathBuilder::from_rect),
            PathBuilder::from_circle(pill_x + r, pill_y + r, r),
            PathBuilder::from_circle(pill_x + pill_w - r, pill_y + r, r),
        ]
        .into_iter()
        .flatten()
        {
            pixmap.fill_path(&path, &opaque, FillRule::Winding, Transform::identity(), None);
        }
        for pixel in pixmap.data_mut().as_chunks_mut::<4>().0 {
            pixel[3] = (pixel[3] as u32 * HINT_PILL_ALPHA / 255) as u8;
        }
        draw_text(
            font,
            &mut pixmap,
            text,
            px,
            -left as f32,
            metrics.ascent - top as f32,
            [255, 255, 255, 255],
        );

        HintSprite {
            text: text.to_string(),
            scale: s.to_bits(),
            pixmap: Arc::new(pixmap),
            origin_x: left,
            origin_y: top,
        }
    }

    fn reduced_image(&mut self, image: &Arc<Image>, scale: f64) -> Arc<Pixmap> {
        let key = (Arc::as_ptr(image) as usize, scale.to_bits());
        if let Some(cached) = self.images.get_mut(&key)
            && cached.source.as_ptr() == Arc::as_ptr(image)
            && cached.source.strong_count() > 0
        {
            cached.used = true;
            return cached.reduced.clone();
        }

        let (width, height) = fitted_card_size(image, 1.0, scale).unwrap_or((1.0, 1.0));
        let reduced = Arc::new(reduce_image(
            image,
            width.round().max(1.0) as u32,
            height.round().max(1.0) as u32,
        ));

        if self.images.len() >= CACHE_LIMIT {
            self.images.clear();
        }
        self.images.insert(
            key,
            CachedImage {
                source: Arc::downgrade(image),
                reduced: reduced.clone(),
                used: true,
            },
        );
        #[cfg(test)]
        {
            self.reductions += 1;
        }
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
    seconds_label(seconds.max(0.0).round() as u64)
}

fn seconds_label(total: u64) -> String {
    format!("{}:{:02}", total / 60, total % 60)
}

fn badge_key(movie: Option<Option<f64>>) -> Option<Option<u64>> {
    movie.map(|duration| duration.map(|seconds| seconds.max(0.0).round() as u64))
}

fn opacity_units(value: f64) -> u32 {
    (value.clamp(0.0, 1.0) * 256.0).round().clamp(0.0, 256.0) as u32
}

fn finite_tilt(tilt: f64) -> f32 {
    let tilt = tilt as f32;
    if tilt.is_finite() { tilt } else { 0.0 }
}

fn leave_progress(progress: f64) -> f64 {
    if progress.is_nan() {
        0.0
    } else {
        progress.clamp(0.0, 1.0)
    }
}

/// Fits `THUMB_MAX * shrink` and adds the stack offset and wobble, in physical pixels.
fn card_geometry(card: &SceneCard, scale: f64) -> Option<(f64, f64, f64, f64)> {
    let shrink = 1.0 - card.depth as f64 * 0.05;
    let (width, height) = fitted_card_size(&card.image, shrink, scale)?;

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

fn fitted_card_size(image: &Image, shrink: f64, scale: f64) -> Option<(f64, f64)> {
    let image_w = image.width as f64;
    let image_h = image.height as f64;
    if image_w <= 0.0 || image_h <= 0.0 {
        return None;
    }
    let max_w = (layout::THUMB_MAX.width * shrink).max(1.0);
    let max_h = (layout::THUMB_MAX.height * shrink).max(1.0);
    let fit = (max_w / image_w).min(max_h / image_h);
    Some((
        (image_w * fit * scale).round().max(1.0),
        (image_h * fit * scale).round().max(1.0),
    ))
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

fn draw_badge(
    font: &Font,
    layer: &mut Pixmap,
    badge: Option<Option<u64>>,
    s: f32,
    origin: (f32, f32),
    card: (f32, f32),
) {
    let Some(badge) = badge else {
        return;
    };
    let (origin_x, origin_y) = origin;
    let (card_w, card_h) = card;
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
    let label = badge.map(seconds_label);
    let text_w = label
        .as_deref()
        .map(|text| text_advance(font, text, px))
        .unwrap_or(0.0);

    let content_h = triangle_h.max(text_h);
    let badge_h = content_h + 2.0 * pad_y;
    let badge_w = 2.0 * pad_x + triangle_w + if label.is_some() { gap + text_w } else { 0.0 };
    if badge_w > card_w || badge_h > card_h {
        return;
    }
    let badge_x = origin_x + 5.0 * s;
    let badge_y = origin_y + card_h - 5.0 * s - badge_h;

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

/// Composites a premultiplied sprite at an integer position, source-over, using whole u32 pixels.
fn blit_sprite(src: &Pixmap, dst: &mut Pixmap, dx: i32, dy: i32, opacity: u32) {
    if opacity == 0 {
        return;
    }
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
            let source = u32::from_le_bytes([
                src_data[si],
                src_data[si + 1],
                src_data[si + 2],
                src_data[si + 3],
            ]);
            if source >> 24 == 0 {
                continue;
            }
            let di = dst_row + x as usize * 4;
            let below = u32::from_le_bytes([data[di], data[di + 1], data[di + 2], data[di + 3]]);
            let out = source_over(below, source, opacity);
            data[di..di + 4].copy_from_slice(&out.to_le_bytes());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_scaled(
    src: &Pixmap,
    dst: &mut Pixmap,
    x: f32,
    y: f32,
    pivot: (f32, f32),
    scale: f32,
    origin_x: i32,
    origin_y: i32,
    opacity: u32,
) {
    if opacity == 0 || scale <= 0.0 || scale.is_nan() {
        return;
    }
    let src_w = src.width() as f32;
    let src_h = src.height() as f32;
    let origin_xf = origin_x as f32;
    let origin_yf = origin_y as f32;

    // Forward map of the sprite rectangle, to find the destination bounds.
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for (qx, qy) in [(0.0, 0.0), (src_w, 0.0), (src_w, src_h), (0.0, src_h)] {
        let fx = x + pivot.0 + scale * (qx + origin_xf - pivot.0);
        let fy = y + pivot.1 + scale * (qy + origin_yf - pivot.1);
        min_x = min_x.min(fx);
        min_y = min_y.min(fy);
        max_x = max_x.max(fx);
        max_y = max_y.max(fy);
    }
    let dst_w = dst.width() as i32;
    let dst_h = dst.height() as i32;
    let left = (min_x.floor() as i32 - 1).max(0);
    let top = (min_y.floor() as i32 - 1).max(0);
    let right = (max_x.ceil() as i32 + 1).min(dst_w);
    let bottom = (max_y.ceil() as i32 + 1).min(dst_h);
    if right <= left || bottom <= top {
        return;
    }

    let inv = (65536.0 / scale as f64).round().max(1.0) as i64;
    let pivot_x = pivot.0 as f64;
    let pivot_y = pivot.1 as f64;
    let start_x = (pivot_x - origin_x as f64) * 65536.0;
    let start_y = (pivot_y - origin_y as f64) * 65536.0;
    let num_x = 1.0 - 2.0 * (x as f64 + pivot_x);
    let num_y = 1.0 - 2.0 * (y as f64 + pivot_y);
    let mut qy = start_y.round() as i64 + (num_y * inv as f64 / 2.0).round() as i64;

    let src_width = src.width() as usize;
    let src_height = src.height() as usize;
    let src_stride = src_width * 4;
    let src_data = src.data();
    let dst_stride = dst_w as usize * 4;
    let data = dst.data_mut();

    for py in top..bottom {
        let mut qx = start_x.round() as i64 + (num_x * inv as f64 / 2.0).round() as i64;
        let dy = py as usize * dst_stride;
        for px in left..right {
            let sx = qx >> 16;
            let sy = qy >> 16;
            let ix = sx.clamp(0, src_width as i64 - 1) as usize;
            let iy = sy.clamp(0, src_height as i64 - 1) as usize;
            let ix1 = (ix + 1).min(src_width - 1);
            let iy1 = (iy + 1).min(src_height - 1);
            let fx = ((qx & 0xffff) >> 8) as u32;
            let fy = ((qy & 0xffff) >> 8) as u32;
            let w00 = (256 - fx) * (256 - fy);
            let w10 = fx * (256 - fy);
            let w01 = (256 - fx) * fy;
            let w11 = fx * fy;
            let p00 = packed_at(src_data, iy * src_stride + ix * 4);
            let p10 = packed_at(src_data, iy * src_stride + ix1 * 4);
            let p01 = packed_at(src_data, iy1 * src_stride + ix * 4);
            let p11 = packed_at(src_data, iy1 * src_stride + ix1 * 4);
            let r =
                ((p00 & 0xff) * w00 + (p10 & 0xff) * w10 + (p01 & 0xff) * w01 + (p11 & 0xff) * w11)
                    >> 16;
            let g = (((p00 >> 8) & 0xff) * w00
                + ((p10 >> 8) & 0xff) * w10
                + ((p01 >> 8) & 0xff) * w01
                + ((p11 >> 8) & 0xff) * w11)
                >> 16;
            let b = (((p00 >> 16) & 0xff) * w00
                + ((p10 >> 16) & 0xff) * w10
                + ((p01 >> 16) & 0xff) * w01
                + ((p11 >> 16) & 0xff) * w11)
                >> 16;
            let a = (((p00 >> 24) & 0xff) * w00
                + ((p10 >> 24) & 0xff) * w10
                + ((p01 >> 24) & 0xff) * w01
                + ((p11 >> 24) & 0xff) * w11)
                >> 16;
            if a == 0 {
                qx += inv;
                continue;
            }
            let source = r | (g << 8) | (b << 16) | (a << 24);
            let di = dy + px as usize * 4;
            let below = u32::from_le_bytes([data[di], data[di + 1], data[di + 2], data[di + 3]]);
            let out = source_over(below, source, opacity);
            data[di..di + 4].copy_from_slice(&out.to_le_bytes());
            qx += inv;
        }
        qy += inv;
    }
}

fn packed_at(data: &[u8], index: usize) -> u32 {
    u32::from_le_bytes([
        data[index],
        data[index + 1],
        data[index + 2],
        data[index + 3],
    ])
}

/// Source-over of premultiplied u32 pixels with a 0..=256 global opacity.
fn source_over(below: u32, source: u32, opacity: u32) -> u32 {
    let source_alpha = source >> 24;
    let alpha = (source_alpha * opacity) >> 8;
    if alpha == 0 {
        return below;
    }
    let red = ((source & 0xff) * opacity) >> 8;
    let green = (((source >> 8) & 0xff) * opacity) >> 8;
    let blue = (((source >> 16) & 0xff) * opacity) >> 8;
    let keep = 255 - alpha;
    let red = red + (((below & 0xff) * keep) >> 8);
    let green = green + ((((below >> 8) & 0xff) * keep) >> 8);
    let blue = blue + ((((below >> 16) & 0xff) * keep) >> 8);
    let alpha = alpha + (((below >> 24) * keep) >> 8);
    red | (green << 8) | (blue << 16) | (alpha << 24)
}

fn blend_pixel(data: &mut [u8], index: usize, color: [u8; 4], alpha: f32) {
    if alpha <= 0.0 || index + 4 > data.len() {
        return;
    }
    let sa = (alpha * color[3] as f32).round().clamp(0.0, 255.0) as u32;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Arc<Image> {
        let mut rgba = vec![0; width as usize * height as usize * 4];
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&color);
        }
        Arc::new(Image::new(width, height, rgba).expect("image"))
    }

    fn scene(cards: Vec<SceneCard>, hint: Option<&str>) -> Scene {
        Scene {
            cards,
            hint: hint.map(str::to_string),
            hint_opacity: 1.0,
            opacity: 1.0,
            scale: 1.0,
        }
    }

    fn card(image: Arc<Image>, tilt: f64, depth: usize) -> SceneCard {
        SceneCard {
            image,
            tilt,
            depth,
            wobble: 0.0,
            leaving: None,
            movie: None,
        }
    }

    #[test]
    fn adding_a_card_only_reduces_the_new_image() {
        let images: Vec<_> = (0..4)
            .map(|i| solid(900 + i * 20, 700 + i * 10, [90, 140, 200, 255]))
            .collect();
        let (width, height) = Renderer::pixmap_size(1.0);
        let mut pixmap = Pixmap::new(width, height).expect("pixmap");
        let mut renderer = Renderer::new();

        let three = scene(
            vec![
                card(images[0].clone(), 10.0, 2),
                card(images[1].clone(), -12.0, 1),
                card(images[2].clone(), 9.0, 0),
            ],
            Some("held"),
        );
        renderer.render(&three, &mut pixmap);
        let before = renderer.reductions;

        let four = scene(
            vec![
                card(images[0].clone(), 10.0, 3),
                card(images[1].clone(), -12.0, 2),
                card(images[2].clone(), 9.0, 1),
                card(images[3].clone(), -9.0, 0),
            ],
            Some("held"),
        );
        renderer.render(&four, &mut pixmap);
        assert_eq!(
            renderer.reductions - before,
            1,
            "only the added image should be reduced again"
        );
    }

    #[test]
    fn movie_layer_leaves_a_transparent_border() {
        let image = solid(1200, 900, [80, 150, 210, 255]);
        let mut movie = card(image, -4.0, 0);
        movie.movie = Some(Some(75.0));
        let scene = scene(vec![movie], None);
        let (width, height) = Renderer::pixmap_size(1.0);
        let mut pixmap = Pixmap::new(width, height).expect("pixmap");
        let mut renderer = Renderer::new();
        renderer.render(&scene, &mut pixmap);

        let layer = &renderer.layer;
        let (layer_w, layer_h) = (layer.width(), layer.height());
        for y in 0..layer_h {
            for x in 0..layer_w {
                if x < 2 || y < 2 || x >= layer_w - 2 || y >= layer_h - 2 {
                    assert_eq!(
                        layer.pixel(x, y).expect("pixel").alpha(),
                        0,
                        "movie card layer border must stay transparent at ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn buffers_do_not_resize_between_frames() {
        let image = solid(1200, 900, [80, 150, 210, 255]);
        let mut movie = card(image, -4.0, 0);
        movie.movie = Some(Some(75.0));
        let scene = scene(vec![movie], Some("3 held · shake to discard"));

        let (width, height) = Renderer::pixmap_size(1.0);
        let mut pixmap = Pixmap::new(width, height).expect("pixmap");
        let mut renderer = Renderer::new();
        renderer.render(&scene, &mut pixmap);

        let layer = (renderer.layer.width(), renderer.layer.height());
        let mask = (renderer.mask.width(), renderer.mask.height());
        let layer_ptr = renderer.layer.data().as_ptr();
        let mask_ptr = renderer.mask.data().as_ptr();
        let hint_sprite = Arc::as_ptr(&renderer.hint.as_ref().expect("hint sprite").pixmap);

        renderer.render(&scene, &mut pixmap);
        assert_eq!((renderer.layer.width(), renderer.layer.height()), layer);
        assert_eq!((renderer.mask.width(), renderer.mask.height()), mask);
        assert_eq!(renderer.layer.data().as_ptr(), layer_ptr);
        assert_eq!(renderer.mask.data().as_ptr(), mask_ptr);
        assert_eq!(
            Arc::as_ptr(&renderer.hint.as_ref().expect("hint sprite").pixmap),
            hint_sprite
        );
    }
}
