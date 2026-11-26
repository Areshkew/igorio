//! Renders the ink (pen and highlighter) scenes to PNG files under `target/ink-demo/` and
//! prints frame timings. Run from the workspace root:
//! `cargo run -p igorio-app --example ink_demo --release`

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;
use std::time::Instant;

use igorio_app::ink_draw::{InkRenderer, tool_cursor};
use igorio_core::geom::Point;
use igorio_core::ink::{Ink, InkKey, InkTool};
use tiny_skia::{Color, Pixmap};

/// Logical size of the simulated screen area.
const PAGE_W: u32 = 800;
const PAGE_H: u32 = 450;
/// The PNGs are written at this enlargement so single pixels of ink are easy to inspect.
const PREVIEW: u32 = 2;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join("target").join("ink-demo");
    fs::create_dir_all(&dir).expect("create target/ink-demo");

    save(
        &dir,
        "01-pen-signature.png",
        &ink_page(&pen_signature(), 1.0),
    );
    save(
        &dir,
        "02-pen-slow-fast.png",
        &ink_page(&pen_slow_fast(), 1.0),
    );
    save(
        &dir,
        "03-highlighter-text.png",
        &ink_page(&highlighter_text(), 1.0),
    );
    save(
        &dir,
        "04-highlighter-self.png",
        &ink_page(&highlighter_self(), 1.0),
    );
    save(
        &dir,
        "05-highlighter-crossing.png",
        &ink_page(&highlighter_crossing(), 1.0),
    );
    save(
        &dir,
        "06-pen-over-highlighter.png",
        &ink_page(&pen_over_highlighter(), 1.0),
    );
    save(&dir, "07-half-opacity.png", &ink_page(&half_opacity(), 0.5));
    save(&dir, "08-cursors.png", &cursors());

    timings();
}

// MARK: - Scenes

fn pen_signature() -> Ink {
    let mut ink = Ink::new();
    take(&mut ink, InkTool::Pen);
    let mut points = Vec::new();
    // A signature-like wiggle: dense points (slow) make the line swell, sparse ones thin it.
    // The x term swings back on itself, so the stroke loops like handwriting.
    for i in 0..170 {
        let t = i as f64 / 169.0;
        let x = 110.0 + 600.0 * t + 55.0 * (t * 6.0 * std::f64::consts::PI).sin();
        let y = 250.0 + (t * 5.0 * std::f64::consts::PI).sin() * 95.0 * (1.0 - 0.2 * t);
        points.push(Point::new(x, y));
    }
    // A long, quick flick at the end thins out.
    let last = *points.last().expect("signature point");
    for i in 1..=25 {
        let t = i as f64 / 25.0;
        points.push(Point::new(
            last.x - 170.0 * t,
            last.y + 80.0 * t + 20.0 * (t * 3.0).sin(),
        ));
    }
    add_stroke(&mut ink, InkTool::Pen, &points);
    ink
}

fn pen_slow_fast() -> Ink {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(110.0, 150.0, 690.0, 150.0, 2.0),
    );
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(110.0, 300.0, 690.0, 300.0, 32.0),
    );
    ink
}

fn highlighter_text() -> Ink {
    let mut ink = Ink::new();
    for y in [102.0, 150.0, 198.0] {
        add_stroke(
            &mut ink,
            InkTool::Highlighter,
            &line(90.0, y, 660.0, y + 2.0, 4.0),
        );
    }
    ink
}

fn highlighter_self() -> Ink {
    let mut ink = Ink::new();
    // A loop: the vertical part crosses the first horizontal stroke at x = 360.
    let points = [
        Point::new(130.0, 300.0),
        Point::new(670.0, 300.0),
        Point::new(670.0, 390.0),
        Point::new(360.0, 390.0),
        Point::new(360.0, 210.0),
        Point::new(560.0, 210.0),
    ];
    add_stroke(&mut ink, InkTool::Highlighter, &points);
    ink
}

fn highlighter_crossing() -> Ink {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(110.0, 220.0, 690.0, 220.0, 4.0),
    );
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(420.0, 100.0, 420.0, 360.0, 4.0),
    );
    ink
}

fn pen_over_highlighter() -> Ink {
    let mut ink = Ink::new();
    add_stroke(
        &mut ink,
        InkTool::Highlighter,
        &line(90.0, 160.0, 700.0, 160.0, 4.0),
    );
    // A couple of pen strokes written straight across the highlight stay crisp on top.
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(140.0, 120.0, 400.0, 205.0, 4.0),
    );
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(300.0, 205.0, 640.0, 115.0, 4.0),
    );
    ink
}

fn half_opacity() -> Ink {
    let mut ink = Ink::new();
    for y in [130.0, 235.0, 330.0] {
        add_stroke(
            &mut ink,
            InkTool::Highlighter,
            &line(100.0, y, 700.0, y, 4.0),
        );
    }
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(150.0, 90.0, 420.0, 370.0, 4.0),
    );
    add_stroke(
        &mut ink,
        InkTool::Pen,
        &line(380.0, 370.0, 660.0, 80.0, 4.0),
    );
    ink
}

fn cursors() -> Pixmap {
    let mut page = page_background();
    // A dark slab behind the tips, so the white rim has something to show against.
    fill_rect(&mut page, 110, 150, 580, 160, [54, 58, 66, 255]);
    for (index, (tool, scale)) in [
        (InkTool::Pen, 1.0),
        (InkTool::Pen, 2.0),
        (InkTool::Highlighter, 1.0),
        (InkTool::Highlighter, 2.0),
    ]
    .into_iter()
    .enumerate()
    {
        let (cursor, _hotspot) = tool_cursor(tool, scale);
        let x = 150 + index as i32 * 140;
        let y = 230 - cursor.height() as i32 / 2;
        blit(&cursor, &mut page, x, y);
    }
    page
}

// MARK: - Ink helpers

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

/// A clicky scribble used to fill a 4K screen with finished strokes in the timing runs.
fn scribble(ink: &mut Ink, index: usize) {
    let x = 150.0 + ((index * 263) % 3480) as f64;
    let y = 130.0 + ((index * 167) % 1880) as f64;
    let points: Vec<Point> = (0..7)
        .map(|step| {
            let t = step as f64;
            Point::new(x + t * 9.0, y + (t * 1.7).sin() * 14.0)
        })
        .collect();
    add_stroke(ink, InkTool::Pen, &points);
}

fn filled_ink(finished: usize) -> Ink {
    let mut ink = Ink::new();
    take(&mut ink, InkTool::Pen);
    for index in 0..finished {
        scribble(&mut ink, index);
    }
    ink
}

/// Points of a signature-like 400-point stroke: about 1.6 logical px apart, gently wavy, so
/// the benchmark exercises a realistic outline rather than a dense knot.
fn timing_point(index: usize) -> Point {
    let t = index as f64;
    Point::new(
        1100.0 + t * 1.6,
        1000.0 + (t * 0.05).sin() * 110.0 + (t * 0.013).sin() * 35.0,
    )
}

// MARK: - Pixels

fn page_background() -> Pixmap {
    let mut page = Pixmap::new(PAGE_W, PAGE_H).expect("page");
    page.fill(Color::from_rgba8(250, 249, 246, 255));
    // A heading and paragraph-ish lines of "text".
    fill_rect(&mut page, 80, 58, 260, 9, [32, 36, 44, 255]);
    let widths = [
        560, 640, 520, 600, 430, 610, 575, 540, 620, 500, 650, 470, 560, 590,
    ];
    for (index, width) in widths.into_iter().enumerate() {
        let y = 76 + index as u32 * 24;
        if y + 5 < PAGE_H - 10 {
            fill_rect(&mut page, 80, y, width, 5, [42, 46, 54, 255]);
        }
    }
    page
}

fn fill_rect(pixmap: &mut Pixmap, x: u32, y: u32, width: u32, height: u32, color: [u8; 4]) {
    let stride = pixmap.width() as usize * 4;
    let page_height = pixmap.height();
    let data = pixmap.data_mut();
    for row in y..(y + height).min(page_height) {
        let start = row as usize * stride + x as usize * 4;
        let end = start + width as usize * 4;
        if end > data.len() {
            continue;
        }
        for pixel in data[start..end].as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&color);
        }
    }
}

/// Source-over of a premultiplied sprite onto an opaque page, at an integer position.
fn blit(src: &Pixmap, dst: &mut Pixmap, x: i32, y: i32) {
    let dst_w = dst.width() as i32;
    let dst_h = dst.height() as i32;
    let left = x.max(0);
    let top = y.max(0);
    let right = (x + src.width() as i32).min(dst_w);
    let bottom = (y + src.height() as i32).min(dst_h);
    if right <= left || bottom <= top {
        return;
    }
    let src_stride = src.width() as usize * 4;
    let dst_stride = dst_w as usize * 4;
    let src_data = src.data();
    let dst_data = dst.data_mut();
    for row in top..bottom {
        for column in left..right {
            let si = (row - y) as usize * src_stride + (column - x) as usize * 4;
            let di = row as usize * dst_stride + column as usize * 4;
            let alpha = src_data[si + 3] as u32;
            if alpha == 0 {
                continue;
            }
            let keep = 255 - alpha;
            for channel in 0..3 {
                dst_data[di + channel] = (src_data[si + channel] as u32
                    + (dst_data[di + channel] as u32 * keep + 127) / 255)
                    .min(255) as u8;
            }
            dst_data[di + 3] = 255;
        }
    }
}

fn ink_page(ink: &Ink, opacity: f64) -> Pixmap {
    let mut layer = Pixmap::new(PAGE_W, PAGE_H).expect("ink layer");
    InkRenderer::new().render(ink, 1.0, opacity, &mut layer);
    let mut page = page_background();
    blit(&layer, &mut page, 0, 0);
    page
}

fn save(dir: &Path, name: &str, page: &Pixmap) {
    let path = dir.join(name);
    let mut out = Pixmap::new(page.width() * PREVIEW, page.height() * PREVIEW).expect("preview");
    {
        let src = page.data();
        let src_stride = page.width() as usize * 4;
        let dst_stride = out.width() as usize * 4;
        let dst = out.data_mut();
        for y in 0..page.height() as usize {
            for x in 0..page.width() as usize {
                let pixel = &src[y * src_stride + x * 4..y * src_stride + x * 4 + 4];
                for dy in 0..PREVIEW as usize {
                    for dx in 0..PREVIEW as usize {
                        let index = (y * PREVIEW as usize + dy) * dst_stride
                            + (x * PREVIEW as usize + dx) * 4;
                        dst[index..index + 4].copy_from_slice(pixel);
                    }
                }
            }
        }
    }
    let file = File::create(&path).expect("create png");
    let mut encoder = png::Encoder::new(BufWriter::new(file), out.width(), out.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(out.data()).expect("png data");
    println!("wrote {}", path.display());
}

// MARK: - Timings

fn timings() {
    for finished in [0usize, 50, 300] {
        let mut ink = filled_ink(finished);
        take(&mut ink, InkTool::Pen);
        ink.begin_stroke(timing_point(0));
        for index in 1..390 {
            ink.extend_stroke(timing_point(index));
        }
        let mut pixmap = Pixmap::new(3840, 2160).expect("4K pixmap");
        let mut renderer = InkRenderer::new();
        let mut next = 390;
        // Warm up (reaching 400 points) and let the committed layer get built.
        for _ in 0..10 {
            ink.extend_stroke(timing_point(next));
            next += 1;
            renderer.render(&ink, 1.0, 1.0, &mut pixmap);
        }
        let start = Instant::now();
        for _ in 0..200 {
            ink.extend_stroke(timing_point(next));
            next += 1;
            renderer.render(&ink, 1.0, 1.0, &mut pixmap);
        }
        let millis = start.elapsed().as_secs_f64() * 1000.0 / 200.0;
        println!(
            "incremental frame, 400-point stroke, {finished} finished strokes: {millis:.3} ms"
        );
    }

    let ink299 = filled_ink(299);
    let ink300 = filled_ink(300);
    let mut pixmap = Pixmap::new(3840, 2160).expect("4K pixmap");
    let mut renderer = InkRenderer::new();
    for _ in 0..5 {
        renderer.render(&ink299, 1.0, 1.0, &mut pixmap);
        renderer.render(&ink300, 1.0, 1.0, &mut pixmap);
    }
    let mut total = 0.0;
    for _ in 0..200 {
        renderer.render(&ink299, 1.0, 1.0, &mut pixmap);
        let start = Instant::now();
        renderer.render(&ink300, 1.0, 1.0, &mut pixmap);
        total += start.elapsed().as_secs_f64() * 1000.0;
    }
    println!(
        "full rebuild, 300 finished strokes: {:.3} ms",
        total / 200.0
    );
}
