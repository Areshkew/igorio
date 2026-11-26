use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, SetLastError, WIN32_ERROR,
};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::*;
use windows::core::{Error, Result, w};

const OPEN_ATTEMPTS: usize = 25;
const OPEN_RETRY: Duration = Duration::from_millis(20);
const WIDTH: usize = 64;
const HEIGHT: usize = 64;

type Formats = Vec<(u32, Vec<u8>)>;

struct Snapshot {
    formats: Formats,
    skipped: Vec<(u32, &'static str)>,
}

impl Snapshot {
    fn save(
        ids: impl IntoIterator<Item = u32>,
        mut read: impl FnMut(u32) -> Option<Vec<u8>>,
    ) -> Self {
        let mut snapshot = Self {
            formats: Vec::new(),
            skipped: Vec::new(),
        };
        for id in ids {
            if let Some(reason) = opaque_format(id) {
                snapshot.skipped.push((id, reason));
            } else if let Some(bytes) = read(id) {
                snapshot.formats.push((id, bytes));
            } else {
                snapshot
                    .skipped
                    .push((id, "unavailable or no lockable HGLOBAL"));
            }
        }
        snapshot
    }
}

fn opaque_format(id: u32) -> Option<&'static str> {
    match id {
        2 | 9 | 0x82 => Some("GDI bitmap or palette"),
        3 | 14 | 0x83 | 0x8e => Some("metafile or handle-bearing metafile picture"),
        0x80 => Some("owner-drawn display"),
        0x200..=0x2ff => Some("private format managed by original owner"),
        _ => None,
    }
}

fn format_name(id: u32) -> String {
    let standard = match id {
        2 => "CF_BITMAP",
        3 => "CF_METAFILEPICT",
        9 => "CF_PALETTE",
        14 => "CF_ENHMETAFILE",
        0x80 => "CF_OWNERDISPLAY",
        0x82 => "CF_DSPBITMAP",
        0x83 => "CF_DSPMETAFILEPICT",
        0x8e => "CF_DSPENHMETAFILE",
        _ => "",
    };
    let mut name = [0u16; 256];
    let len = unsafe { GetClipboardFormatNameW(id, &mut name) };
    let name = if len > 0 {
        String::from_utf16_lossy(&name[..len as usize])
    } else {
        standard.into()
    };
    if name.is_empty() {
        format!("{id:#06x}")
    } else {
        format!("{name} ({id:#06x})")
    }
}

struct Global(HGLOBAL);

impl Global {
    fn new(bytes: &[u8]) -> Result<Self> {
        let block = Self(unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())? });
        let data = unsafe { GlobalLock(block.0) };
        if data.is_null() {
            return Err(Error::from_thread());
        }
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len()) };
        let _ = unsafe { GlobalUnlock(block.0) };
        Ok(block)
    }

    fn set(self, id: u32) -> Result<()> {
        unsafe { SetClipboardData(id, Some(HANDLE(self.0.0)))? };
        // SetClipboardData owns the allocation after a successful transfer.
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        let _ = unsafe { GlobalFree(Some(self.0)) };
    }
}

fn read_global(handle: *mut c_void) -> Option<Vec<u8>> {
    unsafe {
        let global = HGLOBAL(handle);
        let size = GlobalSize(global);
        if size == 0 {
            return None;
        }
        let data = GlobalLock(global);
        if data.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(global);
        Some(bytes)
    }
}

struct OpenClipboardGuard;

impl OpenClipboardGuard {
    fn open(owner: HWND) -> Result<Self> {
        for _ in 1..OPEN_ATTEMPTS {
            if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                return Ok(Self);
            }
            thread::sleep(OPEN_RETRY);
        }
        unsafe { OpenClipboard(Some(owner))? };
        Ok(Self)
    }

    fn save(&self) -> Result<Snapshot> {
        let mut ids = Vec::new();
        let mut prev = 0;
        loop {
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let id = unsafe { EnumClipboardFormats(prev) };
            if id == 0 {
                let error = unsafe { GetLastError() };
                if error.0 != 0 {
                    return Err(Error::from_hresult(error.to_hresult()));
                }
                break;
            }
            ids.push(id);
            prev = id;
        }
        Ok(Snapshot::save(ids, |id| {
            let handle = unsafe { GetClipboardData(id) }.ok()?;
            read_global(handle.0)
        }))
    }

    fn write(&self, prepared: Vec<(u32, Global)>) -> Result<()> {
        let mut first_error = None;
        for (id, data) in prepared {
            if let Err(err) = data.set(id) {
                eprintln!(
                    "SetClipboardData {}: {}",
                    format_name(id),
                    crate::error(&err)
                );
                first_error.get_or_insert(err);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

fn prepare(formats: &Formats) -> Result<Vec<(u32, Global)>> {
    formats
        .iter()
        .map(|(id, bytes)| Ok((*id, Global::new(bytes)?)))
        .collect()
}

pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

pub struct Restore {
    saved: Vec<(u32, Global)>,
    sequence: u32,
}

impl Restore {
    pub fn still_current(&self) -> bool {
        let current = sequence();
        if current == self.sequence {
            return true;
        }
        println!(
            "Clipboard sequence changed ({} -> {current}). Leaving the newer clipboard intact.",
            self.sequence
        );
        false
    }

    pub fn restore(self, owner: HWND) -> Result<()> {
        let clipboard = OpenClipboardGuard::open(owner)?;
        // Check under the clipboard lock so a new copy cannot race the restore.
        if self.still_current() {
            unsafe { EmptyClipboard()? };
            clipboard.write(self.saved)?;
            println!("Clipboard restored (sequence {}).", sequence());
        }
        Ok(())
    }
}

pub fn stage(owner: HWND, formats: &Formats) -> Result<Restore> {
    let outgoing = prepare(formats)?;
    let clipboard = OpenClipboardGuard::open(owner)?;
    let snapshot = clipboard.save()?;
    println!(
        "Saved {} HGLOBAL clipboard formats.",
        snapshot.formats.len()
    );
    for (id, reason) in &snapshot.skipped {
        println!("Skipped clipboard format {} ({reason}).", format_name(*id));
    }
    let saved = prepare(&snapshot.formats)?;
    unsafe { EmptyClipboard()? };
    if let Err(err) = clipboard.write(outgoing) {
        if let Err(restore_err) = unsafe { EmptyClipboard() }.and_then(|()| clipboard.write(saved))
        {
            eprintln!("Clipboard rollback failed: {}", crate::error(&restore_err));
        }
        return Err(err);
    }
    let sequence = sequence();
    println!("Clipboard staged (sequence {sequence}).");
    Ok(Restore { saved, sequence })
}

fn test_image() -> (Vec<u8>, Vec<u8>) {
    let mut rgb = Vec::with_capacity(WIDTH * HEIGHT * 3);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let color = if (x / 8 + y / 8) % 2 == 0 {
                [56, 121, 230]
            } else {
                [255, 217, 82]
            };
            rgb.extend(color);
        }
    }

    // BITMAPINFOHEADER followed by aligned bottom-up 24-bit BGR rows (64 * 3 is aligned).
    let mut dib = Vec::with_capacity(40 + rgb.len());
    dib.extend(40u32.to_le_bytes());
    dib.extend((WIDTH as i32).to_le_bytes());
    dib.extend((HEIGHT as i32).to_le_bytes());
    dib.extend(1u16.to_le_bytes());
    dib.extend(24u16.to_le_bytes());
    dib.extend(0u32.to_le_bytes()); // BI_RGB
    dib.extend((rgb.len() as u32).to_le_bytes());
    dib.extend([0; 16]);
    for row in rgb.chunks_exact(WIDTH * 3).rev() {
        for pixel in row.chunks_exact(3) {
            dib.extend([pixel[2], pixel[1], pixel[0]]);
        }
    }
    (rgb, dib)
}

pub fn payload(terminal: bool) -> std::result::Result<Formats, Box<dyn std::error::Error>> {
    let (rgb, dib) = test_image();
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, WIDTH as u32, HEIGHT as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.write_header()?.write_image_data(&rgb)?;
    if terminal {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path: PathBuf =
            std::env::temp_dir().join(format!("uia-paste-{}-{stamp}.png", std::process::id()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?
            .write_all(&png)?;
        println!("Terminal PNG: {}", path.display());
        let text = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        return Ok(vec![(CF_UNICODETEXT.0 as u32, text)]);
    }
    let format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    if format == 0 {
        return Err(Error::from_thread().into());
    }
    Ok(vec![(format, png), (CF_DIB.0 as u32, dib)])
}

pub fn selftest() -> Result<bool> {
    let fixture = vec![
        (CF_UNICODETEXT.0 as u32, vec![65, 0, 0, 0]),
        (CF_DIB.0 as u32, test_image().1),
    ];
    let allocated = prepare(&fixture)?;
    let ids = allocated
        .iter()
        .map(|(id, _)| *id)
        .chain([CF_BITMAP.0 as u32]);
    let snapshot = Snapshot::save(ids, |id| {
        allocated
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .and_then(|(_, block)| read_global(block.0.0))
    });
    drop(allocated);
    let restored = prepare(&snapshot.formats)?;
    Ok(
        snapshot.skipped == [(CF_BITMAP.0 as u32, "GDI bitmap or palette")]
            && restored.len() == fixture.len()
            && restored
                .iter()
                .zip(fixture)
                .all(|((id, block), (expected_id, bytes))| {
                    *id == expected_id
                        && read_global(block.0.0).is_some_and(|copy| copy.starts_with(&bytes))
                }),
    )
}
