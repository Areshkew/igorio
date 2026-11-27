//! Self-check for `igorio_app::shortcuts::Hotkeys`. Non-interactive: it sends no input. It
//! registers six chords on F13 to F24 (keys most keyboards do not have) with three or four
//! modifiers, the first such set that no other program holds, and a press is simulated by posting `WM_HOTKEY` to the service's own window.
//! Everything it registers is unregistered before it exits. Prints PASS, FAIL or INFO per
//! check; exits 0 only if nothing failed.

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use igorio_app::shortcuts::{Action, Chord, Hotkeys, Pressed, Registration, Shortcuts};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};

struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        self.failed |= !ok;
    }

    fn info(&self, name: &str, detail: impl std::fmt::Display) {
        println!("INFO {name}: {detail}");
    }
}

fn chord(text: &str) -> Chord {
    let chord = Chord::parse(text).expect("test chord parses");
    chord.validate().expect("test chord is valid");
    chord
}

/// Registers `text` on this thread and unregisters it at once. True if it was free.
fn free_from_here(text: &str) -> bool {
    let c = chord(text);
    let id = 0x100 + c.key.vk() as i32;
    let mut mods = MOD_NOREPEAT;
    for (on, flag) in [
        (c.ctrl, MOD_CONTROL),
        (c.alt, MOD_ALT),
        (c.shift, MOD_SHIFT),
        (c.win, MOD_WIN),
    ] {
        if on {
            mods |= flag;
        }
    }
    // SAFETY: registers on this thread (no window) and unregisters the same id at once.
    unsafe {
        if let Err(error) = RegisterHotKey(None, id, mods, c.key.vk() as u32) {
            if std::env::var_os("HOTKEYS_DEBUG").is_some() {
                println!("DEBUG {text}: {error}");
            }
            return false;
        }
        let _ = UnregisterHotKey(None, id);
    }
    true
}

fn status_of(hotkeys: &Hotkeys, action: Action) -> Registration {
    hotkeys
        .status()
        .into_iter()
        .find(|(a, _)| *a == action)
        .map(|(_, r)| r)
        .expect("every action has a status")
}

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    done()
}

fn signalled(hotkeys: &Hotkeys, timeout_ms: u32) -> bool {
    // SAFETY: the wake handle stays open while `hotkeys` lives.
    unsafe { WaitForSingleObject(hotkeys.wake_handle(), timeout_ms) == WAIT_OBJECT_0 }
}

/// Posts a press for `action` and returns what `drain` gave within `wait`.
fn press(hotkeys: &Hotkeys, action: Action, wait: Duration) -> (bool, bool, Vec<Pressed>) {
    let posted = hotkeys.post_hotkey_for_test(action);
    let woke = signalled(hotkeys, wait.as_millis() as u32);
    let mut out = Vec::new();
    hotkeys.drain(&mut out);
    (posted, woke, out)
}

fn main() -> ExitCode {
    let mut report = Report { failed: false };
    // Six chords no person presses. Another program (an AutoHotkey script, a vendor
    // hotkey tool) may hold some; take the first set that is entirely free.
    let sets = [
        ("Ctrl+Alt+Shift+", 13),
        ("Ctrl+Alt+Shift+", 19),
        ("Ctrl+Alt+Shift+Win+", 13),
        ("Ctrl+Alt+Shift+Win+", 19),
    ];
    let mut names = Vec::new();
    for (prefix, first) in sets {
        let candidate: Vec<String> = (first..first + 6)
            .map(|n| format!("{prefix}F{n}"))
            .collect();
        let held: Vec<&str> = candidate
            .iter()
            .map(String::as_str)
            .filter(|n| !free_from_here(n))
            .collect();
        if held.is_empty() {
            names = candidate;
            break;
        }
        report.info("chords held by another program", format!("{held:?}"));
    }
    report.check(
        "six free test chords",
        !names.is_empty(),
        format!("{names:?}"),
    );
    if names.is_empty() {
        return ExitCode::FAILURE;
    }

    let mut set = Shortcuts::none();
    set.set(Action::RestoreLast, Some(chord(&names[0])))
        .unwrap();
    set.set(Action::DiscardTop, Some(chord(&names[1]))).unwrap();
    set.set(Action::DiscardAll, Some(chord(&names[2]))).unwrap();
    set.set(Action::OpenSettings, Some(chord(&names[3])))
        .unwrap();
    // Recording-only: not registered while no recording runs.
    set.set(Action::Pen, Some(chord(&names[4]))).unwrap();

    let started = Instant::now();
    let hotkeys = match Hotkeys::start(&set) {
        Ok(hotkeys) => hotkeys,
        Err(error) => {
            report.check("start", false, error);
            return ExitCode::FAILURE;
        }
    };
    report.info(
        "start",
        format!("{:.1} ms", started.elapsed().as_secs_f64() * 1000.0),
    );
    let status = hotkeys.status();
    let expected = [
        (Action::RestoreLast, Registration::Active),
        (Action::DiscardTop, Registration::Active),
        (Action::DiscardAll, Registration::Active),
        (Action::OpenSettings, Registration::Active),
        (Action::NewNote, Registration::NotSet),
        (Action::ToggleRecording, Registration::NotSet),
        (Action::Pen, Registration::WhileRecording),
        (Action::Highlighter, Registration::NotSet),
    ];
    report.check(
        "status after start",
        status == expected,
        format!("{status:?}"),
    );
    report.check(
        "recording-only action is not registered while idle",
        free_from_here(&names[4]),
        "its chord is still free",
    );

    // A second service in the same process: Windows refuses chords the first one holds.
    match Hotkeys::start(&set) {
        Ok(second) => {
            let taken = Action::ALL[..4]
                .iter()
                .all(|a| status_of(&second, *a) == Registration::Taken);
            report.check(
                "second instance reports Taken",
                taken,
                format!("{:?}", second.status()),
            );
            report.check(
                "second instance shuts down",
                second.shutdown(),
                "within 1 s",
            );
        }
        Err(error) => report.check("second instance starts", false, error),
    }
    report.check(
        "first instance still Active",
        status_of(&hotkeys, Action::RestoreLast) == Registration::Active,
        "a refused registration elsewhere does not touch ours",
    );

    // WM_HOTKEY path.
    let before = igorio_app::hook::now();
    let (posted, woke, out) = press(&hotkeys, Action::DiscardTop, Duration::from_secs(1));
    let after = igorio_app::hook::now();
    report.check("WM_HOTKEY posted", posted, "to the service window");
    report.check("wake event fired", woke, "within 1 s");
    let right = out.len() == 1
        && out[0].action == Action::DiscardTop
        && (before..=after).contains(&out[0].time);
    report.check("drain gives the action", right, format!("{out:?}"));
    let mut again = Vec::new();
    hotkeys.drain(&mut again);
    report.check(
        "drain empties the queue",
        again.is_empty(),
        format!("{again:?}"),
    );

    // apply: move DiscardAll to the sixth chord, clear OpenSettings.
    let mut changed = set.clone();
    changed
        .set(Action::DiscardAll, Some(chord(&names[5])))
        .unwrap();
    changed.set(Action::OpenSettings, None).unwrap();
    let t = Instant::now();
    hotkeys.apply(&changed);
    report.info(
        "apply returned",
        format!("{:.2} ms", t.elapsed().as_secs_f64() * 1000.0),
    );
    let updated = wait_until(|| {
        status_of(&hotkeys, Action::DiscardAll) == Registration::Active
            && status_of(&hotkeys, Action::OpenSettings) == Registration::NotSet
    });
    report.check(
        "apply updates status",
        updated,
        format!("{:?}", hotkeys.status()),
    );
    report.check(
        "old chord released",
        free_from_here(&names[2]),
        "old DiscardAll chord free",
    );
    report.check(
        "cleared chord released",
        free_from_here(&names[3]),
        "old OpenSettings chord free",
    );
    report.check(
        "new chord held",
        !free_from_here(&names[5]),
        "new DiscardAll chord held by the service",
    );
    report.check(
        "unchanged chord kept",
        status_of(&hotkeys, Action::RestoreLast) == Registration::Active
            && !free_from_here(&names[0]),
        "RestoreLast chord held",
    );
    let (_, woke, out) = press(&hotkeys, Action::OpenSettings, Duration::from_millis(200));
    report.check(
        "a cleared action does not fire",
        !woke && out.is_empty(),
        format!("{out:?}"),
    );

    // Pause: everything unregistered, presses dropped; resume registers again.
    hotkeys.set_paused(true);
    let released = wait_until(|| free_from_here(&names[0]) && free_from_here(&names[5]));
    report.check("pause unregisters", released, "chords free while paused");
    let (_, woke, out) = press(&hotkeys, Action::RestoreLast, Duration::from_millis(200));
    report.check(
        "paused: no press",
        !woke && out.is_empty(),
        format!("{out:?}"),
    );
    hotkeys.set_paused(false);
    // The thread handles its messages in order, so once this press arrives the resume is
    // done. Probing the chords before that would race the re-registration.
    let (_, woke, out) = press(&hotkeys, Action::RestoreLast, Duration::from_secs(1));
    report.check(
        "resumed: press arrives",
        woke && out.len() == 1 && out[0].action == Action::RestoreLast,
        format!("{out:?}"),
    );
    report.check(
        "status after resume",
        status_of(&hotkeys, Action::RestoreLast) == Registration::Active
            && status_of(&hotkeys, Action::DiscardAll) == Registration::Active,
        format!("{:?}", hotkeys.status()),
    );
    report.check(
        "resume registers again",
        !free_from_here(&names[0]) && !free_from_here(&names[5]),
        "chords held again",
    );

    // Recording: Pen and Highlighter are registered only while a recording runs.
    let mut drawing = changed.clone();
    drawing.set(Action::Highlighter, Some(chord(&names[3]))).unwrap();
    hotkeys.apply(&drawing);
    let idle = wait_until(|| {
        status_of(&hotkeys, Action::Highlighter) == Registration::WhileRecording
            && status_of(&hotkeys, Action::Pen) == Registration::WhileRecording
    });
    report.check("idle: pen keys wait", idle, format!("{:?}", hotkeys.status()));
    report.check(
        "idle: pen chords free",
        free_from_here(&names[3]) && free_from_here(&names[4]),
        "not registered",
    );
    let (_, woke, out) = press(&hotkeys, Action::Pen, Duration::from_millis(200));
    report.check("idle: no pen press", !woke && out.is_empty(), format!("{out:?}"));
    hotkeys.set_recording(true);
    let active = wait_until(|| {
        status_of(&hotkeys, Action::Pen) == Registration::Active
            && status_of(&hotkeys, Action::Highlighter) == Registration::Active
    });
    report.check("recording: pen keys active", active, format!("{:?}", hotkeys.status()));
    report.check(
        "recording: pen chords held",
        !free_from_here(&names[3]) && !free_from_here(&names[4]),
        "registered by the service",
    );
    let (_, woke, out) = press(&hotkeys, Action::Highlighter, Duration::from_secs(1));
    report.check(
        "recording: highlighter press arrives",
        woke && out.len() == 1 && out[0].action == Action::Highlighter,
        format!("{out:?}"),
    );
    // Pause during a recording (the settings window records a shortcut): all free, then back.
    hotkeys.set_paused(true);
    let released = wait_until(|| free_from_here(&names[3]) && free_from_here(&names[4]));
    report.check("recording, paused: pen chords free", released, "unregistered");
    hotkeys.set_paused(false);
    let back = wait_until(|| !free_from_here(&names[3]) && !free_from_here(&names[4]));
    report.check("recording, resumed: pen chords held", back, "registered again");
    hotkeys.set_recording(false);
    let stopped = wait_until(|| {
        status_of(&hotkeys, Action::Pen) == Registration::WhileRecording
            && free_from_here(&names[3])
            && free_from_here(&names[4])
    });
    report.check("stopped: pen keys released", stopped, format!("{:?}", hotkeys.status()));
    report.check(
        "stopped: others kept",
        status_of(&hotkeys, Action::RestoreLast) == Registration::Active,
        "RestoreLast still Active",
    );

    // Shutdown.
    let t = Instant::now();
    let ended = hotkeys.shutdown();
    let took = t.elapsed();
    report.check(
        "shutdown joins within 1 s",
        ended && took < Duration::from_secs(1),
        format!("{:.1} ms", took.as_secs_f64() * 1000.0),
    );
    let all_free: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| !free_from_here(n))
        .collect();
    report.check(
        "chords free after shutdown",
        all_free.is_empty(),
        if all_free.is_empty() {
            "all six".to_owned()
        } else {
            format!("still held: {all_free:?}")
        },
    );

    if report.failed {
        println!("FAILED");
        ExitCode::FAILURE
    } else {
        println!("ALL PASSED");
        ExitCode::SUCCESS
    }
}
