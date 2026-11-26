//! Scripted tests for `igorio_app::drop_router::Router`: a fake environment stands in for
//! UI Automation, the clipboard thread, the PNG saver and the keyboard, and each test drives
//! the router with events and times as they would arrive.

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
    restores: Vec<(u64, Snap, Option<u32>)>,
    saves: Vec<u64>,
    /// The target is in front.
    front: bool,
    /// Results for Ctrl+V, in order; `Done` once empty.
    pastes: VecDeque<Sent>,
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
    fn clip_restore(&mut self, snap: &Snap, if_sequence: Option<u32>) -> Result<u64, String> {
        let id = self.id();
        self.restores.push((id, snap, if_sequence));
        Ok(id)
    }
    fn save_png(&mut self, _card: &HeldCard) -> u64 {
        let id = self.id();
        self.saves.push(id);
        id
    }
    fn foreground_is(&self, _window: isize, _pid: u32) -> bool {
        self.front
    }
    fn press(&mut self, key: Key) -> Sent {
        self.pressed.push(key);
        Sent::Done
    }
    fn paste_shortcut(&mut self) -> Sent {
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

    /// The remembered snapshot arrives (taken at the first mouse-down).
    fn remembered_arrives(&mut self, t: f64) {
        let ticket = *self.e.snapshots.first().expect("remembered");
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
    assert_eq!(rig.e.restores[0].2, Some(6));
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
    // It answers this time: the focused field is under the click.
    let mut spot = nothing("RobloxStudioBeta.exe");
    spot.focused = Some(Focus {
        pid: 100,
        editable: true,
        frame: Some(Rect::new(450.0, 380.0, 200.0, 30.0)),
    });
    rig.answer(1.1, Seen::Spot(Box::new(spot)));
    rig.remembered_arrives(1.11);
    rig.to(1.4);
    assert_eq!(rig.e.stages.len(), 1);
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
    // No answer at all (a replaced request): the router stops waiting.
    rig.to(0.15 + LOOKUP_DEADLINE + LOOKUP_GRACE + 0.01);
    assert!(rig.e.logged("no answer"));
    rig.to(3.0);
    assert!(!rig.r.busy());
    assert!(rig.e.stages.is_empty());
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
fn a_refused_restore_is_tried_again_then_abandoned() {
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 2.1);
    for n in 0..RESTORE_TRIES {
        let ticket = rig.e.restores.last().unwrap().0;
        rig.reply(rig.now + 0.01, ticket, Err(ClipFail::Changed));
        rig.to(rig.now + 0.2);
        let expected = (n + 2).min(RESTORE_TRIES) as usize;
        assert_eq!(rig.e.restores.len(), expected);
    }
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
    rig.double_click();
    rig.remembered_arrives(0.1);
    rig.e.facts.sequence = 8;
    rig.e.facts.last_change = Some(0.12);
    rig.answer(0.17, field("notepad.exe"));
    assert_eq!(rig.e.snapshots.len(), 1);
    assert!(rig.e.logged("using the copy from the first click"));

    // A remembered copy older than 2 s whose sequence moved on is not used.
    let mut rig = Rig::new(1);
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
    assert!(ticket.is_some());
    assert_eq!(rig.restored(), vec!["user"]);

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
