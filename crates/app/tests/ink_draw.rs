//! Pixel-level checks for the pen and highlighter renderer.

use igorio_app::ink_draw::{InkRenderer, tool_cursor};
use igorio_core::geom::Point;
use igorio_core::ink::{Ink, InkKey, InkTool};
use tiny_skia::Pixmap;

fn take(ink: &mut Ink, tool: InkTool) {
    if ink.tool() == Some(tool) {
        return;
    }
    ink.press(match tool {
        InkTool::Pen => InkKey::Pen,
        InkTool::Highlighter => InkKey::Highlighter,
    });
}

fn add_stroke(ink: &mut Ink, tool: InkTool, points: &[Point]) {
    take(ink, tool);
    ink.begin_stroke(points[0]);
    for point in &points[1..] {
        ink.extend_stroke(*point);
    }
    ink.end_stroke();
}

fn line(x0: f64, y0: f64, x1: f64, y1: f64, step: f64) -> Vec<Point> {
    let length = (x1 - x0).hypot(y1 - y0);
    let count = (length / step).ceil().max(1.0) as usize;
    (0..=count)
        .map(|i| {
            let t = i as f64 / count as f64;
            Point::new(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
        })
        .collect()
}

fn render(ink: &Ink, scale: f64, opacity: f64, width: u32, height: u32) -> Pixmap {
    let mut pixmap = Pixmap::new(width, height).expect("pixmap");
    InkRenderer::new().render(ink, scale, opacity, &mut pixmap);
    pixmap
}

fn alpha(pixmap: &Pixmap, x: u32, y: u32) -> u8 {
    pixmap.pixel(x, y).expect("pixel").alpha()
}

fn nonzero_pixels(pixmap: &Pixmap) -> usize {
    pixmap
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[3] != 0)
        .count()
}

fn all_transparent(pixmap: &Pixmap) -> bool {
    pixmap.data().as_chunks::<4>().0.iter().all(|p| p[3] == 0)
}

fn assert_premultiplied(pixmap: &Pixmap) {
    for pixel in pixmap.pixels() {
        assert!(
            pixel.red() <= pixel.alpha()
                && pixel.green() <= pixel.alpha()
                && pixel.blue() <= pixel.alpha(),
            "non-premultiplied pixel {pixel:?}"
        );
    }
}

/// Largest per-channel difference between two pixmaps. Panics on a size mismatch.
fn max_difference(a: &Pixmap, b: &Pixmap) -> u8 {
    assert_eq!(
        (a.width(), a.height()),
        (b.width(), b.height()),
        "pixmap sizes differ"
    );
    a.data()
        .iter()
        .zip(b.data())
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

fn assert_same(a: &Pixmap, b: &Pixmap, tolerance: u8) {
    let difference = max_difference(a, b);
    assert!(
        difference <= tolerance,
        "pixmaps differ by {difference} (tolerance {tolerance})"
    );
}

#[test]
fn empty_ink_is_transparent() {
    let ink = Ink::new();
    let mut pixmap = Pixmap::new(200, 100).expect("pixmap");
    let mut renderer = InkRenderer::new();
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    assert!(all_transparent(&pixmap));
    assert_eq!(renderer.damage(), None);
}

#[test]
fn zero_or_nan_scale_draws_nothing() {
    let mut ink = Ink::new();
    add_stroke(&mut ink, InkTool::Pen, &line(20.0, 50.0, 180.0, 50.0, 4.0));
    for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let pixmap = render(&ink, scale, 1.0, 200, 100);
        assert!(all_transparent(&pixmap), "scale {scale} drew something");
    }
}

#[test]
fn premultiplied_invariant_holds_for_busy_ink() {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(20.0, 30.0, 360.0, 30.0, 4.0),
    );
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(20.0, 90.0, 360.0, 120.0, 4.0),
    );
    add_stroke(&mut ink, InkTool::Pen, &line(30.0, 150.0, 350.0, 20.0, 5.0));
    add_stroke(&mut ink, InkTool::Pen, &line(30.0, 60.0, 350.0, 140.0, 5.0));
    let pixmap = render(&ink, 1.5, 0.8, 600, 250);
    assert_premultiplied(&pixmap);
}

#[test]
fn the_same_ink_renders_the_same_bytes() {
    let mut ink = Ink::new();
    add_stroke(&mut ink, InkTool::Pen, &line(20.0, 50.0, 180.0, 40.0, 3.0));
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(20.0, 90.0, 180.0, 90.0, 3.0),
    );
    let first = render(&ink, 1.0, 1.0, 200, 140);
    let second = render(&ink, 1.0, 1.0, 200, 140);
    assert_same(&first, &second, 0);

    // The same renderer must also fill a new surface, not trust the previous one's pixels.
    let mut renderer = InkRenderer::new();
    let mut owned_first = Pixmap::new(200, 140).expect("pixmap");
    renderer.render(&ink, 1.0, 1.0, &mut owned_first);
    let mut owned_second = Pixmap::new(200, 140).expect("pixmap");
    renderer.render(&ink, 1.0, 1.0, &mut owned_second);
    assert_same(&owned_first, &owned_second, 0);
    assert_eq!(renderer.damage(), Some((0, 0, 200, 140)));
}

#[test]
fn points_outside_the_pixmap_do_not_panic() {
    let mut ink = Ink::new();
    // Crosses the area and leaves it on both ends.
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(-1000.0, -1000.0, 5000.0, 5000.0, 20.0),
    );
    let crossed = render(&ink, 1.0, 1.0, 120, 80);
    assert_premultiplied(&crossed);

    let mut outside = Ink::new();
    add_stroke(
        &mut outside,
        InkTool::Pen,
        &line(2000.0, 2000.0, 3000.0, 2600.0, 20.0),
    );
    let away = render(&outside, 1.0, 1.0, 120, 80);
    assert!(all_transparent(&away));
}

#[test]
fn pen_is_opaque_along_the_path_and_clear_away_from_it() {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(40.0, 100.0, 360.0, 100.0, 3.0),
    );
    let pixmap = render(&ink, 1.0, 1.0, 400, 200);
    assert!(
        alpha(&pixmap, 200, 100) >= 200,
        "pen centre is too thin: {}",
        alpha(&pixmap, 200, 100)
    );
    assert!(alpha(&pixmap, 100, 100) >= 200);
    assert_eq!(alpha(&pixmap, 200, 150), 0, "ink found far from the stroke");
    assert_eq!(alpha(&pixmap, 200, 50), 0);
}

#[test]
fn highlighter_does_not_darken_where_it_crosses_itself() {
    let mut ink = Ink::new();
    // One stroke: a horizontal line, then a loop whose vertical part crosses it at x = 100.
    let points = [
        Point::new(20.0, 50.0),
        Point::new(180.0, 50.0),
        Point::new(180.0, 85.0),
        Point::new(100.0, 85.0),
        Point::new(100.0, 15.0),
        Point::new(60.0, 15.0),
    ];
    add_stroke(&mut ink, InkTool::Highlighter, &points);
    let pixmap = render(&ink, 1.0, 1.0, 220, 110);
    let plain = alpha(&pixmap, 50, 50);
    let crossing = alpha(&pixmap, 100, 50);
    assert!(
        (95..=99).contains(&plain),
        "unexpected highlighter alpha {plain}"
    );
    assert!(
        crossing.abs_diff(plain) <= 1,
        "self-crossing darkened: {plain} vs {crossing}"
    );
}

#[test]
fn two_highlighter_strokes_darken_where_they_cross() {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(20.0, 50.0, 180.0, 50.0, 4.0),
    );
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(100.0, 10.0, 100.0, 100.0, 4.0),
    );
    let pixmap = render(&ink, 1.0, 1.0, 220, 120);
    let one = alpha(&pixmap, 60, 50);
    let two = alpha(&pixmap, 100, 50);
    assert!((95..=99).contains(&one), "unexpected single alpha {one}");
    assert!(
        two > 140,
        "two highlighters did not darken at the crossing: {one} vs {two}"
    );
}

#[test]
fn growing_a_stroke_matches_a_fresh_render() {
    let mut ink = Ink::new();
    add_stroke(&mut ink, InkTool::Pen, &line(20.0, 30.0, 380.0, 30.0, 4.0));
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(20.0, 80.0, 380.0, 80.0, 4.0),
    );
    take(&mut ink, InkTool::Pen);
    ink.begin_stroke(Point::new(20.0, 140.0));
    let mut renderer = InkRenderer::new();
    let mut pixmap = Pixmap::new(400, 200).expect("pixmap");
    for i in 1..=60 {
        ink.extend_stroke(Point::new(
            20.0 + i as f64 * 5.0,
            140.0 + (i as f64 / 5.0).sin() * 20.0,
        ));
        renderer.render(&ink, 1.0, 1.0, &mut pixmap);
        if i % 20 == 0 {
            assert_same(&pixmap, &render(&ink, 1.0, 1.0, 400, 200), 0);
        }
    }
    ink.end_stroke();
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    assert_same(&pixmap, &render(&ink, 1.0, 1.0, 400, 200), 0);
}

#[test]
fn a_current_highlighter_stays_under_the_pen_layer() {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(20.0, 100.0, 380.0, 100.0, 4.0),
    );
    take(&mut ink, InkTool::Highlighter);
    ink.begin_stroke(Point::new(20.0, 80.0));
    let mut renderer = InkRenderer::new();
    let mut pixmap = Pixmap::new(400, 200).expect("pixmap");
    for i in 1..=40 {
        ink.extend_stroke(Point::new(20.0 + i as f64 * 8.0, 80.0 + i as f64));
        renderer.render(&ink, 1.0, 1.0, &mut pixmap);
        if i % 10 == 0 {
            assert_same(&pixmap, &render(&ink, 1.0, 1.0, 400, 200), 0);
        }
    }
    ink.end_stroke();
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    assert_same(&pixmap, &render(&ink, 1.0, 1.0, 400, 200), 0);
    // Where the core of a pen stroke crosses the highlight, the pen wins.
    assert!(alpha(&pixmap, 200, 100) >= 200);
}

#[test]
fn undo_and_clear_match_a_fresh_render() {
    let mut ink = Ink::new();
    add_stroke(&mut ink, InkTool::Pen, &line(20.0, 50.0, 300.0, 50.0, 4.0));
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(20.0, 100.0, 300.0, 100.0, 4.0),
    );
    let mut renderer = InkRenderer::new();
    let mut pixmap = Pixmap::new(320, 150).expect("pixmap");
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);

    // The highlighter is in hand, so Undo takes back the last finished stroke.
    assert!(ink.press(InkKey::Undo));
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    assert_same(&pixmap, &render(&ink, 1.0, 1.0, 320, 150), 0);

    assert!(ink.press(InkKey::Clear));
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    assert!(all_transparent(&pixmap));
}

#[test]
fn one_new_point_damages_only_a_small_rectangle() {
    const WIDTH: u32 = 3840;
    const HEIGHT: u32 = 2160;
    let mut ink = Ink::new();
    take(&mut ink, InkTool::Pen);
    ink.begin_stroke(Point::new(100.0, 1000.0));
    for i in 1..400 {
        let x = 100.0 + i as f64;
        let y = 1000.0 + (i as f64 * 0.2).sin() * 6.0;
        ink.extend_stroke(Point::new(x, y));
    }
    let last = ink
        .current()
        .and_then(|stroke| stroke.points().last().copied())
        .expect("a last point");
    let mut renderer = InkRenderer::new();
    let mut pixmap = Pixmap::new(WIDTH, HEIGHT).expect("4K pixmap");
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    assert!(renderer.damage().is_some());

    let next = Point::new(last.x + 8.0, 1002.0);
    ink.extend_stroke(next);
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    let (x, y, w, h) = renderer.damage().expect("damage after a new point");
    assert!(
        (x as f64) <= last.x && (x + w) as f64 >= next.x,
        "damage x range {x}..{} does not cover {last:?}..{next:?}",
        x + w
    );
    assert!(
        (y as f64) <= 994.0 && (y + h) as f64 >= 1008.0,
        "damage y range {y}..{} does not cover the stroke",
        y + h
    );
    assert!(
        u64::from(w) * u64::from(h) < u64::from(WIDTH) * u64::from(HEIGHT) / 20,
        "damage {w}x{h} is over 5% of the frame"
    );
}

#[test]
fn nothing_changed_gives_no_damage() {
    let mut ink = Ink::new();
    add_stroke(&mut ink, InkTool::Pen, &line(20.0, 50.0, 180.0, 50.0, 4.0));
    let mut renderer = InkRenderer::new();
    let mut pixmap = Pixmap::new(200, 100).expect("pixmap");
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    assert!(renderer.damage().is_some());
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    assert_eq!(renderer.damage(), None);
}

#[test]
fn half_opacity_halves_the_alpha_of_a_pen_pixel() {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(40.0, 100.0, 360.0, 100.0, 3.0),
    );
    let solid = render(&ink, 1.0, 1.0, 400, 200);
    assert_eq!(alpha(&solid, 200, 100), 255);

    let mut renderer = InkRenderer::new();
    let mut pixmap = Pixmap::new(400, 200).expect("pixmap");
    renderer.render(&ink, 1.0, 1.0, &mut pixmap);
    renderer.render(&ink, 1.0, 0.5, &mut pixmap);
    let faded = alpha(&pixmap, 200, 100);
    assert!(
        (127..=129).contains(&faded),
        "half opacity gave alpha {faded}"
    );
    assert_premultiplied(&pixmap);
}

#[test]
fn scale_two_covers_about_four_times_the_pixels() {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(50.0, 100.0, 350.0, 100.0, 3.0),
    );
    let one = render(&ink, 1.0, 1.0, 400, 200);
    let two = render(&ink, 2.0, 1.0, 800, 400);
    let ratio = nonzero_pixels(&two) as f64 / nonzero_pixels(&one) as f64;
    assert!(
        (3.5..=4.5).contains(&ratio),
        "scale 2 covered {ratio} times the pixels"
    );
}

#[test]
fn cursors_are_drawn_at_the_requested_scale() {
    let (pen, pen_hotspot) = tool_cursor(InkTool::Pen, 1.0);
    assert_eq!((pen.width(), pen.height()), (13, 13));
    assert_eq!(pen_hotspot, (6, 6));
    assert_premultiplied(&pen);

    let (highlighter, highlighter_hotspot) = tool_cursor(InkTool::Highlighter, 1.0);
    assert_eq!((highlighter.width(), highlighter.height()), (12, 26));
    assert_eq!(highlighter_hotspot, (6, 13));
    assert_premultiplied(&highlighter);

    let (pen2, _) = tool_cursor(InkTool::Pen, 2.0);
    assert_eq!((pen2.width(), pen2.height()), (26, 26));
    assert!(alpha(&pen, 6, 6) > 200);
    assert!(alpha(&highlighter, 6, 13) > 150);
    assert_premultiplied(&pen2);
}
