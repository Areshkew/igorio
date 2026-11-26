use std::{fmt, mem::size_of, slice};
use windows::{
    Win32::{
        Foundation::{ERROR_SUCCESS, GetLastError, HGLOBAL, HWND, SetLastError},
        Graphics::Gdi::{BITMAP, GetObjectW, HGDIOBJ},
        System::{
            DataExchange::*,
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        },
    },
    core::Error,
};

use crate::{classify::Signals, observation::WindowInfo};

const CF_BITMAP: u32 = 2;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;

const HISTORY: &str = "CanIncludeInClipboardHistory";
const CLOUD: &str = "CanUploadToCloudClipboard";
const EXCLUDE: &str = "ExcludeClipboardContentFromMonitorProcessing";

pub type PixelSize = (u32, u32);

pub struct Snapshot {
    pub sequence: u32,
    pub owner: WindowInfo,
    pub formats: Vec<(u32, String)>,
    pub dimensions: Option<PixelSize>,
    pub error: Option<Error>,
    history: Option<u32>,
    cloud: Option<u32>,
}

impl Snapshot {
    pub fn read(window: Option<HWND>, read_payload_headers: bool) -> Self {
        let mut snapshot = Self {
            sequence: 0,
            owner: WindowInfo::default(),
            formats: Vec::new(),
            dimensions: None,
            error: None,
            history: None,
            cloud: None,
        };
        unsafe {
            let opened = OpenClipboard(window);
            snapshot.sequence = GetClipboardSequenceNumber();
            let owner = GetClipboardOwner().unwrap_or_default();
            if let Err(error) = opened {
                snapshot.error = Some(error);
            } else {
                let mut id = 0;
                loop {
                    SetLastError(ERROR_SUCCESS);
                    id = EnumClipboardFormats(id);
                    if id == 0 {
                        if GetLastError() != ERROR_SUCCESS {
                            snapshot.error = Some(Error::from_thread());
                        }
                        break;
                    }
                    snapshot.formats.push((id, format_name(id)));
                }
                if read_payload_headers {
                    snapshot.dimensions = snapshot.image_dimensions();
                    snapshot.history = snapshot.marker_value(HISTORY);
                    snapshot.cloud = snapshot.marker_value(CLOUD);
                    // Reading a delayed-rendered format can advance the sequence.
                    snapshot.sequence = GetClipboardSequenceNumber();
                }
                let _ = CloseClipboard();
            }
            snapshot.owner = WindowInfo::read(owner);
        }
        snapshot
    }

    pub fn signals(&self) -> Signals {
        Signals {
            clipboard_read: self.error.is_none(),
            image: self
                .formats
                .iter()
                .any(|(id, _)| matches!(*id, CF_BITMAP | CF_DIB | CF_DIBV5))
                || ["PNG", "image/png"]
                    .iter()
                    .any(|name| self.id(name).is_some()),
            file_transfer: [
                "CF_HDROP",
                "Shell IDList Array",
                "FileGroupDescriptor",
                "FileGroupDescriptorW",
                "FileContents",
            ]
            .iter()
            .any(|name| self.id(name).is_some()),
            html: ["HTML Format", "text/html"]
                .iter()
                .any(|name| self.id(name).is_some()),
            text: [
                "CF_TEXT",
                "CF_OEMTEXT",
                "CF_UNICODETEXT",
                "Rich Text Format",
            ]
            .iter()
            .any(|name| self.id(name).is_some()),
            capture_owner: self.owner.capture_process,
            ..Default::default()
        }
    }

    pub fn markers(&self) -> impl fmt::Display {
        fn value(id: Option<u32>, value: Option<u32>) -> String {
            match (id, value) {
                (Some(_), Some(value)) => value.to_string(),
                (Some(_), None) => "unreadable".into(),
                _ => "absent".into(),
            }
        }
        format!(
            "history={} cloud={} exclude_from_history_and_cloud={}",
            value(self.id(HISTORY), self.history),
            value(self.id(CLOUD), self.cloud),
            self.id(EXCLUDE).is_some(),
        )
    }

    fn id(&self, name: &str) -> Option<u32> {
        self.formats
            .iter()
            .find(|(_, format)| format.eq_ignore_ascii_case(name))
            .map(|(id, _)| *id)
    }

    fn marker_value(&self, name: &str) -> Option<u32> {
        let bytes = global_prefix::<4>(self.id(name)?)?;
        Some(u32::from_le_bytes(bytes))
    }

    fn image_dimensions(&self) -> Option<PixelSize> {
        for (id, name) in &self.formats {
            let size = match *id {
                CF_DIB | CF_DIBV5 => global_prefix::<12>(*id).and_then(dib_size),
                CF_BITMAP => bitmap_size(),
                _ if ["PNG", "image/png"]
                    .iter()
                    .any(|png| name.eq_ignore_ascii_case(png)) =>
                {
                    global_prefix::<24>(*id).and_then(png_size)
                }
                _ => None,
            };
            if size.is_some() {
                return size;
            }
        }
        None
    }
}

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "seq={} formats={:?} owner={}",
            self.sequence, self.formats, self.owner
        )?;
        if let Some(error) = &self.error {
            write!(f, " error={error:?}")?;
        }
        Ok(())
    }
}

pub fn format_size(size: Option<PixelSize>) -> String {
    size.map_or_else(|| "unknown".into(), |(w, h)| format!("{w}x{h}"))
}

fn format_name(id: u32) -> String {
    const STANDARD: [&str; 18] = [
        "",
        "CF_TEXT",
        "CF_BITMAP",
        "CF_METAFILEPICT",
        "CF_SYLK",
        "CF_DIF",
        "CF_TIFF",
        "CF_OEMTEXT",
        "CF_DIB",
        "CF_PALETTE",
        "CF_PENDATA",
        "CF_RIFF",
        "CF_WAVE",
        "CF_UNICODETEXT",
        "CF_ENHMETAFILE",
        "CF_HDROP",
        "CF_LOCALE",
        "CF_DIBV5",
    ];
    let name = match id {
        1..=17 => STANDARD[id as usize],
        0x0080 => "CF_OWNERDISPLAY",
        0x0081 => "CF_DSPTEXT",
        0x0082 => "CF_DSPBITMAP",
        0x0083 => "CF_DSPMETAFILEPICT",
        0x008e => "CF_DSPENHMETAFILE",
        _ => {
            let mut name = [0u16; 256];
            let len = unsafe { GetClipboardFormatNameW(id, &mut name) };
            return if len > 0 {
                String::from_utf16_lossy(&name[..len as usize])
            } else {
                format!("#{id:#06x}")
            };
        }
    };
    name.into()
}

// Clipboard-owned memory is valid only while the clipboard is open and the handle is locked.
fn global_prefix<const N: usize>(format: u32) -> Option<[u8; N]> {
    unsafe {
        let memory = HGLOBAL(GetClipboardData(format).ok()?.0);
        if GlobalSize(memory) < N {
            return None;
        }
        let data = GlobalLock(memory).cast::<u8>();
        if data.is_null() {
            return None;
        }
        let mut prefix = [0; N];
        prefix.copy_from_slice(slice::from_raw_parts(data, N));
        let _ = GlobalUnlock(memory);
        Some(prefix)
    }
}

fn dib_size(data: [u8; 12]) -> Option<PixelSize> {
    let header_size = u32::from_le_bytes(data[..4].try_into().unwrap());
    let (width, height) = match header_size {
        12 => (
            u16::from_le_bytes(data[4..6].try_into().unwrap()).into(),
            u16::from_le_bytes(data[6..8].try_into().unwrap()).into(),
        ),
        40.. => (
            u32::try_from(i32::from_le_bytes(data[4..8].try_into().unwrap())).ok()?,
            i32::from_le_bytes(data[8..12].try_into().unwrap()).unsigned_abs(),
        ),
        _ => return None,
    };
    (width > 0 && height > 0).then_some((width, height))
}

fn png_size(data: [u8; 24]) -> Option<PixelSize> {
    if &data[..16] != b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR" {
        return None;
    }
    let width = u32::from_be_bytes(data[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(data[20..24].try_into().unwrap());
    (width > 0 && height > 0).then_some((width, height))
}

fn bitmap_size() -> Option<PixelSize> {
    unsafe {
        let handle = GetClipboardData(CF_BITMAP).ok()?;
        let mut bitmap = BITMAP::default();
        let size = size_of::<BITMAP>() as i32;
        if GetObjectW(
            HGDIOBJ(handle.0),
            size,
            Some((&mut bitmap as *mut BITMAP).cast()),
        ) != size
        {
            return None;
        }
        let width = u32::try_from(bitmap.bmWidth).ok()?;
        let height = bitmap.bmHeight.unsigned_abs();
        (width > 0 && height > 0).then_some((width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_top_down_dib_dimensions() {
        let header = [40, 0, 0, 0, 32, 0, 0, 0, 224, 255, 255, 255];
        assert_eq!(dib_size(header), Some((32, 32)));
    }

    #[test]
    fn reads_png_dimensions() {
        let header = *b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x20\0\0\0\x10";
        assert_eq!(png_size(header), Some((32, 16)));
    }
}
