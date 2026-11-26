use std::{
    collections::HashMap,
    ffi::OsString,
    fs, io,
    os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
    slice,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{ERROR_IO_INCOMPLETE, HANDLE},
        Storage::FileSystem::{
            FILE_ACTION_ADDED, FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME,
            ReadDirectoryChangesW,
        },
        System::{
            Com::CoTaskMemFree,
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        },
        UI::Shell::{FOLDERID_Screenshots, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath},
    },
    core::HRESULT,
};

use crate::clipboard::PixelSize;

const POLL: Duration = Duration::from_millis(100);
const FILE_TIMEOUT: Duration = Duration::from_secs(3);

pub enum Event {
    Status(String),
    Image {
        path: PathBuf,
        at: Instant,
        dimensions: Option<PixelSize>,
    },
}

pub fn known_path() -> windows::core::Result<PathBuf> {
    // The path can be redirected or absent before the first screenshot. Do not create it.
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Screenshots, KF_FLAG_DONT_VERIFY, None)?;
        let result = OsString::from_wide(path.as_wide()).into();
        CoTaskMemFree(Some(path.0.cast()));
        Ok(result)
    }
}

pub fn watch(path: PathBuf, stop: Receiver<()>, events: Sender<Event>) {
    let _ = events.send(Event::Status(format!("folder={path:?}")));
    let mut last_failure = None;
    loop {
        let error = match fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OVERLAPPED.0)
            .open(&path)
        {
            Ok(directory) => {
                let scan_first = last_failure == Some(io::ErrorKind::NotFound);
                if let Err(error) = watch_directory(
                    &path,
                    HANDLE(directory.as_raw_handle()),
                    scan_first,
                    &stop,
                    &events,
                ) {
                    let _ = events.send(Event::Status(format!("directory watch stopped: {error}")));
                }
                return;
            }
            Err(error) => error,
        };
        if last_failure != Some(error.kind()) {
            let _ = events.send(Event::Status(format!("waiting for folder: {error}")));
            last_failure = Some(error.kind());
        }
        if stopped(&stop, POLL) {
            return;
        }
    }
}

fn watch_directory(
    path: &Path,
    handle: HANDLE,
    scan_first: bool,
    stop: &Receiver<()>,
    events: &Sender<Event>,
) -> windows::core::Result<()> {
    // DWORD alignment and stable storage are required until each overlapped request completes.
    let mut buffer = [0u32; 4096];
    let mut pending = HashMap::new();
    let mut first_request = true;
    loop {
        let mut request = OVERLAPPED::default();
        unsafe {
            ReadDirectoryChangesW(
                handle,
                buffer.as_mut_ptr().cast(),
                size_of_val(&buffer) as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME,
                None,
                Some(&raw mut request),
                None,
            )?;
        }
        if first_request {
            let _ = events.send(Event::Status("ReadDirectoryChangesW active".into()));
            // Catch the first image if Windows created the folder and the file together.
            if scan_first && let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    queue_image(entry.path(), &mut pending);
                }
            }
            first_request = false;
        }
        let mut length = 0;
        loop {
            if stopped(stop, POLL) {
                unsafe {
                    let _ = CancelIoEx(handle, Some(&raw const request));
                    let _ = GetOverlappedResult(handle, &raw const request, &mut length, true);
                }
                return Ok(());
            }
            match unsafe { GetOverlappedResult(handle, &raw const request, &mut length, false) } {
                Ok(()) => break,
                Err(error) if error.code() == HRESULT::from_win32(ERROR_IO_INCOMPLETE.0) => {}
                Err(error) => return Err(error),
            }
            report_ready_images(&mut pending, events);
        }
        if length == 0 {
            let _ = events.send(Event::Status(
                "directory notification buffer overflow; files may have been missed".into(),
            ));
        }
        let bytes = unsafe { slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length as usize) };
        for name in added_names(bytes) {
            queue_image(path.join(name), &mut pending);
        }
        report_ready_images(&mut pending, events);
    }
}

fn stopped(stop: &Receiver<()>, timeout: Duration) -> bool {
    !matches!(stop.recv_timeout(timeout), Err(RecvTimeoutError::Timeout))
}

fn queue_image(path: PathBuf, pending: &mut HashMap<PathBuf, Instant>) {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "tif" | "tiff"
            )
        })
    {
        pending.entry(path).or_insert_with(Instant::now);
    }
}

fn report_ready_images(pending: &mut HashMap<PathBuf, Instant>, events: &Sender<Event>) {
    pending.retain(|path, at| {
        let dimensions = image::ImageReader::open(path)
            .ok()
            .and_then(|reader| reader.into_dimensions().ok());
        if dimensions.is_none() && at.elapsed() < FILE_TIMEOUT {
            return true;
        }
        if dimensions.is_some() || path.is_file() {
            let _ = events.send(Event::Image {
                path: path.clone(),
                at: *at,
                dimensions,
            });
        }
        false
    });
}

fn added_names(mut bytes: &[u8]) -> Vec<OsString> {
    let mut names = Vec::new();
    // FILE_NOTIFY_INFORMATION has three DWORDs followed by a length-delimited UTF-16 name.
    while bytes.len() >= 12 {
        let next = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        if (action == FILE_ACTION_ADDED.0 || action == FILE_ACTION_RENAMED_NEW_NAME.0)
            && let Some(name) = bytes[12..].get(..length)
        {
            let wide: Vec<_> = name
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            names.push(OsString::from_wide(&wide));
        }
        if next < 12 || next > bytes.len() {
            break;
        }
        bytes = &bytes[next..];
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_added_and_renamed_files_but_ignores_modified_files() {
        let mut bytes = Vec::new();
        for (action, name, next) in [(1u32, "a.png", 24u32), (3, "b.png", 24), (5, "c.png", 0)] {
            bytes.extend(next.to_le_bytes());
            bytes.extend(action.to_le_bytes());
            bytes.extend(10u32.to_le_bytes());
            bytes.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
            bytes.extend([0, 0]);
        }
        assert_eq!(added_names(&bytes), ["a.png", "c.png"]);
    }

    #[test]
    fn watches_a_new_image_in_a_scratch_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!("snip-folder-test-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("fixture.png");
        let (stop, stop_receiver) = std::sync::mpsc::channel();
        let (events, event_receiver) = std::sync::mpsc::channel();
        let watch_path = directory.clone();
        let worker = std::thread::spawn(move || watch(watch_path, stop_receiver, events));
        let timeout = Duration::from_secs(3);
        assert!(matches!(
            event_receiver.recv_timeout(timeout),
            Ok(Event::Status(_))
        ));
        assert!(
            matches!(event_receiver.recv_timeout(timeout), Ok(Event::Status(status))
            if status == "ReadDirectoryChangesW active")
        );
        image::save_buffer(&path, &[0; 6], 2, 1, image::ColorType::Rgb8).unwrap();
        let event = event_receiver.recv_timeout(timeout);
        drop(stop);
        worker.join().unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
        assert!(
            matches!(event, Ok(Event::Image { path: observed, dimensions: Some((2, 1)), .. })
            if observed == path)
        );
    }
}
