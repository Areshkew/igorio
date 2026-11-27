//! A crash log for the whole app: when a thread panics, or the process dies of an unhandled
//! Windows exception, one block goes to `%LOCALAPPDATA%\Igorio\crash.log` (always, not only
//! with `IGORIO_DEBUG`), and the process then ends as it would have.
//!
//! The handlers run in a process that may be broken, so they do little:
//!
//! - The panic hook formats one block (message, location, thread, a backtrace), cuts it to
//!   [`MAX_BLOCK`] bytes and appends it with one `WriteFile`, then calls the hook that was there
//!   before (Rust's default message). It never runs the app's quit path.
//! - A vectored exception handler only remembers the last fatal-looking exception (code, address)
//!   in two atomics: on 64-bit Windows a fault inside a window procedure reaches the unhandled
//!   filter as `STATUS_FATAL_USER_CALLBACK_EXCEPTION`, without the real address.
//! - The unhandled-exception filter writes the exception (and the remembered fault) into a fixed
//!   buffer on the stack, with the module each address falls in, appends it, and passes the
//!   exception on (`EXCEPTION_CONTINUE_SEARCH`), so Windows ends the process as before.
//!
//! Neither handler takes a lock the crashed thread may hold (the file is opened and written with
//! `CreateFileW`/`WriteFile`, the path is fixed at install time).

use std::fmt::{self, Write as _};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HLOCAL, HMODULE, LocalFree, NTSTATUS, SYSTEMTIME,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_ALWAYS, WriteFile,
};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, LPTOP_LEVEL_EXCEPTION_FILTER,
    SetUnhandledExceptionFilter,
};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, GetThreadDescription,
};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::core::PCWSTR;

/// The most one block may take in the log.
pub const MAX_BLOCK: usize = 32 * 1024;

static LOG: OnceLock<Vec<u16>> = OnceLock::new();
static PREVIOUS_FILTER: AtomicUsize = AtomicUsize::new(0);
static LAST_FAULT_CODE: AtomicU32 = AtomicU32::new(0);
static LAST_FAULT_ADDRESS: AtomicUsize = AtomicUsize::new(0);

/// Installs the panic hook and the exception handlers, logging to
/// `%LOCALAPPDATA%\Igorio\crash.log`. Call once, first thing in `main`.
pub fn install() {
    let base =
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    install_at(&base.join("Igorio").join("crash.log"));
}

/// Like [`install`], with another log file (tests and examples keep theirs under `target/`).
pub fn install_at(path: &Path) {
    if let Some(folder) = path.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    if LOG.set(wide).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_owned()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "(no message)".to_owned()
        };
        let location = info.location().map_or_else(|| "unknown".to_owned(), |at| at.to_string());
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let mut block = String::new();
        let _ = format_block(
            &mut block,
            &Report {
                time: local_time(),
                version: env!("CARGO_PKG_VERSION"),
                thread: name,
                thread_id: current_thread_id(),
                what: &format!("panic: {message}"),
                location: &location,
                details: &backtrace,
            },
        );
        append(cut(&block).as_bytes());
        previous(info);
    }));
    // SAFETY: registers our handlers; both only touch atomics, stack buffers and the log.
    unsafe {
        AddVectoredExceptionHandler(0, Some(remember_fault));
        let previous: LPTOP_LEVEL_EXCEPTION_FILTER = SetUnhandledExceptionFilter(Some(unhandled));
        PREVIOUS_FILTER.store(previous.map_or(0, |filter| filter as usize), Ordering::SeqCst);
    }
}

/// One crash, ready to be written.
pub struct Report<'a> {
    /// Local time: year, month, day, hour, minute, second.
    pub time: [u16; 6],
    pub version: &'a str,
    pub thread: &'a str,
    pub thread_id: u32,
    /// "panic: …" or "exception 0x… at 0x… (module)".
    pub what: &'a str,
    pub location: &'a str,
    /// A backtrace, or the remembered fault for an exception.
    pub details: &'a str,
}

/// Writes one block of the log.
pub fn format_block(out: &mut impl fmt::Write, report: &Report<'_>) -> fmt::Result {
    let [year, month, day, hour, minute, second] = report.time;
    writeln!(out, "=== crash {year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} ===")?;
    writeln!(out, "app: igorio {}", report.version)?;
    writeln!(out, "thread: {} (id {})", report.thread, report.thread_id)?;
    writeln!(out, "{}", report.what)?;
    writeln!(out, "at: {}", report.location)?;
    if !report.details.is_empty() {
        writeln!(out, "{}", report.details.trim_end())?;
    }
    writeln!(out)
}

/// The block cut to [`MAX_BLOCK`] bytes at a character boundary, ending with a newline.
pub fn cut(block: &str) -> std::borrow::Cow<'_, str> {
    if block.len() <= MAX_BLOCK {
        return block.into();
    }
    let mut end = MAX_BLOCK - 32;
    while !block.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[cut at {MAX_BLOCK} bytes]\n\n", &block[..end]).into()
}

/// Appends bytes to the log with one write. No Rust locks, no allocation.
fn append(bytes: &[u8]) {
    let Some(path) = LOG.get() else { return };
    // SAFETY: a NUL-terminated path set at install time; the handle is closed below.
    unsafe {
        let Ok(file) = CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_APPEND_DATA.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) else {
            return;
        };
        let _ = WriteFile(file, Some(&bytes[..bytes.len().min(MAX_BLOCK)]), None, None);
        let _ = CloseHandle(file);
    }
}

fn local_time() -> [u16; 6] {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let ticks = since.as_nanos() as u64 / 100 + 116_444_736_000_000_000;
    let file = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    // SAFETY: plain conversions between locals.
    let ok = unsafe {
        FileTimeToSystemTime(&file, &mut utc).is_ok()
            && SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_ok()
    };
    let t = if ok { local } else { utc };
    [t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond]
}

fn current_thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { GetCurrentThreadId() }
}

/// A fixed buffer that takes formatted text and drops what does not fit.
struct StackText<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> StackText<N> {
    const fn new() -> Self {
        Self { bytes: [0; N], len: 0 }
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> fmt::Write for StackText<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for ch in text.chars() {
            let mut buffer = [0u8; 4];
            let encoded = ch.encode_utf8(&mut buffer);
            if self.len + encoded.len() > N {
                return Ok(());
            }
            self.bytes[self.len..self.len + encoded.len()].copy_from_slice(encoded.as_bytes());
            self.len += encoded.len();
        }
        Ok(())
    }
}

/// Writes the file name of the module holding `address` (or "?") into `out`.
fn module_of(address: usize, out: &mut impl fmt::Write) {
    let mut module = HMODULE::default();
    let mut name = [0u16; 260];
    // SAFETY: looks up the module of an address without changing its reference count; the
    // name goes into a stack buffer.
    let length = unsafe {
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(address as *const u16),
            &mut module,
        )
        .is_err()
        {
            let _ = out.write_str("?");
            return;
        }
        GetModuleFileNameW(Some(module), &mut name) as usize
    };
    let units = &name[..length.min(name.len())];
    let start = units.iter().rposition(|unit| *unit == u16::from(b'\\')).map_or(0, |at| at + 1);
    for ch in char::decode_utf16(units[start..].iter().copied()) {
        let _ = out.write_char(ch.unwrap_or('?'));
    }
    let _ = write!(out, "+0x{:x}", address.wrapping_sub(module.0 as usize));
}

fn thread_name(out: &mut impl fmt::Write) {
    // SAFETY: the description is a LocalAlloc string the system gives us; it is freed below.
    unsafe {
        if let Ok(text) = GetThreadDescription(GetCurrentThread())
            && !text.is_null()
        {
            let mut length = 0;
            while *text.0.add(length) != 0 && length < 128 {
                length += 1;
            }
            let units = std::slice::from_raw_parts(text.0, length);
            for ch in char::decode_utf16(units.iter().copied()) {
                let _ = out.write_char(ch.unwrap_or('?'));
            }
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        }
    }
}

const FATAL_CODES: [u32; 6] = [
    0xC000_0005, // access violation
    0xC000_001D, // illegal instruction
    0xC000_0094, // integer divide by zero
    0xC000_00FD, // stack overflow
    0xC000_0409, // stack buffer overrun / fail fast
    0xC000_0374, // heap corruption
];

unsafe extern "system" fn remember_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: Windows passes valid exception pointers for the call.
    unsafe {
        if let Some(record) = info.as_ref().and_then(|info| info.ExceptionRecord.as_ref()) {
            let code = record.ExceptionCode.0 as u32;
            if FATAL_CODES.contains(&code) {
                LAST_FAULT_CODE.store(code, Ordering::SeqCst);
                LAST_FAULT_ADDRESS.store(record.ExceptionAddress as usize, Ordering::SeqCst);
            }
        }
    }
    0 // EXCEPTION_CONTINUE_SEARCH: only watching.
}

unsafe extern "system" fn unhandled(info: *const EXCEPTION_POINTERS) -> i32 {
    // SAFETY: Windows passes valid exception pointers for the call.
    let (code, address) = unsafe {
        info.as_ref()
            .and_then(|info| info.ExceptionRecord.as_ref())
            .map_or((NTSTATUS(0), 0), |record| {
                (record.ExceptionCode, record.ExceptionAddress as usize)
            })
    };
    let mut what = StackText::<512>::new();
    let _ = write!(what, "exception 0x{:08x} at 0x{address:x} (", code.0 as u32);
    module_of(address, &mut what);
    let _ = what.write_str(")");
    let mut details = StackText::<512>::new();
    let fault = LAST_FAULT_CODE.load(Ordering::SeqCst);
    if fault != 0 {
        let fault_address = LAST_FAULT_ADDRESS.load(Ordering::SeqCst);
        let _ = write!(details, "last fault seen: 0x{fault:08x} at 0x{fault_address:x} (");
        module_of(fault_address, &mut details);
        let _ = details.write_str(")");
    }
    let mut thread = StackText::<256>::new();
    thread_name(&mut thread);
    let mut block = StackText::<1536>::new();
    let _ = format_block(
        &mut block,
        &Report {
            time: local_time(),
            version: env!("CARGO_PKG_VERSION"),
            thread: if thread.len == 0 { "unnamed" } else { thread.as_str() },
            thread_id: current_thread_id(),
            what: what.as_str(),
            location: "see the address above",
            details: details.as_str(),
        },
    );
    append(block.as_str().as_bytes());
    let previous = PREVIOUS_FILTER.load(Ordering::SeqCst);
    if previous != 0 {
        // SAFETY: the filter that was installed before ours, called as Windows would.
        let previous: unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32 =
            unsafe { std::mem::transmute(previous) };
        // SAFETY: as above.
        return unsafe { previous(info) };
    }
    0 // EXCEPTION_CONTINUE_SEARCH: the process ends as it would have.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_has_time_version_thread_what_where_and_details() {
        let mut out = String::new();
        format_block(
            &mut out,
            &Report {
                time: [2026, 11, 26, 9, 5, 3],
                version: "0.1.0",
                thread: "notes",
                thread_id: 42,
                what: "panic: already borrowed",
                location: "crates\\app\\src\\notes\\window.rs:10:5",
                details: "   0: std::backtrace\n",
            },
        )
        .unwrap();
        assert_eq!(
            out,
            "=== crash 2026-11-26 09:05:03 ===\napp: igorio 0.1.0\nthread: notes (id 42)\npanic: already borrowed\nat: crates\\app\\src\\notes\\window.rs:10:5\n   0: std::backtrace\n\n"
        );
    }

    #[test]
    fn long_blocks_are_cut_and_small_buffers_drop_the_rest() {
        let long = "é".repeat(MAX_BLOCK);
        let cut = cut(&long);
        assert!(cut.len() <= MAX_BLOCK && cut.ends_with("bytes]\n\n"));
        let mut small = StackText::<8>::new();
        write!(small, "abcdéfghij").unwrap();
        assert_eq!(small.as_str(), "abcdéfg", "8 bytes: é takes two");
        let mut out = StackText::<64>::new();
        module_of(
            a_block_has_time_version_thread_what_where_and_details as *const () as usize,
            &mut out,
        );
        assert!(out.as_str().contains(".exe+0x"), "{}", out.as_str());
    }
}
