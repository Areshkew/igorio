//! Saves held screenshots and recordings into the folder the user chose.
//!
//! From `ScreenshotSaver.swift` and `FinderTarget.swift`: names follow the Snipping Tool
//! pattern, an existing file is never overwritten, the PNG carries the display's pixel
//! density so viewers show it at its on-screen size, and encoding happens off the caller's
//! thread. Files are written through a temp file in the same folder and then renamed, so a
//! crash never leaves half a PNG under the final name.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use igorio_kit::image::Image;
use png::{BitDepth, ColorType, Compression, Encoder, PixelDimensions, Unit};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

/// Windows' baseline: 96 physical pixels per logical pixel at 100% scaling.
const BASE_DPI: f64 = 96.0;
/// Metres in an inch, for converting DPI to the pHYs chunk's pixels per metre.
const METRES_PER_INCH: f64 = 0.0254;
/// Counter that keeps temp names distinct within one process.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum SaveError {
    Io(io::Error),
    Encode(String),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Io(error) => write!(f, "could not save the file: {error}"),
            SaveError::Encode(message) => write!(f, "could not encode the PNG: {message}"),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SaveError::Io(error) => Some(error),
            SaveError::Encode(_) => None,
        }
    }
}

impl From<io::Error> for SaveError {
    fn from(error: io::Error) -> Self {
        SaveError::Io(error)
    }
}

/// PNG bytes for an image. `scale` is physical pixels per logical pixel (1.0 at 96 DPI); it is
/// written as a pHYs chunk (pixels per metre = 96 * scale / 0.0254, rounded) so viewers show
/// the image at its on-screen size.
///
/// 8-bit RGBA in straight alpha, as `Image` stores it. When every pixel is opaque the alpha
/// channel is dropped and the file is RGB: smaller, and no app can show a wrong background
/// behind a "transparent" screenshot. Compression is the png crate's `Fast` level (its quick
/// setting; screenshots are large and the default level costs noticeably more time).
pub fn encode_png(image: &Image, scale: f64) -> Result<Vec<u8>, SaveError> {
    let pixel_dims = pixel_dimensions(scale)?;
    let opaque = image.rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255);

    let mut bytes = Vec::new();
    {
        let mut encoder = Encoder::new(&mut bytes, image.width, image.height);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_color(if opaque { ColorType::Rgb } else { ColorType::Rgba });
        encoder.set_compression(Compression::Fast);
        encoder.set_pixel_dims(Some(pixel_dims));
        let mut writer = encoder.write_header().map_err(encode_error)?;
        if opaque {
            let mut rgb = Vec::with_capacity(image.rgba.len() / 4 * 3);
            for pixel in image.rgba.as_chunks::<4>().0 {
                rgb.extend_from_slice(&pixel[..3]);
            }
            writer.write_image_data(&rgb).map_err(encode_error)?;
        } else {
            writer.write_image_data(&image.rgba).map_err(encode_error)?;
        }
    }
    Ok(bytes)
}

/// The pHYs value for a scale, or an error when the scale cannot be stored. Rounding is
/// plain `f64::round`: 96/0.0254 is 3779.5275..., so scale 1.25 gives 120/0.0254 =
/// 4724.4094 -> 4724 (not 4725) and scale 2.0 gives 192/0.0254 = 7559.0551 -> 7559.
fn pixel_dimensions(scale: f64) -> Result<PixelDimensions, SaveError> {
    let per_metre = (BASE_DPI / METRES_PER_INCH * scale).round();
    if !per_metre.is_finite() || !(1.0..=f64::from(u32::MAX)).contains(&per_metre) {
        return Err(SaveError::Encode(format!(
            "scale {scale} does not give a usable pixel density"
        )));
    }
    let per_metre = per_metre as u32;
    Ok(PixelDimensions {
        xppu: per_metre,
        yppu: per_metre,
        unit: Unit::Meter,
    })
}

fn encode_error(error: png::EncodingError) -> SaveError {
    SaveError::Encode(error.to_string())
}

/// "Screenshot 2026-11-11 143205.png": the pattern Windows uses for Win+PrtScn files is
/// "Screenshot (N).png" and Snipping Tool uses "Screenshot 2026-11-11 143205.png"; use the
/// Snipping Tool pattern, with the LOCAL date and time of `captured_at`.
pub fn screenshot_file_name(captured_at: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = local_parts(captured_at);
    format_name(year, month, day, hour, minute, second)
}

/// The name itself, without touching the clock or the time zone. Public so the integration
/// test can check the zero padding directly.
pub fn format_name(year: u16, month: u16, day: u16, hour: u16, minute: u16, second: u16) -> String {
    format!("Screenshot {year:04}-{month:02}-{day:02} {hour:02}{minute:02}{second:02}.png")
}

/// Local wall-clock parts of an instant, through Windows' own time zone rules. Falls back to
/// UTC if the local conversion fails, and to the epoch if the instant cannot be represented.
pub(crate) fn local_parts(captured_at: SystemTime) -> (u16, u16, u16, u16, u16, u16) {
    // SAFETY: both calls only read the FILETIME/SYSTEMTIME values behind the pointers (all
    // live for the whole block) and write into `mut` locals that also live across the call;
    // no pointer escapes. `None` asks for the machine's current time zone.
    unsafe {
        let mut utc = SYSTEMTIME::default();
        if FileTimeToSystemTime(&file_time(captured_at), &mut utc).is_err() {
            return (1970, 1, 1, 0, 0, 0);
        }
        let mut local = SYSTEMTIME::default();
        if SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            return (utc.wYear, utc.wMonth, utc.wDay, utc.wHour, utc.wMinute, utc.wSecond);
        }
        (local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond)
    }
}

/// A `SystemTime` as a Win32 FILETIME: 100 ns ticks since 1601-01-01 UTC. Times outside the
/// FILETIME range saturate at its ends (year 1601 is the earliest it can name).
fn file_time(time: SystemTime) -> FILETIME {
    /// 1601-01-01 to 1970-01-01 in 100 ns ticks.
    const EPOCH_TICKS: u64 = 116_444_736_000_000_000;

    fn ticks(duration: std::time::Duration) -> u64 {
        duration
            .as_secs()
            .saturating_mul(10_000_000)
            .saturating_add(u64::from(duration.subsec_nanos()) / 100)
    }

    let since_epoch = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => EPOCH_TICKS.saturating_add(ticks(after)),
        Err(before) => EPOCH_TICKS.saturating_sub(ticks(before.duration())),
    };
    FILETIME {
        dwLowDateTime: since_epoch as u32,
        dwHighDateTime: (since_epoch >> 32) as u32,
    }
}

/// A path in `folder` that does not exist yet: `name`, or "stem (2).ext", "stem (3).ext", ...
///
/// This is only a suggestion for callers that need a name up front; it can go stale at once.
/// `save_screenshot` and `copy_movie` claim the name atomically when they write.
pub fn free_path(folder: &Path, name: &str) -> PathBuf {
    free_path_os(folder, OsStr::new(name))
}

fn free_path_os(folder: &Path, name: &OsStr) -> PathBuf {
    let first = folder.join(name);
    if !first.exists() {
        return first;
    }
    let mut number = 2;
    loop {
        let candidate = folder.join(numbered_name(name, number));
        if !candidate.exists() {
            return candidate;
        }
        number += 1;
    }
}

/// `name` for `number` 1, "stem (2).ext" for 2, "stem (3).ext" for 3, ...
fn numbered_name(name: &OsStr, number: u32) -> OsString {
    if number <= 1 {
        return name.to_os_string();
    }
    let path = Path::new(name);
    let mut result = path.file_stem().map(OsStr::to_os_string).unwrap_or_default();
    result.push(format!(" ({number})"));
    if let Some(extension) = path.extension() {
        result.push(".");
        result.push(extension);
    }
    result
}

/// Encodes and writes the image into `folder` (created if missing) under a free name. Writes to
/// a temp file in the same folder first and renames it, so a half-written PNG never appears
/// under the final name. Returns the final path.
pub fn save_screenshot(
    image: &Image,
    scale: f64,
    captured_at: SystemTime,
    folder: &Path,
) -> Result<PathBuf, SaveError> {
    // Fail before spending time on a large encode when the folder cannot be used.
    fs::create_dir_all(folder)?;
    let bytes = encode_png(image, scale)?;
    let name = screenshot_file_name(captured_at);
    write_and_publish(folder, OsStr::new(&name), |file| file.write_all(&bytes))
}

/// Encodes and writes the image at `path`, a name the user chose in a Save dialog (which asked
/// before replacing a file). Writes to a temp file in the same folder first and renames it over
/// `path`, so a half-written PNG never appears under that name.
pub fn save_png_as(image: &Image, scale: f64, path: &Path) -> Result<PathBuf, SaveError> {
    let (folder, name) = match (path.parent(), path.file_name()) {
        (Some(folder), Some(name)) => (folder, name),
        _ => {
            return Err(SaveError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a file path", path.display()),
            )));
        }
    };
    let bytes = encode_png(image, scale)?;
    let (temp, mut file) = create_temp(folder, name)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp, path));
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(SaveError::Io(error));
    }
    Ok(path.to_path_buf())
}

/// Copies a recording file into `folder` under a free name based on its own file name.
pub fn copy_movie(movie: &Path, folder: &Path) -> Result<PathBuf, SaveError> {
    fs::create_dir_all(folder)?;
    let name = movie.file_name().ok_or_else(|| {
        SaveError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", movie.display()),
        ))
    })?;
    let mut source = File::open(movie)?;
    write_and_publish(folder, name, |file| io::copy(&mut source, file).map(|_| ()))
}

/// Runs `save_screenshot` on a worker thread and calls `done` with the result on that thread.
/// The image is shared, not copied.
pub fn save_screenshot_in_background(
    image: Arc<Image>,
    scale: f64,
    captured_at: SystemTime,
    folder: PathBuf,
    done: impl FnOnce(Result<PathBuf, SaveError>) + Send + 'static,
) {
    std::thread::spawn(move || done(save_screenshot(&image, scale, captured_at, &folder)));
}

/// Writes through a fresh temp file, then publishes it under the first free `name`.
/// `write` receives the open temp file; it is always removed on failure.
fn write_and_publish(
    folder: &Path,
    name: &OsStr,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<PathBuf, SaveError> {
    let (temp, mut file) = create_temp(folder, name)?;
    let written = write(&mut file);
    drop(file);
    let result = match written {
        Ok(()) => publish(folder, name, &temp),
        Err(error) => Err(SaveError::Io(error)),
    };
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Opens a new temp file in `folder` with a leading dot and a `.tmp` extension, so a file left
/// by a crash is never mistaken for a screenshot. Retries past a stale name from an old run.
fn create_temp(folder: &Path, name: &OsStr) -> Result<(PathBuf, File), SaveError> {
    loop {
        let mut file_name = OsString::from(".");
        file_name.push(name);
        file_name.push(format!(
            ".{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let path = folder.join(file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SaveError::Io(error)),
        }
    }
}

/// Moves the complete temp file to the first name nobody else has claimed.
///
/// The choice here is the create-new claim rather than a rename that fails on an existing
/// target: Rust's `fs::rename` on Windows replaces the target, so a check-then-rename would
/// let two saves at the same moment both pick "Screenshot ....png" and one would vanish.
/// `create_new` is atomic, so a racing save gets `(2)`, `(3)`, ... instead. The claim is an
/// empty placeholder for the moment between the open and the rename; after the rename the
/// final name only ever holds the complete file, never a prefix.
fn publish(folder: &Path, name: &OsStr, temp: &Path) -> Result<PathBuf, SaveError> {
    let mut number = 1;
    loop {
        let candidate = folder.join(numbered_name(name, number));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(claim) => {
                drop(claim);
                return match fs::rename(temp, &candidate) {
                    Ok(()) => Ok(candidate),
                    Err(error) => {
                        let _ = fs::remove_file(&candidate);
                        Err(SaveError::Io(error))
                    }
                };
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => number += 1,
            Err(error) => return Err(SaveError::Io(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_local_and_follow_the_snipping_tool_pattern() {
        // Every real instant formats to the same shape; the zone only shifts the fields.
        let name = screenshot_file_name(SystemTime::UNIX_EPOCH);
        assert_eq!(name.len(), "Screenshot 1970-01-01 000000.png".len(), "{name}");
        assert!(name.starts_with("Screenshot "), "{name}");
        assert!(name.ends_with(".png"), "{name}");
    }

    #[test]
    fn a_scale_that_cannot_be_stored_is_an_encode_error() {
        assert!(encode_png(&image(1, 1, &[0, 0, 0, 255]), 0.0).is_err());
        assert!(encode_png(&image(1, 1, &[0, 0, 0, 255]), f64::NAN).is_err());
    }

    /// A one-pixel image, for the scale checks above.
    fn image(width: u32, height: u32, pixel: &[u8; 4]) -> Image {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width * height {
            rgba.extend_from_slice(pixel);
        }
        Image::new(width, height, rgba).expect("valid image")
    }
}
