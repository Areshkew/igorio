//! The small looping pictures of the guide, ported from `GuideScenes.swift`: an empty stage,
//! one cursor arrow, and only what the gesture needs. Every scene is a pure function of time,
//! so a loop always plays the same way and any moment can be drawn on its own (in a test, or
//! as the still frame when motion is off).
//!
//! Coordinates are stage units (the Swift points): the stage is [`STAGE`] units, scaled to
//! pixels by [`Stage::scale`]. Shapes go straight into a tiny-skia pixmap; the few words a
//! scene shows (the recording timer, the typed note) come back as [`SceneText`] for the
//! window to draw with GDI, like every other label.

use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, PixmapMut, PixmapPaint, Rect,
    Stroke, Transform,
};

/// The stage every scene is drawn on, in stage units.
pub const STAGE: (f32, f32) = (440.0, 210.0);

/// Which picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    Screenshot,
    Drop,
    Shake,
    ToolMenu,
    Record,
}

impl Scene {
    /// How long one loop takes, in seconds.
    pub fn duration(self) -> f64 {
        match self {
            Scene::Screenshot => 5.2,
            Scene::Drop => 4.4,
            Scene::Shake => 3.0,
            Scene::ToolMenu => 6.0,
            Scene::Record => 7.4,
        }
    }

    /// The moment shown when motion is off: the gesture's result is visible.
    pub fn still(self) -> f64 {
        match self {
            Scene::Screenshot => 3.4,
            Scene::Drop => 1.9,
            Scene::Shake => 1.2,
            Scene::ToolMenu => 1.9,
            Scene::Record => 4.2,
        }
    }

    /// The opacity of the whole picture at `t` in its loop: it fades in and out across the seam.
    pub fn opacity(self, t: f64) -> f64 {
        ramp(t, 0.0, 0.3).min(1.0 - ramp(t, self.duration() - 0.35, self.duration()))
    }
}

/// Straight-alpha RGBA.
pub type Rgba = [u8; 4];

/// The colors a scene uses.
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    /// The stage behind everything.
    pub stage: Rgba,
    /// Cards, the text box and the note.
    pub paper: Rgba,
    /// The page's picture and its text lines.
    pub fill: Rgba,
    /// The picture glyph.
    pub faint: Rgba,
    /// Text, the caret.
    pub ink: Rgba,
    /// A carried screenshot's picture.
    pub picture: Rgba,
    /// The recording frame and the pen.
    pub accent: Rgba,
    /// The edge of a card or field.
    pub edge: Rgba,
    pub shadow: Rgba,
}

/// A word or two a scene shows, in stage units, for the window to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneText {
    pub rect: [f32; 4],
    pub text: String,
    /// Already mixed with what is behind it (GDI text has no alpha).
    pub color: Rgba,
    /// Left aligned (else centered).
    pub left: bool,
}

/// Where and how a scene is drawn.
pub struct Stage<'a, 'p> {
    pub pixmap: &'a mut PixmapMut<'p>,
    /// The stage's top-left corner, in pixels.
    pub origin: (f32, f32),
    /// Pixels per stage unit.
    pub scale: f32,
    pub colors: Colors,
    /// The pixmap is BGRA (a GDI DIB): red and blue are swapped when painting.
    pub bgr: bool,
    /// The tool menu, already drawn at `scale` in the pixmap's channel order (the app's own
    /// menu look, from `summoner::render_menu`), and where its first row's hot spot is in it.
    pub menu: Option<(&'a Pixmap, (f32, f32))>,
    pub texts: Vec<SceneText>,
}

// ---------------------------------------------------------------------------------------
// Motion
// ---------------------------------------------------------------------------------------

/// 0 before `a`, 1 after `b`, linear between.
pub fn ramp(t: f64, a: f64, b: f64) -> f64 {
    ((t - a) / (b - a)).clamp(0.0, 1.0)
}

pub fn in_out(p: f64) -> f64 {
    if p < 0.5 { 4.0 * p * p * p } else { 1.0 - (-2.0 * p + 2.0).powi(3) / 2.0 }
}

/// In and out with a hair of overshoot at the far end, so a stop looks like it has weight.
pub fn settle(p: f64) -> f64 {
    let (c, c3) = (1.15, 2.15);
    let out = 1.0 + c3 * (p - 1.0).powi(3) + c * (p - 1.0).powi(2);
    if p < 0.5 { in_out(p) } else { in_out(p) * (1.0 - (p - 0.5) * 2.0) + out * (p - 0.5) * 2.0 }
}

fn mix(a: f64, b: f64, p: f64) -> f64 {
    a + (b - a) * p
}

/// How far the cursor and what it carries lean into a sideways speed, in degrees.
fn lean(speed: f64, k: f64, limit: f64) -> f64 {
    (speed * k).clamp(-limit, limit)
}

/// Where the cursor is asked to be at each moment: points in time, eased between.
pub struct Track(pub &'static [(f64, f64, f64)]);

impl Track {
    pub fn at(&self, t: f64) -> (f64, f64) {
        let keys = self.0;
        let (first, last) = (keys[0], keys[keys.len() - 1]);
        if t <= first.0 {
            return (first.1, first.2);
        }
        if t >= last.0 {
            return (last.1, last.2);
        }
        let i = keys.iter().position(|k| k.0 > t).unwrap_or(keys.len() - 1);
        let (a, b) = (keys[i - 1], keys[i]);
        let e = settle((t - a.0) / (b.0 - a.0));
        (mix(a.1, b.1, e), mix(a.2, b.2, e))
    }

    /// Sideways speed, units per second.
    pub fn speed_x(&self, t: f64) -> f64 {
        (self.at(t + 0.02).0 - self.at(t - 0.02).0) / 0.04
    }
}

// ---------------------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------------------

/// A carried screenshot's size, and where it hangs from the cursor's tip.
const CARRIED: (f64, f64) = (84.0, 54.0);
const HAND: (f64, f64) = (17.0, 20.0);
/// The page's picture: the thing worth a screenshot.
const PICTURE: [f64; 4] = [44.0, 34.0, 168.0, 108.0];

fn with_alpha(color: Rgba, alpha: f64) -> Rgba {
    [color[0], color[1], color[2], (f64::from(color[3]) * alpha.clamp(0.0, 1.0)).round() as u8]
}

fn rounded(x: f64, y: f64, w: f64, h: f64, r: f64) -> Option<Path> {
    let (x, y, w, h) = (x as f32, y as f32, w.max(0.5) as f32, h.max(0.5) as f32);
    let r = (r as f32).min(w / 2.0).min(h / 2.0).max(0.0);
    let k = r * 0.552_284_8;
    let (l, t, rr, b) = (x, y, x + w, y + h);
    let mut p = PathBuilder::new();
    p.move_to(l + r, t);
    p.line_to(rr - r, t);
    p.cubic_to(rr - r + k, t, rr, t + r - k, rr, t + r);
    p.line_to(rr, b - r);
    p.cubic_to(rr, b - r + k, rr - r + k, b, rr - r, b);
    p.line_to(l + r, b);
    p.cubic_to(l + r - k, b, l, b - r + k, l, b - r);
    p.line_to(l, t + r);
    p.cubic_to(l, t + r - k, l + r - k, t, l + r, t);
    p.close();
    p.finish()
}

fn circle(x: f64, y: f64, r: f64) -> Option<Path> {
    PathBuilder::from_circle(x as f32, y as f32, r.max(0.1) as f32)
}

impl Stage<'_, '_> {
    fn base(&self) -> Transform {
        Transform::from_translate(self.origin.0, self.origin.1).pre_scale(self.scale, self.scale)
    }

    fn paint(&self, color: Rgba, alpha: f64) -> Paint<'static> {
        let [r, g, b, a] = with_alpha(color, alpha);
        let mut paint = Paint::default();
        if self.bgr {
            paint.set_color_rgba8(b, g, r, a);
        } else {
            paint.set_color_rgba8(r, g, b, a);
        }
        paint.anti_alias = true;
        paint
    }

    fn fill(&mut self, path: Option<Path>, color: Rgba, alpha: f64, local: Transform) {
        if alpha <= 0.0 {
            return;
        }
        if let Some(path) = path {
            let paint = self.paint(color, alpha);
            let transform = self.base().pre_concat(local);
            self.pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
        }
    }

    fn stroke(&mut self, path: Option<Path>, color: Rgba, alpha: f64, width: f64, local: Transform) {
        if alpha <= 0.0 {
            return;
        }
        if let Some(path) = path {
            let paint = self.paint(color, alpha);
            let stroke = Stroke {
                width: width as f32,
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                ..Stroke::default()
            };
            let transform = self.base().pre_concat(local);
            self.pixmap.stroke_path(&path, &paint, &stroke, transform, None);
        }
    }

    /// A text the window draws later, mixed over the stage color.
    fn text(&mut self, rect: [f64; 4], text: &str, color: Rgba, over: Rgba, alpha: f64, left: bool) {
        let a = alpha * f64::from(color[3]) / 255.0;
        if a <= 0.02 || text.is_empty() {
            return;
        }
        let blend = |i: usize| (f64::from(over[i]) + (f64::from(color[i]) - f64::from(over[i])) * a).round() as u8;
        self.texts.push(SceneText {
            rect: rect.map(|v| v as f32),
            text: text.to_owned(),
            color: [blend(0), blend(1), blend(2), 255],
            left,
        });
    }

    /// A soft shadow under a rounded shape: a few spreading layers.
    #[allow(clippy::too_many_arguments)]
    fn shadow(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, alpha: f64, local: Transform) {
        let color = self.colors.shadow;
        for (spread, dy, a) in [(6.0, 6.0, 0.05), (3.5, 4.5, 0.07), (1.5, 2.5, 0.09), (0.5, 1.0, 0.08)] {
            let path = rounded(x - spread, y - spread + dy, w + 2.0 * spread, h + 2.0 * spread, r + spread);
            self.fill(path, color, alpha * a, local);
        }
    }

    /// A small landscape glyph: a hill and a sun.
    fn glyph(&mut self, cx: f64, cy: f64, size: f64, color: Rgba, alpha: f64) {
        let s = size / 26.0;
        let mut hill = PathBuilder::new();
        hill.move_to((cx - 12.0 * s) as f32, (cy + 8.0 * s) as f32);
        hill.line_to((cx - 4.0 * s) as f32, (cy - 1.0 * s) as f32);
        hill.line_to((cx + 1.0 * s) as f32, (cy + 4.0 * s) as f32);
        hill.line_to((cx + 5.0 * s) as f32, (cy) as f32);
        hill.line_to((cx + 12.0 * s) as f32, (cy + 8.0 * s) as f32);
        self.stroke(hill.finish(), color, alpha, 1.6 * s.max(0.6), Transform::identity());
        let frame = rounded(cx - 13.0 * s, cy - 10.0 * s, 26.0 * s, 20.0 * s, 3.0 * s);
        self.stroke(frame, color, alpha, 1.4 * s.max(0.6), Transform::identity());
        self.fill(circle(cx + 5.5 * s, cy - 4.5 * s, 2.2 * s), color, alpha, Transform::identity());
    }

    /// The page: a picture and two lines of text under it.
    fn page(&mut self, glyph: bool, alpha: f64) {
        let [x, y, w, h] = PICTURE;
        let fill = self.colors.fill;
        self.fill(rounded(x, y, w, h, 8.0), fill, alpha, Transform::identity());
        if glyph {
            let faint = self.colors.faint;
            self.glyph(x + w / 2.0, y + h / 2.0, 34.0, faint, alpha * 0.75);
        }
        self.fill(rounded(44.0, 156.0, 168.0, 7.0, 3.5), fill, alpha, Transform::identity());
        self.fill(rounded(44.0, 171.0, 112.0, 7.0, 3.5), fill, alpha, Transform::identity());
    }

    /// A screenshot in hand: the picture with a paper edge and a shadow, its top-left corner
    /// at `(x, y)`, turned `degrees` about that corner.
    #[allow(clippy::too_many_arguments)]
    fn sheet(&mut self, x: f64, y: f64, w: f64, h: f64, degrees: f64, alpha: f64) {
        let turn = Transform::from_rotate_at(degrees as f32, x as f32, y as f32);
        self.shadow(x, y, w, h, 7.0, alpha, turn);
        let paper = self.colors.paper;
        self.fill(rounded(x - 2.5, y - 2.5, w + 5.0, h + 5.0, 9.0), paper, alpha, turn);
        let picture = self.colors.picture;
        self.fill(rounded(x, y, w, h, 7.0), picture, alpha, turn);
        // A sun and a hill, like the settings window's card preview.
        let accent = self.colors.accent;
        let r = (h * 0.09).max(2.0);
        self.fill(circle(x + w - r * 2.6, y + r * 2.4, r), accent, alpha, turn);
        let mut hill = PathBuilder::new();
        hill.move_to((x + w * 0.12) as f32, (y + h * 0.78) as f32);
        hill.line_to((x + w * 0.38) as f32, (y + h * 0.48) as f32);
        hill.line_to((x + w * 0.55) as f32, (y + h * 0.66) as f32);
        hill.line_to((x + w * 0.68) as f32, (y + h * 0.56) as f32);
        hill.line_to((x + w * 0.88) as f32, (y + h * 0.78) as f32);
        let faint = self.colors.faint;
        self.stroke(hill.finish(), faint, alpha, (h * 0.045).max(1.0), turn);
    }

    /// A text box: somewhere a double-click can put things.
    fn field(&mut self, x: f64, y: f64, caret_x: f64, caret: bool, alpha: f64) {
        let (paper, edge, ink) = (self.colors.paper, self.colors.edge, self.colors.ink);
        self.fill(rounded(x - 1.0, y - 1.0, 162.0, 46.0, 12.0), edge, alpha, Transform::identity());
        self.fill(rounded(x, y, 160.0, 44.0, 11.0), paper, alpha, Transform::identity());
        if caret {
            self.fill(rounded(x + caret_x, y + 13.5, 1.5, 17.0, 0.0), ink, alpha, Transform::identity());
        }
    }

    /// The Windows arrow: white with a dark edge. Its tip is at `(x, y)`; the body leans as
    /// it moves and shrinks a little on a click.
    fn cursor(&mut self, x: f64, y: f64, degrees: f64, size: f64, alpha: f64) {
        const ARROW: [(f32, f32); 7] =
            [(0.0, 0.0), (0.0, 17.0), (4.2, 13.2), (7.0, 19.6), (9.6, 18.5), (6.9, 12.3), (12.2, 12.3)];
        let mut arrow = PathBuilder::new();
        arrow.move_to(ARROW[0].0, ARROW[0].1);
        for (px, py) in &ARROW[1..] {
            arrow.line_to(*px, *py);
        }
        arrow.close();
        let at = Transform::from_translate(x as f32, y as f32)
            .pre_rotate(degrees as f32)
            .pre_scale(size as f32 * 1.15, size as f32 * 1.15);
        let path = arrow.finish();
        let shadow = self.colors.shadow;
        self.fill(path.clone(), shadow, alpha * 0.22, Transform::from_translate(0.0, 1.5).pre_concat(at));
        self.fill(path.clone(), [255, 255, 255, 255], alpha, at);
        self.stroke(path, [0, 0, 0, 255], alpha, 1.0 / 1.15, at);
    }
}

/// The text box's caret blinks once a second, and stays on while it is in use.
fn blink(t: f64) -> bool {
    t.rem_euclid(1.0) < 0.5
}

// ---------------------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------------------

/// Draws `scene` as it looks `t` seconds into its loop, at `alpha`; `faded` also fades it
/// across the loop's seam.
pub fn draw(scene: Scene, t: f64, stage: &mut Stage<'_, '_>, faded: bool, alpha: f64) {
    let alpha = alpha * if faded { scene.opacity(t) } else { 1.0 };
    match scene {
        Scene::Screenshot => screenshot(t, stage, alpha),
        Scene::Drop => double_click(t, stage, alpha),
        Scene::Shake => shake(t, stage, alpha),
        Scene::ToolMenu => tool_menu(t, stage, alpha),
        Scene::Record => record(t, stage, alpha),
    }
}

/// Win+Shift+S: drag a box over the picture, and it comes away in the cursor's hand.
fn screenshot(t: f64, s: &mut Stage<'_, '_>, alpha: f64) {
    let [px, py, pw, ph] = PICTURE;
    let from = (px - 6.0, py - 6.0);
    let to = (px + pw + 6.0, py + ph + 6.0);
    const TRACK: Track = Track(&[
        (0.0, 270.0, 160.0),
        (0.8, 38.0, 28.0),
        (1.0, 38.0, 28.0),
        (1.8, 218.0, 148.0),
        (2.25, 218.0, 148.0),
        (3.2, 318.0, 84.0),
        (9.0, 318.0, 84.0),
    ]);
    let c = TRACK.at(t);
    let dragging = (1.0..1.85).contains(&t);
    let taken = settle(ramp(t, 1.85, 2.5));
    let h = TRACK.at(t - 0.07);
    let size = (mix(to.0 - from.0, CARRIED.0, taken), mix(to.1 - from.1, CARRIED.1, taken));
    let origin = (mix(from.0, h.0 + HAND.0, taken), mix(from.1, h.1 + HAND.1, taken));
    s.page(true, alpha);
    if (1.0..2.2).contains(&t) {
        let end = if dragging { c } else { to };
        let (w, hh) = ((end.0 - from.0).max(1.0), (end.1 - from.1).max(1.0));
        let fade = alpha * (1.0 - ramp(t, 1.85, 2.15));
        let ink = s.colors.ink;
        s.fill(rounded(from.0, from.1, w, hh, 2.0), ink, fade * 0.06, Transform::identity());
        s.stroke(rounded(from.0, from.1, w, hh, 2.0), ink, fade * 0.45, 1.0, Transform::identity());
    }
    if t >= 1.85 {
        let degrees = -5.0 * taken + lean(TRACK.speed_x(t - 0.07), 0.014, 11.0);
        s.sheet(origin.0, origin.1, size.0, size.1, degrees, alpha);
    }
    let scale = if dragging { 0.86 } else { 1.0 };
    s.cursor(c.0, c.1, lean(TRACK.speed_x(t), 0.016, 13.0), scale, alpha);
}

/// Double-click a text box and what the cursor carries goes in.
fn double_click(t: f64, s: &mut Stage<'_, '_>, alpha: f64) {
    const TRACK: Track = Track(&[(0.0, 96.0, 132.0), (0.5, 96.0, 132.0), (1.5, 330.0, 104.0), (9.0, 330.0, 104.0)]);
    let (c, h) = (TRACK.at(t), TRACK.at(t - 0.07));
    let down = (1.75..1.86).contains(&t) || (1.98..2.09).contains(&t);
    let placed = settle(ramp(t, 2.1, 2.55));
    let field = (246.0, 80.0);
    let small = (42.0, 27.0);
    let origin = (mix(h.0 + HAND.0, field.0 + 9.0, placed), mix(h.1 + HAND.1, field.1 + 8.5, placed));
    let size = (mix(CARRIED.0, small.0, placed), mix(CARRIED.1, small.1, placed));
    s.field(field.0, field.1, if placed > 0.5 { 60.0 } else { 14.0 }, blink(t) || t < 2.6, alpha);
    let degrees = (-5.0 + lean(TRACK.speed_x(t - 0.07), 0.014, 11.0)) * (1.0 - placed);
    s.sheet(origin.0, origin.1, size.0, size.1, degrees, alpha);
    s.cursor(c.0, c.1, lean(TRACK.speed_x(t), 0.016, 13.0), if down { 0.84 } else { 1.0 }, alpha);
}

/// Shake, and the cursor lets go: the picture falls behind and fades.
fn shake(t: f64, s: &mut Stage<'_, '_>, alpha: f64) {
    let x = |t: f64| {
        let p = ramp(t, 0.8, 1.9);
        196.0 + if p > 0.0 && p < 1.0 { (p * std::f64::consts::PI * 7.0).sin() * 44.0 * (1.0 - p * 0.25) } else { 0.0 }
    };
    let (c, h) = ((x(t), 78.0), (x(t - 0.07), 78.0));
    let speed = (x(t + 0.02) - x(t - 0.02)) / 0.04;
    let gone = ramp(t, 1.4, 1.95);
    // Once let go it stays where it was dropped, a little lower, and fades.
    let drop_x = x(1.4 - 0.07);
    let origin = (
        if gone > 0.0 { drop_x + HAND.0 + 6.0 * gone } else { h.0 + HAND.0 },
        h.1 + HAND.1 + 26.0 * in_out(gone),
    );
    let degrees = if gone > 0.0 {
        -5.0 * (1.0 - gone)
    } else {
        -5.0 + lean((x(t - 0.05) - x(t - 0.09)) / 0.04, 0.014, 11.0)
    };
    s.sheet(origin.0, origin.1, CARRIED.0, CARRIED.1, degrees, alpha * (1.0 - gone));
    s.cursor(c.0, c.1, lean(speed, 0.016, 13.0), 1.0, alpha);
}

/// Hold Alt: the tool menu opens under the cursor, New note is picked, and a note opens there.
fn tool_menu(t: f64, s: &mut Stage<'_, '_>, alpha: f64) {
    let spot = (118.0, 58.0);
    const TRACK: Track = Track(&[(0.0, 60.0, 176.0), (0.8, 118.0, 58.0), (2.4, 118.0, 58.0), (3.2, 396.0, 176.0), (9.0, 396.0, 176.0)]);
    let c = TRACK.at(t);
    // Alt goes down once the cursor is still, and comes up after the click.
    let alt = ramp(t, 0.9, 1.0) * (1.0 - ramp(t, 2.1, 2.2));
    let menu = ramp(t, 1.2, 1.32) * (1.0 - ramp(t, 1.9, 1.98));
    let down = (1.78..1.9).contains(&t);
    let open = ramp(t, 1.95, 2.3);
    let grow = 0.3 + 0.7 * settle(open);

    // The note: grows from where the menu was picked, then the typing starts.
    if open > 0.0 {
        let (w, h) = (176.0 * grow, 108.0 * grow);
        let a = alpha * open;
        s.shadow(spot.0, spot.1, w, h, 12.0, a, Transform::identity());
        let (paper, edge, fill, stage) = (s.colors.paper, s.colors.edge, s.colors.fill, s.colors.stage);
        s.fill(rounded(spot.0 - 0.5, spot.1 - 0.5, w + 1.0, h + 1.0, 12.5), edge, a * 0.6, Transform::identity());
        s.fill(rounded(spot.0, spot.1, w, h, 12.0), paper, a, Transform::identity());
        // The header: where the note was made.
        let header = ramp(t, 2.25, 2.5);
        s.fill(rounded(spot.0 + 12.0, spot.1 + 12.0, 66.0, 14.0, 7.0), fill, a * header, Transform::identity());
        let text = "look at this again";
        let typed = ((t - 2.6) / 0.07).max(0.0) as usize;
        let shown: String = text.chars().take(typed).collect();
        if open >= 1.0 {
            let ink = s.colors.ink;
            let mixed = blend(stage, paper, a);
            s.text([spot.0 + 12.0, spot.1 + 34.0, 156.0, 20.0], &shown, ink, mixed, a, true);
        }
    }
    // The menu: the app's own look, its first row under the cursor.
    if menu > 0.0
        && let Some((picture, (hx, hy))) = s.menu
    {
        let rise = 4.0 * (1.0 - menu);
        let x = (f64::from(s.origin.0) + spot.0 * f64::from(s.scale) - f64::from(hx)).round() as i32;
        let y = (f64::from(s.origin.1) + (spot.1 + rise) * f64::from(s.scale) - f64::from(hy)).round() as i32;
        let paint = PixmapPaint { opacity: (alpha * menu) as f32, ..PixmapPaint::default() };
        s.pixmap.draw_pixmap(x, y, picture.as_ref(), &paint, Transform::identity(), None);
    }
    // The Alt key, held, next to the cursor.
    if alt > 0.0 {
        let (kx, ky) = (spot.0 - 52.0, spot.1 - 6.0);
        let (paper, edge, ink, stage) = (s.colors.paper, s.colors.edge, s.colors.ink, s.colors.stage);
        let a = alpha * alt;
        s.fill(rounded(kx, ky + 1.5, 38.0, 24.0, 6.0), edge, a * 0.7, Transform::identity());
        s.fill(rounded(kx, ky, 38.0, 23.0, 6.0), paper, a, Transform::identity());
        s.text([kx, ky, 38.0, 23.0], "Alt", ink, blend(stage, paper, a), a, false);
    }
    let pick = if down { 0.84 } else { 1.0 };
    s.cursor(c.0, c.1, lean(TRACK.speed_x(t), 0.016, 13.0), pick, alpha);
}

/// The record shortcut: a blue outline shows what is in the video. It zooms in around the
/// cursor, the pen draws on screen, then the whole screen comes back.
fn record(t: f64, s: &mut Stage<'_, '_>, alpha: f64) {
    let whole = [22.0, 20.0, 396.0, 176.0];
    let close = [22.0, 32.0, 252.0, 252.0 * 176.0 / 396.0];
    let face = (PICTURE[0] + PICTURE[2] / 2.0, PICTURE[1] + PICTURE[3] / 2.0 - 2.0);
    let zoom = in_out(ramp(t, 1.7, 2.3)) - in_out(ramp(t, 4.9, 5.5));
    let area: [f64; 4] = std::array::from_fn(|i| mix(whole[i], close[i], zoom));
    let on = ramp(t, 0.6, 0.85);
    // The pen: two eyes, then a smile.
    let eye1 = ramp(t, 2.9, 3.05);
    let eye2 = ramp(t, 3.2, 3.35);
    let smile = in_out(ramp(t, 3.5, 4.0));
    let smile_at = |p: f64| {
        let a = (158.0 - p * 136.0).to_radians();
        (face.0 + 20.0 * a.cos(), face.1 - 3.0 + 20.0 * a.sin())
    };
    let drawing = (2.7..4.3).contains(&t);
    const TRACK: Track = Track(&[(0.0, 310.0, 160.0), (0.6, 310.0, 160.0), (1.5, 128.0, 86.0), (2.7, 128.0, 86.0), (2.9, 119.0, 71.0)]);
    let mut c = TRACK.at(t);
    let between = |t: f64, a: (f64, f64, f64), b: (f64, f64, f64)| {
        let p = settle(ramp(t, a.0, b.0));
        (mix(a.1, b.1, p), mix(a.2, b.2, p))
    };
    if t >= 2.9 {
        c = if t < 3.05 {
            (face.0 - 9.0, face.1 - 15.0 + 9.0 * eye1)
        } else if t < 3.2 {
            between(t, (3.05, face.0 - 9.0, face.1 - 6.0), (3.2, face.0 + 9.0, face.1 - 15.0))
        } else if t < 3.35 {
            (face.0 + 9.0, face.1 - 15.0 + 9.0 * eye2)
        } else if t < 3.5 {
            let s0 = smile_at(0.0);
            between(t, (3.35, face.0 + 9.0, face.1 - 6.0), (3.5, s0.0, s0.1))
        } else if t < 4.3 {
            smile_at(smile)
        } else {
            let s1 = smile_at(1.0);
            between(t, (4.3, s1.0, s1.1), (5.0, 310.0, 150.0))
        };
    }
    let ink = alpha * (1.0 - ramp(t, 6.6, 6.9));
    s.page(false, alpha);
    let mut pen = PathBuilder::new();
    if eye1 > 0.0 {
        pen.move_to((face.0 - 9.0) as f32, (face.1 - 15.0) as f32);
        pen.line_to((face.0 - 9.0) as f32, (face.1 - 15.0 + 9.0 * eye1) as f32);
    }
    if eye2 > 0.0 {
        pen.move_to((face.0 + 9.0) as f32, (face.1 - 15.0) as f32);
        pen.line_to((face.0 + 9.0) as f32, (face.1 - 15.0 + 9.0 * eye2) as f32);
    }
    if smile > 0.0 {
        let start = smile_at(0.0);
        pen.move_to(start.0 as f32, start.1 as f32);
        for i in 1..=24 {
            let p = f64::from(i) / 24.0;
            if p <= smile {
                let at = smile_at(p);
                pen.line_to(at.0 as f32, at.1 as f32);
            }
        }
        let end = smile_at(smile);
        pen.line_to(end.0 as f32, end.1 as f32);
    }
    let accent = s.colors.accent;
    s.stroke(pen.finish(), accent, ink, 3.6, Transform::identity());
    // The outline of what is recorded.
    let [ax, ay, aw, ah] = area;
    s.stroke(rounded(ax + 1.25, ay + 1.25, aw - 2.5, ah - 2.5, 8.0), accent, alpha * on, 2.5, Transform::identity());
    // The timer pill on the outline's corner.
    let (pill_x, pill_y) = (whole[0] + whole[2] - 62.0, whole[1] - 9.0);
    let (paper, stage, text) = (s.colors.paper, s.colors.stage, s.colors.ink);
    s.shadow(pill_x, pill_y, 54.0, 20.0, 10.0, alpha * on * 0.6, Transform::identity());
    s.fill(rounded(pill_x, pill_y, 54.0, 20.0, 10.0), paper, alpha * on, Transform::identity());
    let pulse = 0.45 + 0.55 * (t * 1.3).sin().abs();
    s.fill(circle(pill_x + 11.0, pill_y + 10.0, 3.25), accent, alpha * on * pulse, Transform::identity());
    let seconds = ((t - 0.6).max(0.0) as u32).min(9);
    let label = format!("0:0{seconds}");
    s.text([pill_x + 17.0, pill_y, 34.0, 20.0], &label, text, blend(stage, paper, alpha * on), alpha * on, false);
    if drawing {
        // The pen's tip instead of the cursor.
        let white = [255, 255, 255, 255];
        s.fill(circle(c.0, c.1, 6.5), white, alpha, Transform::identity());
        s.fill(circle(c.0, c.1, 4.5), accent, alpha, Transform::identity());
    } else {
        // The cursor is drawn bigger while a recording runs.
        s.cursor(c.0, c.1, lean(TRACK.speed_x(t), 0.016, 13.0), 1.0 + 0.3 * on, alpha);
    }
}

/// `top` over `under` at `amount`, as an opaque color.
fn blend(under: Rgba, top: Rgba, amount: f64) -> Rgba {
    let a = amount.clamp(0.0, 1.0);
    let mix = |i: usize| (f64::from(under[i]) + (f64::from(top[i]) - f64::from(under[i])) * a).round() as u8;
    [mix(0), mix(1), mix(2), 255]
}

/// Fills the stage rectangle in `color` (used by tests and the still frame).
pub fn clear(pixmap: &mut PixmapMut<'_>, color: Rgba) {
    if let Some(rect) = Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLORS: Colors = Colors {
        stage: [244, 247, 251, 255],
        paper: [255, 255, 255, 255],
        fill: [233, 238, 246, 255],
        faint: [178, 186, 199, 255],
        ink: [42, 51, 70, 255],
        picture: [220, 233, 252, 255],
        accent: [59, 130, 246, 255],
        edge: [118, 131, 152, 255],
        shadow: [28, 38, 58, 255],
    };

    fn render(scene: Scene, t: f64) -> (Pixmap, Vec<SceneText>) {
        let mut pixmap = Pixmap::new(440, 210).unwrap();
        let mut target = pixmap.as_mut();
        clear(&mut target, COLORS.stage);
        let mut stage = Stage {
            pixmap: &mut target,
            origin: (0.0, 0.0),
            scale: 1.0,
            colors: COLORS,
            bgr: false,
            menu: None,
            texts: Vec::new(),
        };
        draw(scene, t, &mut stage, true, 1.0);
        let texts = std::mem::take(&mut stage.texts);
        (pixmap, texts)
    }

    fn white_at(pixmap: &Pixmap, x: u32, y: u32) -> bool {
        let p = pixmap.pixel(x, y).unwrap().demultiply();
        p.red() > 250 && p.green() > 250 && p.blue() > 250
    }

    #[test]
    fn a_scene_is_the_same_picture_for_the_same_moment() {
        for scene in [Scene::Screenshot, Scene::Drop, Scene::Shake, Scene::ToolMenu, Scene::Record] {
            for t in [0.0, scene.still(), scene.duration() * 0.8] {
                let (a, ta) = render(scene, t);
                let (b, tb) = render(scene, t);
                assert_eq!(a.data(), b.data(), "{scene:?} at {t}");
                assert_eq!(ta, tb);
            }
        }
    }

    #[test]
    fn the_cursor_follows_its_track_and_the_loop_fades_at_the_seam() {
        // The screenshot scene: the cursor rests at (318, 84) once the shot is in hand; its
        // white body is just below and right of the tip.
        let (still, _) = render(Scene::Screenshot, 3.4);
        assert!(white_at(&still, 320, 92));
        // At the seam the whole picture is faded out: only the stage shows.
        let (seam, _) = render(Scene::Screenshot, 0.0);
        let stage = seam.pixel(0, 0).unwrap();
        assert!(seam.pixels().iter().all(|p| *p == stage));
        assert_eq!(Scene::Drop.opacity(0.0), 0.0);
        assert_eq!(Scene::Drop.opacity(2.0), 1.0);
        assert_eq!(Scene::Drop.opacity(Scene::Drop.duration()), 0.0);
    }

    #[test]
    fn the_record_scene_counts_seconds_and_the_note_types_its_text() {
        let (_, texts) = render(Scene::Record, 4.2);
        assert!(texts.iter().any(|text| text.text == "0:03"));
        let (_, texts) = render(Scene::ToolMenu, 3.5);
        let typed = texts.iter().find(|text| text.left).expect("the note's text");
        assert!(typed.text.starts_with("look at"));
        let (_, texts) = render(Scene::ToolMenu, 1.0);
        assert!(texts.iter().any(|text| text.text == "Alt"), "Alt is held before the menu opens");
    }

    #[test]
    fn tracks_ease_between_their_keys() {
        const TRACK: Track = Track(&[(0.0, 0.0, 0.0), (1.0, 100.0, 50.0)]);
        assert_eq!(TRACK.at(-1.0), (0.0, 0.0));
        assert_eq!(TRACK.at(2.0), (100.0, 50.0));
        let (x, _) = TRACK.at(0.5);
        assert!((x - 50.0).abs() < 1.0);
        assert!(TRACK.speed_x(0.5) > 0.0);
        assert_eq!(ramp(0.5, 0.0, 1.0), 0.5);
        assert_eq!(settle(0.0), 0.0);
        assert!((settle(1.0) - 1.0).abs() < 1e-9);
    }
}
