//! Pixel-level checks for the software renderer.

use std::sync::Arc;

use igorio_app::draw::{Renderer, movie_label};
use igorio_app::scene::{Leaving, Scene, SceneCard};
use igorio_kit::Image;
use tiny_skia::Pixmap;

fn solid(width: u32, height: u32, color: [u8; 4]) -> Arc<Image> {
    let mut rgba = vec![0; width as usize * height as usize * 4];
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&color);
    }
    Arc::new(Image::new(width, height, rgba).expect("valid image"))
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

fn scene(cards: Vec<SceneCard>, hint: Option<&str>, scale: f64) -> Scene {
    Scene {
        cards,
        hint: hint.map(str::to_string),
        hint_opacity: 1.0,
        opacity: 1.0,
        scale,
    }
}

fn render(scene: &Scene) -> Pixmap {
    let (width, height) = Renderer::pixmap_size(scene.scale);
    let mut pixmap = Pixmap::new(width, height).expect("pixmap");
    Renderer::new().render(scene, &mut pixmap);
    pixmap
}

fn opaque_pixels(pixmap: &Pixmap) -> usize {
    pixmap
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] > 0)
        .count()
}

#[test]
fn empty_scene_is_transparent() {
    let scene = scene(Vec::new(), Some("nothing held"), 1.0);
    let pixmap = render(&scene);
    assert!(
        pixmap.data().as_chunks::<4>().0.iter().all(|p| p[3] == 0),
        "an empty scene must be fully transparent"
    );
}

#[test]
fn zero_opacity_is_transparent() {
    let mut scene = scene(
        vec![card(solid(160, 100, [200, 60, 60, 255]), 0.0, 0)],
        Some("hi"),
        1.0,
    );
    scene.opacity = 0.0;
    let pixmap = render(&scene);
    assert!(
        pixmap.data().as_chunks::<4>().0.iter().all(|p| p[3] == 0),
        "opacity 0 must be fully transparent"
    );
}

#[test]
fn one_card_has_content_and_a_clear_corner() {
    let scene = scene(
        vec![card(solid(160, 100, [240, 240, 240, 255]), 0.0, 0)],
        None,
        1.0,
    );
    let pixmap = render(&scene);
    let centre = pixmap.pixel(150, 43).expect("centre pixel");
    assert_eq!(centre.alpha(), 255, "the card centre must be opaque");
    let corner = pixmap.pixel(299, 169).expect("corner pixel");
    assert_eq!(corner.alpha(), 0, "the far corner must stay transparent");
    let behind = pixmap.pixel(4, 4).expect("top-left pixel");
    assert_eq!(behind.alpha(), 0, "the inset area must stay transparent");
}

#[test]
fn premultiplied_invariant_holds_for_a_busy_scene() {
    let mut top = card(solid(900, 600, [80, 160, 220, 180]), -11.0, 0);
    top.movie = Some(Some(75.4));
    let mut scene = scene(
        vec![
            card(solid(1600, 1000, [230, 120, 60, 255]), 12.0, 3),
            card(solid(900, 1400, [70, 190, 130, 255]), -7.0, 2),
            card(solid(1200, 1200, [150, 90, 210, 255]), 9.0, 1),
            top,
        ],
        Some("3 held · shake to discard"),
        1.25,
    );
    scene.opacity = 0.9;
    let pixmap = render(&scene);
    for pixel in pixmap.data().as_chunks::<4>().0 {
        assert!(pixel[0] <= pixel[3], "red exceeds alpha");
        assert!(pixel[1] <= pixel[3], "green exceeds alpha");
        assert!(pixel[2] <= pixel[3], "blue exceeds alpha");
    }
}

#[test]
fn rendering_twice_gives_identical_bytes() {
    let scene = scene(
        vec![
            card(solid(1600, 1000, [230, 120, 60, 255]), 10.0, 2),
            card(solid(900, 1400, [70, 190, 130, 255]), -12.0, 1),
            card(solid(1200, 1200, [150, 90, 210, 255]), 9.0, 0),
        ],
        Some("3 held"),
        1.25,
    );
    let mut renderer = Renderer::new();
    let (width, height) = Renderer::pixmap_size(scene.scale);
    let mut first = Pixmap::new(width, height).expect("first");
    let mut second = Pixmap::new(width, height).expect("second");
    renderer.render(&scene, &mut first);
    renderer.render(&scene, &mut second);
    assert_eq!(first.data(), second.data());
}

#[test]
fn scale_two_quadruples_the_card_area() {
    let image = solid(1600, 1000, [210, 140, 90, 255]);
    let one = render(&scene(vec![card(image.clone(), 0.0, 0)], None, 1.0));
    let two = render(&scene(vec![card(image, 0.0, 0)], None, 2.0));

    assert_eq!(Renderer::pixmap_size(1.0), (300, 170));
    assert_eq!((two.width(), two.height()), (600, 340));

    let small = opaque_pixels(&one) as f64;
    let large = opaque_pixels(&two) as f64;
    assert!(small > 0.0, "the small card must draw something");
    let ratio = large / small;
    assert!(
        (ratio - 4.0).abs() <= 0.4,
        "expected about four times the pixels, got {ratio}"
    );
}

#[test]
fn finished_leave_animations_draw_nothing() {
    for leaving in [Leaving::Dissolve(1.0), Leaving::Drop(1.0)] {
        let mut top = card(solid(1600, 1000, [200, 90, 60, 255]), 0.0, 0);
        top.leaving = Some(leaving);
        let pixmap = render(&scene(vec![top], None, 1.0));
        assert!(
            pixmap.data().as_chunks::<4>().0.iter().all(|p| p[3] == 0),
            "{leaving:?} at 1.0 must leave no pixels"
        );
    }
}

#[test]
fn movie_labels_round_to_mm_ss() {
    assert_eq!(movie_label(0.0), "0:00");
    assert_eq!(movie_label(9.4), "0:09");
    assert_eq!(movie_label(59.6), "1:00");
    assert_eq!(movie_label(75.4), "1:15");
    assert_eq!(movie_label(3599.6), "60:00");
}

fn max_alpha(pixmap: &Pixmap, x0: u32, x1: u32, y0: u32, y1: u32) -> u8 {
    let mut peak = 0;
    for y in y0..y1.min(pixmap.height()) {
        for x in x0..x1.min(pixmap.width()) {
            peak = peak.max(pixmap.pixel(x, y).expect("pixel").alpha());
        }
    }
    peak
}

fn first_row_above(pixmap: &Pixmap, x0: u32, x1: u32, y0: u32, threshold: u8) -> Option<u32> {
    (y0..pixmap.height()).find(|&y| max_alpha(pixmap, x0, x1, y, y + 1) > threshold)
}

#[test]
fn leaving_cards_fade_the_image() {
    for dissolve in [true, false] {
        let mut alphas = Vec::new();
        for progress in [0.5, 0.9] {
            let leaving = if dissolve {
                Leaving::Dissolve(progress)
            } else {
                Leaving::Drop(progress)
            };
            let mut top = card(solid(160, 100, [210, 120, 70, 255]), 0.0, 0);
            top.leaving = Some(leaving);
            let pixmap = render(&scene(vec![top], None, 1.0));
            let (x, y) = if dissolve {
                (150, 43)
            } else {
                let scale = 1.0 - 0.65 * progress * progress;
                (
                    (110.0 + 40.0 * scale).round() as u32,
                    (18.0 + 25.0 * scale).round() as u32,
                )
            };
            let alpha = pixmap.pixel(x, y).expect("card centre").alpha();
            assert!(
                alpha < 255,
                "{} at {progress} still has opaque image pixels",
                if dissolve { "Dissolve" } else { "Drop" }
            );
            alphas.push(alpha);
        }
        assert!(
            alphas[1] < alphas[0],
            "alpha must fall as the leave progresses: {alphas:?}"
        );
        assert!(alphas[1] < 100, "a 90% gone card must be nearly invisible");
    }
}

#[test]
fn hint_opacity_fades_the_text() {
    let build = |opacity: f64| {
        let mut scene = scene(
            vec![card(solid(160, 100, [230, 230, 230, 255]), 0.0, 0)],
            Some("3 held · shake to discard"),
            1.0,
        );
        scene.hint_opacity = opacity;
        render(&scene)
    };
    let full = build(1.0);
    let faint = build(0.2);
    let shadow = build(0.0);

    // Remove the card's shadow from the alpha so only the hint (glow + text) is measured.
    let mut peak_full = 0.0f64;
    let mut peak_faint = 0.0f64;
    for y in 90..150 {
        for x in 15..190 {
            let s = shadow.pixel(x, y).expect("pixel").alpha() as f64 / 255.0;
            if s >= 0.99 {
                continue;
            }
            let a1 = full.pixel(x, y).expect("pixel").alpha() as f64 / 255.0;
            let a2 = faint.pixel(x, y).expect("pixel").alpha() as f64 / 255.0;
            peak_full = peak_full.max((a1 - s) / (1.0 - s));
            peak_faint = peak_faint.max((a2 - s) / (1.0 - s));
        }
    }
    assert!(
        peak_full > 0.4,
        "the hint should be clearly visible at opacity 1"
    );
    let ratio = peak_faint / peak_full;
    assert!(
        (0.17..0.23).contains(&ratio),
        "hint alpha should scale with hint_opacity, ratio {ratio}"
    );
}

#[test]
fn discarded_images_are_freed() {
    let image = solid(640, 480, [80, 140, 210, 255]);
    let painted = scene(vec![card(image.clone(), 0.0, 0)], Some("held"), 1.0);
    let (width, height) = Renderer::pixmap_size(1.0);
    let mut pixmap = Pixmap::new(width, height).expect("pixmap");
    let mut renderer = Renderer::new();
    renderer.render(&painted, &mut pixmap);
    drop(painted);
    assert_eq!(
        Arc::strong_count(&image),
        1,
        "the image cache must not keep screenshot pixels alive"
    );
    renderer.render(&scene(Vec::new(), None, 1.0), &mut pixmap);
    assert_eq!(Arc::strong_count(&image), 1);
}

#[test]
fn hint_sits_below_the_tallest_card_box() {
    let wide = solid(1600, 1000, [200, 160, 100, 255]);
    let tall = solid(900, 1400, [100, 190, 150, 255]);
    let hint = Some("3 held · shake to discard");

    let one = render(&scene(vec![card(wide.clone(), 0.0, 0)], hint, 1.0));
    let two = render(&scene(
        vec![card(tall, 10.0, 1), card(wide, -9.0, 0)],
        hint,
        1.0,
    ));

    let one_top = first_row_above(&one, 200, 228, 20, 80).expect("hint in one-card scene");
    let two_top = first_row_above(&two, 200, 228, 20, 80).expect("hint in two-card scene");
    assert!(
        two_top >= one_top + 6,
        "the taller lower card should push the hint down: {one_top} vs {two_top}"
    );
}

#[test]
fn non_finite_tilt_is_drawn_as_zero() {
    let image = solid(1600, 1000, [210, 140, 90, 255]);
    let normal = render(&scene(vec![card(image.clone(), 0.0, 0)], Some("hi"), 1.0));
    let nan = render(&scene(vec![card(image, f64::NAN, 0)], Some("hi"), 1.0));
    assert_eq!(normal.data(), nan.data());
}

#[test]
fn invalid_scale_or_size_leaves_a_clear_pixmap() {
    let image = solid(160, 100, [200, 90, 60, 255]);
    let mut scene = scene(vec![card(image, 0.0, 0)], None, 1.0);
    let mut pixmap = Pixmap::new(200, 170).expect("pixmap");

    for scale in [f64::NAN, 0.0, -1.0, 40.0] {
        scene.scale = scale;
        pixmap.fill(tiny_skia::Color::from_rgba8(9, 9, 9, 255));
        Renderer::new().render(&scene, &mut pixmap);
        assert!(
            pixmap.data().as_chunks::<4>().0.iter().all(|p| p[3] == 0),
            "scale {scale} must leave the pixmap transparent"
        );
    }

    scene.scale = 1.0;
    let mut wrong = Pixmap::new(100, 100).expect("wrong size");
    wrong.fill(tiny_skia::Color::from_rgba8(9, 9, 9, 255));
    Renderer::new().render(&scene, &mut wrong);
    assert!(
        wrong.data().as_chunks::<4>().0.iter().all(|p| p[3] == 0),
        "a mismatched pixmap size must be cleared, not painted"
    );
}

#[test]
fn leave_progress_is_clamped_and_nan_is_zero() {
    for dissolve in [true, false] {
        let leaving = |p| {
            if dissolve {
                Some(Leaving::Dissolve(p))
            } else {
                Some(Leaving::Drop(p))
            }
        };
        let mut nan_card = card(solid(160, 100, [200, 90, 60, 255]), 0.0, 0);
        nan_card.leaving = leaving(f64::NAN);
        let mut zero_card = card(solid(160, 100, [200, 90, 60, 255]), 0.0, 0);
        zero_card.leaving = leaving(0.0);
        let nan = render(&scene(vec![nan_card], None, 1.0));
        let zero = render(&scene(vec![zero_card], None, 1.0));
        assert_eq!(nan.data(), zero.data(), "NaN progress must act as zero");

        let mut done_card = card(solid(160, 100, [200, 90, 60, 255]), 0.0, 0);
        done_card.leaving = leaving(5.0);
        let done = render(&scene(vec![done_card], None, 1.0));
        assert!(
            done.data().as_chunks::<4>().0.iter().all(|p| p[3] == 0),
            "progress above 1 must be clamped to 1"
        );
    }
}
