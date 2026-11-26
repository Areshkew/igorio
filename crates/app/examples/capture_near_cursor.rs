//! Saves a PNG of the screen around the mouse cursor, layered windows included.
//! For checking by eye what the overlay shows: run the app with FAIRY_CAPTURABLE=1, then
//! `cargo run -p igorio-app --example capture_near_cursor -- out.png`.

use std::ffi::c_void;
use std::fs::File;
use std::io::BufWriter;

use igorio_app::overlay;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, ReleaseDC, SRCCOPY, SelectObject,
};

const WIDTH: i32 = 520;
const HEIGHT: i32 = 440;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: capture_near_cursor <out.png>")?;
    overlay::ensure_dpi_awareness();
    let cursor = overlay::cursor_pos();
    let (left, top) = (cursor.x as i32 - 140, cursor.y as i32 - 160);

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: WIDTH,
            biHeight: -HEIGHT, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: every handle is checked or used only after a successful create, `bits` points
    // to WIDTH * HEIGHT * 4 bytes owned by the DIB section, and it is read before the
    // section is deleted.
    let bgra = unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let dib = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let old = SelectObject(memory, dib.into());
        let copied = BitBlt(memory, 0, 0, WIDTH, HEIGHT, Some(screen), left, top, SRCCOPY | CAPTUREBLT);
        let pixels = std::slice::from_raw_parts(bits as *const u8, (WIDTH * HEIGHT * 4) as usize).to_vec();
        SelectObject(memory, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        copied?;
        pixels
    };

    let rgba: Vec<u8> = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    let mut encoder = png::Encoder::new(BufWriter::new(File::create(&path)?), WIDTH as u32, HEIGHT as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgba)?;
    println!("saved {path} (cursor at {}, {})", cursor.x, cursor.y);
    Ok(())
}
