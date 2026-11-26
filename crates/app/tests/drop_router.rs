//! Scripted tests for `igorio_app::drop_router::Router`: a fake environment stands in for
//! UI Automation, the clipboard thread, the PNG saver and the keyboard, and each test drives
//! the router with events and times as they would arrive.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use igorio_app::clipboard::Payload;
use igorio_app::drop_router::*;
use igorio_app::hook::{Button, MouseEvent};
use igorio_app::input::{Key, Modifier, Sent};
use igorio_app::state::FairyState;
use igorio_app::uia::{ElementInfo, Outcome, Shell, Snapshot, Target, UiaNode};
use igorio_core::geom::{CardId, Rect};
use igorio_kit::{HeldCard, Image};
use windows::Win32::UI::Accessibility::*;

// MARK: Fake environment

type Snap = &'static str;

#[derive(Default)]
struct Fake {
    next: u64,
    lookups: Vec<(u64, (i32, i32), bool, f64)>,
    facts: ClipFacts,
    snapshots: Vec<u64>,
    stages: Vec<(u64, Payload)>,
    restages: Vec<u64>,
    restores: Vec<(u64, Snap, RestoreGuard)>,
    saves: Vec<u64>,
    /// The foreground window and its process; `front: false` puts something else there.
    front: bool,
    fg_window: isize,
    fg_pid: u32,
    /// Every foreground check: (window, pid) asked about.
    checks: RefCell<Vec<(isize, u32)>>,
    /// The user's main button is down (the real state).
    button: bool,
    /// The window under a first click is a terminal.
    terminal: bool,
    /// Results for the paste keys, in order; `Done` once empty.
    pastes: VecDeque<Sent>,
    paste_keys: Vec<PasteKeys>,
    pressed: Vec<Key>,
    sent: u32,
    log: Vec<String>,
}

impl Fake {
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn logged(&self, text: &str) -> bool {
        self.log.iter().any(|line| line.contains(text))
    }

    /// Every foreground check asked about the double-clicked window (0x10 of pid 100).
    fn assert_aimed(&self) {
        let checks = self.checks.borrow();
        assert!(!checks.is_empty(), "no foreground check");
        assert!(
            checks.iter().all(|c| *c == (0x10, 100)),
            "checked {checks:?}"
        );
    }
}

impl Env for Fake {
    type Snap = Snap;

    fn lookup(&mut self, point: (i32, i32), want_focus: bool, deadline: f64) -> u64 {
        let id = self.id();
        self.lookups.push((id, point, want_focus, deadline));
        id
    }
    fn clip_facts(&self, _now: f64) -> ClipFacts {
        self.facts
    }
    fn clip_snapshot(&mut self) -> Result<u64, String> {
        let id = self.id();
        self.snapshots.push(id);
        Ok(id)
    }
    fn clip_stage(&mut self, payload: Payload) -> Result<u64, String> {
        let id = self.id();
        self.stages.push((id, payload));
        Ok(id)
    }
    fn clip_restage(&mut self) -> Result<u64, String> {
        let id = self.id();
        self.restages.push(id);
        Ok(id)
    }
    fn clip_restore(&mut self, snap: &Snap, guard: RestoreGuard) -> Result<u64, String> {
        let id = self.id();
        self.restores.push((id, snap, guard));
        Ok(id)
    }
    fn save_png(&mut self, _card: &HeldCard) -> u64 {
        let id = self.id();
        self.saves.push(id);
        id
    }
    fn foreground_is(&self, window: isize, pid: u32) -> bool {
        self.checks.borrow_mut().push((window, pid));
        let matches = if window != 0 {
            window == self.fg_window
        } else {
            pid == self.fg_pid
        };
        self.front && matches
    }
    fn primary_down(&self) -> bool {
        self.button
    }
    fn terminal_at(&self, _point: (i32, i32)) -> bool {
        self.terminal
    }
    fn press(&mut self, key: Key) -> Sent {
        self.pressed.push(key);
        Sent::Done
    }
    fn paste_shortcut(&mut self, keys: PasteKeys) -> Sent {
        self.paste_keys.push(keys);
        let result = self.pastes.pop_front().unwrap_or(Sent::Done);
        if !matches!(result, Sent::Blocked(_)) {
            self.sent += 1;
        }
        result
    }
    fn log(&mut self, now: f64, line: &str) {
        self.log.push(format!("{now:.3} {line}"));
    }
}

// MARK: Rig

const AT: (i32, i32) = (500, 400);

struct Rig {
    r: Router<Snap>,
    s: FairyState,
    e: Fake,
    now: f64,
}

fn image() -> Arc<Image> {
    Arc::new(Image::new(2, 2, vec![255; 16]).unwrap())
}

impl Rig {
    fn new(cards: usize) -> Rig {
        let mut s = FairyState::new(7);
        for _ in 0..cards {
            s.hold(
                image(),
                1.0,
                SystemTime::UNIX_EPOCH,
                false,
                None,
                None,
                -10.0,
            );
        }
        let e = Fake {
            front: true,
            fg_window: 0x10,
            fg_pid: 100,
            facts: ClipFacts {
                sequence: 5,
                ..ClipFacts::default()
            },
            ..Fake::default()
        };
        Rig {
            r: Router::new(Button::Left),
            s,
            e,
            now: 0.0,
        }
    }

    fn with_movie() -> Rig {
        let mut rig = Rig::new(0);
        rig.s.hold(
            image(),
            1.0,
            SystemTime::UNIX_EPOCH,
            true,
            Some(PathBuf::from(r"C:\Videos\clip one.mp4")),
            Some(3.0),
            -10.0,
        );
        rig
    }

    /// Runs every deadline up to `t`, as the main loop would.
    fn to(&mut self, t: f64) {
        for _ in 0..100_000 {
            match self.r.next_deadline() {
                Some(d) if d <= t => {
                    self.now = d.max(self.now);
                    self.r.tick(self.now, &mut self.s, &mut self.e);
                }
                _ => {
                    self.now = t.max(self.now);
                    return;
                }
            }
        }
        panic!("the router does not settle before {t}");
    }

    fn mouse(&mut self, event: MouseEvent, t: f64) {
        self.to(t);
        self.r.mouse(event, &mut self.s, &mut self.e);
    }

    fn down(&mut self, t: f64, click_count: u32) {
        let (x, y) = AT;
        self.to(t);
        self.e.button = true;
        self.mouse(
            MouseEvent::Down {
                button: Button::Left,
                x,
                y,
                time: t,
                click_count,
                injected: false,
            },
            t,
        );
    }

    fn up(&mut self, t: f64) {
        let (x, y) = AT;
        self.to(t);
        self.e.button = false;
        self.mouse(
            MouseEvent::Up {
                button: Button::Left,
                x,
                y,
                time: t,
                injected: false,
            },
            t,
        );
    }

    fn moved(&mut self, t: f64, x: i32, y: i32) {
        self.mouse(
            MouseEvent::Move {
                x,
                y,
                time: t,
                injected: false,
            },
            t,
        );
    }

    fn answer(&mut self, t: f64, seen: Seen) {
        self.to(t);
        let id = self.e.lookups.last().expect("a lookup").0;
        self.r.seen(id, seen, t, &mut self.s, &mut self.e);
    }

    fn reply(&mut self, t: f64, ticket: u64, result: Result<ClipDone<Snap>, ClipFail>) {
        self.to(t);
        self.r
            .clip_reply(ClipReply { ticket, result }, t, &mut self.s, &mut self.e);
    }

    fn snapshot_reply(&mut self, t: f64, ticket: u64, lost: Option<&str>) {
        self.reply(
            t,
            ticket,
            Ok(ClipDone::Snapshot {
                snap: "user",
                sequence: 5,
                lost: lost.map(str::to_string),
            }),
        );
    }

    fn staged_reply(&mut self, t: f64) {
        let ticket = self.e.stages.last().expect("a stage").0;
        // The clipboard thread updates its facts before it replies.
        self.e.facts.sequence = 6;
        self.e.facts.staged = Some(StagedFacts {
            ticket,
            ..StagedFacts::default()
        });
        self.reply(t, ticket, Ok(ClipDone::Staged { sequence: 6 }));
    }

    fn read(&mut self, t: f64, reads: u32) {
        self.to(t);
        let staged = self.e.facts.staged.as_mut().expect("staged");
        staged.reads = reads;
        staged.last_read_at = Some(t);
        self.r.clipboard_changed(t, &mut self.s, &mut self.e);
    }

    fn overwrite(&mut self, t: f64) {
        self.to(t);
        self.e.facts.last_change = Some(t);
        self.e.facts.sequence += 1;
        if let Some(staged) = self.e.facts.staged.as_mut() {
            staged.overwritten_at.get_or_insert(t);
        }
        self.r.clipboard_changed(t, &mut self.s, &mut self.e);
    }

    /// First click (remembers the clipboard) and the second mouse-down at 0.15 s.
    fn double_click(&mut self) {
        self.down(0.0, 1);
        self.up(0.08);
        self.down(0.15, 2);
    }

    /// The first snapshot arrives (taken when the double-click was decided, or at the first
    /// mouse-down in a terminal).
    fn remembered_arrives(&mut self, t: f64) {
        let ticket = *self.e.snapshots.first().expect("a snapshot");
        self.snapshot_reply(t, ticket, None);
    }

    fn ids(&self) -> Vec<CardId> {
        self.s.deliverable().iter().map(|c| c.id).collect()
    }

    fn restored(&self) -> Vec<Snap> {
        self.e.restores.iter().map(|r| r.1).collect()
    }
}

fn target(exe: &str, terminal: bool) -> Target {
    Target {
        pid: 100,
        exe: Some(exe.to_string()),
        window: 0x10,
        child_window: 0x11,
        class_name: "Window".into(),
        is_terminal: terminal,
        shell: None,
    }
}

fn field(exe: &str) -> Seen {
    let mut spot = Spot::new(target(exe, false));
    spot.role = "AXTextArea".into();
    spot.editable_hit = true;
    spot.editable = true;
    Seen::Spot(Box::new(spot))
}

fn terminal() -> Seen {
    let mut spot = Spot::new(target("WindowsTerminal.exe", true));
    spot.role = "AXStaticText".into();
    spot.editable_hit = true;
    spot.editable = true;
    Seen::Spot(Box::new(spot))
}

fn nothing(exe: &str) -> Spot {
    let mut spot = Spot::new(target(exe, false));
    spot.role = "AXGroup".into();
    spot
}

/// Double-click on a field, the remembered clipboard, the release, then everything up to
/// the Ctrl+V. Returns the time Ctrl+V went out.
fn paste_into_field(rig: &mut Rig) -> f64 {
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    assert_eq!(rig.e.stages.len(), 1, "{:#?}", rig.e.log);
    rig.staged_reply(0.41);
    assert_eq!(rig.e.sent, 1, "{:#?}", rig.e.log);
    rig.e.assert_aimed();
    0.41
}

// MARK: Tests

#[test]
fn double_click_on_a_field_pastes_the_image_and_restores_the_clipboard() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    let sent = paste_into_field(&mut rig);
    // The first lookup is the double-click's, without focus, with the usual deadline.
    assert_eq!(rig.e.lookups[0].1, AT);
    assert!(!rig.e.lookups[0].2);
    assert_eq!(rig.e.lookups[0].3, LOOKUP_DEADLINE);
    assert!(rig.e.logged("→ Paste"));
    // One snapshot only: the remembered one is used.
    assert_eq!(rig.e.snapshots.len(), 1);
    assert_eq!(rig.e.pressed, vec![Key::Right]);
    assert!(matches!(rig.e.stages[0].1, Payload::Image(_)));
    rig.read(sent + 0.02, 2);
    rig.to(sent + 0.2);
    assert_eq!(rig.s.hint(), Some(HINT_PASTED));
    assert!(rig.s.card(id).is_none());
    // Restore waits for RestoreRules (min 2 s after Ctrl+V).
    rig.to(sent + 1.9);
    assert!(rig.e.restores.is_empty());
    rig.to(sent + 2.1);
    assert_eq!(rig.restored(), vec!["user"]);
    assert_eq!(rig.e.restores[0].2, RestoreGuard::OwnStaging);
    assert_eq!(rig.e.paste_keys, vec![PasteKeys::CtrlV]);
    let ticket = rig.e.restores[0].0;
    rig.reply(sent + 2.2, ticket, Ok(ClipDone::Restored { sequence: 7 }));
    assert!(!rig.r.restore_pending());
    assert!(!rig.r.busy());
    assert_eq!(rig.r.next_deadline(), None);
}

#[test]
fn a_terminal_gets_the_saved_png_as_a_quoted_path_without_the_right_arrow() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, terminal());
    assert_eq!(rig.e.saves.len(), 1);
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    let job = rig.e.saves[0];
    rig.to(0.26);
    rig.r.saved(
        job,
        Ok(PathBuf::from(
            r"C:\Pics\Pasted\Screenshot 2026-11-26 101010.png",
        )),
        0.26,
        &mut rig.s,
        &mut rig.e,
    );
    // TerminalSettle: 0.14 s after the mouse-up, then the 0.06 s gap.
    rig.to(0.38);
    assert_eq!(rig.e.stages.len(), 0, "{:#?}", rig.e.log);
    rig.to(0.5);
    assert_eq!(rig.e.stages.len(), 1, "{:#?}", rig.e.log);
    match &rig.e.stages[0].1 {
        Payload::Text(text) => {
            assert_eq!(
                text,
                "\"C:\\Pics\\Pasted\\Screenshot 2026-11-26 101010.png\" "
            )
        }
        other => panic!("{other:?}"),
    }
    assert!(rig.e.pressed.is_empty());
    rig.staged_reply(0.46);
    rig.read(0.48, 1);
    rig.to(0.7);
    assert_eq!(rig.s.hint(), Some(HINT_PASTED_PATH));
}

#[test]
fn a_recording_goes_as_the_file_into_a_field_and_as_its_path_into_a_terminal() {
    let mut rig = Rig::with_movie();
    paste_into_field(&mut rig);
    assert!(
        matches!(&rig.e.stages[0].1, Payload::File(p) if p == &PathBuf::from(r"C:\Videos\clip one.mp4"))
    );

    let mut rig = Rig::with_movie();
    rig.double_click();
    rig.answer(0.17, terminal());
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.5);
    assert!(rig.e.saves.is_empty());
    assert!(matches!(&rig.e.stages[0].1, Payload::Text(t) if t == "\"C:\\Videos\\clip one.mp4\" "));
}

#[test]
fn a_canvas_is_confirmed_after_the_mouse_up_and_pasted_without_the_right_arrow() {
    let mut rig = Rig::new(1);
    rig.double_click();
    let mut spot = nothing("Figma.exe");
    spot.canvas = CanvasFacts {
        canvas: true,
        known: true,
        document: false,
    };
    rig.answer(0.17, Seen::Spot(Box::new(spot.clone())));
    assert!(rig.e.logged("CheckFocusAfterMouseUp"));
    assert_eq!(rig.e.lookups.len(), 1);
    rig.up(0.25);
    // Settle 0.1 s, then the re-check with focus.
    rig.to(0.34);
    assert_eq!(rig.e.lookups.len(), 1);
    rig.to(0.36);
    assert_eq!(rig.e.lookups.len(), 2);
    assert!(rig.e.lookups[1].2);
    rig.answer(0.4, Seen::Spot(Box::new(spot)));
    rig.remembered_arrives(0.41);
    rig.to(0.6);
    assert_eq!(rig.e.stages.len(), 1);
    assert!(rig.e.pressed.is_empty());
    rig.staged_reply(0.61);
    // A canvas gives up after 1 s without a read.
    rig.to(1.7);
    assert_eq!(rig.s.hint(), Some(HINT_CANVAS_FAILED));
}

#[test]
fn a_whiteboard_text_box_opened_by_the_double_click_is_closed_with_escape() {
    let mut before = CanvasFacts {
        canvas: true,
        known: true,
        document: false,
    };
    let mut after = nothing("Figma.exe");
    after.editable = true;
    after.editable_hit = true;
    let confirmed = confirm_after_up(&after, AT, Some(100), before);
    assert_eq!(
        confirmed,
        Confirmed::Paste {
            surface: Surface::Canvas,
            dismiss_editor: true
        }
    );
    // In a page-drawn document the text box is where the paste goes (Swift: `.field`).
    before.document = true;
    assert_eq!(
        confirm_after_up(&after, AT, Some(100), before),
        Confirmed::Paste {
            surface: Surface::Field,
            dismiss_editor: false
        }
    );

    let mut rig = Rig::new(1);
    rig.double_click();
    let mut first = nothing("Figma.exe");
    first.canvas = CanvasFacts {
        canvas: true,
        known: true,
        document: false,
    };
    rig.answer(0.17, Seen::Spot(Box::new(first)));
    rig.up(0.25);
    rig.to(0.36);
    rig.answer(0.4, Seen::Spot(Box::new(after)));
    rig.remembered_arrives(0.41);
    rig.to(0.8);
    assert_eq!(rig.e.pressed, vec![Key::Escape]);
    assert_eq!(rig.e.stages.len(), 1);
}

#[test]
fn a_focused_field_counts_only_when_the_click_is_inside_it() {
    let mut spot = nothing("slack.exe");
    spot.pid = 100;
    spot.focused = Some(Focus {
        pid: 100,
        editable: true,
        frame: Some(Rect::new(400.0, 380.0, 300.0, 40.0)),
    });
    assert!(matches!(
        confirm_after_up(&spot, AT, Some(100), CanvasFacts::default()),
        Confirmed::Paste {
            surface: Surface::Field,
            ..
        }
    ));
    // A double-click on a toolbar must not paste into a field elsewhere.
    spot.focused.as_mut().unwrap().frame = Some(Rect::new(0.0, 900.0, 300.0, 40.0));
    assert_eq!(
        confirm_after_up(&spot, AT, Some(100), CanvasFacts::default()),
        Confirmed::Ignore
    );
    // Focus in another process does not count.
    spot.focused = Some(Focus {
        pid: 999,
        editable: true,
        frame: Some(Rect::new(400.0, 380.0, 300.0, 40.0)),
    });
    assert_eq!(
        confirm_after_up(&spot, AT, Some(100), CanvasFacts::default()),
        Confirmed::Ignore
    );
}

#[test]
fn nothing_recognised_after_the_mouse_up_is_ignored() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, Seen::Spot(Box::new(nothing("app.exe"))));
    rig.up(0.25);
    rig.to(0.36);
    rig.answer(0.4, Seen::Spot(Box::new(nothing("app.exe"))));
    rig.to(3.0);
    assert!(rig.e.stages.is_empty());
    assert!(rig.s.is_holding());
    assert!(!rig.r.busy());
    assert!(rig.e.logged("→ Ignore"));
}

#[test]
fn a_slow_app_gets_one_longer_lookup_after_the_mouse_up() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(
        0.4,
        Seen::TimedOut {
            target: target("RobloxStudioBeta.exe", false),
        },
    );
    assert!(rig.e.logged("CheckFocusAfterMouseUp"));
    rig.up(0.42);
    rig.to(0.6);
    assert_eq!(rig.e.lookups.len(), 2);
    let (_, point, focus, deadline) = rig.e.lookups[1];
    assert_eq!((point, focus, deadline), (AT, true, SLOW_DEADLINE));
    // It answers this time: a text field is at the click point.
    let mut spot = nothing("RobloxStudioBeta.exe");
    spot.editable_hit = true;
    spot.editable = true;
    rig.answer(1.1, Seen::Spot(Box::new(spot)));
    rig.remembered_arrives(1.11);
    rig.to(1.4);
    assert_eq!(rig.e.stages.len(), 1);
    rig.e.assert_aimed();
}

#[test]
fn a_slow_app_that_never_answers_is_not_pasted_into() {
    let mut rig = Rig::new(1);
    rig.double_click();
    let slow = Seen::TimedOut {
        target: target("RobloxStudioBeta.exe", false),
    };
    rig.answer(0.4, slow.clone());
    rig.up(0.42);
    rig.to(0.6);
    rig.answer(1.6, slow);
    rig.to(5.0);
    assert!(rig.e.stages.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_SLOW));
    assert!(rig.s.is_holding());
}

#[test]
fn a_lookup_that_never_answers_is_given_up() {
    let mut rig = Rig::new(1);
    rig.double_click();
    // No answer at all (a replaced request): the router stops waiting and does nothing.
    rig.up(0.2);
    rig.to(0.15 + LOOKUP_DEADLINE + LOOKUP_GRACE + 0.01);
    assert!(rig.e.logged("lookup failed (no answer) → Ignore"));
    rig.to(3.0);
    assert!(!rig.r.busy());
    assert_eq!(rig.e.lookups.len(), 1, "no re-check after a failed lookup");
    assert!(rig.e.stages.is_empty());
    assert!(rig.e.snapshots.is_empty());
}

#[test]
fn a_silent_terminal_is_still_pasted_into() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(
        0.4,
        Seen::TimedOut {
            target: target("WindowsTerminal.exe", true),
        },
    );
    assert!(rig.e.logged("→ Paste"));
    assert_eq!(rig.e.saves.len(), 1);
}

#[test]
fn an_elevated_window_is_refused_with_a_hint() {
    let mut rig = Rig::new(1);
    rig.double_click();
    let tick = rig.s.reject_tick();
    rig.answer(0.17, Seen::Elevated { pid: 4 });
    assert_eq!(rig.s.hint(), Some(HINT_ELEVATED));
    assert_eq!(rig.s.reject_tick(), tick + 1);
    rig.to(3.0);
    assert!(rig.e.stages.is_empty());
    assert!(rig.s.is_holding());
}

#[test]
fn a_double_click_on_our_own_window_is_ignored() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, Seen::OwnProcess);
    rig.to(3.0);
    assert!(rig.e.logged("our own window → Ignore"));
    assert_eq!(rig.e.lookups.len(), 1);
    assert!(rig.e.stages.is_empty());
    assert!(!rig.r.busy());
}

#[test]
fn explorer_background_says_saving_comes_next_and_keeps_the_card() {
    let mut rig = Rig::new(1);
    rig.double_click();
    let mut t = target("explorer.exe", false);
    t.shell = Some(Shell::Explorer);
    let mut spot = Spot::new(t);
    spot.drop_spot = true;
    rig.answer(0.17, Seen::Spot(Box::new(spot.clone())));
    assert!(rig.e.logged("SaveToFinder"));
    assert_eq!(rig.s.hint(), Some(HINT_FOLDERS_NEXT));
    assert!(rig.s.is_holding());
    // An item (not the background) is the user opening a file: ignored.
    let mut rig = Rig::new(1);
    rig.double_click();
    spot.drop_spot = false;
    rig.answer(0.17, Seen::Spot(Box::new(spot)));
    assert!(rig.e.logged("→ Ignore"));
}

#[test]
fn a_lost_clipboard_snapshot_keeps_the_card_and_stages_nothing() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    let ticket = rig.e.snapshots[0];
    rig.snapshot_reply(0.18, ticket, Some("Kind(Files)"));
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.stages.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_CLIPBOARD));
    assert!(rig.s.is_holding());
    assert!(!rig.r.busy());
}

#[test]
fn no_clipboard_snapshot_at_all_keeps_the_card() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    let ticket = rig.e.snapshots[0];
    rig.reply(
        0.18,
        ticket,
        Err(ClipFail::Other("clipboard held open".into())),
    );
    rig.to(3.0);
    assert!(rig.e.stages.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_CLIPBOARD));
    assert!(rig.s.is_holding());
}

#[test]
fn a_held_modifier_delays_the_paste_and_then_gives_up() {
    // Shift held for a moment: the paste waits and then goes.
    let mut rig = Rig::new(1);
    rig.e.pastes = VecDeque::from([
        Sent::Blocked(Modifier::Shift),
        Sent::Blocked(Modifier::Shift),
    ]);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.staged_reply(0.41);
    assert_eq!(rig.e.sent, 0);
    rig.to(0.6);
    assert_eq!(rig.e.sent, 1);
    assert!(rig.e.pastes.is_empty());

    // Shift held throughout: give up, put the clipboard back at once, keep the card.
    let mut rig = Rig::new(1);
    rig.e.pastes = std::iter::repeat_n(Sent::Blocked(Modifier::Shift), 1000).collect();
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.staged_reply(0.41);
    rig.to(0.41 + MODIFIER_WAIT + 0.1);
    assert_eq!(rig.e.sent, 0);
    assert_eq!(
        rig.s.hint(),
        Some("let go of Shift to paste · still holding it")
    );
    assert_eq!(rig.restored(), vec!["user"]);
    assert!(rig.s.is_holding());
}

#[test]
fn a_target_that_leaves_the_front_before_ctrl_v_gets_nothing() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.e.front = false;
    rig.staged_reply(0.41);
    assert_eq!(rig.e.sent, 0);
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    assert_eq!(rig.restored(), vec!["user"]);
}

#[test]
fn a_target_never_in_front_is_not_typed_into() {
    let mut rig = Rig::new(1);
    rig.e.front = false;
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.pressed.is_empty());
    assert!(rig.e.stages.is_empty());
    assert!(rig.e.restores.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    assert!(rig.e.logged("not in front"));
}

#[test]
fn copy_on_select_after_the_paste_is_adopted_and_the_clipboard_still_restored() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, terminal());
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    let job = rig.e.saves[0];
    rig.r.saved(
        job,
        Ok(PathBuf::from(r"C:\p\a.png")),
        0.26,
        &mut rig.s,
        &mut rig.e,
    );
    rig.to(0.5);
    rig.staged_reply(0.5);
    rig.read(0.52, 1);
    // The terminal copies the selection on its own after the paste.
    rig.overwrite(0.6);
    assert!(rig.e.logged("selection copied after the paste"));
    rig.to(0.5 + 2.2);
    assert_eq!(rig.restored(), vec!["user"]);
}

#[test]
fn an_overwrite_before_any_read_writes_the_screenshot_again() {
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.overwrite(sent + 0.05);
    assert_eq!(rig.e.restages.len(), 1);
    let ticket = rig.e.restages[0];
    // The clipboard thread clears `overwritten_at` and counts the rewrite, then replies.
    rig.e.facts.staged.as_mut().unwrap().overwritten_at = None;
    rig.e.facts.staged.as_mut().unwrap().rewrites = 1;
    rig.reply(sent + 0.06, ticket, Ok(ClipDone::Staged { sequence: 9 }));
    assert_eq!(rig.e.restages.len(), 1);
    rig.read(sent + 0.1, 2);
    rig.to(sent + 0.3);
    assert_eq!(rig.s.hint(), Some(HINT_PASTED));
    rig.to(sent + 2.2);
    assert_eq!(rig.restored(), vec!["user"], "{:#?}", rig.e.log);
}

#[test]
fn a_real_copy_by_the_user_after_the_paste_is_left_alone() {
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 0.3);
    rig.overwrite(sent + 1.5);
    rig.to(sent + 10.0);
    assert!(rig.e.restores.is_empty());
    assert!(!rig.r.restore_pending());
}

#[test]
fn no_read_fails_and_restores_late_but_a_late_read_still_delivers() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    let sent = paste_into_field(&mut rig);
    rig.to(sent + 1.7);
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    assert!(rig.s.card(id).is_some());
    assert!(!rig.r.busy());
    // The busy app takes the Ctrl+V late.
    rig.read(sent + 3.0, 1);
    rig.to(sent + 3.1);
    assert!(rig.s.card(id).is_none());
    assert_eq!(rig.s.hint(), Some(HINT_PASTED));

    // Nobody ever reads: the clipboard comes back after the long no-read delay.
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.to(sent + 7.9);
    assert!(rig.e.restores.is_empty());
    rig.to(sent + 8.1);
    assert_eq!(rig.restored(), vec!["user"]);
}

#[test]
fn a_restore_refused_because_the_clipboard_changed_is_not_tried_again() {
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 2.1);
    let ticket = rig.e.restores[0].0;
    rig.reply(sent + 2.15, ticket, Err(ClipFail::Changed));
    rig.to(sent + 20.0);
    assert_eq!(rig.e.restores.len(), 1);
    assert!(!rig.r.restore_pending());
}

#[test]
fn a_locked_clipboard_delays_the_restore_and_then_gives_up() {
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 2.1);
    let first = rig.now;
    // Locked again and again: tries about 0.5 s apart.
    while rig.r.restore_pending() && rig.now < first + 20.0 {
        let ticket = rig.e.restores.last().unwrap().0;
        rig.reply(
            rig.now + 0.01,
            ticket,
            Err(ClipFail::Retry("clipboard held open".into())),
        );
        rig.to(rig.now + 0.6);
    }
    assert!(!rig.r.restore_pending());
    let tries = rig.e.restores.len();
    assert!((15..=22).contains(&tries), "{tries} tries");
    assert!(rig.now - first >= RESTORE_GIVE_UP - 0.6);
    assert_eq!(rig.s.hint(), Some(HINT_RESTORE_FAILED));

    // A lock that clears: the next try restores.
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 2.1);
    let ticket = rig.e.restores[0].0;
    rig.reply(sent + 2.12, ticket, Err(ClipFail::Retry("locked".into())));
    rig.to(sent + 2.5);
    assert_eq!(rig.e.restores.len(), 1, "not before 0.5 s");
    rig.to(sent + 2.7);
    assert_eq!(rig.e.restores.len(), 2);
    let ticket = rig.e.restores[1].0;
    rig.reply(sent + 2.75, ticket, Ok(ClipDone::Restored { sequence: 9 }));
    assert!(!rig.r.restore_pending());
}

#[test]
fn a_second_double_click_during_a_delivery_is_ignored() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.down(0.3, 1);
    rig.down(0.4, 2);
    assert_eq!(rig.e.lookups.len(), 1);
    assert!(rig.e.logged("busy=yes"));
}

#[test]
fn the_whole_pile_goes_top_first_and_skips_cards_shaken_off_meanwhile() {
    let mut rig = Rig::new(3);
    let ids = rig.ids();
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 0.2);
    assert_eq!(rig.s.hint(), Some("pasted 3"));
    assert!(rig.s.card(ids[0]).is_none());
    // The next card is shaken off before its turn.
    rig.s.dissolve(rig.now);
    assert!(rig.s.card(ids[1]).is_none());
    rig.to(sent + 1.0);
    assert_eq!(rig.e.stages.len(), 2);
    rig.staged_reply(rig.now);
    // Later cards keep the caret where it is.
    assert_eq!(rig.e.pressed, vec![Key::Right]);
    let next_sent = rig.now;
    rig.read(next_sent + 0.02, 2);
    rig.to(next_sent + 0.3);
    assert!(rig.s.card(ids[2]).is_none());
    assert!(!rig.r.busy());
    // One restore at the end, with the copy from before the first paste.
    rig.to(next_sent + 2.2);
    assert_eq!(rig.restored(), vec!["user"]);
    assert_eq!(rig.e.snapshots.len(), 1);
}

#[test]
fn a_later_card_that_is_not_read_ends_the_pile_with_a_hint() {
    let mut rig = Rig::new(2);
    let ids = rig.ids();
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 0.2);
    rig.to(sent + 1.0);
    rig.staged_reply(rig.now);
    rig.to(rig.now + 1.7);
    assert_eq!(rig.s.hint(), Some(HINT_REST_FAILED));
    assert!(rig.s.card(ids[1]).is_some());
}

#[test]
fn a_capture_landing_during_a_delivery_stays_on_the_pile() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.s
        .hold(image(), 1.0, SystemTime::UNIX_EPOCH, false, None, None, 0.2);
    rig.remembered_arrives(0.21);
    rig.up(0.25);
    rig.to(0.4);
    rig.staged_reply(0.41);
    rig.read(0.43, 2);
    rig.to(3.0);
    assert_eq!(rig.e.stages.len(), 1);
    assert!(rig.s.is_holding());
    assert_eq!(rig.s.deliverable().len(), 1);
}

#[test]
fn a_card_shaken_off_before_its_paste_is_not_pasted() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.s.discard_all(0.2);
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.stages.is_empty());
    assert!(!rig.r.busy());
}

#[test]
fn the_click_copy_of_a_terminal_is_not_taken_for_the_users_clipboard() {
    // The clipboard changed between the first click and the paste (copy-on-select): the
    // copy remembered at the first mouse-down is used.
    let mut rig = Rig::new(1);
    rig.e.terminal = true;
    rig.double_click();
    assert_eq!(
        rig.e.snapshots.len(),
        1,
        "a terminal's first click keeps a copy"
    );
    rig.remembered_arrives(0.1);
    rig.e.facts.sequence = 8;
    rig.e.facts.last_change = Some(0.12);
    rig.answer(0.17, terminal());
    assert_eq!(rig.e.snapshots.len(), 1);
    assert!(rig.e.logged("using the copy from the first click"));

    // A remembered copy older than 2 s whose sequence moved on is not used.
    let mut rig = Rig::new(1);
    rig.e.terminal = true;
    rig.down(0.0, 1);
    rig.remembered_arrives(0.05);
    rig.up(0.08);
    rig.e.facts.sequence = 8;
    rig.to(5.0);
    rig.mouse(
        MouseEvent::Down {
            button: Button::Left,
            x: AT.0,
            y: AT.1,
            time: 5.0,
            click_count: 2,
            injected: false,
        },
        5.0,
    );
    rig.answer(5.02, field("notepad.exe"));
    assert_eq!(rig.e.snapshots.len(), 2);
}

#[test]
fn a_paste_during_a_pending_restore_reuses_the_saved_clipboard() {
    let mut rig = Rig::new(2);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 0.2);
    // Make the second card fail so the delivery ends with the restore still waiting.
    rig.to(sent + 1.0);
    rig.staged_reply(rig.now);
    rig.to(rig.now + 1.7);
    assert!(rig.r.restore_pending());
    let t = rig.now + 0.1;
    rig.down(t, 1);
    rig.up(t + 0.05);
    rig.down(t + 0.1, 2);
    rig.answer(t + 0.12, field("notepad.exe"));
    rig.up(t + 0.15);
    rig.to(t + 0.4);
    assert_eq!(
        rig.e.snapshots.len(),
        1,
        "no new snapshot of our own screenshot"
    );
    assert_eq!(rig.e.stages.len(), 3);
    assert!(rig.e.logged("taking over the pending restore"));
}

#[test]
fn quit_puts_the_clipboard_back_when_ours_is_still_there() {
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 0.5);
    let ticket = rig.r.quit(sent + 0.5, &mut rig.e);
    assert_eq!(ticket, Some(rig.e.restores[0].0));
    assert_eq!(rig.restored(), vec!["user"]);
    assert_eq!(rig.e.restores[0].2, RestoreGuard::OwnStaging);
    let reply = ClipReply {
        ticket: ticket.unwrap(),
        result: Ok(ClipDone::Restored { sequence: 9 }),
    };
    assert_eq!(rig.r.quit_reply(reply, sent + 0.6, &mut rig.e), None);

    // Not when the user copied something meanwhile.
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.e.facts.staged.as_mut().unwrap().overwritten_at = Some(sent + 0.3);
    assert_eq!(rig.r.quit(sent + 0.5, &mut rig.e), None);
    assert!(rig.e.restores.is_empty());
}

#[test]
fn injected_clicks_and_clicks_without_cards_do_nothing() {
    let mut rig = Rig::new(1);
    rig.mouse(
        MouseEvent::Down {
            button: Button::Left,
            x: 1,
            y: 1,
            time: 0.1,
            click_count: 2,
            injected: true,
        },
        0.1,
    );
    assert!(rig.e.lookups.is_empty());
    let mut rig = Rig::new(0);
    rig.double_click();
    assert!(rig.e.lookups.is_empty());
    assert!(rig.e.snapshots.is_empty());
}

#[test]
fn the_hover_hint_looks_after_a_rest_at_most_four_times_a_second() {
    let mut rig = Rig::new(1);
    rig.moved(0.0, 10, 10);
    rig.to(0.2);
    assert!(rig.e.lookups.is_empty());
    rig.to(0.23);
    assert_eq!(rig.e.lookups.len(), 1);
    assert!(!rig.e.lookups[0].2);
    rig.answer(0.25, field("notepad.exe"));
    assert_eq!(rig.s.hint(), Some(HINT_HOVER_PASTE));
    // Rest again quickly: the next lookup waits for the 0.25 s gap.
    rig.moved(0.26, 20, 20);
    rig.to(0.47);
    assert_eq!(rig.e.lookups.len(), 1);
    rig.to(0.49);
    assert_eq!(rig.e.lookups.len(), 2);
    rig.answer(0.5, Seen::Elevated { pid: 4 });
    assert_eq!(rig.s.hint(), Some(HINT_ELEVATED));
    // Moving on before the answer: the stale answer is dropped.
    rig.moved(0.8, 30, 30);
    rig.to(1.1);
    assert_eq!(rig.e.lookups.len(), 3);
    rig.moved(1.12, 40, 40);
    rig.answer(1.13, field("notepad.exe"));
    assert_eq!(rig.s.hint(), Some(HINT_ELEVATED));
    // Nothing held: no lookups.
    let mut rig = Rig::new(0);
    rig.moved(0.0, 10, 10);
    rig.to(2.0);
    assert!(rig.e.lookups.is_empty());
}

#[test]
fn hover_hints_follow_the_swift_rules() {
    assert_eq!(hover_hint(&field("x.exe")), Some(HINT_HOVER_PASTE));
    assert_eq!(hover_hint(&terminal()), Some(HINT_HOVER_PASTE));
    assert_eq!(hover_hint(&Seen::OwnProcess), None);
    assert_eq!(hover_hint(&Seen::Elevated { pid: 1 }), Some(HINT_ELEVATED));
    assert_eq!(hover_hint(&Seen::Spot(Box::new(nothing("x.exe")))), None);
    let mut t = target("explorer.exe", false);
    t.shell = Some(Shell::Desktop);
    let mut spot = Spot::new(t);
    spot.drop_spot = true;
    assert_eq!(
        hover_hint(&Seen::Spot(Box::new(spot))),
        Some(HINT_HOVER_SAVE)
    );
}

// MARK: Review fixes

/// Review item 1: A double-click on window chrome or an item means "maximise" / "open", never a paste.
#[test]
fn a_title_bar_or_an_item_double_click_is_ignored() {
    for role in ["AXToolbar", "AXRow", "AXRadioButton", "AXButton"] {
        let mut rig = Rig::new(1);
        rig.double_click();
        let mut spot = nothing("notepad.exe");
        spot.role = role.into();
        spot.activating = true;
        rig.answer(0.17, Seen::Spot(Box::new(spot)));
        rig.up(0.25);
        rig.to(3.0);
        assert!(
            rig.e.logged("window chrome or a control → Ignore"),
            "{role}"
        );
        assert_eq!(rig.e.lookups.len(), 1, "{role}: no re-check");
        assert!(rig.e.stages.is_empty() && rig.e.pressed.is_empty());
    }
}

/// Review item 1: The double-click opened a file from an Open dialog (or maximised the window): what is
/// under the point after the release is another window, even in the same process.
#[test]
fn a_window_switch_between_the_click_and_the_recheck_is_ignored() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, Seen::Spot(Box::new(nothing("editor.exe"))));
    rig.up(0.25);
    rig.to(0.36);
    let mut after = nothing("editor.exe");
    after.target.window = 0x20;
    after.editable_hit = true;
    after.editable = true;
    rig.answer(0.4, Seen::Spot(Box::new(after)));
    rig.to(3.0);
    assert!(rig.e.logged("is not the double-clicked 0x10 → ignore"));
    assert!(rig.e.stages.is_empty() && rig.e.pressed.is_empty());
    assert!(rig.e.checks.borrow().is_empty());
}

/// Review item 2: Double-click and keep holding (select by words, a touchpad double-tap-drag): no keys
/// mid-drag, the card stays, no hint.
#[test]
fn a_held_button_cancels_the_paste_quietly() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.to(3.0);
    assert!(rig.e.logged("the button is still down (a drag)"));
    assert!(rig.e.pressed.is_empty() && rig.e.stages.is_empty() && rig.e.sent == 0);
    assert!(rig.s.is_holding());
    assert_ne!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    assert!(!rig.r.busy());

    // The same while waiting to re-check after the release.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, Seen::Spot(Box::new(nothing("app.exe"))));
    rig.to(3.0);
    assert_eq!(rig.e.lookups.len(), 1);
    assert!(!rig.r.busy());
}

/// Review item 2: The hook never saw the release (over an elevated window): the real button state says
/// it is up, so the paste goes on.
#[test]
fn a_release_the_hook_missed_is_read_from_the_button_state() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.to(0.25);
    rig.e.button = false;
    rig.to(0.45);
    assert_eq!(rig.e.stages.len(), 1, "{:#?}", rig.e.log);
}

/// Review item 12: Deadlines that land exactly on `start + 1.0` must not re-arm in the past.
#[test]
fn deadlines_reached_exactly_do_not_spin() {
    for start in [0.1, 0.15, 0.2, 0.3, 0.7, 1.1, 2.3] {
        let mut rig = Rig::new(1);
        rig.to(start - 0.15);
        rig.down(start - 0.15, 1);
        rig.up(start - 0.07);
        rig.down(start, 2);
        rig.answer(start + 0.02, field("notepad.exe"));
        let ticket = *rig.e.snapshots.first().unwrap();
        rig.snapshot_reply(start + 0.03, ticket, None);
        // `to` panics when the router does not settle.
        rig.to(start + paste_policy_timeout());
        rig.to(start + 5.0);
        assert!(!rig.r.busy(), "{start}");
    }
}

fn paste_policy_timeout() -> f64 {
    igorio_core::paste_policy::MOUSE_UP_TIMEOUT
}

/// Review item 3: A failed first lookup is Ignore: no re-check, no keys later.
#[test]
fn a_failed_first_lookup_is_ignored() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, Seen::Failed("ElementFromPoint failed".into()));
    rig.up(0.25);
    rig.to(3.0);
    assert_eq!(rig.e.lookups.len(), 1);
    assert!(rig.e.stages.is_empty() && rig.e.pressed.is_empty());
    assert!(!rig.r.busy());
}

/// Review item 3: After a timeout, a focused field elsewhere is not enough: the field must be at the
/// click point.
#[test]
fn a_slow_app_needs_an_editable_element_at_the_point() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(
        0.4,
        Seen::TimedOut {
            target: target("RobloxStudioBeta.exe", false),
        },
    );
    rig.up(0.42);
    rig.to(0.6);
    let mut spot = nothing("RobloxStudioBeta.exe");
    spot.focused = Some(Focus {
        pid: 100,
        editable: true,
        frame: Some(Rect::new(450.0, 380.0, 200.0, 30.0)),
    });
    rig.answer(1.1, Seen::Spot(Box::new(spot)));
    // No delivery starts: no clipboard snapshot, no keys, no stage.
    assert!(rig.e.snapshots.is_empty(), "{:#?}", rig.e.log);
    assert!(!rig.r.busy());
    rig.to(3.0);
    assert!(rig.e.stages.is_empty() && rig.e.pressed.is_empty());
}

/// Review item 4: The stage reply comes after the router gave up: the screenshot that then lands is
/// still replaced by the user's clipboard.
#[test]
fn a_stage_reply_after_the_timeout_is_still_undone() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.4);
    assert_eq!(rig.e.stages.len(), 1);
    rig.to(0.39 + REPLY_TIMEOUT + 0.1);
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    assert!(rig.e.restores.is_empty());
    assert!(rig.r.restore_pending());
    rig.staged_reply(5.0);
    assert_eq!(rig.e.sent, 0, "no paste keys after giving up");
    assert_eq!(rig.restored(), vec!["user"]);
    assert_eq!(rig.e.restores[0].2, RestoreGuard::OwnStaging);
}

/// Review item 5: A stage that failed before touching the clipboard restores nothing over the user's
/// data.
#[test]
fn a_stage_error_before_emptying_restores_nothing() {
    for error in [
        ClipFail::Retry("clipboard held open".into()),
        ClipFail::Other("bad payload".into()),
    ] {
        let mut rig = Rig::new(1);
        rig.double_click();
        rig.answer(0.17, field("notepad.exe"));
        rig.remembered_arrives(0.18);
        rig.up(0.25);
        rig.to(0.4);
        let ticket = rig.e.stages[0].0;
        rig.reply(0.41, ticket, Err(error.clone()));
        rig.to(10.0);
        assert!(rig.e.restores.is_empty(), "{error:?}");
        assert!(!rig.r.restore_pending());
        assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    }
}

/// Review item 5: A stage that failed after emptying the clipboard: the user's clipboard goes back,
/// but only over the empty clipboard that stage left.
#[test]
fn a_stage_error_after_emptying_restores_over_exactly_that() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.4);
    let ticket = rig.e.stages[0].0;
    rig.reply(0.41, ticket, Err(ClipFail::Emptied { sequence: 12 }));
    assert_eq!(rig.restored(), vec!["user"]);
    assert_eq!(rig.e.restores[0].2, RestoreGuard::Sequence(12));
    assert_eq!(rig.e.sent, 0);
}

/// Review item 5: A failed stage during a taken-over restore keeps the earlier staging (still on the
/// clipboard) and its restore.
#[test]
fn a_stage_error_keeps_the_earlier_paste_restore() {
    let mut rig = Rig::new(2);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 0.2);
    rig.to(sent + 1.0);
    let first_stage = rig.e.stages[0].0;
    let ticket = rig.e.stages[1].0;
    rig.reply(rig.now, ticket, Err(ClipFail::Retry("locked".into())));
    // The first card's staging is still ours: the restore goes on with it.
    rig.e.facts.staged = Some(StagedFacts {
        ticket: first_stage,
        reads: 2,
        last_read_at: Some(sent + 0.02),
        ..StagedFacts::default()
    });
    rig.to(sent + 2.2);
    assert_eq!(rig.restored(), vec!["user"]);
}

/// Review item 8: Quit in each step of a delivery.
#[test]
fn quit_in_each_delivery_step() {
    // Before anything was staged: nothing to restore.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    assert_eq!(rig.r.quit(0.2, &mut rig.e), None);
    assert!(rig.e.restores.is_empty());

    // A stage in flight: wait for it, then restore.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.4);
    let stage = rig.e.stages[0].0;
    assert_eq!(rig.r.quit(0.4, &mut rig.e), Some(stage));
    assert!(rig.e.restores.is_empty());
    rig.e.facts.staged = Some(StagedFacts {
        ticket: stage,
        ..StagedFacts::default()
    });
    let next = rig.r.quit_reply(
        ClipReply {
            ticket: stage,
            result: Ok(ClipDone::Staged { sequence: 6 }),
        },
        0.45,
        &mut rig.e,
    );
    assert_eq!(next, Some(rig.e.restores[0].0));
    assert_eq!(rig.e.restores[0].2, RestoreGuard::OwnStaging);
    assert_eq!(rig.e.sent, 0);

    // A stage in flight that then fails after emptying: restore over the empty clipboard.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.4);
    let stage = rig.e.stages[0].0;
    rig.r.quit(0.4, &mut rig.e);
    rig.r.quit_reply(
        ClipReply {
            ticket: stage,
            result: Err(ClipFail::Emptied { sequence: 7 }),
        },
        0.45,
        &mut rig.e,
    );
    assert_eq!(rig.e.restores[0].2, RestoreGuard::Sequence(7));

    // A stage in flight that then fails without touching the clipboard: nothing to do.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.4);
    let stage = rig.e.stages[0].0;
    rig.r.quit(0.4, &mut rig.e);
    rig.r.quit_reply(
        ClipReply {
            ticket: stage,
            result: Err(ClipFail::Retry("locked".into())),
        },
        0.45,
        &mut rig.e,
    );
    assert!(rig.e.restores.is_empty(), "nothing was written");

    // While watching for reads, and with a restore already in flight.
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    let ticket = rig.r.quit(sent + 0.1, &mut rig.e);
    assert_eq!(ticket, Some(rig.e.restores[0].0));
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 2.1);
    let in_flight = rig.e.restores[0].0;
    assert_eq!(rig.r.quit(sent + 2.11, &mut rig.e), Some(in_flight));
    assert_eq!(rig.e.restores.len(), 1);
    // A locked clipboard at quit: try again within the caller's time.
    let again = rig.r.quit_reply(
        ClipReply {
            ticket: in_flight,
            result: Err(ClipFail::Retry("locked".into())),
        },
        sent + 2.2,
        &mut rig.e,
    );
    assert_eq!(again, Some(rig.e.restores[1].0));
}

/// Review item 9: A read by another process (a clipboard manager, the user pasting elsewhere) is not
/// the target taking the paste.
#[test]
fn reads_by_another_process_are_not_a_paste() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    let sent = paste_into_field(&mut rig);
    rig.to(sent + 0.02);
    {
        let staged = rig.e.facts.staged.as_mut().unwrap();
        staged.reads = 2;
        staged.last_read_at = Some(sent + 0.02);
        staged.last_reader = Some(4242);
    }
    rig.r.clipboard_changed(sent + 0.02, &mut rig.s, &mut rig.e);
    rig.to(sent + 1.7);
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    assert!(rig.s.card(id).is_some());
    assert!(rig.e.logged("not the target → not counted"));
    // A later read by the same other process is not a late paste either.
    {
        let staged = rig.e.facts.staged.as_mut().unwrap();
        staged.reads = 3;
        staged.last_reader = Some(4242);
    }
    rig.r.clipboard_changed(sent + 3.0, &mut rig.s, &mut rig.e);
    rig.to(sent + 3.1);
    assert!(rig.s.card(id).is_some());

    // The target's own process reading counts.
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    let sent = paste_into_field(&mut rig);
    rig.to(sent + 0.02);
    {
        let staged = rig.e.facts.staged.as_mut().unwrap();
        staged.reads = 2;
        staged.last_read_at = Some(sent + 0.02);
        staged.last_reader = Some(100);
    }
    rig.r.clipboard_changed(sent + 0.02, &mut rig.s, &mut rig.e);
    rig.to(sent + 0.3);
    assert!(rig.s.card(id).is_none());
}

/// Review item 10: WezTerm and Alacritty paste on Ctrl+Shift+V; Windows Terminal and conhost on Ctrl+V.
#[test]
fn each_terminal_gets_its_paste_keys() {
    let keys = |exe: &str| paste_keys_for(&target(exe, true));
    assert_eq!(keys("WindowsTerminal.exe"), PasteKeys::CtrlV);
    assert_eq!(keys("OpenConsole.exe"), PasteKeys::CtrlV);
    assert_eq!(keys("conhost.exe"), PasteKeys::CtrlV);
    assert_eq!(keys("wezterm-gui.exe"), PasteKeys::CtrlShiftV);
    assert_eq!(keys("ALACRITTY.EXE"), PasteKeys::CtrlShiftV);
    assert_eq!(
        paste_keys_for(&target("wezterm-gui.exe", false)),
        PasteKeys::CtrlV
    );

    let mut rig = Rig::new(1);
    rig.double_click();
    let mut spot = Spot::new(target("wezterm-gui.exe", true));
    spot.editable_hit = true;
    spot.editable = true;
    rig.answer(0.17, Seen::Spot(Box::new(spot)));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    let job = rig.e.saves[0];
    rig.r.saved(
        job,
        Ok(PathBuf::from(r"C:\p\a.png")),
        0.26,
        &mut rig.s,
        &mut rig.e,
    );
    rig.to(0.5);
    rig.staged_reply(0.5);
    assert_eq!(rig.e.paste_keys, vec![PasteKeys::CtrlShiftV]);
    rig.e.assert_aimed();
}

/// Review item 11: A plain first click takes no clipboard snapshot; the snapshot is taken once the
/// double-click is decided.
#[test]
fn a_first_click_takes_no_snapshot_outside_terminals() {
    let mut rig = Rig::new(1);
    rig.down(0.0, 1);
    rig.up(0.08);
    assert!(rig.e.snapshots.is_empty());
    rig.down(0.15, 2);
    assert!(rig.e.snapshots.is_empty());
    rig.answer(0.17, field("notepad.exe"));
    assert_eq!(rig.e.snapshots.len(), 1);
    // Not for an ignored double-click either.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, Seen::OwnProcess);
    assert!(rig.e.snapshots.is_empty());
}

/// The canvas rule is kept as in Swift, and the hover says so first.
#[test]
fn a_web_canvas_shows_the_paste_hint_on_hover() {
    let mut spot = nothing("chrome.exe");
    spot.canvas = CanvasFacts {
        canvas: true,
        known: false,
        document: false,
    };
    assert_eq!(
        hover_hint(&Seen::Spot(Box::new(spot.clone()))),
        Some(HINT_HOVER_PASTE)
    );
    let mut rig = Rig::new(1);
    rig.moved(0.0, AT.0, AT.1);
    rig.to(0.23);
    rig.answer(0.24, Seen::Spot(Box::new(spot)));
    assert_eq!(rig.s.hint(), Some(HINT_HOVER_PASTE));
}

#[test]
fn title_bar_and_items_are_activating_but_an_address_bar_is_not() {
    let title = el(UIA_TitleBarControlTypeId);
    let s = snapshot(target("notepad.exe", false), vec![title, window()], None);
    assert!(spot_from_snapshot(&s, AT).activating);
    // A text inside a list item.
    let s = snapshot(
        target("explorer.exe", false),
        vec![
            el(UIA_TextControlTypeId),
            el(UIA_ListItemControlTypeId),
            el(UIA_ListControlTypeId),
            window(),
        ],
        None,
    );
    assert!(spot_from_snapshot(&s, AT).activating);
    // Chrome's address bar sits in a toolbar but takes text.
    let omnibox = ElementInfo {
        value_pattern: true,
        value_read_only: Some(false),
        ..el(UIA_EditControlTypeId)
    };
    let s = snapshot(
        target("chrome.exe", false),
        vec![omnibox, el(UIA_ToolBarControlTypeId), window()],
        None,
    );
    let spot = spot_from_snapshot(&s, AT);
    assert!(spot.editable_hit && !spot.activating);
}

// MARK: Snapshot → facts

fn el(control: UIA_CONTROLTYPE_ID) -> ElementInfo {
    ElementInfo {
        control_type: control.0,
        enabled: true,
        framework: "Win32".into(),
        pid: 100,
        ..ElementInfo::default()
    }
}

fn snapshot(target: Target, chain: Vec<ElementInfo>, focused: Option<ElementInfo>) -> Snapshot {
    Snapshot {
        target,
        hit: UiaNode::from_chain(chain).unwrap(),
        focused: focused.map(|f| UiaNode::from_chain(vec![f]).unwrap()),
        calls: 1,
        elapsed: std::time::Duration::ZERO,
        truncated: false,
    }
}

fn window() -> ElementInfo {
    ElementInfo {
        top_level: true,
        ..el(UIA_WindowControlTypeId)
    }
}

#[test]
fn notepad_text_is_an_editable_field() {
    let edit = ElementInfo {
        text_pattern: true,
        focusable: true,
        ..el(UIA_DocumentControlTypeId)
    };
    let s = snapshot(target("Notepad.exe", false), vec![edit, window()], None);
    let spot = spot_from_snapshot(&s, AT);
    assert!(spot.editable_hit && spot.editable);
    assert_eq!(spot.surface(), Surface::Field);
}

#[test]
fn a_password_field_is_not_editable() {
    let edit = ElementInfo {
        password: true,
        value_pattern: true,
        value_read_only: Some(false),
        ..el(UIA_EditControlTypeId)
    };
    let s = snapshot(target("app.exe", false), vec![edit, window()], None);
    let spot = spot_from_snapshot(&s, AT);
    assert!(!spot.editable_hit && !spot.editable);
}

#[test]
fn a_chrome_search_box_is_a_field_in_a_web_page() {
    let input = ElementInfo {
        value_pattern: true,
        value_read_only: Some(false),
        aria_role: "searchbox".into(),
        framework: "Chrome".into(),
        ..el(UIA_EditControlTypeId)
    };
    let page = ElementInfo {
        framework: "Chrome".into(),
        url: Some("https://www.google.com/".into()),
        ..el(UIA_DocumentControlTypeId)
    };
    let s = snapshot(target("chrome.exe", false), vec![input, page], None);
    let spot = spot_from_snapshot(&s, AT);
    assert_eq!(spot.role, "AXSearchField");
    assert!(spot.editable_hit);
    assert!(spot.chromium);
}

#[test]
fn windows_terminal_text_takes_a_paste_but_its_tabs_do_not() {
    let text = ElementInfo {
        text_pattern: true,
        focusable: true,
        framework: "XAML".into(),
        ..el(UIA_TextControlTypeId)
    };
    let s = snapshot(
        target("WindowsTerminal.exe", true),
        vec![text, el(UIA_PaneControlTypeId), window()],
        None,
    );
    let spot = spot_from_snapshot(&s, AT);
    assert!(spot.editable_hit && spot.editable);
    assert_eq!(spot.surface(), Surface::Terminal);
    let tab = el(UIA_TabItemControlTypeId);
    let s = snapshot(
        target("WindowsTerminal.exe", true),
        vec![tab, window()],
        None,
    );
    assert!(!spot_from_snapshot(&s, AT).editable_hit);
}

#[test]
fn vs_code_terminal_is_a_web_terminal() {
    let rows = ElementInfo {
        class_name: "xterm-rows".into(),
        framework: "Chrome".into(),
        ..el(UIA_GroupControlTypeId)
    };
    let page = ElementInfo {
        framework: "Chrome".into(),
        ..el(UIA_DocumentControlTypeId)
    };
    let s = snapshot(target("Code.exe", false), vec![rows, page], None);
    let spot = spot_from_snapshot(&s, AT);
    assert!(spot.web_terminal);
    assert_eq!(spot.surface(), Surface::Terminal);
}

#[test]
fn google_docs_is_a_page_drawn_document() {
    let canvas = ElementInfo {
        framework: "Chrome".into(),
        frame: Some(Rect::new(0.0, 100.0, 1000.0, 800.0)),
        ..el(UIA_ImageControlTypeId)
    };
    let page = ElementInfo {
        framework: "Chrome".into(),
        url: Some("https://docs.google.com/document/d/abc/edit".into()),
        frame: Some(Rect::new(0.0, 100.0, 1000.0, 800.0)),
        ..el(UIA_DocumentControlTypeId)
    };
    let s = snapshot(target("chrome.exe", false), vec![canvas, page], None);
    let spot = spot_from_snapshot(&s, AT);
    assert!(spot.canvas.canvas && spot.canvas.known && spot.canvas.document);
    assert_eq!(spot.surface(), Surface::Document);
}

#[test]
fn explorer_background_is_a_drop_spot_and_an_item_is_not() {
    let mut t = target("explorer.exe", false);
    t.shell = Some(Shell::Explorer);
    let list = el(UIA_ListControlTypeId);
    let s = snapshot(t.clone(), vec![list.clone(), window()], None);
    assert!(spot_from_snapshot(&s, AT).drop_spot);
    let item = el(UIA_ListItemControlTypeId);
    let s = snapshot(t, vec![item, list, window()], None);
    assert!(!spot_from_snapshot(&s, AT).drop_spot);
}

#[test]
fn the_focused_element_is_read_from_the_snapshot() {
    let focus = ElementInfo {
        value_pattern: true,
        value_read_only: Some(false),
        frame: Some(Rect::new(1.0, 2.0, 300.0, 30.0)),
        ..el(UIA_EditControlTypeId)
    };
    let s = snapshot(
        target("app.exe", false),
        vec![el(UIA_PaneControlTypeId), window()],
        Some(focus),
    );
    let spot = spot_from_snapshot(&s, AT);
    let f = spot.focused.unwrap();
    assert!(f.editable);
    assert_eq!(f.pid, 100);
    assert_eq!(f.frame, Some(Rect::new(1.0, 2.0, 300.0, 30.0)));
}

#[test]
fn outcomes_become_facts() {
    assert_eq!(seen_from_outcome(Outcome::OwnProcess, AT), Seen::OwnProcess);
    assert_eq!(
        seen_from_outcome(Outcome::Elevated { pid: 3 }, AT),
        Seen::Elevated { pid: 3 }
    );
    let t = target("a.exe", true);
    assert_eq!(
        seen_from_outcome(Outcome::TimedOut { target: t.clone() }, AT),
        Seen::TimedOut { target: t }
    );
}
