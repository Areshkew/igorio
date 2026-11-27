use igorio_app::input::Held;
use igorio_app::shortcuts::{
    Action, Chord, ChordError, Hotkeys, Key, RecorderInput, Registration, SetError, Shortcuts,
    chord_from_key_event,
};

fn chord(text: &str) -> Chord {
    Chord::parse(text).unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn held(ctrl: bool, alt: bool, shift: bool, win: bool) -> Held {
    Held {
        shift,
        ctrl,
        alt,
        win,
    }
}

#[test]
fn ids_round_trip_and_are_distinct() {
    for action in Action::ALL {
        assert_eq!(Action::from_id(action.id()), Some(*action));
        assert!(!action.label().is_empty());
    }
    assert_eq!(Action::from_id("nonsense"), None);
    assert_eq!(Action::RestoreLast.id(), "restore_last");
    assert_eq!(Action::ToggleRecording.label(), "Start or stop recording");
}

#[test]
fn defaults_follow_the_roadmap() {
    let shortcuts = Shortcuts::defaults();
    for action in [
        Action::RestoreLast,
        Action::DiscardTop,
        Action::DiscardAll,
        Action::OpenSettings,
        Action::NewNote,
    ] {
        assert_eq!(shortcuts.get(action), None, "{action:?}");
    }
    assert_eq!(
        shortcuts.get(Action::ToggleRecording),
        Some(chord("Ctrl+Shift+6"))
    );
    assert_eq!(shortcuts.get(Action::Pen), Some(chord("Ctrl+Shift+7")));
    assert_eq!(
        shortcuts.get(Action::Highlighter),
        Some(chord("Ctrl+Shift+8"))
    );
    for action in Action::ALL {
        if let Some(chord) = action.default_chord() {
            assert_eq!(chord.validate(), Ok(()), "{action:?}");
        }
    }
    assert!(Action::RestoreLast.available() && Action::OpenSettings.available());
    assert!(Action::NewNote.available() && !Action::Highlighter.available());
}

/// The note editor uses Ctrl+Shift+7 (numbered list) and Ctrl+Shift+8 (bulleted list), the
/// default chords of Pen and Highlighter. A registered hotkey would take those keys from every
/// note. While the actions are unavailable nothing registers them; whoever makes them available
/// (the recording milestone) must decide the clash and change this test.
#[test]
fn pen_and_highlighter_keys_stay_free_for_the_note_editor() {
    let defaults = Shortcuts::defaults();
    assert_eq!(defaults.get(Action::Pen), Some(chord("Ctrl+Shift+7")));
    assert_eq!(defaults.get(Action::Highlighter), Some(chord("Ctrl+Shift+8")));
    assert!(!Action::Pen.available() && !Action::Highlighter.available());
    // The rule itself: an unavailable action is never registered, whatever its chord.
    let hotkeys = Hotkeys::start(&defaults).expect("hotkey thread");
    let status = hotkeys.status();
    assert!(hotkeys.shutdown());
    for action in Action::ALL {
        let state = status.iter().find(|(a, _)| a == action).map(|(_, s)| s.clone());
        if action.available() {
            assert_ne!(state, Some(Registration::Unavailable), "{action:?}");
        } else {
            assert_eq!(state, Some(Registration::Unavailable), "{action:?}");
        }
    }
}

#[test]
fn every_key_round_trips_with_every_modifier_set() {
    let keys: Vec<Key> = Key::all().collect();
    // 26 letters, 10 digits, 24 function keys, 10 numpad digits, 33 others.
    assert_eq!(keys.len(), 26 + 10 + 24 + 10 + 33);
    for key in keys {
        for bits in 0..16u8 {
            let chord = Chord {
                ctrl: bits & 1 != 0,
                alt: bits & 2 != 0,
                shift: bits & 4 != 0,
                win: bits & 8 != 0,
                key,
            };
            let text = chord.display();
            assert_eq!(Chord::parse(&text), Ok(chord), "{text}");
            assert_eq!(Chord::parse(&text.to_lowercase()), Ok(chord), "{text}");
            assert_eq!(Chord::parse(&text.replace('+', " + ")), Ok(chord), "{text}");
        }
    }
}

#[test]
fn display_order_and_chips() {
    let c = Chord {
        ctrl: true,
        alt: true,
        shift: true,
        win: true,
        key: Key::from_vk(0x78).unwrap(),
    };
    assert_eq!(c.display(), "Ctrl+Alt+Shift+Win+F9");
    assert_eq!(chord("shift+ctrl+6").display(), "Ctrl+Shift+6");
    assert_eq!(chord("Ctrl+Shift+6").chips(), ["Ctrl", "Shift", "6"]);
    assert_eq!(chord("ctrl+alt+num 0").display(), "Ctrl+Alt+Num 0");
    assert_eq!(chord("Ctrl+Alt+Num Plus").key.vk(), 0x6B);
    assert_eq!(chord("Ctrl+Alt+=").key.vk(), 0xBB);
    assert_eq!(chord("Ctrl+Alt+\\").key.vk(), 0xDC);
    assert_eq!(chord("control+escape").display(), "Ctrl+Esc");
    assert_eq!(chord("Windows+Alt+PgUp").display(), "Alt+Win+PageUp");
}

#[test]
fn parse_errors() {
    assert_eq!(Chord::parse(""), Err(ChordError::Empty));
    assert_eq!(Chord::parse("   "), Err(ChordError::Empty));
    assert_eq!(Chord::parse("Ctrl+Shift"), Err(ChordError::Empty));
    assert_eq!(Chord::parse("Win"), Err(ChordError::Empty));
    assert_eq!(
        Chord::parse("Ctrl+Hyper"),
        Err(ChordError::UnknownKey("Hyper".into()))
    );
    assert_eq!(
        Chord::parse("Ctrl+A+B"),
        Err(ChordError::UnknownKey("B".into()))
    );
    assert_eq!(
        Chord::parse("Ctrl+"),
        Err(ChordError::UnknownKey("".into()))
    );
}

fn refused(text: &str) -> ChordError {
    chord(text).validate().expect_err(text)
}

fn accepted(text: &str) {
    assert_eq!(chord(text).validate(), Ok(()), "{text}");
}

#[test]
fn modifier_rule() {
    accepted("Ctrl+Alt+Z");
    accepted("Alt+Q");
    accepted("F9");
    accepted("Shift+F9");
    accepted("F24");
    assert_eq!(refused("A"), ChordError::NoModifier);
    assert_eq!(refused("Home"), ChordError::NoModifier);
    // Shift alone types a character or selects text.
    assert_eq!(refused("Shift+A"), ChordError::NoModifier);
    assert_eq!(refused("Shift+5"), ChordError::NoModifier);
    assert_eq!(refused("Shift+Home"), ChordError::NoModifier);
    accepted("Ctrl+Shift+A");
}

#[test]
fn screenshot_keys_are_refused() {
    for text in [
        "PrtScn",
        "Alt+PrtScn",
        "Win+PrtScn",
        "Ctrl+Shift+PrtScn",
        "Win+Shift+S",
    ] {
        assert_eq!(refused(text), ChordError::ScreenshotKey, "{text}");
    }
    accepted("Ctrl+Shift+S");
}

#[test]
fn windows_chords_are_refused() {
    let reserved = |text: &str| {
        assert!(
            matches!(refused(text), ChordError::ReservedByWindows(_)),
            "{text}"
        )
    };
    for text in [
        "Ctrl+Alt+Delete",
        "Ctrl+Alt+Shift+Delete",
        "F12",
        "Ctrl+F12",
        "Alt+Tab",
        "Alt+Shift+Tab",
        "Ctrl+Alt+Tab",
        "Alt+F4",
        "Ctrl+Esc",
        "Ctrl+Shift+Esc",
        "Alt+Esc",
        "Alt+Space",
        "Win+L",
        "Win+D",
        "Win+Alt+K",
        "Win+1",
        "Win+Shift+0",
        "Win+Left",
        "Win+Home",
        "Win+Tab",
        "Win+Space",
        "Win+.",
        "Win+Num Plus",
        "Win+F1",
        "Ctrl+Win+F4",
    ] {
        reserved(text);
    }
    assert_eq!(
        refused("Win+L"),
        ChordError::ReservedByWindows("Win+L locks the computer")
    );
    // Nearby chords Windows does not use stay allowed.
    for text in [
        "Ctrl+Alt+End",
        "Ctrl+Tab",
        "Alt+F5",
        "Ctrl+Alt+F4",
        "Ctrl+Space",
        "Win+Alt+F9",
        "Win+F13",
        "Win+PageUp",
        "Win+Num 5",
        "F11",
        "Ctrl+Alt+Shift+F13",
    ] {
        accepted(text);
    }
}

#[test]
fn the_apps_own_keystrokes_are_refused() {
    for text in ["Ctrl+V", "Ctrl+Shift+V", "Esc", "Right"] {
        assert!(matches!(refused(text), ChordError::UsedByApp(_)), "{text}");
    }
    accepted("Ctrl+Alt+V");
    accepted("Ctrl+Alt+Right");
}

#[test]
fn errors_have_text() {
    for error in [
        ChordError::NoModifier,
        ChordError::ReservedByWindows("x"),
        ChordError::ScreenshotKey,
        ChordError::UsedByApp("x"),
        ChordError::UnknownKey("x".into()),
        ChordError::Empty,
    ] {
        assert!(!error.to_string().is_empty());
    }
    assert_eq!(
        SetError::InUse(Action::Pen).to_string(),
        "Already used by \"Pen\"."
    );
}

#[test]
fn set_refuses_invalid_and_shared_chords() {
    let mut s = Shortcuts::defaults();
    assert_eq!(
        s.set(Action::RestoreLast, Some(chord("Ctrl+V"))),
        Err(SetError::Invalid(ChordError::UsedByApp(
            "Ctrl+V pastes the screenshot"
        )))
    );
    assert_eq!(
        s.set(Action::RestoreLast, Some(chord("Ctrl+Shift+7"))),
        Err(SetError::InUse(Action::Pen))
    );
    assert_eq!(
        s.get(Action::RestoreLast),
        None,
        "a refused set changes nothing"
    );
    s.set(Action::RestoreLast, Some(chord("Ctrl+Alt+Z")))
        .unwrap();
    // Setting an action's own chord again is fine.
    s.set(Action::RestoreLast, Some(chord("Ctrl+Alt+Z")))
        .unwrap();
    assert_eq!(s.action_for(chord("Ctrl+Alt+Z")), Some(Action::RestoreLast));
    assert_eq!(s.action_for(chord("Ctrl+Shift+7")), Some(Action::Pen));
    assert_eq!(s.action_for(chord("Ctrl+Alt+Y")), None);
    s.set(Action::Pen, None).unwrap();
    assert_eq!(s.get(Action::Pen), None);
    assert_eq!(s.action_for(chord("Ctrl+Shift+7")), None);
    s.set(Action::DiscardTop, Some(chord("Ctrl+Shift+7")))
        .unwrap();
}

#[test]
fn reset_takes_the_default_from_another_action() {
    let mut s = Shortcuts::defaults();
    s.set(Action::Pen, None).unwrap();
    s.set(Action::DiscardTop, Some(chord("Ctrl+Shift+7")))
        .unwrap();
    s.reset(Action::Pen);
    assert_eq!(s.get(Action::Pen), Some(chord("Ctrl+Shift+7")));
    assert_eq!(s.get(Action::DiscardTop), None);
    s.set(Action::RestoreLast, Some(chord("Ctrl+Alt+Z")))
        .unwrap();
    s.reset(Action::RestoreLast);
    assert_eq!(s, Shortcuts::defaults());
}

#[test]
fn serde_round_trip_and_format() {
    let mut s = Shortcuts::defaults();
    s.set(Action::RestoreLast, Some(chord("Ctrl+Alt+Z")))
        .unwrap();
    s.set(Action::Pen, None).unwrap();
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(json["restore_last"], "Ctrl+Alt+Z");
    assert_eq!(json["discard_top"], serde_json::Value::Null);
    assert_eq!(json["pen"], serde_json::Value::Null);
    assert_eq!(json["toggle_recording"], "Ctrl+Shift+6");
    assert_eq!(json.as_object().unwrap().len(), Action::ALL.len());
    let back: Shortcuts = serde_json::from_value(json).unwrap();
    assert_eq!(back, s);
}

#[test]
fn serde_is_lenient_per_action() {
    let read = |text: &str| -> Shortcuts { serde_json::from_str(text).unwrap() };
    // Missing actions take their default; unknown ids are ignored.
    let s = read(r#"{"restore_last": "ctrl + alt + z", "teleport": "Ctrl+Alt+T"}"#);
    assert_eq!(s.get(Action::RestoreLast), Some(chord("Ctrl+Alt+Z")));
    assert_eq!(s.get(Action::Pen), Some(chord("Ctrl+Shift+7")));
    // An unparsable, invalid or wrongly typed entry falls back to that action's default only.
    let s = read(
        r#"{"restore_last": "Ctrl+Alt+Z", "pen": "Ctrl+Hyper+7", "highlighter": "Ctrl+V",
            "toggle_recording": 42, "discard_top": "Win+L", "discard_all": "Ctrl+Alt+X"}"#,
    );
    assert_eq!(s.get(Action::RestoreLast), Some(chord("Ctrl+Alt+Z")));
    assert_eq!(s.get(Action::Pen), Some(chord("Ctrl+Shift+7")));
    assert_eq!(s.get(Action::Highlighter), Some(chord("Ctrl+Shift+8")));
    assert_eq!(s.get(Action::ToggleRecording), Some(chord("Ctrl+Shift+6")));
    assert_eq!(s.get(Action::DiscardTop), None);
    assert_eq!(s.get(Action::DiscardAll), Some(chord("Ctrl+Alt+X")));
    // null means no shortcut.
    let s = read(r#"{"pen": null}"#);
    assert_eq!(s.get(Action::Pen), None);
    // Two actions with one chord: the first in Action::ALL keeps it.
    let s = read(r#"{"discard_all": "Ctrl+Alt+Z", "restore_last": "Ctrl+Alt+Z"}"#);
    assert_eq!(s.get(Action::RestoreLast), Some(chord("Ctrl+Alt+Z")));
    assert_eq!(s.get(Action::DiscardAll), None);
    // Also against a default: restore_last takes Ctrl+Shift+6, recording loses it.
    let s = read(r#"{"restore_last": "Ctrl+Shift+6"}"#);
    assert_eq!(s.get(Action::RestoreLast), Some(chord("Ctrl+Shift+6")));
    assert_eq!(s.get(Action::ToggleRecording), None);
    // Not an object: the defaults.
    assert_eq!(read("[1, 2]"), Shortcuts::defaults());
    assert_eq!(read("null"), Shortcuts::defaults());
}

#[test]
fn recorder_input() {
    let none = Held::default();
    let ctrl_shift = held(true, false, true, false);
    // Letters, digits, function keys, numpad.
    assert_eq!(
        chord_from_key_event(0x41, ctrl_shift),
        RecorderInput::Chord(chord("Ctrl+Shift+A"))
    );
    assert_eq!(
        chord_from_key_event(0x36, ctrl_shift),
        RecorderInput::Chord(chord("Ctrl+Shift+6"))
    );
    assert_eq!(
        chord_from_key_event(0x78, none),
        RecorderInput::Chord(chord("F9"))
    );
    assert_eq!(
        chord_from_key_event(0x87, held(false, true, false, true)),
        RecorderInput::Chord(chord("Win+Alt+F24"))
    );
    assert_eq!(
        chord_from_key_event(0x65, held(true, true, false, false)),
        RecorderInput::Chord(chord("Ctrl+Alt+Num 5"))
    );
    // A chord the rules refuse is still reported; `Shortcuts::set` says why.
    assert_eq!(
        chord_from_key_event(0x56, held(true, false, false, false)),
        RecorderInput::Chord(chord("Ctrl+V"))
    );
    // Bare modifiers, left and right, keep waiting.
    for vk in [
        0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5,
    ] {
        assert_eq!(
            chord_from_key_event(vk, ctrl_shift),
            RecorderInput::Pending,
            "{vk:#x}"
        );
    }
    // Esc alone cancels; Backspace or Delete alone clears.
    assert_eq!(chord_from_key_event(0x1B, none), RecorderInput::Cancel);
    assert_eq!(chord_from_key_event(0x08, none), RecorderInput::Clear);
    assert_eq!(chord_from_key_event(0x2E, none), RecorderInput::Clear);
    // With modifiers they are chords.
    assert_eq!(
        chord_from_key_event(0x2E, held(true, true, false, false)),
        RecorderInput::Chord(chord("Ctrl+Alt+Delete"))
    );
    assert_eq!(
        chord_from_key_event(0x08, held(true, true, false, false)),
        RecorderInput::Chord(chord("Ctrl+Alt+Backspace"))
    );
    // Keys a shortcut cannot use (IME process key, volume up) keep waiting.
    assert_eq!(
        chord_from_key_event(0xE5, ctrl_shift),
        RecorderInput::Pending
    );
    assert_eq!(
        chord_from_key_event(0xAF, ctrl_shift),
        RecorderInput::Pending
    );
}
