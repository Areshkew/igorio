//! Regenerate assets/igorio.ico with `cargo run -p igorio-app --example app_icon`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use igorio_app::tray::logo_pixmap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sizes = [16, 20, 24, 32, 40, 48, 64, 256];
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/igorio.ico");
    let mut file = BufWriter::new(File::create(path)?);
    file.write_all(&[0, 0, 1, 0, sizes.len() as u8, 0])?;

    // An ICO directory followed by RGBA PNG images. A dimension byte of 0 means 256.
    let mut offset = 6 + 16 * sizes.len() as u32;
    let mut images = Vec::new();
    for size in sizes {
        let pixmap = logo_pixmap(size, [0, 0, 0, 255]);
        let mut image = Vec::new();
        let mut encoder = png::Encoder::new(&mut image, size, size);
        encoder.set_color(png::ColorType::Rgba);
        encoder.write_header()?.write_image_data(pixmap.data())?;
        file.write_all(&[size as u8, size as u8, 0, 0, 1, 0, 32, 0])?;
        file.write_all(&(image.len() as u32).to_le_bytes())?;
        file.write_all(&offset.to_le_bytes())?;
        offset += image.len() as u32;
        images.push(image);
    }
    for image in images {
        file.write_all(&image)?;
    }
    file.flush()?;
    Ok(())
}
