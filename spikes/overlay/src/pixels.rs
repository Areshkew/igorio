use std::{ffi::c_void, mem::size_of, ptr, slice};

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};
use windows::core::Error as WinError;

use crate::Result;

pub fn draw(hwnd: HWND, origin: POINT, size: SIZE, scale: f32) -> Result<()> {
    let mut pixmap =
        Pixmap::new(size.cx as u32, size.cy as u32).ok_or("could not allocate card pixmap")?;
    let mut paint = Paint::default();
    let mut rounded = |x: f32, y: f32, w: f32, h: f32, r: f32, color, transform| {
        let (right, bottom) = (x + w, y + h);
        let mut path = PathBuilder::new();
        path.move_to(x + r, y);
        path.line_to(right - r, y);
        path.quad_to(right, y, right, y + r);
        path.line_to(right, bottom - r);
        path.quad_to(right, bottom, right - r, bottom);
        path.line_to(x + r, bottom);
        path.quad_to(x, bottom, x, bottom - r);
        path.line_to(x, y + r);
        path.quad_to(x, y, x + r, y);
        path.close();
        let [red, green, blue, alpha] = color;
        paint.set_color_rgba8(red, green, blue, alpha);
        pixmap.fill_path(
            &path.finish().unwrap(),
            &paint,
            FillRule::Winding,
            transform,
            None,
        );
    };

    for (x, y, tilt, paper, picture) in [
        (30.0, 36.0, -13.0, [232, 221, 255, 255], [112, 86, 171, 255]),
        (57.0, 37.0, 12.0, [219, 242, 255, 255], [67, 135, 175, 255]),
        (45.0, 51.0, -4.0, [255, 244, 224, 255], [216, 137, 87, 255]),
    ] {
        let (w, h) = (148.0, 104.0);
        let transform =
            Transform::from_rotate_at(tilt, x + w / 2.0, y + h / 2.0).post_scale(scale, scale);
        for spread in (1..=12).rev() {
            let s = spread as f32;
            rounded(
                x - s,
                y + 6.0 - s,
                w + 2.0 * s,
                h + 2.0 * s,
                11.0 + s,
                [25, 20, 43, 2 * (13 - spread)],
                transform,
            );
        }
        rounded(x, y, w, h, 11.0, paper, transform);
        rounded(x + 8.0, y + 8.0, w - 16.0, 72.0, 6.0, picture, transform);
        rounded(
            x + 10.0,
            y + 88.0,
            50.0,
            5.0,
            2.5,
            [70, 57, 91, 130],
            transform,
        );
    }

    let mut dib = Dib::new(size)?;
    // tiny-skia's RGBA channels are already premultiplied. Win32 wants BGRA.
    for (src, dst) in pixmap
        .data()
        .chunks_exact(4)
        .zip(dib.bytes_mut().chunks_exact_mut(4))
    {
        dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
    }
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
        ..Default::default()
    };
    unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&origin),
            Some(&size),
            Some(dib.dc),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )?;
    }
    Ok(())
}

pub struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    data: *mut c_void,
    size: SIZE,
}

impl Dib {
    pub fn new(size: SIZE) -> Result<Self> {
        let mut dib = Self {
            dc: unsafe { CreateCompatibleDC(None) },
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            data: ptr::null_mut(),
            size,
        };
        if dib.dc.is_invalid() {
            return Err("CreateCompatibleDC failed".into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size.cx,
                biHeight: -size.cy, // top-down, like the tiny-skia pixmap
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            dib.bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut dib.data, None, 0)?;
            dib.previous = SelectObject(dib.dc, dib.bitmap.into());
        }
        if dib.previous.is_invalid() {
            return Err("SelectObject failed".into());
        }
        Ok(dib)
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        let len = self.size.cx as usize * self.size.cy as usize * 4;
        // The DIB owns a tightly packed 32-bit buffer. GDI is flushed before reading it.
        unsafe { slice::from_raw_parts_mut(self.data.cast(), len) }
    }

    pub fn capture(&mut self, rect: RECT) -> Result<&[u8]> {
        let screen = unsafe { GetDC(None) };
        if screen.is_invalid() {
            return Err("GetDC for the screen failed".into());
        }
        unsafe {
            // With plain SRCCOPY the positive control could omit layered windows too.
            let copied = BitBlt(
                self.dc,
                0,
                0,
                self.size.cx,
                self.size.cy,
                Some(screen),
                rect.left,
                rect.top,
                SRCCOPY | CAPTUREBLT,
            );
            let flushed = GdiFlush();
            ReleaseDC(None, screen);
            copied?;
            if !flushed.as_bool() {
                return Err(WinError::from_thread().into());
            }
        }
        Ok(self.bytes_mut())
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}
