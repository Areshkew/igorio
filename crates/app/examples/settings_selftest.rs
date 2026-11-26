//! Exercises only our settings window and scratch files. No foreground activation or synthetic input.
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use igorio_app::layout::CardSize;
use igorio_app::overlay::ensure_dpi_awareness;
use igorio_app::settings::Settings;
use igorio_app::settings_window::{
    Control::*, SettingsWindow, TestMode, TestScene, test_client_size,
};
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HWND, LPARAM, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentProcess,
    GetProcessTimes, SYNCHRONIZATION_SYNCHRONIZE, SetWaitableTimer, TIMER_MODIFY_STATE,
    WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::*;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Checks {
    ok: bool,
}

impl Checks {
    fn check(&mut self, name: &str, pass: bool) {
        self.ok &= pass;
        println!("{} {name}", if pass { "PASS" } else { "FAIL" });
    }
}

fn child(hwnd: HWND, id: igorio_app::settings_window::Control) -> HWND {
    // SAFETY: a same-process window created by the self-test.
    unsafe { GetDlgItem(Some(hwnd), id as i32) }.unwrap_or_default()
}

fn text(hwnd: HWND) -> String {
    let mut units = [0u16; 1024];
    // SAFETY: the same-process window copies its title into the valid output slice.
    let len = unsafe { GetWindowTextW(hwnd, &mut units) };
    String::from_utf16_lossy(&units[..len as usize])
}

fn checked(hwnd: HWND) -> bool {
    // SAFETY: scalar query to a test BUTTON.
    unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None).0 == 1 }
}

fn enabled(hwnd: HWND) -> bool {
    // SAFETY: query a live test HWND.
    unsafe { IsWindowEnabled(hwnd).as_bool() }
}

fn click(hwnd: HWND, id: igorio_app::settings_window::Control) {
    // SAFETY: send the same control notification as a click, only to the window we created.
    unsafe {
        SendMessageW(
            hwnd,
            WM_COMMAND,
            Some(WPARAM(id as usize)),
            Some(LPARAM(child(hwnd, id).0 as isize)),
        );
    }
}

fn open(controller: &mut SettingsWindow) -> Result<HWND> {
    controller.open()?;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if let Some(hwnd) = controller.test_hwnd() {
            // SAFETY: this HWND was published by our test window only after setup.
            if unsafe { IsWindowVisible(hwnd).as_bool() } {
                return Ok(hwnd);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err("settings window did not become visible".into())
}

fn close(controller: &mut SettingsWindow, hwnd: HWND) -> bool {
    let at = Instant::now();
    let closed = controller.close();
    // SAFETY: same-process HWND. This checks that the old window has been destroyed.
    closed && at.elapsed() < Duration::from_secs(1) && !unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

fn disk(controller: &SettingsWindow, path: &Path, expected: &Settings) -> bool {
    Settings::load(path).is_ok_and(|settings| &settings == expected)
        && controller.saved().collect::<Vec<_>>() == [expected.clone()]
}

fn tab_order(hwnd: HWND) -> bool {
    // SAFETY: both iterators run only on the test window's child group.
    unsafe {
        let mut current = None;
        for id in [
            Screenshots,
            Keep,
            ChooseNotes,
            OpenNotes,
            ResetNotes,
            ChooseCaptures,
            OpenCaptures,
            ResetCaptures,
            Medium,
            Launch,
        ] {
            let child = child(hwnd, id);
            if GetNextDlgTabItem(hwnd, current, false).ok() != Some(child) {
                return false;
            }
            current = Some(child);
        }
        GetNextDlgTabItem(hwnd, current, false).ok() == Some(child(hwnd, Screenshots))
            && GetNextDlgGroupItem(hwnd, Some(child(hwnd, Small)), false).ok()
                == Some(child(hwnd, Medium))
            && GetNextDlgGroupItem(hwnd, Some(child(hwnd, Medium)), false).ok()
                == Some(child(hwnd, Large))
            && GetNextDlgGroupItem(hwnd, Some(child(hwnd, Large)), false).ok()
                == Some(child(hwnd, Small))
    }
}

fn lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).share_mode(0).open(path)
}

fn verify_controls(
    checks: &mut Checks,
    controller: &mut SettingsWindow,
    hwnd: HWND,
    path: &Path,
    mode: &TestMode,
) -> Result<()> {
    let mut expected = Settings::load(path)?;
    for (id, name, state, available) in [
        (Screenshots, "Screenshots", expected.is_enabled("fairy.screenshots"), true),
        (Keep, "Keep every screenshot", expected.keep_every_screenshot, true),
        (Notes, "Notes, not available yet", false, false),
        (Recording, "Screen recording, not available yet", false, false),
        (Small, "Small", false, true),
        (Medium, "Medium", true, true),
        (Large, "Large", false, true),
        (Launch, "Launch at login", false, true),
    ] {
        let control = child(hwnd, id);
        checks.check(
            name,
            text(control) == name && checked(control) == state && enabled(control) == available,
        );
    }
    for (id, label) in [
        (ChooseNotes, "&Choose notes folder…"),
        (OpenNotes, "Open notes folder"),
        (ResetNotes, "Reset notes folder"),
        (ChooseCaptures, "Ch&oose captures folder…"),
        (OpenCaptures, "Open captures folder"),
        (ResetCaptures, "Reset captures folder"),
    ] {
        let control = child(hwnd, id);
        checks.check(label, text(control) == label && enabled(control));
    }
    checks.check(
        "window title and static names are available through Win32",
        text(hwnd) == "Igorio Settings"
            && [
                (Capabilities, "Capabilities"),
                (ScreenshotsTitle, "Screenshots"),
                (ScreenshotKeys, "Windows plus Shift plus S, or Print Screen"),
                (ScreenshotDetail, "Use Win+Shift+S or PrtScn and your cursor carries the shot."),
                (KeepTitle, "Keep every screenshot"),
                (NotesTitle, "Notes"),
                (NotesKeys, "Alt plus click"),
                (NotesDetail, "Not available yet"),
                (RecordingTitle, "Screen recording"),
                (RecordingKeys, "Control plus Shift plus 6, 7, or 8"),
                (RecordingDetail, "Not available yet"),
                (General, "General"),
                (NotesFolder, "Notes folder"),
                (CapturesFolder, "Captures folder"),
                (CardTitle, "Card size"),
                (Preview, "Card size preview"),
                (LaunchTitle, "Launch at login"),
                (Retry, "&Retry saving settings"),
            ]
            .into_iter()
            .all(|(id, label)| text(child(hwnd, id)) == label),
    );
    checks.check(
        "initial folder paths and Medium dimensions",
        text(child(hwnd, NotesPath)).ends_with("Fairy Notes")
            && text(child(hwnd, CapturesPath)).ends_with("Screenshots")
            && text(child(hwnd, CardDimensions)) == "96 × 72 px",
    );
    checks.check(
        "Tab wraps once through all tab stops and the radio group contains three choices",
        tab_order(hwnd),
    );
    controller.open()?;
    thread::sleep(Duration::from_millis(40));
    checks.check("second Open retains the existing HWND", controller.test_hwnd() == Some(hwnd));

    for on in [true, false, true] {
        click(hwnd, Screenshots);
        expected.set_enabled("fairy.screenshots", on);
        checks.check(
            &format!("Screenshots {on} saves and publishes"),
            disk(controller, path, &expected) && checked(child(hwnd, Screenshots)) == on,
        );
    }
    click(hwnd, Keep);
    expected.keep_every_screenshot = false;
    checks.check(
        "Keep every screenshot saves and publishes",
        disk(controller, path, &expected) && !checked(child(hwnd, Keep)),
    );
    for id in [Notes, Recording] {
        click(hwnd, id);
    }
    checks.check(
        "disabled capability notifications leave the file alone",
        Settings::load(path)? == expected && controller.saved().next().is_none(),
    );

    for (id, size, dimensions) in [
        (Large, CardSize::Large, "112 × 84 px"),
        (Small, CardSize::Small, "80 × 60 px"),
        (Medium, CardSize::Medium, "96 × 72 px"),
    ] {
        click(hwnd, id);
        expected.card_size = size;
        checks.check(
            &format!("{size:?} saves, checks one radio and updates dimensions"),
            disk(controller, path, &expected)
                && [Small, Medium, Large]
                    .into_iter()
                    .all(|radio| checked(child(hwnd, radio)) == (radio == id))
                && text(child(hwnd, CardDimensions)) == dimensions,
        );
    }

    for (notes, choose, open, reset, path_id, fallback, default) in [
        (true, ChooseNotes, OpenNotes, ResetNotes, NotesPath, NotesFallback, &mode.notes),
        (
            false,
            ChooseCaptures,
            OpenCaptures,
            ResetCaptures,
            CapturesPath,
            CapturesFallback,
            &mode.captures,
        ),
    ] {
        click(hwnd, choose);
        click(hwnd, open);
        checks.check(
            "Choose and Open test notifications leave disk unchanged",
            Settings::load(path)? == expected,
        );
        let chosen =
            default.with_file_name("A deliberately long user-selected folder").join("Chosen");
        controller.test_choose_folder(notes, &chosen);
        if notes {
            expected.notes_folder = Some(chosen);
        } else {
            expected.captures_folder = Some(chosen);
        }
        let label = text(child(hwnd, path_id));
        checks.check(
            if notes {
                "Choose notes folder saves, resolves and shortens the path"
            } else {
                "Choose captures folder saves, resolves and shortens the path"
            },
            disk(controller, path, &expected) && label.contains('…') && label.ends_with("Chosen"),
        );

        let unavailable = default.with_extension("file");
        fs::write(&unavailable, b"a file, not a folder")?;
        controller.test_choose_folder(notes, &unavailable);
        if notes {
            expected.notes_folder = Some(unavailable);
        } else {
            expected.captures_folder = Some(unavailable);
        }
        checks.check(
            "unusable folder shows default path and fallback note",
            disk(controller, path, &expected)
                && text(child(hwnd, fallback)).contains("Using the default")
                && text(child(hwnd, path_id))
                    .ends_with(default.file_name().unwrap().to_str().unwrap()),
        );

        click(hwnd, reset);
        if notes {
            expected.notes_folder = None;
        } else {
            expected.captures_folder = None;
        }
        checks.check(
            if notes {
                "Reset notes folder saves and hides fallback"
            } else {
                "Reset captures folder saves and hides fallback"
            },
            disk(controller, path, &expected)
                && text(child(hwnd, fallback)).is_empty()
                && !enabled(child(hwnd, reset)),
        );
    }

    for on in [true, false] {
        click(hwnd, Launch);
        expected.launch_at_login = on;
        checks.check(
            &format!("Launch at login {on} writes throwaway Run value and JSON mirror"),
            disk(controller, path, &expected) && checked(child(hwnd, Launch)) == on,
        );
    }

    let locked = lock(path)?;
    click(hwnd, Keep);
    checks.check(
        "failed save retains the choice, shows Retry, and publishes nothing",
        checked(child(hwnd, Keep))
            && text(child(hwnd, Status)).contains("could not be saved")
            && enabled(child(hwnd, Retry))
            && controller.saved().next().is_none(),
    );
    drop(locked);
    expected.keep_every_screenshot = true;
    click(hwnd, Retry);
    checks.check(
        "Retry saves retained choice and clears the error",
        disk(controller, path, &expected) && text(child(hwnd, Status)).is_empty(),
    );
    Ok(())
}

fn memory() -> Result<(usize, usize)> {
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: EX has the documented base prefix; the size declares the extended buffer.
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            memory.cb,
        )?
    };
    Ok((memory.WorkingSetSize, memory.PrivateUsage))
}

fn report_memory(label: &str) -> Result<()> {
    let (working, private) = memory()?;
    println!("INFO memory {label}, working set {working} bytes, private {private} bytes");
    Ok(())
}

fn process_cpu() -> Result<u64> {
    let (mut created, mut exited, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: four writable FILETIMEs and the pseudo-handle of this process.
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user)?
    };
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Ok(ticks(kernel) + ticks(user))
}

fn check_idle(checks: &mut Checks, window: &SettingsWindow) -> Result<()> {
    thread::sleep(Duration::from_millis(250));
    let timer_stopped = window.test_idle();
    let before = process_cpu()?;
    let at = Instant::now();
    thread::sleep(Duration::from_secs(2));
    let cpu = process_cpu()? - before;
    let seconds = at.elapsed().as_secs_f64();
    checks.check(
        "animation timer is stopped before and after two idle seconds",
        timer_stopped && window.test_idle(),
    );
    checks.check(
        &format!("idle process CPU {:.3} ms over {seconds:.3} s", cpu as f64 / 10_000.0),
        cpu == 0,
    );
    Ok(())
}

fn capture(hwnd: HWND, path: &Path, dpi: u32) -> Result<bool> {
    // SAFETY: wait for the compositor to consume the frozen client paint before PrintWindow.
    let _ = unsafe { DwmFlush() };
    let mut client = RECT::default();
    // SAFETY: this is our visible test HWND and client is writable.
    unsafe { GetClientRect(hwnd, &mut client)? };
    let (w, h) = (client.right, client.bottom);
    let mut bits = std::ptr::null_mut();
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: BITMAPINFO defines a 32-bit top-down DIB; bits is a writable out pointer.
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    // SAFETY: allocate a private memory DC, and restore its original bitmap before deletion.
    let dc = unsafe { CreateCompatibleDC(None) };
    let rendered = if dc.is_invalid() {
        false
    } else {
        // SAFETY: our bitmap and DC are live, and PrintWindow targets only the test HWND.
        unsafe {
            let previous = SelectObject(dc, bitmap.into());
            const PW_RENDERFULLCONTENT: u32 = 2;
            let printed =
                PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT))
                    .as_bool();
            let _ = GdiFlush();
            SelectObject(dc, previous);
            let _ = DeleteDC(dc);
            printed
        }
    };
    // SAFETY: the DIB is top-down, 32-bit, tightly packed, and alive through the copy.
    let rgb: Vec<u8> = unsafe { slice::from_raw_parts(bits.cast::<u8>(), (w * h * 4) as usize) }
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
        .collect();
    // SAFETY: the bitmap is no longer selected into any DC.
    let _ = unsafe { DeleteObject(bitmap.into()) };
    let mut encoder = png::Encoder::new(File::create(path)?, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgb)?;
    let pixels = rgb.as_chunks::<3>().0;
    let varied = pixels.iter().filter(|pixel| **pixel != pixels[0]).count() > 1000;
    Ok(rendered && varied && (w, h) == test_client_size(dpi))
}

fn run(cycles_done: &AtomicBool) -> Result<bool> {
    let mut checks = Checks { ok: true };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(format!("settings-selftest-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let root = fs::canonicalize(root)?;
    let path = root.join("settings.json");
    let defaults = TestMode {
        notes: root.join("Documents").join("Fairy Notes"),
        captures: root.join("Pictures").join("Screenshots"),
        dark: true,
        dpi: 96,
        run_suffix: 0,
    };
    fs::create_dir_all(defaults.notes.parent().unwrap())?;
    fs::create_dir_all(defaults.captures.parent().unwrap())?;
    struct RemoveRun(TestMode);
    impl Drop for RemoveRun {
        fn drop(&mut self) {
            let _ = self.0.remove_run_value();
        }
    }
    let _remove_run = RemoveRun(defaults.clone());
    let mut baseline = Settings {
        card_size: CardSize::Medium,
        keep_every_screenshot: true,
        launch_at_login: true,
        notes_folder: Some(defaults.notes.clone()),
        captures_folder: Some(defaults.captures.clone()),
        ..Settings::default()
    };
    baseline.set_enabled("fairy.screenshots", false);
    baseline.save(&path)?;
    let mut controller = SettingsWindow::for_test(path.clone(), defaults.clone())?;
    report_memory("before first open")?;
    for cycle in 1..=3 {
        let hwnd = open(&mut controller)?;
        if cycle == 1 {
            // Include the composed surface after the opening transition settles.
            thread::sleep(Duration::from_millis(200));
            report_memory("while open")?;
            verify_controls(&mut checks, &mut controller, hwnd, &path, &defaults)?;
        } else {
            checks.check(
                &format!("cycle {cycle} reloads the saved check states"),
                checked(child(hwnd, Screenshots))
                    && checked(child(hwnd, Medium))
                    && checked(child(hwnd, Keep))
                    && !checked(child(hwnd, Launch)),
            );
        }
        checks.check(
            &format!("cycle {cycle} closes HWND and joins the STA within one second"),
            close(&mut controller, hwnd),
        );
        thread::sleep(Duration::from_millis(100));
    }
    cycles_done.store(true, Relaxed);
    thread::sleep(Duration::from_millis(250));
    report_memory("after third close")?;

    let locked = lock(&path)?;
    let hwnd = open(&mut controller)?;
    checks.check(
        "locked file disables all buttons and shows a load error",
        [
            Screenshots,
            Keep,
            Notes,
            Recording,
            ChooseNotes,
            OpenNotes,
            ResetNotes,
            ChooseCaptures,
            OpenCaptures,
            ResetCaptures,
            Small,
            Medium,
            Large,
            Launch,
            Retry,
        ]
        .into_iter()
        .all(|id| !enabled(child(hwnd, id)))
            && text(child(hwnd, Status)).contains("could not be loaded"),
    );
    click(hwnd, Screenshots);
    drop(locked);
    checks.check(
        "locked-file window cannot overwrite after the file unlocks",
        Settings::load(&path)?.is_enabled("fairy.screenshots")
            && controller.saved().next().is_none(),
    );
    checks.check("locked-file window closes within one second", close(&mut controller, hwnd));

    Settings::default().save(&path)?;
    for (name, dark, dpi) in [
        ("settings-light-100.png", false, 96),
        ("settings-dark-100.png", true, 96),
        ("settings-dark-125.png", true, 120),
    ] {
        let mode = TestMode { dark, dpi, ..defaults.clone() };
        let mut window = SettingsWindow::for_test(path.clone(), mode)?;
        let hwnd = open(&mut window)?;
        if dark && dpi == 120 {
            check_idle(&mut checks, &window)?;
        }
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        window.test_scene(TestScene::Settled);
        let output = folder.join(name);
        checks.check(name, capture(hwnd, &output, dpi)?);
        if dark && dpi == 120 {
            let mut frames = std::collections::BTreeSet::from([fs::read(&output)?]);
            for (scene, name) in [
                (TestScene::SwitchSlide, "settings-motion-switch.png"),
                (TestScene::CardSlide, "settings-motion-card.png"),
                (TestScene::ButtonHover, "settings-motion-button-hover.png"),
                (TestScene::ButtonPress, "settings-motion-button-pressed.png"),
                (TestScene::SwitchFocus, "settings-focus-switch.png"),
                (TestScene::CardFocus, "settings-focus-card.png"),
            ] {
                window.test_scene(scene);
                let output = folder.join(name);
                checks
                    .check(name, capture(hwnd, &output, dpi)? && frames.insert(fs::read(&output)?));
            }
        }
        checks.check("capture window closes within one second", close(&mut window, hwnd));
    }
    Ok(checks.ok)
}

struct Timer(HANDLE);

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: this Timer owns the handle and closes it once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn main() -> Result<()> {
    ensure_dpi_awareness();
    // SAFETY: unnamed high-resolution timer, owned by Timer.
    let timer = Timer(unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            (TIMER_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE).0,
        )?
    });
    // Run Win32 probes on a test driver, leaving the stand-in UI thread free to tick.
    let cycles_done = std::sync::Arc::new(AtomicBool::new(false));
    let progress = cycles_done.clone();
    let driver = thread::spawn(move || run(&progress).map_err(|error| error.to_string()));
    let interval = Duration::from_nanos(1_000_000_000 / 120);
    let mut last = Instant::now();
    let mut next = last + interval;
    let mut worst = Duration::ZERO;
    let mut frames = 0;
    while !cycles_done.load(Relaxed) && !driver.is_finished() {
        let due =
            -((next.saturating_duration_since(Instant::now()).as_nanos() as i64 / 100).max(1));
        // SAFETY: the timer remains live and the relative due time outlives SetWaitableTimer.
        unsafe {
            SetWaitableTimer(timer.0, &due, 0, None, None, false)?;
            if WaitForSingleObject(timer.0, 100) != WAIT_OBJECT_0 {
                return Err("120 Hz timer wait failed".into());
            }
        }
        let now = Instant::now();
        worst = worst.max(now - last);
        last = now;
        next += interval;
        if next < now {
            next = now + interval;
        }
        frames += 1;
    }
    let result = driver.join().map_err(|_| "settings self-test driver panicked")?;
    let mut checks = Checks { ok: true };
    checks.check(
        &format!(
            "120 Hz stand-in worst frame gap {:.2} ms over {frames} frames",
            worst.as_secs_f64() * 1000.0
        ),
        worst < Duration::from_millis(20),
    );
    match result {
        Ok(true) if checks.ok => Ok(()),
        Ok(_) => Err("settings self-test failed".into()),
        Err(error) => {
            println!("FAIL {error}");
            Err(error.into())
        }
    }
}
