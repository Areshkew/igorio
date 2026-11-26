//! Renders the draw scenes to PNG files under `target/draw-demo/` for inspection.
//! Run from the workspace root:
//! `cargo run -p igorio-app --example draw_demo --release`

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use igorio_app::draw::Renderer;
use igorio_app::layout::CardSize;
use igorio_app::scene::{Leaving, Scene, SceneCard};
use igorio_kit::Image;
use tiny_skia::Pixmap;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join("target").join("draw-demo");
    fs::create_dir_all(&dir).expect("create target/draw-demo");

    let wide = make_image(
        1600,
        1000,
        [28, 82, 148],
        [196, 224, 252],
        [244, 152, 64],
        [38, 176, 158],
    );
    let tall = make_image(
        900,
        1400,
        [62, 42, 112],
        [250, 176, 206],
        [84, 198, 124],
        [252, 218, 94],
    );
    let square = make_image(
        1200,
        1200,
        [20, 108, 98],
        [218, 240, 180],
        [228, 82, 92],
        [62, 102, 220],
    );
    let wide_second = make_image(
        1500,
        900,
        [112, 48, 36],
        [250, 226, 196],
        [70, 130, 240],
        [244, 196, 60],
    );

    let one = scene(vec![card(&wide, 0.0, 0.0)], None, 1.0);
    save(&dir, "01-one-card.png", &one);

    let tilts = three(&wide, &tall, &square, 1.0);
    save(&dir, "02-three-tilts.png", &tilts);

    let four = scene(
        vec![
            card(&wide, 8.0, 3.0),
            card(&tall, -7.0, 2.0),
            card(&square, 11.0, 1.0),
            card(&wide_second, -9.0, 0.0),
        ],
        Some("3 held · shake to discard"),
        1.0,
    );
    save(&dir, "03-four-hint.png", &four);

    let mut movie_card = card(&wide_second, -4.0, 0.0);
    movie_card.movie = Some(Some(75.0));
    let movie = scene(vec![movie_card], None, 1.0);
    save(&dir, "04-movie.png", &movie);

    let mut wobble_card = card(&square, 0.0, 0.0);
    wobble_card.wobble = 0.25;
    let wobble = scene(vec![wobble_card], None, 1.0);
    save(&dir, "05-wobble.png", &wobble);

    let mut dissolve = three(&wide, &tall, &square, 1.0);
    dissolve.cards.last_mut().expect("top card").leaving = Some(Leaving::Dissolve(0.5));
    save(&dir, "06-dissolve.png", &dissolve);

    let mut drop = three(&wide, &tall, &square, 1.0);
    drop.cards.last_mut().expect("top card").leaving = Some(Leaving::Drop(0.5));
    save(&dir, "07-drop.png", &drop);

    let scaled = three(&wide, &tall, &square, 1.5);
    save(&dir, "08-scale-1_5.png", &scaled);

    let scaled = three(&wide, &tall, &square, 2.0);
    save(&dir, "09-scale-2_0.png", &scaled);

    let mut appear = three(&wide, &tall, &square, 1.0);
    appear.cards.last_mut().expect("top card").appear = 0.5;
    save(&dir, "10-appear.png", &appear);

    let mut restack = three(&wide, &tall, &square, 1.0);
    for (index, card) in restack.cards.iter_mut().enumerate() {
        card.depth = (2 - index) as f64 + 0.5;
    }
    save(&dir, "11-restack.png", &restack);

    for (name, size) in [
        ("12-size-small.png", CardSize::Small),
        ("13-size-medium.png", CardSize::Medium),
        ("14-size-large.png", CardSize::Large),
    ] {
        let mut pile = three(&wide, &tall, &square, 1.25);
        pile.hint = Some("3 held · shake to discard".to_string());
        pile.card_size = size;
        save(&dir, name, &pile);
    }

    let four_a = four.with_scale(1.0);
    let four_b = four.with_scale(2.0);
    measure(&four_a);
    measure(&four_b);

    timing_table(&four);
}

fn scene(cards: Vec<SceneCard>, hint: Option<&str>, scale: f64) -> Scene {
    Scene {
        cards,
        hint: hint.map(str::to_string),
        hint_opacity: 1.0,
        opacity: 1.0,
        card_size: CardSize::Small,
        scale,
    }
}

fn card(image: &Arc<Image>, tilt: f64, depth: f64) -> SceneCard {
    SceneCard {
        image: image.clone(),
        tilt,
        depth,
        appear: 1.0,
        wobble: 0.0,
        leaving: None,
        movie: None,
    }
}

fn three(wide: &Arc<Image>, tall: &Arc<Image>, square: &Arc<Image>, scale: f64) -> Scene {
    scene(
        vec![
            card(wide, 10.0, 2.0),
            card(tall, -12.0, 1.0),
            card(square, 9.0, 0.0),
        ],
        None,
        scale,
    )
}

fn save(dir: &Path, name: &str, scene: &Scene) {
    let (width, height) = Renderer::pixmap_size(scene.scale);
    let mut pixmap = Pixmap::new(width, height).expect("pixmap");
    Renderer::new().render(scene, &mut pixmap);
    assert_premultiplied(&pixmap);
    save_preview(&pixmap, &dir.join(name));
    println!("wrote {}", dir.join(name).display());
}

fn assert_premultiplied(pixmap: &Pixmap) {
    for pixel in pixmap.pixels() {
        assert!(
            pixel.red() <= pixel.alpha()
                && pixel.green() <= pixel.alpha()
                && pixel.blue() <= pixel.alpha(),
            "non-premultiplied pixel in {pixel:?}"
        );
    }
}

/// Composites over a light/dark checkerboard and enlarges 3x with nearest neighbour.
fn save_preview(pixmap: &Pixmap, path: &Path) {
    let factor = 3u32;
    let cell = 12u32;
    let src_w = pixmap.width();
    let src_h = pixmap.height();
    let out_w = src_w * factor;
    let out_h = src_h * factor;
    let mut out = vec![0u8; out_w as usize * out_h as usize * 4];
    let pixels = pixmap.pixels();

    for y in 0..src_h {
        for x in 0..src_w {
            let pixel = pixels[(y * src_w + x) as usize];
            let alpha = pixel.alpha() as f32 / 255.0;
            let checker = if (x / cell + y / cell).is_multiple_of(2) {
                214.0
            } else {
                118.0
            };
            let red = pixel.red() as f32 + checker * (1.0 - alpha);
            let green = pixel.green() as f32 + checker * (1.0 - alpha);
            let blue = pixel.blue() as f32 + checker * (1.0 - alpha);
            let rgb = [
                red.round().clamp(0.0, 255.0) as u8,
                green.round().clamp(0.0, 255.0) as u8,
                blue.round().clamp(0.0, 255.0) as u8,
                255,
            ];

            for dy in 0..factor {
                for dx in 0..factor {
                    let ox = x * factor + dx;
                    let oy = y * factor + dy;
                    let index = (oy * out_w + ox) as usize * 4;
                    out[index..index + 4].copy_from_slice(&rgb);
                }
            }
        }
    }

    let file = File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(BufWriter::new(file), out_w, out_h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&out).expect("png data");
}

fn measure(scene: &Scene) {
    let (width, height) = Renderer::pixmap_size(scene.scale);
    let mut pixmap = Pixmap::new(width, height).expect("pixmap");
    let mut renderer = Renderer::new();
    for _ in 0..10 {
        renderer.render(scene, &mut pixmap);
    }
    let start = Instant::now();
    for _ in 0..200 {
        renderer.render(scene, &mut pixmap);
    }
    let millis = start.elapsed().as_secs_f64() * 1000.0 / 200.0;
    println!(
        "render four cards @ scale {:.1}: {millis:.3} ms",
        scene.scale
    );
}

fn timing_table(base: &Scene) {
    for scale in [1.0, 1.25, 2.0] {
        time_scene("rest", base.with_scale(scale), |_, _| {});
        time_scene("wobble", base.with_scale(scale), |i, scene| {
            if let Some(top) = scene.cards.last_mut() {
                top.wobble = (i % 200) as f64 / 199.0;
            }
        });
        time_scene("dissolve", base.with_scale(scale), |i, scene| {
            if let Some(top) = scene.cards.last_mut() {
                top.leaving = Some(Leaving::Dissolve((i % 200) as f64 / 199.0));
            }
        });
    }
    time_scene("appear", base.with_scale(2.0), |_, scene| {
        if let Some(top) = scene.cards.last_mut() {
            top.appear = 0.5;
        }
    });
    time_scene("restack", base.with_scale(2.0), |i, scene| {
        let count = scene.cards.len();
        let t = ((i % 200) as f64 + 0.5) / 200.0;
        for (index, card) in scene.cards.iter_mut().enumerate() {
            card.depth = (count - 1 - index) as f64 + t;
        }
    });
    first_frame_timing();
}

fn time_scene(label: &str, mut scene: Scene, mut step: impl FnMut(usize, &mut Scene)) {
    let (width, height) = Renderer::pixmap_size(scene.scale);
    let mut pixmap = Pixmap::new(width, height).expect("pixmap");
    let mut renderer = Renderer::new();
    for i in 0..10 {
        step(i, &mut scene);
        renderer.render(&scene, &mut pixmap);
    }
    let start = Instant::now();
    for i in 0..200 {
        step(i + 10, &mut scene);
        renderer.render(&scene, &mut pixmap);
    }
    let millis = start.elapsed().as_secs_f64() * 1000.0 / 200.0;
    println!(
        "four cards + hint, scale {:.2}, {label}: {millis:.3} ms",
        scene.scale
    );
}

fn first_frame_timing() {
    let colours = [
        [180, 70, 55, 255],
        [60, 130, 200, 255],
        [70, 170, 110, 255],
        [200, 160, 60, 255],
    ];
    let large: Vec<_> = colours
        .into_iter()
        .map(|colour| solid_image(3840, 2160, colour))
        .collect();
    let three = scene(
        vec![
            card(&large[0], 10.0, 2.0),
            card(&large[1], -12.0, 1.0),
            card(&large[2], 9.0, 0.0),
        ],
        None,
        2.0,
    );
    let four = scene(
        vec![
            card(&large[0], 10.0, 3.0),
            card(&large[1], -12.0, 2.0),
            card(&large[2], 9.0, 1.0),
            card(&large[3], -9.0, 0.0),
        ],
        None,
        2.0,
    );

    let (width, height) = Renderer::pixmap_size(2.0);
    let mut pixmap = Pixmap::new(width, height).expect("pixmap");
    let mut renderer = Renderer::new();
    for _ in 0..5 {
        renderer.render(&three, &mut pixmap);
    }

    let mut first_total = 0.0;
    for _ in 0..200 {
        renderer.render(&three, &mut pixmap);
        let start = Instant::now();
        renderer.render(&four, &mut pixmap);
        first_total += start.elapsed().as_secs_f64() * 1000.0;
    }
    println!(
        "first frame after adding a 3840x2160 card @ scale 2.0: {:.3} ms",
        first_total / 200.0
    );
}

fn solid_image(width: u32, height: u32, colour: [u8; 4]) -> Arc<Image> {
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&colour);
    }
    Arc::new(Image::new(width, height, rgba).expect("solid image"))
}

/// A deterministic screenshot stand-in: gradient, coloured blocks and thin lines.
fn make_image(
    width: u32,
    height: u32,
    top: [u8; 3],
    bottom: [u8; 3],
    block_a: [u8; 3],
    block_b: [u8; 3],
) -> Arc<Image> {
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    let w = width as f32;
    let h = height as f32;

    for y in 0..height {
        let ty = y as f32 / (h - 1.0).max(1.0);
        for x in 0..width {
            let tx = x as f32 / (w - 1.0).max(1.0);
            let mut colour = mix(top, bottom, (tx * 0.35 + ty * 0.65).clamp(0.0, 1.0));

            let fx = x as f32 / w;
            let fy = y as f32 / h;
            if (0.10..0.46).contains(&fx) && (0.15..0.43).contains(&fy) {
                colour = mix(colour, block_a, 0.72);
            }
            if (0.55..0.88).contains(&fx) && (0.58..0.84).contains(&fy) {
                colour = mix(colour, block_b, 0.70);
            }
            if (0.70..0.92).contains(&fx) && (0.10..0.24).contains(&fy) {
                colour = mix(colour, [255, 255, 255], 0.55);
            }
            if x % 173 == 0 || y % 191 == 0 {
                colour = mix(colour, [255, 255, 255], 0.20);
            }
            if (fx - 0.5).abs() * (fy - 0.5).abs() < 0.0015 {
                colour = mix(colour, [20, 24, 34], 0.35);
            }

            let index = (y as usize * width as usize + x as usize) * 4;
            rgba[index] = colour[0];
            rgba[index + 1] = colour[1];
            rgba[index + 2] = colour[2];
            rgba[index + 3] = 255;
        }
    }
    Arc::new(Image::new(width, height, rgba).expect("generated image"))
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    [
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * t) as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t) as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t) as u8,
    ]
}

trait WithScale {
    fn with_scale(&self, scale: f64) -> Scene;
}

impl WithScale for Scene {
    fn with_scale(&self, scale: f64) -> Scene {
        let mut scene = self.clone();
        scene.scale = scale;
        scene
    }
}
