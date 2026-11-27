//! Checks for the saver: file names, free names, PNG contents, and atomic writes.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use igorio_app::saver::{
    SaveError, copy_movie, encode_png, format_name, free_path, save_png_as, save_screenshot,
    save_screenshot_in_background, screenshot_file_name,
};
use igorio_kit::Image;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh folder under the temp directory, removed when the test ends.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "igorio-saver-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A small opaque image with a different colour in every pixel.
fn opaque(width: u32, height: u32) -> Image {
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[
                (x * 37) as u8,
                (y * 53) as u8,
                (x * 11 + y * 7) as u8,
                255,
            ]);
        }
    }
    Image::new(width, height, rgba).expect("valid image")
}

/// The same picture with a range of alpha values.
fn with_alpha(width: u32, height: u32) -> Image {
    let mut image = opaque(width, height);
    for (index, pixel) in image.rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        pixel[3] = (index * 40) as u8;
    }
    image
}

fn rgb_of(image: &Image) -> Vec<u8> {
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| pixel[..3].to_vec())
        .collect()
}

struct Decoded {
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    pixels: Vec<u8>,
}

fn decode(bytes: &[u8]) -> Decoded {
    let decoder = png::Decoder::new(io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buffer = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).expect("png frame");
    buffer.truncate(frame.buffer_size());
    Decoded {
        width: frame.width,
        height: frame.height,
        color: frame.color_type,
        depth: frame.bit_depth,
        pixels: buffer,
    }
}

/// The pHYs chunk as the decoder reads it.
fn pixel_density(bytes: &[u8]) -> (u32, u32, png::Unit) {
    let decoder = png::Decoder::new(io::Cursor::new(bytes));
    let reader = decoder.read_info().expect("png header");
    let dims = reader.info().pixel_dims.expect("pHYs chunk");
    (dims.xppu, dims.yppu, dims.unit)
}

#[test]
fn opaque_images_encode_as_rgb_and_round_trip() {
    let image = opaque(5, 3);
    let decoded = decode(&encode_png(&image, 1.0).unwrap());

    assert_eq!(decoded.color, png::ColorType::Rgb);
    assert_eq!(decoded.depth, png::BitDepth::Eight);
    assert_eq!((decoded.width, decoded.height), (5, 3));
    assert_eq!(decoded.pixels, rgb_of(&image));
}

#[test]
fn translucent_images_encode_as_rgba_and_round_trip() {
    let image = with_alpha(4, 2);
    let decoded = decode(&encode_png(&image, 1.0).unwrap());

    assert_eq!(decoded.color, png::ColorType::Rgba);
    assert_eq!(decoded.depth, png::BitDepth::Eight);
    assert_eq!((decoded.width, decoded.height), (4, 2));
    assert_eq!(decoded.pixels, image.rgba);
}

#[test]
fn pixel_density_follows_the_scale() {
    let image = opaque(1, 1);
    // 96/0.0254 = 3779.5275..., 120/0.0254 = 4724.4094..., 192/0.0254 = 7559.0551...
    // Plain rounding, so 1.25 gives 4724 (not 4725) and 2.0 gives 7559 (not 7560).
    for (scale, expected) in [(1.0, 3780), (1.25, 4724), (2.0, 7559)] {
        let (x, y, unit) = pixel_density(&encode_png(&image, scale).unwrap());
        assert_eq!((x, y), (expected, expected), "scale {scale}");
        assert_eq!(unit, png::Unit::Meter, "scale {scale}");
    }
}

#[test]
fn format_name_pads_with_zeros() {
    assert_eq!(
        format_name(2026, 11, 1, 9, 5, 3),
        "Screenshot 2026-11-01 090503.png"
    );
    assert_eq!(
        format_name(999, 12, 31, 23, 59, 59),
        "Screenshot 0999-12-31 235959.png"
    );
}

#[test]
fn screenshot_file_name_has_the_snipping_tool_shape() {
    let name = screenshot_file_name(SystemTime::UNIX_EPOCH);
    assert_eq!(name.len(), "Screenshot 1970-01-01 000000.png".len(), "{name}");
    assert!(name.starts_with("Screenshot "), "{name}");
    assert!(name.ends_with(".png"), "{name}");
}

#[test]
fn free_path_suggests_the_plain_name_then_numbered_ones() {
    let scratch = Scratch::new("free-path");
    let first = scratch.path("shot.png");
    assert_eq!(free_path(&scratch.dir, "shot.png"), first);

    fs::write(&first, b"x").unwrap();
    assert_eq!(free_path(&scratch.dir, "shot.png"), scratch.path("shot (2).png"));

    fs::write(scratch.path("shot (2).png"), b"x").unwrap();
    assert_eq!(free_path(&scratch.dir, "shot.png"), scratch.path("shot (3).png"));
}

#[test]
fn free_path_handles_a_name_without_an_extension() {
    let scratch = Scratch::new("free-path-no-ext");
    fs::write(scratch.path("notes"), b"x").unwrap();
    assert_eq!(free_path(&scratch.dir, "notes"), scratch.path("notes (2)"));

    fs::write(scratch.path("notes (2)"), b"x").unwrap();
    assert_eq!(free_path(&scratch.dir, "notes"), scratch.path("notes (3)"));
}

#[test]
fn save_creates_the_folder_and_leaves_no_temp_file() {
    let scratch = Scratch::new("save");
    let folder = scratch.path("deep/shots");
    let image = opaque(4, 4);
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    let path = save_screenshot(&image, 1.0, at, &folder).unwrap();

    assert!(folder.is_dir());
    assert!(path.is_file());
    assert_eq!(path.parent(), Some(folder.as_path()));
    assert_eq!(path.file_name().unwrap(), screenshot_file_name(at).as_str());

    let decoded = decode(&fs::read(&path).unwrap());
    assert_eq!((decoded.width, decoded.height), (4, 4));
    assert_eq!(decoded.pixels, rgb_of(&image));

    let names: Vec<String> = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(!names[0].starts_with('.'), "{names:?}");
    assert!(!names[0].ends_with(".tmp"), "{names:?}");
}

#[test]
fn two_saves_of_the_same_moment_make_two_files_with_the_same_pixels() {
    let scratch = Scratch::new("two-saves");
    let folder = scratch.path("shots");
    let image = opaque(6, 4);
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    let first = save_screenshot(&image, 1.0, at, &folder).unwrap();
    let second = save_screenshot(&image, 1.0, at, &folder).unwrap();

    assert_ne!(first, second);
    let expected = format!(
        "{} (2).png",
        Path::new(&screenshot_file_name(at))
            .file_stem()
            .unwrap()
            .to_string_lossy()
    );
    assert_eq!(
        second.file_name().unwrap().to_string_lossy().into_owned(),
        expected
    );

    let first_pixels = decode(&fs::read(&first).unwrap()).pixels;
    let second_pixels = decode(&fs::read(&second).unwrap()).pixels;
    assert_eq!(first_pixels, rgb_of(&image));
    assert_eq!(second_pixels, first_pixels);
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 2);
}

#[test]
fn saving_into_a_file_path_fails_and_leaves_nothing() {
    let scratch = Scratch::new("into-a-file");
    let blocker = scratch.path("blocker");
    fs::write(&blocker, b"not a folder").unwrap();

    let error = save_screenshot(
        &opaque(2, 2),
        1.0,
        SystemTime::UNIX_EPOCH,
        &blocker,
    )
    .unwrap_err();

    assert!(matches!(error, SaveError::Io(_)), "{error:?}");
    assert!(blocker.is_file());
    assert_eq!(fs::read(&blocker).unwrap(), b"not a folder");
    let names: Vec<String> = fs::read_dir(&scratch.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["blocker"]);
}

#[test]
fn copy_movie_copies_bytes_and_picks_a_free_name() {
    let scratch = Scratch::new("movie");
    let movie = scratch.path("Recording 2026-11-11 143205.mp4");
    let bytes = b"not really an mp4\x00\x01\x02";
    fs::write(&movie, bytes).unwrap();
    let folder = scratch.path("captures");

    let first = copy_movie(&movie, &folder).unwrap();
    let second = copy_movie(&movie, &folder).unwrap();

    assert_eq!(first, folder.join("Recording 2026-11-11 143205.mp4"));
    assert_eq!(second, folder.join("Recording 2026-11-11 143205 (2).mp4"));
    assert_eq!(fs::read(&first).unwrap(), bytes);
    assert_eq!(fs::read(&second).unwrap(), bytes);
    let names: Vec<String> = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.iter().all(|name| !name.ends_with(".tmp")), "{names:?}");
}

#[test]
fn background_save_calls_done_once() {
    let scratch = Scratch::new("background");
    let folder = scratch.path("shots");
    let (sender, receiver) = mpsc::channel();

    save_screenshot_in_background(
        Arc::new(opaque(3, 3)),
        1.0,
        SystemTime::UNIX_EPOCH,
        folder.clone(),
        move |result| {
            sender.send(result).unwrap();
        },
    );

    let path = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("done was called")
        .unwrap();
    assert!(path.is_file());
    assert!(folder.is_dir());
    // The sender lives only inside `done`, so after the one call the channel is closed.
    assert!(
        receiver.recv_timeout(Duration::from_millis(200)).is_err(),
        "done was called more than once"
    );
}

#[test]
fn ten_background_saves_started_together_make_ten_files() {
    let scratch = Scratch::new("race");
    let folder = scratch.path("shots");
    let image = Arc::new(opaque(8, 8));
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let (sender, receiver) = mpsc::channel();

    for _ in 0..10 {
        let sender = sender.clone();
        save_screenshot_in_background(
            Arc::clone(&image),
            1.0,
            at,
            folder.clone(),
            move |result| {
                sender.send(result).unwrap();
            },
        );
    }
    drop(sender);

    let mut paths = Vec::new();
    for _ in 0..10 {
        let path = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("every background save finished")
            .unwrap();
        assert!(path.is_file(), "{path:?}");
        paths.push(path);
    }

    let unique: BTreeSet<&PathBuf> = paths.iter().collect();
    assert_eq!(unique.len(), 10, "{paths:?}");
    let expected = rgb_of(&image);
    for path in &paths {
        assert_eq!(decode(&fs::read(path).unwrap()).pixels, expected, "{path:?}");
    }

    let names: Vec<String> = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 10, "{names:?}");
    assert!(
        names.iter().all(|name| name.ends_with(".png") && !name.starts_with('.')),
        "{names:?}"
    );
}

#[test]
fn errors_describe_themselves() {
    let encode = SaveError::Encode("boom".to_owned());
    assert!(encode.to_string().contains("boom"));
    assert!(std::error::Error::source(&encode).is_none());

    let io_error = SaveError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "no"));
    assert!(io_error.to_string().contains("no"));
    assert!(std::error::Error::source(&io_error).is_some());
}

/// Not an assertion: prints the encode time and file size of a 4K screenshot-like image.
/// Run with `cargo test -p igorio-app --test saver -- --ignored --nocapture`.
#[test]
#[ignore = "measures a 3840x2160 encode"]
fn encode_a_4k_screenshot() {
    let image = synthetic_screenshot(3840, 2160);
    let start = Instant::now();
    let bytes = encode_png(&image, 1.0).unwrap();
    let elapsed = start.elapsed();
    println!(
        "3840x2160 opaque, Compression::Fast: {elapsed:?}, {} bytes ({} KiB)",
        bytes.len(),
        bytes.len() / 1024
    );
    assert!(!bytes.is_empty());
}

/// Flat panels, a gradient sidebar and bands of letter-like dashes: roughly what a screenshot
/// of a text-heavy window compresses like.
fn synthetic_screenshot(width: u32, height: u32) -> Image {
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    let sidebar = width / 6;
    for y in 0..height {
        for x in 0..width {
            let pixel = if y < height / 24 {
                [32, 34, 38, 255] // title bar
            } else if x < sidebar {
                let shade = (210 - (y * 120 / height)) as u8; // sidebar gradient
                [shade, shade, shade.saturating_add(8), 255]
            } else if y > height * 23 / 24 {
                [238, 238, 240, 255] // status bar
            } else if text_like(x - sidebar, y) {
                [64, 66, 70, 255]
            } else {
                [252, 252, 253, 255]
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    Image::new(width, height, rgba).expect("valid image")
}

fn text_like(x: u32, y: u32) -> bool {
    let band = (y / 40) % 4;
    if band == 3 || y % 40 >= 8 + band * 3 {
        return false;
    }
    x > 24 && x < 900 && !(x / (6 + band)).is_multiple_of(7)
}

#[test]
fn save_as_writes_the_chosen_name_and_replaces_it() {
    let scratch = Scratch::new("save-as");
    let path = scratch.path("pinned.png");
    fs::write(&path, b"old").unwrap();
    let image = opaque(5, 4);
    assert_eq!(save_png_as(&image, 1.25, &path).unwrap(), path);
    assert_eq!(fs::read(&path).unwrap(), encode_png(&image, 1.25).unwrap());
    // Only the PNG is left: the temp file was renamed over it.
    assert_eq!(fs::read_dir(&scratch.dir).unwrap().count(), 1);
    // A folder that does not exist is an error and leaves nothing behind.
    assert!(matches!(
        save_png_as(&image, 1.0, &scratch.path("missing").join("x.png")),
        Err(SaveError::Io(_))
    ));
}
