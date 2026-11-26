//! Self-check for `igorio_app::clipboard`. Prints PASS, FAIL or SKIP per check and exits 0
//! only if nothing failed.
//!
//! It changes the real clipboard for well under a second, so it protects the user's data:
//! it first takes a snapshot, and if a restore of that snapshot would lose anything the user
//! would notice, it skips every check that writes. Otherwise it stages a test image, checks it
//! from a second process, lets that process overwrite it, writes it again, stops the clipboard
//! thread (which must render the delayed formats), and finally restores the snapshot and
//! checks from the second process that every saved format is back byte for byte. Everything
//! the test writes carries the history exclusion formats.
//!
//! `clipboard_selftest --probe read|write|dump [IDS...]` is the second process.

use std::collections::HashMap;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use igorio_app::clipboard::{
    ClipError, Clipboard, Done, Payload, Ticket, Watch, format_name, sequence,
};
use igorio_kit::Image;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::w;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [flag, mode, rest @ ..] if flag == "--probe" => probe(mode, rest).map(|()| true),
        [] => run(),
        _ => Err("usage: clipboard_selftest [--probe read|write|dump IDS...]".into()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("clipboard_selftest: {err}");
            ExitCode::FAILURE
        }
    }
}

/// 7 x 5 with transparent, semi-transparent and opaque pixels; the colour of transparent
/// pixels must survive too.
fn test_image() -> Image {
    let (width, height) = (7u32, 5u32);
    let mut rgba = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let alpha = [0, 1, 64, 128, 200, 254, 255][x as usize];
            rgba.extend([(x * 36) as u8, (y * 60) as u8, 200 - (x * y) as u8, alpha]);
        }
    }
    Image::new(width, height, rgba).expect("test image size")
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

#[derive(Default)]
struct Checks {
    failed: u32,
}

impl Checks {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) -> bool {
        if ok {
            println!("PASS  {name}  ({detail})");
        } else {
            self.failed += 1;
            println!("FAIL  {name}  ({detail})");
        }
        ok
    }

    fn skip(&self, name: &str, why: &str) {
        println!("SKIP  {name}  ({why})");
    }
}

fn reply(
    clip: &Clipboard,
    ticket: std::result::Result<Ticket, ClipError>,
) -> std::result::Result<Done, ClipError> {
    let ticket = ticket?;
    let deadline = Instant::now() + REPLY_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match clip.wait_reply(left) {
            Some(r) if r.ticket == ticket => return r.result,
            Some(_) => continue,
            None => break,
        }
    }
    Err(ClipError::Stopped)
}

fn wait_for(clip: &Clipboard, timeout: Duration, done: impl Fn(&Watch) -> bool) -> Watch {
    let deadline = Instant::now() + timeout;
    loop {
        let watch = clip.watch();
        if done(&watch) || Instant::now() >= deadline {
            return watch;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// Runs this example as the second process; returns its pid and its `key=value` lines.
fn run_probe(args: &[String]) -> Result<(u32, HashMap<String, String>)> {
    let child = Command::new(std::env::current_exe()?)
        .arg("--probe")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "probe {args:?} failed: {}",
            String::from_utf8_lossy(&output.stdout)
        )
        .into());
    }
    let map = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok((pid, map))
}

fn run() -> Result<bool> {
    let mut checks = Checks::default();
    let clip = Clipboard::start(|| {})?;
    let before = clip.watch();
    println!("sequence at start: {}", before.sequence);

    // Checks that do not write.
    let snapshot = match reply(&clip, clip.snapshot()) {
        Ok(Done::Snapshot(snapshot)) => snapshot,
        other => {
            checks.check("snapshot", false, format!("{other:?}"));
            checks.skip("live checks", "no snapshot");
            return Ok(false);
        }
    };
    checks.check(
        "snapshot",
        true,
        format!(
            "{} formats, {} bytes, {} skipped, sequence {}",
            snapshot.formats().count(),
            snapshot.total_bytes(),
            snapshot.skipped().len(),
            snapshot.sequence()
        ),
    );
    for (id, name, bytes) in snapshot.formats() {
        println!("      saved   {name} ({id:#06x}) {} bytes", bytes.len());
    }
    for skipped in snapshot.skipped() {
        println!(
            "      skipped {} ({:#06x}): {:?}",
            skipped.name, skipped.format, skipped.reason
        );
    }
    checks.check(
        "sequence read after close matches the watch",
        snapshot.sequence() == clip.watch().sequence,
        format!(
            "snapshot {} watch {}",
            snapshot.sequence(),
            clip.watch().sequence
        ),
    );
    let started = Instant::now();
    let _ = clip.watch();
    checks.check(
        "watch does not wait",
        started.elapsed() < Duration::from_millis(5),
        format!("{:?}", started.elapsed()),
    );

    if let Some(lost) = snapshot.lost() {
        for name in [
            "stage and read",
            "foreign write",
            "restage",
            "render on exit",
            "restore",
        ] {
            checks.skip(
                name,
                &format!("a restore would lose {lost:?}; the clipboard is left alone"),
            );
        }
        return Ok(checks.failed == 0);
    }

    // Live part: from here on the snapshot must be restored, whatever happens.
    let live_started = Instant::now();
    let mut clip = Some(clip);
    let live = live_checks(&mut checks, &mut clip);
    drop(clip);
    let restorer = Clipboard::start(|| {})?;
    let restore_started = Instant::now();
    let restored = reply(&restorer, restorer.restore(&snapshot, None));
    let live_time = live_started.elapsed();
    match &restored {
        Ok(Done::Restored { sequence }) => {
            checks.check(
                "restore",
                true,
                format!("sequence {sequence}, took {:?}", restore_started.elapsed()),
            );
        }
        other => {
            checks.check("restore", false, format!("{other:?}"));
        }
    }
    if let Err(err) = live {
        checks.check("live checks ran to the end", false, err);
    }

    // Byte-for-byte from the other process.
    let mut args = vec!["dump".to_string()];
    args.extend(snapshot.formats().map(|(id, _, _)| id.to_string()));
    let (_, dump) = run_probe(&args)?;
    let mut same = 0;
    for (id, name, bytes) in snapshot.formats() {
        let expected = format!("{}:{:016x}", bytes.len(), fnv(bytes));
        let got = dump.get(&format!("fmt{id}")).cloned().unwrap_or_default();
        if checks.check(
            &format!("restored {name} byte for byte"),
            got == expected,
            format!("want {expected}, got {got}"),
        ) {
            same += 1;
        }
    }
    if snapshot.formats().count() == 0 {
        let count: usize = dump
            .get("content")
            .and_then(|v| v.parse().ok())
            .unwrap_or(usize::MAX);
        checks.check(
            "restored an empty clipboard",
            count == 0,
            format!("{count} content formats"),
        );
    }
    let markers = dump.get("exclude").map(String::as_str) == Some("1")
        && dump.get("history").map(String::as_str) == Some("0")
        && dump.get("cloud").map(String::as_str) == Some("0");
    checks.check(
        "restored data excluded from history and cloud",
        markers,
        format!("{dump:?}"),
    );
    let after = wait_for(&restorer, Duration::from_millis(60), |w| {
        w.last_change.is_some()
    });
    checks.check(
        "own restore not reported as a foreign change",
        after.last_change.is_none(),
        format!("{:?}", after.last_change),
    );
    println!("sequence at end: {}  ({same} formats compared)", sequence());
    checks.check(
        "live part under one second",
        live_time < Duration::from_secs(1),
        format!("{live_time:?}"),
    );
    Ok(checks.failed == 0)
}

fn live_checks(checks: &mut Checks, slot: &mut Option<Clipboard>) -> Result<()> {
    let clip = slot.as_ref().ok_or("no clipboard")?;
    let start_change = clip.watch().last_change;
    let image = Arc::new(test_image());

    // Stage, then read from another process.
    let staged = reply(clip, clip.stage(Payload::Image(image.clone())))?;
    let Done::Staged {
        sequence: staged_seq,
    } = staged
    else {
        return Err("stage reply".into());
    };
    println!("sequence after stage: {staged_seq}");
    let (reader, read) = run_probe(&["read".into()])?;
    println!(
        "sequence after probe read: {}",
        read.get("seq").cloned().unwrap_or_default()
    );
    let flag = |key: &str| read.get(key).map(String::as_str) == Some("1");
    checks.check(
        "PNG format present",
        flag("has_png"),
        read.get("formats").cloned().unwrap_or_default(),
    );
    checks.check("CF_DIBV5 format present", flag("has_dibv5"), "");
    checks.check(
        "PNG pixels match",
        flag("png_ok"),
        read.get("png").cloned().unwrap_or_default(),
    );
    checks.check(
        "CF_DIBV5 pixels match (straight alpha)",
        flag("dibv5_ok"),
        read.get("dibv5").cloned().unwrap_or_default(),
    );
    checks.check(
        "CF_DIB synthesized from CF_DIBV5",
        flag("dib_ok"),
        read.get("dib").cloned().unwrap_or_default(),
    );
    checks.check(
        "history and cloud exclusion present",
        flag("exclude")
            && read.get("history").map(String::as_str) == Some("0")
            && read.get("cloud").map(String::as_str) == Some("0"),
        format!(
            "exclude={:?} history={:?} cloud={:?}",
            read.get("exclude"),
            read.get("history"),
            read.get("cloud")
        ),
    );
    let watch = wait_for(clip, Duration::from_millis(50), |w| {
        w.staged.is_some_and(|s| s.reads >= 3)
    });
    let staged = watch.staged.ok_or("no staged facts")?;
    checks.check(
        "reads counted",
        staged.reads >= 2 && staged.first_read_at.is_some(),
        format!(
            "reads={} first read {:?} after staging",
            staged.reads,
            staged.first_read_at.map(|t| t - staged.at)
        ),
    );
    checks.check(
        "reader process known",
        staged.last_reader == Some(reader),
        format!("reader {:?}, probe {reader}", staged.last_reader),
    );
    checks.check(
        "own write and reads not reported as foreign",
        watch.last_change == start_change && staged.overwritten_at.is_none(),
        format!(
            "last_change {:?} overwritten {:?}",
            watch.last_change.map(|_| "set"),
            staged.overwritten_at.map(|_| "set")
        ),
    );

    // Another process writes: reported as an overwrite.
    let (_, wrote) = run_probe(&["write".into()])?;
    println!(
        "sequence after foreign write: {}",
        wrote.get("seq").cloned().unwrap_or_default()
    );
    let watch = wait_for(clip, Duration::from_millis(200), |w| {
        w.last_change.is_some() && w.staged.is_some_and(|s| s.overwritten_at.is_some())
    });
    let staged = watch.staged.ok_or("no staged facts")?;
    checks.check(
        "foreign write reported as overwrite",
        staged.overwritten_at.is_some() && watch.last_change.is_some(),
        format!(
            "overwritten {:?} after staging",
            staged.overwritten_at.map(|t| t - staged.at)
        ),
    );

    // Write again (OverwriteStep::Rewrite).
    let restaged = reply(clip, clip.restage())?;
    let Done::Staged {
        sequence: restaged_seq,
    } = restaged
    else {
        return Err("restage reply".into());
    };
    let watch = clip.watch();
    let staged = watch.staged.ok_or("no staged facts")?;
    checks.check(
        "restage clears overwrite, keeps reads",
        staged.overwritten_at.is_none() && staged.rewrites == 1 && staged.reads >= 2,
        format!(
            "sequence {restaged_seq}, rewrites {}, reads {}",
            staged.rewrites, staged.reads
        ),
    );

    // Stop the thread while it owns delayed formats: WM_RENDERALLFORMATS must render them.
    drop(slot.take());
    let (_, read) = run_probe(&["read".into()])?;
    let flag = |key: &str| read.get(key).map(String::as_str) == Some("1");
    checks.check(
        "render on exit: data still on the clipboard",
        flag("png_ok") && flag("dibv5_ok"),
        format!(
            "png {} dibv5 {}",
            read.get("png").cloned().unwrap_or_default(),
            read.get("dibv5").cloned().unwrap_or_default()
        ),
    );
    Ok(())
}

// MARK: Probe (the second process)

fn probe(mode: &str, rest: &[String]) -> Result<()> {
    // SAFETY: a message-only STATIC window: a clipboard owner for `write`, and a window for
    // `GetOpenClipboardWindow` in the parent to see.
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )?
    };
    open(window)?;
    let result = match mode {
        "read" => probe_read(),
        "write" => probe_write(),
        "dump" => probe_dump(rest),
        _ => Err(format!("unknown probe mode {mode}").into()),
    };
    // SAFETY: opened above by this thread.
    let _ = unsafe { CloseClipboard() };
    // SAFETY: no arguments.
    println!("seq={}", unsafe { GetClipboardSequenceNumber() });
    result
}

fn open(window: HWND) -> Result<()> {
    for _ in 0..50 {
        // SAFETY: our own window.
        if unsafe { OpenClipboard(Some(window)) }.is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err("probe could not open the clipboard".into())
}

fn register(name: &str) -> u32 {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated local string.
    unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(wide.as_ptr())) }
}

fn formats() -> Vec<u32> {
    let mut all = Vec::new();
    let mut id = 0;
    loop {
        // SAFETY: the clipboard is open.
        id = unsafe { EnumClipboardFormats(id) };
        if id == 0 {
            return all;
        }
        all.push(id);
    }
}

fn data(format: u32) -> Option<Vec<u8>> {
    // SAFETY: the clipboard is open; the formats read here are HGLOBALs.
    unsafe {
        let handle = GetClipboardData(format).ok()?;
        let global = HGLOBAL(handle.0);
        let size = GlobalSize(global);
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(global);
        Some(bytes)
    }
}

fn dword(format: u32) -> String {
    data(format)
        .filter(|b| b.len() >= 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]).to_string())
        .unwrap_or_else(|| "missing".into())
}

fn probe_read() -> Result<()> {
    let expected = test_image();
    let png = register("PNG");
    let present = formats();
    let names: Vec<String> = present.iter().map(|&id| format_name(id)).collect();
    println!("formats={}", names.join(", "));
    let has = |id: u32| u8::from(present.contains(&id));
    println!("has_png={}", has(png));
    println!("has_dibv5={}", has(CF_DIBV5));
    let exclude = register("ExcludeClipboardContentFromMonitorProcessing");
    println!("exclude={}", has(exclude));
    println!(
        "history={}",
        dword(register("CanIncludeInClipboardHistory"))
    );
    println!("cloud={}", dword(register("CanUploadToCloudClipboard")));

    let png_result = data(png).ok_or("no data".to_string()).and_then(|bytes| {
        let mut reader = png::Decoder::new(bytes.as_slice())
            .read_info()
            .map_err(|e| e.to_string())?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let same = (info.width, info.height) == (expected.width, expected.height)
            && info.color_type == png::ColorType::Rgba
            && buf[..info.buffer_size()] == expected.rgba[..];
        Ok(format!(
            "{}x{} {:?} {}",
            info.width,
            info.height,
            info.color_type,
            if same { "same" } else { "different" }
        ))
    });
    println!(
        "png_ok={}",
        u8::from(png_result.as_deref().is_ok_and(|s| s.ends_with("same")))
    );
    println!("png={}", png_result.unwrap_or_else(|e| e));

    let dib_result =
        data(CF_DIBV5)
            .ok_or("no data".to_string())
            .map(|bytes| match decode_dib(&bytes) {
                Some((w, h, rgba)) => {
                    let same = (w, h) == (expected.width, expected.height) && rgba == expected.rgba;
                    format!("{w}x{h} {}", if same { "same" } else { "different" })
                }
                None => format!("unparsed header ({} bytes)", bytes.len()),
            });
    println!(
        "dibv5_ok={}",
        u8::from(dib_result.as_deref().is_ok_and(|s| s.ends_with("same")))
    );
    println!("dibv5={}", dib_result.unwrap_or_else(|e| e));

    // CF_DIB is synthesized by Windows from CF_DIBV5; check that the colour survived (its
    // alpha byte is not defined for CF_DIB, so only RGB is compared).
    let dib = data(CF_DIB).map(|bytes| match decode_dib(&bytes) {
        Some((w, h, rgba)) => {
            let rgb_same = rgba
                .chunks(4)
                .zip(expected.rgba.chunks(4))
                .all(|(a, b)| a[..3] == b[..3]);
            format!(
                "{w}x{h} rgb {}",
                if rgb_same && (w, h) == (expected.width, expected.height) {
                    "same"
                } else {
                    "different"
                }
            )
        }
        None => format!("unparsed header ({} bytes)", bytes.len()),
    });
    println!(
        "dib_ok={}",
        u8::from(dib.as_deref().is_some_and(|s| s.ends_with("same")))
    );
    println!("dib={}", dib.unwrap_or_else(|| "no data".into()));
    Ok(())
}

/// A 32-bit DIB (header of any size, BI_RGB or BI_BITFIELDS) to straight RGBA, top row first.
fn decode_dib(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let u32_at = |o: usize| Some(u32::from_le_bytes(bytes.get(o..o + 4)?.try_into().ok()?));
    let header = u32_at(0)? as usize;
    let width = u32_at(4)? as i32;
    let height = u32_at(8)? as i32;
    let bits = u16::from_le_bytes(bytes.get(14..16)?.try_into().ok()?);
    let compression = u32_at(16)?;
    if bits != 32 || width <= 0 || height == 0 {
        return None;
    }
    // With a 40-byte header, BI_BITFIELDS masks follow it.
    let offset = header
        + if header == 40 && compression == 3 {
            12
        } else {
            0
        };
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let pixels = bytes.get(offset..offset + w * h * 4)?;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = if height > 0 { h - 1 - y } else { y };
        for px in pixels[row * w * 4..(row + 1) * w * 4].chunks(4) {
            rgba.extend([px[2], px[1], px[0], px[3]]);
        }
    }
    Some((w as u32, h as u32, rgba))
}

fn put(format: u32, bytes: &[u8]) -> Result<()> {
    // SAFETY: a fresh movable block filled while locked; on success the clipboard owns it
    // (this process exits right after, so a failure only leaks until exit).
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let ptr = GlobalLock(global);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(global);
        SetClipboardData(format, Some(HANDLE(global.0)))?;
    }
    Ok(())
}

fn probe_write() -> Result<()> {
    // SAFETY: the clipboard is open by this thread with a window.
    unsafe { EmptyClipboard()? };
    let text: Vec<u8> = "igorio selftest probe"
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    put(CF_UNICODETEXT, &text)?;
    for name in [
        "ExcludeClipboardContentFromMonitorProcessing",
        "CanIncludeInClipboardHistory",
        "CanUploadToCloudClipboard",
    ] {
        put(register(name), &0u32.to_le_bytes())?;
    }
    Ok(())
}

fn probe_dump(ids: &[String]) -> Result<()> {
    for id in ids {
        let id: u32 = id.parse()?;
        match data(id) {
            Some(bytes) => println!("fmt{id}={}:{:016x}", bytes.len(), fnv(&bytes)),
            None => println!("fmt{id}=missing"),
        }
    }
    let markers = [
        register("ExcludeClipboardContentFromMonitorProcessing"),
        register("CanIncludeInClipboardHistory"),
        register("CanUploadToCloudClipboard"),
    ];
    let present = formats();
    println!("exclude={}", u8::from(present.contains(&markers[0])));
    println!("history={}", dword(markers[1]));
    println!("cloud={}", dword(markers[2]));
    println!(
        "content={}",
        present.iter().filter(|id| !markers.contains(id)).count()
    );
    Ok(())
}
