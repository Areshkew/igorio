//! Scripted tests for `igorio_app::drop_router::Router`: a fake environment stands in for
//! UI Automation, the clipboard thread, the PNG saver and the keyboard, and each test drives
//! the router with events and times as they would arrive.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use igorio_app::clipboard::Expect;
use igorio_app::clipboard::Payload;
use igorio_app::drop_router::*;
use igorio_app::explorer::{FolderOutcome, FolderRequest};
use igorio_app::hook::{Button, MouseEvent};
use igorio_app::input::{Batch, Key, Modifier, Sent};
use igorio_app::state::FairyState;
use igorio_app::uia::{
    ElementInfo, Outcome, Probe, Shell, Snapshot, Target, UiaNode, ValueSignature,
};
use igorio_core::geom::{CardId, Rect};
use igorio_kit::{HeldCard, Image};
use windows::Win32::UI::Accessibility::*;

// MARK: Fake environment

type Snap = &'static str;
/// A lookup the router asked for: (id, point, want_focus, deadline, probe).
type LookupRec = (u64, (i32, i32), bool, f64, bool);

#[derive(Default)]
struct Fake {
    next: u64,
    /// (id, point, want_focus, deadline, probe)
    lookups: Vec<LookupRec>,
    /// Probe lookups already answered by the rig.
    answered: std::collections::HashSet<u64>,
    probes_answered: u32,
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
    /// What the guarded sends were given.
    guards: Vec<SendGuard>,
    /// The focused control the fake target reports; a later batch must see the same.
    focus_now: isize,
    /// The staging still owns the clipboard (B3's last check).
    owns: bool,
    /// The point is in the client area (silent terminals).
    client: bool,
    /// The user's key presses.
    keys: u64,
    /// The rollback copy the last fresh stage handed over, and the stages' expectations.
    rollback_snap: Option<Snap>,
    expects: Vec<(Expect, bool)>,
    abandons: u32,
    pressed: Vec<Key>,
    sent: u32,
    log: Vec<String>,
    /// Folder lookups: (id, request, deadline).
    folders: Vec<(u64, FolderRequest, f64)>,
    /// Folder saves: (job, card, folder, movie).
    writes: Vec<(u64, CardId, PathBuf, Option<PathBuf>)>,
    /// The saver refuses new jobs (queue full).
    saver_busy: bool,
    reveals: Vec<(isize, PathBuf)>,
    /// One of our note windows covers this rectangle.
    note: Option<Rect>,
    /// Cards given to a note: (point, ids top first).
    gifts: Vec<((i32, i32), Vec<CardId>)>,
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

    fn lookup(&mut self, point: (i32, i32), want_focus: bool, probe: bool, deadline: f64) -> u64 {
        let id = self.id();
        self.lookups.push((id, point, want_focus, deadline, probe));
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
    fn clip_stage(
        &mut self,
        payload: Payload,
        expect: Expect,
        rollback: Option<&Snap>,
    ) -> Result<u64, String> {
        let id = self.id();
        self.stages.push((id, payload));
        self.expects.push((expect, rollback.is_some()));
        if let Some(snap) = rollback {
            self.rollback_snap = Some(*snap);
        }
        Ok(id)
    }
    fn clip_restage(&mut self, expect: Expect) -> Result<u64, String> {
        let id = self.id();
        self.restages.push(id);
        self.expects.push((expect, false));
        Ok(id)
    }
    fn clip_rollback(&mut self, guard: RestoreGuard) -> Result<u64, String> {
        let id = self.id();
        let snap = self.rollback_snap.unwrap_or("none");
        self.restores.push((id, snap, guard));
        Ok(id)
    }
    fn clip_abandon(&mut self) {
        self.abandons += 1;
    }
    fn save_png(&mut self, _card: &HeldCard) -> u64 {
        let id = self.id();
        self.saves.push(id);
        id
    }
    fn folder_lookup(&mut self, request: FolderRequest, deadline: f64) -> u64 {
        let id = self.id();
        self.folders.push((id, request, deadline));
        id
    }
    fn save_to_folder(&mut self, card: &HeldCard, folder: &Path) -> Result<u64, String> {
        if self.saver_busy {
            return Err("the saver is busy".into());
        }
        let id = self.id();
        self.writes
            .push((id, card.id, folder.to_path_buf(), card.movie.clone()));
        Ok(id)
    }
    fn reveal(&mut self, window: isize, file: &Path) {
        self.reveals.push((window, file.to_path_buf()));
    }
    fn foreground_is(&self, window: isize, pid: u32) -> bool {
        self.checks.borrow_mut().push((window, pid));
        self.front && window == self.fg_window && (pid == 0 || pid == self.fg_pid)
    }
    fn point_in_client(&self, _window: isize, _point: (i32, i32)) -> bool {
        self.client
    }
    fn key_presses(&self) -> u64 {
        self.keys
    }
    fn primary_down(&self) -> bool {
        self.button
    }
    fn terminal_at(&self, _point: (i32, i32)) -> bool {
        self.terminal
    }
    fn process_name(&self, pid: u32) -> Option<String> {
        (pid == 4242).then(|| "copyq.exe".to_string())
    }
    fn send(&mut self, batch: Batch, guard: SendGuard) -> (Sent, Option<isize>) {
        self.guards.push(guard);
        self.checks.borrow_mut().push((guard.window, guard.pid));
        // The order of `input::send_guarded`.
        if guard.stage.is_some() && !self.owns {
            return (Sent::Refused, None);
        }
        if !self.foreground_is(guard.window, guard.pid) {
            return (Sent::NotInFront, None);
        }
        if guard.focus.is_some_and(|f| f != self.focus_now) {
            return (Sent::FocusMoved, Some(self.focus_now));
        }
        match batch {
            Batch::Key(key) => {
                self.pressed.push(key);
                (Sent::Done, Some(self.focus_now))
            }
            Batch::Paste | Batch::PasteWithShift => {
                self.paste_keys.push(if batch == Batch::Paste {
                    PasteKeys::CtrlV
                } else {
                    PasteKeys::CtrlShiftV
                });
                let result = self.pastes.pop_front().unwrap_or(Sent::Done);
                if matches!(result, Sent::Done | Sent::Incomplete { .. }) {
                    self.sent += 1;
                }
                (result, Some(self.focus_now))
            }
        }
    }
    fn log(&mut self, now: f64, line: &str) {
        self.log.push(format!("{now:.3} {line}"));
    }
    fn note_at(&self, (x, y): (i32, i32)) -> bool {
        self.note
            .is_some_and(|r| r.contains(igorio_core::geom::Point::new(x as f64, y as f64)))
    }
    fn give_to_note(&mut self, point: (i32, i32), cards: Vec<HeldCard>) {
        self.gifts
            .push((point, cards.iter().map(|card| card.id).collect()));
    }
}

// MARK: Rig

const AT: (i32, i32) = (500, 400);

struct Rig {
    r: Router<Snap>,
    s: FairyState,
    e: Fake,
    now: f64,
    /// Answers for probe lookups: the first of a delivery (the focus check) and the later
    /// ones (after the paste keys). None leaves them unanswered.
    focus: Option<Seen>,
    after: Option<Seen>,
    /// When set, every probe (the focus check of each card and the probes after its keys)
    /// sees a multi-line composer holding this many images.
    images: Option<u32>,
    /// File chips in that composer.
    chips: u32,
}

/// The focused composer with `images` images and `chips` file chips, as a probe sees it.
fn composer_with(images: u32, chips: u32) -> Seen {
    let mut spot = nothing("T3 Code (Beta).exe");
    spot.point = Some(AT);
    spot.focused = Some(Focus {
        pid: 100,
        editable: true,
        frame: Some(Rect::new(450.0, 380.0, 200.0, 40.0)),
        single_line: false,
        xterm: false,
    });
    spot.probe = Some(Probe {
        value: Some(ValueSignature::of("hello")),
        images: Some(images),
        // The composer's own send button, plus every image and file chip.
        attachments: Some(1 + images + chips),
    });
    Seen::Spot(Box::new(spot))
}

/// The focused element a good paste sees: an editable field of pid 100 under `AT`.
fn focused_field(single_line: bool, value: &str) -> Seen {
    let mut spot = nothing("notepad.exe");
    spot.point = Some(AT);
    spot.focused = Some(Focus {
        pid: 100,
        editable: true,
        frame: Some(Rect::new(450.0, 380.0, 200.0, 40.0)),
        single_line,
        xterm: false,
    });
    spot.probe = Some(Probe {
        value: Some(ValueSignature::of(value)),
        images: Some(0),
        attachments: Some(0),
    });
    Seen::Spot(Box::new(spot))
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
            focus_now: 0x77,
            owns: true,
            client: true,
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
            focus: Some(focused_field(false, "")),
            after: Some(focused_field(false, "")),
            images: None,
            chips: 0,
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
        self.auto_answer();
        for _ in 0..100_000 {
            match self.r.next_deadline() {
                Some(d) if d <= t => {
                    self.now = d.max(self.now);
                    self.r.tick(self.now, &mut self.s, &mut self.e);
                    self.auto_answer();
                }
                _ => {
                    self.now = t.max(self.now);
                    return;
                }
            }
        }
        panic!("the router does not settle before {t}");
    }

    /// Answers probe lookups (the focus check, then the probes after the paste keys) at
    /// once with `focus` / `after`.
    fn auto_answer(&mut self) {
        loop {
            let next = self
                .e
                .lookups
                .iter()
                .find(|l| l.4 && !self.e.answered.contains(&l.0))
                .map(|l| l.0);
            let Some(id) = next else {
                return;
            };
            self.e.answered.insert(id);
            let first = self.e.probes_answered == 0;
            self.e.probes_answered += 1;
            let seen = if let Some(images) = self.images {
                Some(composer_with(images, self.chips))
            } else if first {
                self.focus.clone()
            } else {
                self.after.clone()
            };
            if let Some(seen) = seen {
                self.r.seen(id, seen, self.now, &mut self.s, &mut self.e);
            }
        }
    }

    fn mouse(&mut self, event: MouseEvent, t: f64) {
        self.to(t);
        self.r.mouse(event, &mut self.s, &mut self.e);
        self.auto_answer();
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
                window: 0x10,
                touch: false,
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

    /// Answers the newest lookup that is not a probe.
    fn answer(&mut self, t: f64, seen: Seen) {
        self.to(t);
        let id = self
            .e
            .lookups
            .iter()
            .rev()
            .find(|l| !l.4)
            .expect("a lookup")
            .0;
        self.r.seen(id, seen, t, &mut self.s, &mut self.e);
        self.auto_answer();
    }

    fn reply(&mut self, t: f64, ticket: u64, result: Result<ClipDone<Snap>, ClipFail>) {
        self.to(t);
        self.r
            .clip_reply(ClipReply { ticket, result }, t, &mut self.s, &mut self.e);
        self.auto_answer();
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

    /// The target (pid 100) has read `total` formats by `t`.
    fn read(&mut self, t: f64, total: u32) {
        self.read_by(t, total, Some(100));
    }

    /// `total` formats rendered by `t`, the new ones read by `reader`.
    fn read_by(&mut self, t: f64, total: u32, reader: Option<u32>) {
        self.to(t);
        let staged = self.e.facts.staged.as_mut().expect("staged");
        let had = staged.renders.iter().flatten().count() as u32;
        for _ in had..total {
            if let Some(slot) = staged.renders.iter_mut().find(|r| r.is_none()) {
                *slot = Some((reader, t));
            }
        }
        staged.reads = total;
        staged.last_read_at = Some(t);
        staged.last_reader = reader;
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

    /// A double-click whose second mouse-down is at `t`.
    fn double_click_at(&mut self, t: f64) {
        self.down(t - 0.15, 1);
        self.up(t - 0.07);
        self.down(t, 2);
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
        ..Focus::default()
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
        ..Focus::default()
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
    let (_, point, focus, deadline, _) = rig.e.lookups[1];
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
fn a_silent_console_is_pasted_into_only_in_its_text_area() {
    let console = || {
        let mut t = target("cmd.exe", true);
        t.class_name = "ConsoleWindowClass".into();
        Seen::TimedOut { target: t }
    };
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.4, console());
    assert!(rig.e.logged("→ Paste"));
    assert_eq!(rig.e.saves.len(), 1);

    // Its title bar or scroll bar (outside the client area): no path typed blind.
    let mut rig = Rig::new(1);
    rig.e.client = false;
    rig.double_click();
    rig.answer(0.4, console());
    assert!(rig.e.saves.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_SLOW));
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
fn explorer_background_starts_a_folder_save_and_an_item_is_ignored() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, shell_spot(Shell::Explorer, true));
    assert!(rig.e.logged("SaveToFinder"));
    assert_eq!(rig.e.folders.len(), 1);
    assert!(rig.s.is_holding());
    // An item (not the background) is the user opening a file: ignored.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, shell_spot(Shell::Explorer, false));
    assert!(rig.e.logged("→ Ignore"));
    assert!(rig.e.folders.is_empty());
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
fn an_overwrite_before_any_read_writes_the_screenshot_again_in_a_terminal() {
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
    let sent = 0.5;
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
    assert_eq!(rig.s.hint(), Some(HINT_PASTED_PATH));
    assert!(
        matches!(rig.e.expects[1].0, Expect::Sequence(_)),
        "the rewrite is guarded"
    );
    rig.to(sent + 2.2);
    assert_eq!(rig.restored(), vec!["user"], "{:#?}", rig.e.log);
}

/// B4: outside terminals a write after our stage is the user copying: never rewrite, never
/// restore.
#[test]
fn an_overwrite_in_a_field_is_the_user_copying() {
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.overwrite(sent + 0.05);
    assert!(rig.e.restages.is_empty());
    assert_eq!(rig.e.abandons, 1);
    rig.to(sent + 10.0);
    assert!(rig.e.restores.is_empty());
    assert!(!rig.r.restore_pending());
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
fn another_press_during_a_delivery_stops_it_quietly() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.down(0.3, 1);
    rig.to(3.0);
    assert!(rig.e.logged("another press → the delivery stops"));
    assert!(rig.e.pressed.is_empty() && rig.e.stages.is_empty() && rig.e.sent == 0);
    assert!(rig.s.card(id).is_some());
    assert_ne!(rig.s.hint(), Some(HINT_PASTE_FAILED));
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
            window: 0x10,
            touch: false,
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
            window: 0x10,
            touch: false,
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
        ..Focus::default()
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
    // B5: a rollback is queued behind the stage at once; the clipboard thread runs it after
    // the stage, whenever that happens.
    assert_eq!(rig.restored(), vec!["user"]);
    assert_eq!(rig.e.restores[0].2, RestoreGuard::OwnStaging);
    rig.staged_reply(5.0);
    assert_eq!(rig.e.sent, 0, "no paste keys after giving up");
    assert_eq!(rig.e.restores.len(), 1, "no second rollback");
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
    // B5: a rollback is queued behind the stage at once.
    let rollback = rig.r.quit(0.4, &mut rig.e);
    assert_eq!(rollback, Some(rig.e.restores[0].0));
    assert_eq!(rig.e.restores[0].2, RestoreGuard::OwnStaging);
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
    assert_eq!(next, rollback, "still waiting for the rollback");
    assert_eq!(rig.e.restores.len(), 1);
    assert_eq!(rig.e.sent, 0);

    // A stage in flight that then fails after emptying: restore over the empty clipboard.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.4);
    let stage = rig.e.stages[0].0;
    let rollback = rig.r.quit(0.4, &mut rig.e);
    let next = rig.r.quit_reply(
        ClipReply {
            ticket: stage,
            result: Err(ClipFail::Emptied { sequence: 7 }),
        },
        0.45,
        &mut rig.e,
    );
    // The queued rollback covers it: the clipboard thread counts the empty clipboard its
    // own failed stage left as its own.
    assert_eq!(next, rollback);
    assert_eq!(rig.e.restores.len(), 1);

    // A stage in flight that then fails without touching the clipboard: nothing to do.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.4);
    let stage = rig.e.stages[0].0;
    let rollback = rig.r.quit(0.4, &mut rig.e).unwrap();
    rig.r.quit_reply(
        ClipReply {
            ticket: stage,
            result: Err(ClipFail::Retry("locked".into())),
        },
        0.45,
        &mut rig.e,
    );
    // The clipboard thread finds nothing of ours there and writes nothing.
    let done = rig.r.quit_reply(
        ClipReply {
            ticket: rollback,
            result: Err(ClipFail::Other("nothing staged".into())),
        },
        0.46,
        &mut rig.e,
    );
    assert_eq!(done, None);

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
    rig.read_by(sent + 0.02, 2, Some(4242));
    // Its read may hide the target's: the probe looks, sees no change, and by the probe
    // timeout the paste is not confirmed: the card stays and the user is asked to check.
    rig.to(sent + 1.7);
    assert!(rig.s.card(id).is_some());
    rig.to(sent + 3.1);
    assert_eq!(rig.s.hint(), Some(HINT_UNCERTAIN));
    assert!(rig.s.card(id).is_some());
    assert!(rig.e.logged("(copyq.exe), not the target → not counted"));
    // A later read by the same other process is not a late paste either.
    rig.read_by(sent + 4.0, 3, Some(4242));
    rig.to(sent + 4.1);
    assert!(rig.s.card(id).is_some());

    // The target's own process reading counts.
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    let sent = paste_into_field(&mut rig);
    rig.read_by(sent + 0.02, 2, Some(100));
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

// MARK: Round 3: probe, focus, web terminals, Scintilla, readers

/// Up to the paste keys with a single-line field (probe verifies).
fn paste_into_single_line(rig: &mut Rig, after: &str) -> f64 {
    rig.focus = Some(focused_field(true, "query"));
    rig.after = Some(focused_field(true, after));
    paste_into_field(rig)
}

/// Item 1a: Chromium reads the clipboard on Ctrl+V in any text input and drops an image it
/// cannot use. A single-line field that was read but did not change refused the image.
#[test]
fn a_single_line_field_that_does_not_change_refused_the_image() {
    let mut rig = Rig::new(2);
    let ids = rig.ids();
    let sent = paste_into_single_line(&mut rig, "query");
    assert!(rig.e.logged("probe taken (verify)"));
    rig.read(sent + 0.02, 1);
    rig.to(sent + 1.0);
    assert!(
        rig.s.card(ids[0]).is_some(),
        "a read alone is not acceptance"
    );
    rig.to(sent + 3.1);
    assert_eq!(rig.s.hint(), Some(HINT_NO_IMAGES));
    assert!(rig.s.card(ids[0]).is_some() && rig.s.card(ids[1]).is_some());
    assert_eq!(rig.e.stages.len(), 1, "the rest of the pile is not tried");
    // The probes went out every 0.2 s, each with focus and a probe, at the click point.
    let probes: Vec<_> = rig.e.lookups.iter().filter(|l| l.4).collect();
    assert!(probes.len() >= 10, "{} probes", probes.len());
    assert!(probes.iter().all(|l| l.1 == AT && l.2));
    // The clipboard is still put back.
    rig.to(sent + 6.1);
    assert_eq!(rig.restored(), vec!["user"]);
}

#[test]
fn a_single_line_field_that_changes_took_the_image() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    let sent = paste_into_single_line(&mut rig, "query[image]");
    rig.read(sent + 0.02, 1);
    rig.to(sent + 0.5);
    assert!(rig.s.card(id).is_none());
    assert!(rig.e.logged("probe: the field changed"));
}

/// A probe that never answers is no evidence either way: the reads decide.
#[test]
fn a_probe_that_never_answers_falls_back_to_the_reads() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.focus = Some(focused_field(true, "query"));
    rig.after = None;
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 1);
    rig.to(sent + 2.9);
    assert!(rig.s.card(id).is_some());
    rig.to(sent + 3.1);
    assert!(rig.s.card(id).is_none(), "{:#?}", rig.e.log);
}

/// The rule table of `verdict` (the outcome rule).
#[test]
fn the_outcome_rule() {
    use Verdict::*;
    let reads = |target, probable, other| Reads {
        target,
        probable,
        other,
    };
    let view = |verify, answered, changed| {
        Some(ProbeView {
            verify,
            answered,
            can_tell: true,
            changed,
        })
    };
    // No probe: reads by the target, or probably the target, decide; others never count.
    assert_eq!(verdict(0.2, reads(1, 0, 0), false, None, false), Accepted);
    assert_eq!(verdict(0.2, reads(0, 1, 0), false, None, false), Accepted);
    assert_eq!(verdict(1.7, reads(0, 0, 3), false, None, false), Uncertain);
    assert_eq!(verdict(1.7, reads(0, 0, 0), false, None, false), Failed);
    // A change is acceptance.
    assert_eq!(
        verdict(0.3, reads(0, 0, 1), false, view(false, true, true), false),
        Accepted
    );
    // Single-line, read by the target, unchanged at 3 s: refused; a read alone is not enough.
    assert_eq!(
        verdict(0.5, reads(1, 0, 0), false, view(true, true, false), false),
        Pending
    );
    assert_eq!(
        verdict(3.0, reads(1, 0, 0), false, view(true, true, false), false),
        Rejected
    );
    assert_eq!(
        verdict(3.0, reads(0, 1, 0), false, view(true, true, false), false),
        Rejected
    );
    // ... read only by others: uncertain; not read at all: failed.
    assert_eq!(
        verdict(3.0, reads(0, 0, 2), false, view(true, true, false), false),
        Uncertain
    );
    assert_eq!(
        verdict(1.7, reads(0, 0, 0), false, view(true, true, false), false),
        Failed
    );
    // Single-line with no probe answer by 3 s: the reads decide.
    assert_eq!(
        verdict(2.9, reads(0, 1, 0), false, view(true, false, false), false),
        Pending
    );
    assert_eq!(
        verdict(3.0, reads(0, 1, 0), false, view(true, false, false), false),
        Accepted
    );
    // A probe that cannot tell (no value, no image count) leaves it to the reads at once.
    let blind = Some(ProbeView {
        verify: true,
        answered: true,
        can_tell: false,
        changed: false,
    });
    assert_eq!(verdict(0.2, reads(0, 1, 0), false, blind, false), Accepted);
    // Multi-line, read only by others: the probe may confirm until 3 s, then uncertain.
    assert_eq!(
        verdict(2.0, reads(0, 0, 2), false, view(false, true, false), false),
        Pending
    );
    assert_eq!(
        verdict(3.0, reads(0, 0, 2), false, view(false, true, false), false),
        Uncertain
    );
    // Stopped by the user after the keys: never refused or failed, uncertain.
    assert_eq!(
        verdict(3.0, reads(1, 0, 0), false, view(true, true, false), true),
        Uncertain
    );
    assert_eq!(verdict(1.7, reads(0, 0, 0), false, None, true), Uncertain);
    assert_eq!(verdict(0.2, reads(1, 0, 0), false, None, true), Accepted);
}

/// Item 4: a clipboard manager read first and the target's read is invisible; the probe
/// sees the image arrive in a multi-line composer.
#[test]
fn a_paste_hidden_by_a_clipboard_manager_is_confirmed_by_the_probe() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.after = Some(focused_field(false, "hello\u{fffc}"));
    let sent = paste_into_field(&mut rig);
    rig.read_by(sent + 0.025, 1, Some(4242));
    rig.to(sent + 0.6);
    assert!(rig.s.card(id).is_none(), "{:#?}", rig.e.log);
    assert!(rig.e.logged("others 1, probe changed → accepted"));
}

/// Item 1c / (c): the double-click landed on text (a hidden input under it was the hit),
/// and the keyboard focus is not in a visible field under the click: no key goes out.
#[test]
fn no_keys_when_the_focus_is_not_in_the_target_field() {
    for focus in [
        None,
        // A hidden one-pixel input at the caret (Monaco, a read-only code view).
        Some(Rect::new(500.0, 400.0, 1.0, 1.0)),
        // A field elsewhere in the window.
        Some(Rect::new(0.0, 900.0, 300.0, 40.0)),
    ] {
        let mut rig = Rig::new(1);
        let mut spot = nothing("T3 Code (Beta).exe");
        spot.point = Some(AT);
        spot.focused = focus.map(|frame| Focus {
            pid: 100,
            editable: true,
            frame: Some(frame),
            ..Focus::default()
        });
        rig.focus = Some(Seen::Spot(Box::new(spot)));
        rig.double_click();
        rig.answer(0.17, field("T3 Code (Beta).exe"));
        rig.remembered_arrives(0.18);
        rig.up(0.25);
        rig.to(3.0);
        assert!(rig.e.pressed.is_empty() && rig.e.stages.is_empty() && rig.e.sent == 0);
        assert_eq!(rig.s.hint(), Some(HINT_NOT_FOCUSED), "{focus:?}");
        assert!(rig.s.is_holding());
        assert!(rig.e.logged("focus is not in the target field"));
    }
}

/// A focus lookup that does not answer is no evidence: no paste.
#[test]
fn no_keys_when_the_focus_check_times_out() {
    let mut rig = Rig::new(1);
    rig.focus = Some(Seen::TimedOut {
        target: target("slow.exe", false),
    });
    rig.double_click();
    rig.answer(0.17, field("slow.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.pressed.is_empty() && rig.e.stages.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_SLOW));

    // Never answered at all: given up after the deadline, same result.
    let mut rig = Rig::new(1);
    rig.focus = None;
    rig.double_click();
    rig.answer(0.17, field("slow.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.pressed.is_empty() && rig.e.stages.is_empty());
    assert!(!rig.r.busy());
}

/// (b): an editable found only by the nearby search (not the hit or its ancestors) goes
/// through the re-check, and then the focus must be in it.
#[test]
fn a_nearby_editable_needs_the_focus_in_it() {
    let mut rig = Rig::new(1);
    let mut label = nothing("chat.exe");
    label.role = "AXStaticText".into();
    label.editable = true; // found by the nearby search
    label.field_role = Some("AXTextArea".into());
    rig.focus = Some(Seen::Spot(Box::new(nothing("chat.exe"))));
    rig.double_click();
    rig.answer(0.17, Seen::Spot(Box::new(label.clone())));
    assert!(rig.e.logged("CheckFocusAfterMouseUp"));
    rig.up(0.25);
    rig.to(0.36);
    rig.answer(0.4, Seen::Spot(Box::new(label)));
    rig.remembered_arrives(0.41);
    rig.to(3.0);
    assert!(rig.e.pressed.is_empty() && rig.e.stages.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_NOT_FOCUSED));
}

/// (a): an answer about another point (a hover lookup for the composer) cannot decide a
/// double-click elsewhere.
#[test]
fn an_answer_about_another_point_is_not_taken() {
    let mut rig = Rig::new(1);
    rig.double_click();
    let mut composer = match field("T3 Code (Beta).exe") {
        Seen::Spot(spot) => spot,
        _ => unreachable!(),
    };
    composer.point = Some((1340, 681));
    rig.answer(0.17, Seen::Spot(composer));
    assert!(
        rig.e
            .logged("answer about Some((1340, 681)), asked about (500, 400)")
    );
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.stages.is_empty() && rig.e.snapshots.is_empty());
}

/// The hover said "double-click to paste" over the composer; the double-click lands on
/// text: its own lookup decides, and a late hover answer changes nothing.
#[test]
fn the_hover_hint_never_decides_a_double_click() {
    let mut rig = Rig::new(1);
    rig.moved(0.0, 1340, 681);
    rig.to(0.23);
    let hover = rig.e.lookups[0].0;
    rig.double_click_at(0.3);
    let click = rig.e.lookups.last().unwrap().0;
    assert_ne!(hover, click);
    // The hover's answer (the composer) arrives first, under its own id.
    let mut composer = match field("T3 Code (Beta).exe") {
        Seen::Spot(spot) => spot,
        _ => unreachable!(),
    };
    composer.point = Some((1340, 681));
    rig.r
        .seen(hover, Seen::Spot(composer), 0.47, &mut rig.s, &mut rig.e);
    assert!(
        rig.r.busy(),
        "still waiting for the double-click's own answer"
    );
    let mut text = nothing("T3 Code (Beta).exe");
    text.role = "AXStaticText".into();
    rig.r.seen(
        click,
        Seen::Spot(Box::new(text)),
        0.48,
        &mut rig.s,
        &mut rig.e,
    );
    assert!(rig.e.logged("→ CheckFocusAfterMouseUp"));
    rig.up(0.5);
    rig.to(3.0);
    assert!(rig.e.stages.is_empty());
}

/// Item 2: the debug line says what the hit and the focus are, without contents.
#[test]
fn the_double_click_line_describes_hit_and_surface() {
    let mut rig = Rig::new(1);
    rig.double_click();
    let mut spot = nothing("T3 Code (Beta).exe");
    spot.editable_hit = true;
    spot.editable = true;
    spot.field_role = Some("AXTextField".into());
    spot.hit_desc = Desc {
        control: "Edit",
        class: "inputarea monaco-mouse-cursor-text".into(),
        name: "Editor content".into(),
        aria: "textbox".into(),
        role: "AXTextField".into(),
    };
    rig.answer(0.17, Seen::Spot(Box::new(spot)));
    let line = rig
        .e
        .log
        .iter()
        .find(|l| l.contains("→ Paste"))
        .expect("the decision line")
        .clone();
    for part in [
        "hit Edit \"Editor content\" class=\"inputarea monaco-mouse-cursor-text\" aria=textbox role AXTextField",
        "single-line",
        "surface=Field",
    ] {
        assert!(line.contains(part), "{part} not in {line}");
    }
}

/// Item 3: Notepad++ takes text only: the image is not read, the card stays with the
/// honest hint.
#[test]
fn notepad_plus_plus_gets_the_honest_refusal() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.double_click();
    let mut scintilla = nothing("notepad++.exe");
    scintilla.role = "AXGroup".into();
    scintilla.editable_hit = true;
    scintilla.editable = true;
    scintilla.field_role = Some("AXGroup".into());
    rig.answer(0.17, Seen::Spot(Box::new(scintilla)));
    assert!(rig.e.logged("→ Paste"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.45);
    rig.staged_reply(0.45);
    rig.to(2.2);
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    assert!(rig.s.card(id).is_some());
}

/// Item 1b: a double-click in an integrated xterm.js terminal pastes the path, without the
/// Right arrow and without a focus check.
#[test]
fn an_integrated_terminal_gets_the_path() {
    let mut rig = Rig::new(1);
    rig.double_click();
    let mut spot = nothing("T3 Code (Beta).exe");
    spot.web_terminal = true;
    spot.editable = true;
    rig.answer(0.17, Seen::Spot(Box::new(spot)));
    rig.up(0.25);
    rig.to(0.36);
    let mut again = nothing("T3 Code (Beta).exe");
    again.web_terminal = true;
    again.editable = true;
    rig.answer(0.4, Seen::Spot(Box::new(again)));
    assert_eq!(rig.e.saves.len(), 1, "{:#?}", rig.e.log);
    rig.remembered_arrives(0.41);
    let job = rig.e.saves[0];
    rig.r.saved(
        job,
        Ok(PathBuf::from(r"C:\p\a.png")),
        0.42,
        &mut rig.s,
        &mut rig.e,
    );
    rig.to(0.8);
    assert!(matches!(&rig.e.stages[0].1, Payload::Text(t) if t == r"C:\p\a.png "));
    assert!(rig.e.pressed.is_empty());
    assert!(
        !rig.e.lookups.iter().any(|l| l.4),
        "no focus check for a terminal"
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
        point: AT,
        probe: None,
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
    let list = ElementInfo {
        class_name: "UIItemsView".into(),
        ..el(UIA_ListControlTypeId)
    };
    let s = snapshot(t.clone(), vec![list.clone(), window()], None);
    assert!(spot_from_snapshot(&s, AT).drop_spot);
    // A list that is not the items view (the role rule alone said yes before).
    let s = snapshot(t.clone(), vec![el(UIA_ListControlTypeId), window()], None);
    assert!(!spot_from_snapshot(&s, AT).drop_spot);
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

fn web(control: UIA_CONTROLTYPE_ID, class: &str) -> ElementInfo {
    ElementInfo {
        framework: "Chrome".into(),
        class_name: class.into(),
        ..el(control)
    }
}

fn page() -> ElementInfo {
    web(UIA_DocumentControlTypeId, "")
}

#[test]
fn an_xterm_canvas_with_the_helper_input_focused_is_a_terminal() {
    // The canvas's terminal ancestors are not in the tree UI Automation shows.
    let canvas = web(UIA_ImageControlTypeId, "");
    let helper = ElementInfo {
        value_pattern: true,
        value_read_only: Some(false),
        ..web(UIA_EditControlTypeId, "xterm-helper-textarea")
    };
    let s = snapshot(
        target("Code.exe", false),
        vec![canvas, web(UIA_GroupControlTypeId, "panel"), page()],
        Some(helper),
    );
    let spot = spot_from_snapshot(&s, AT);
    assert!(spot.web_terminal, "{spot:?}");
    assert_eq!(spot.surface(), Surface::Terminal);
    assert!(spot.focused.unwrap().xterm);
}

#[test]
fn the_xterm_helper_input_itself_is_a_terminal() {
    let helper = ElementInfo {
        value_pattern: true,
        value_read_only: Some(false),
        ..web(UIA_EditControlTypeId, "xterm-helper-textarea")
    };
    let s = snapshot(
        target("T3 Code (Beta).exe", false),
        vec![helper, web(UIA_GroupControlTypeId, "xterm-helpers"), page()],
        None,
    );
    let spot = spot_from_snapshot(&s, AT);
    assert!(spot.web_terminal);
    assert_eq!(spot.surface(), Surface::Terminal);
}

#[test]
fn a_hit_deep_inside_xterm_screen_is_a_terminal() {
    let mut chain = vec![web(UIA_TextControlTypeId, "")];
    for _ in 0..9 {
        chain.push(web(UIA_GroupControlTypeId, ""));
    }
    chain.push(web(UIA_GroupControlTypeId, "xterm-screen"));
    chain.push(page());
    let s = snapshot(target("Code.exe", false), chain, None);
    assert!(spot_from_snapshot(&s, AT).web_terminal);
}

#[test]
fn monaco_is_an_editor_not_a_terminal() {
    let input = ElementInfo {
        value_pattern: true,
        value_read_only: Some(false),
        ..web(UIA_EditControlTypeId, "inputarea monaco-mouse-cursor-text")
    };
    let s = snapshot(
        target("Code.exe", false),
        vec![input, web(UIA_GroupControlTypeId, "monaco-editor"), page()],
        None,
    );
    let spot = spot_from_snapshot(&s, AT);
    assert!(!spot.web_terminal);
    assert!(spot.editable_hit);
    assert_eq!(spot.field_role.as_deref(), Some("AXTextField"));
    assert_eq!(lines(spot.field_role.as_deref()), "single-line");
}

#[test]
fn a_scintilla_editor_is_an_editable_field() {
    let scintilla = ElementInfo {
        class_name: "Scintilla".into(),
        hwnd: 0x4605ca,
        focusable: true,
        ..el(UIA_PaneControlTypeId)
    };
    let s = snapshot(
        target("notepad++.exe", false),
        vec![scintilla, window()],
        None,
    );
    let spot = spot_from_snapshot(&s, AT);
    assert!(spot.editable_hit && spot.editable);
    assert_eq!(spot.role, "AXGroup");
    assert_eq!(spot.surface(), Surface::Field);
    assert!(spot.describe().contains("class=\"Scintilla\""));
}

#[test]
fn the_focus_check_rules() {
    let mut spot = nothing("app.exe");
    let field = Rect::new(450.0, 380.0, 200.0, 40.0);
    spot.focused = Some(Focus {
        pid: 100,
        editable: true,
        frame: Some(field),
        ..Focus::default()
    });
    let check = |spot: &Spot, surface| focus_evidence(spot, (100, 100), 0x10, AT, surface);
    assert!(check(&spot, Surface::Field));
    // Another window, another process, not editable, not under the click: no.
    let mut other = spot.clone();
    other.target.window = 0x20;
    assert!(!check(&other, Surface::Field));
    let mut other = spot.clone();
    other.focused.as_mut().unwrap().pid = 7;
    assert!(!check(&other, Surface::Field));
    let mut other = spot.clone();
    other.focused.as_mut().unwrap().editable = false;
    assert!(!check(&other, Surface::Field));
    let mut tiny = spot.clone();
    tiny.focused.as_mut().unwrap().frame = Some(Rect::new(500.0, 400.0, 1.0, 1.0));
    assert!(!check(&tiny, Surface::Field));
    // A page-drawn document types into a hidden input: focus in the target is enough.
    assert!(check(&tiny, Surface::Document));
}

// MARK: Round 4: keys to the wrong place, the user's clipboard, sending

/// A1(i): the user starts typing after the double-click: nothing more goes out.
#[test]
fn a_key_press_stops_a_delivery_before_its_next_batch() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.e.keys += 1;
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.pressed.is_empty() && rig.e.stages.is_empty() && rig.e.sent == 0);
    assert!(rig.s.card(id).is_some());
    assert!(rig.e.logged("the user typed"));
}

/// A1(ii): the focused control of the target thread must stay the one of the first batch.
#[test]
fn the_focused_control_must_not_change_between_batches() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    assert_eq!(rig.e.pressed, vec![Key::Right]);
    assert_eq!(
        rig.e.guards[0].focus, None,
        "the first batch records the focus"
    );
    // The user tabs to another control before Ctrl+V.
    rig.e.focus_now = 0x88;
    rig.staged_reply(0.41);
    assert_eq!(rig.e.guards[1].focus, Some(0x77));
    assert_eq!(rig.e.sent, 0);
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
    assert_eq!(
        rig.restored(),
        vec!["user"],
        "the staging is rolled back at once"
    );
}

/// A1(iii): the window handle alone is not enough: the process must match too.
#[test]
fn the_target_process_must_be_in_front_too() {
    let mut rig = Rig::new(1);
    rig.e.fg_pid = 999;
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.pressed.is_empty() && rig.e.stages.is_empty());
    assert!(rig.e.logged("the target is not in front"));
}

/// A2: two presses on different windows (a dialog closed by the first) are no double-click,
/// and the answer must be about the window that was pressed.
#[test]
fn presses_and_answer_must_be_on_one_window() {
    let mut rig = Rig::new(1);
    rig.down(0.0, 1);
    rig.up(0.08);
    rig.to(0.15);
    rig.e.button = true;
    rig.r.mouse(
        MouseEvent::Down {
            button: Button::Left,
            x: AT.0,
            y: AT.1,
            time: 0.15,
            click_count: 2,
            injected: false,
            window: 0x20,
            touch: false,
        },
        &mut rig.s,
        &mut rig.e,
    );
    assert!(rig.e.lookups.is_empty());
    assert!(rig.e.logged("presses on windows 0x10 and 0x20 → Ignore"));

    let mut rig = Rig::new(1);
    rig.double_click();
    let mut exposed = match field("notepad.exe") {
        Seen::Spot(spot) => spot,
        _ => unreachable!(),
    };
    exposed.target.window = 0x20;
    rig.answer(0.17, Seen::Spot(exposed));
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.stages.is_empty() && rig.e.snapshots.is_empty());
    assert!(rig.e.logged("the presses were on 0x10 → Ignore"));
}

/// A3: a canvas first, a button after the release: no paste.
#[test]
fn a_recheck_that_finds_a_control_is_ignored() {
    let mut rig = Rig::new(1);
    rig.double_click();
    let mut canvas = nothing("Figma.exe");
    canvas.canvas = CanvasFacts {
        canvas: true,
        known: true,
        document: false,
    };
    rig.answer(0.17, Seen::Spot(Box::new(canvas)));
    rig.up(0.25);
    rig.to(0.36);
    let mut button = nothing("Figma.exe");
    button.role = "AXButton".into();
    button.activating = true;
    button.canvas = CanvasFacts {
        canvas: true,
        known: true,
        document: false,
    };
    rig.answer(0.4, Seen::Spot(Box::new(button)));
    rig.to(3.0);
    assert!(rig.e.stages.is_empty() && rig.e.snapshots.is_empty());
    assert!(rig.e.logged("after mouse-up:"));
    assert!(rig.e.logged("is window chrome or a control → ignore"));
}

/// A4: Windows Terminal draws its tabs in the client area: silent, it is not pasted into.
#[test]
fn a_silent_windows_terminal_is_not_pasted_into() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(
        0.4,
        Seen::TimedOut {
            target: target("WindowsTerminal.exe", true),
        },
    );
    assert!(rig.e.saves.is_empty() && rig.e.snapshots.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_SLOW));
}

/// A5: a pen or touch double-tap promoted to mouse presses starts nothing.
#[test]
fn a_touch_double_tap_starts_nothing() {
    let mut rig = Rig::new(1);
    for (t, count) in [(0.0, 1), (0.15, 2)] {
        rig.mouse(
            MouseEvent::Down {
                button: Button::Left,
                x: AT.0,
                y: AT.1,
                time: t,
                click_count: count,
                injected: false,
                window: 0x10,
                touch: true,
            },
            t,
        );
    }
    assert!(rig.e.lookups.is_empty() && rig.e.snapshots.is_empty());
}

/// B1: the user copies between the snapshot and the stage: nothing is written over it.
#[test]
fn a_copy_between_the_snapshot_and_the_stage_is_kept() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    // The first stage expects exactly the saved clipboard and hands over the rollback copy.
    assert_eq!(rig.e.expects[0], (Expect::Sequence(5), true));
    let ticket = rig.e.stages[0].0;
    rig.reply(0.41, ticket, Err(ClipFail::Changed));
    rig.to(10.0);
    assert_eq!(rig.e.sent, 0);
    assert!(rig.e.restores.is_empty());
    assert!(rig.s.card(id).is_some());
    assert_ne!(rig.s.hint(), Some(HINT_PASTE_FAILED), "quietly");
}

/// B1: a later card expects our own staging still there.
#[test]
fn later_cards_stage_only_over_our_own_staging() {
    let mut rig = Rig::new(2);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 1.0);
    assert_eq!(rig.e.stages.len(), 2);
    assert_eq!(rig.e.expects[1], (Expect::OwnStaging, false));
}

/// B3: someone wrote the clipboard after our stage: no Ctrl+V (it would paste their data).
#[test]
fn no_paste_keys_when_the_staging_lost_the_clipboard() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.e.owns = false;
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.staged_reply(0.41);
    assert_eq!(rig.e.sent, 0);
    assert_eq!(rig.e.guards.last().unwrap().stage, Some(rig.e.stages[0].0));
    assert_eq!(rig.e.abandons, 1);
    rig.to(10.0);
    assert!(rig.e.restores.is_empty());
    assert!(rig.s.card(id).is_some());
}

/// B5: no new gesture while an earlier stage has not answered.
#[test]
fn no_new_gesture_while_a_stage_is_unanswered() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.to(0.39 + REPLY_TIMEOUT + 0.1);
    let lookups = rig.e.lookups.len();
    rig.double_click_at(4.0);
    assert_eq!(rig.e.lookups.len(), lookups);
    assert!(rig.e.logged("an earlier stage has not answered → Ignore"));
}

/// B5: unanswered requests are polled for a bounded time, then left to the clipboard thread.
#[test]
fn unanswered_requests_stop_being_polled() {
    // A rollback that never answers.
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 60.0);
    assert!(!rig.r.restore_pending());
    assert_eq!(rig.r.next_deadline(), None);
    assert!(rig.e.logged("abandoned"));

    // A terminal rewrite that never answers: guarding stops, the restore still runs.
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
    rig.overwrite(0.55);
    assert_eq!(rig.e.restages.len(), 1);
    rig.to(0.55 + REPLY_TIMEOUT + 0.2);
    assert!(rig.e.logged("the rewrite never answered"));
}

/// B6: a partial restore is tried again over itself.
#[test]
fn a_partial_restore_is_tried_again_over_itself() {
    let mut rig = Rig::new(1);
    let sent = paste_into_field(&mut rig);
    rig.read(sent + 0.02, 2);
    rig.to(sent + 2.1);
    let ticket = rig.e.restores[0].0;
    // The clipboard thread emptied the clipboard: our staging facts are gone.
    rig.e.facts.staged = None;
    rig.reply(sent + 2.15, ticket, Err(ClipFail::Partial));
    rig.to(sent + 2.8);
    assert_eq!(rig.e.restores.len(), 2);
    assert_eq!(rig.e.restores[1].2, RestoreGuard::OwnStaging);
}

/// C1: the card is shaken off after the stage went out: no Ctrl+V.
#[test]
fn a_card_shaken_off_after_the_stage_gets_no_paste_keys() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.s.dissolve(0.405);
    rig.staged_reply(0.41);
    assert_eq!(rig.e.sent, 0);
    assert!(rig.e.logged("the card left the pile → stopped"));
    assert_eq!(rig.restored(), vec!["user"]);
}

/// C3: a send that took nothing is not a paste, even with a read before it.
#[test]
fn a_failed_send_is_not_a_paste() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.e.pastes = VecDeque::from([Sent::Failed]);
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.staged_reply(0.41);
    rig.read(0.42, 1);
    rig.to(3.0);
    assert!(rig.s.card(id).is_some());
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
}

/// C4: only renders by the identified target after the send count.
#[test]
fn only_target_reads_after_the_send_count() {
    // A read before the send (a clipboard manager on the stage).
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    let ticket = rig.e.stages[0].0;
    rig.e.facts.staged = Some(StagedFacts {
        ticket,
        reads: 1,
        renders: {
            let mut r = [None; igorio_app::clipboard::RENDER_LOG];
            r[0] = Some((Some(100), 0.405));
            r
        },
        ..StagedFacts::default()
    });
    rig.reply(0.41, ticket, Ok(ClipDone::Staged { sequence: 6 }));
    rig.to(3.5);
    assert!(rig.s.card(id).is_some(), "{:#?}", rig.e.log);

    // A windowless reader that was already reading before the send (something polling the
    // clipboard), in an app that is not Chromium-based: its reads are not the target's.
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.after = None;
    rig.double_click();
    rig.answer(0.17, field("notepad.exe"));
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    let ticket = rig.e.stages[0].0;
    let mut renders = [None; igorio_app::clipboard::RENDER_LOG];
    renders[0] = Some((None, 0.405));
    rig.e.facts.staged = Some(StagedFacts {
        ticket,
        reads: 1,
        renders,
        ..StagedFacts::default()
    });
    rig.reply(0.41, ticket, Ok(ClipDone::Staged { sequence: 6 }));
    rig.read_by(0.43, 2, None);
    rig.to(3.5);
    assert!(rig.s.card(id).is_some(), "{:#?}", rig.e.log);
    assert_eq!(rig.s.hint(), Some(HINT_UNCERTAIN));

    // A windowless read while the target is not in front: not counted.
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.after = None;
    let sent = paste_into_field(&mut rig);
    rig.e.front = false;
    rig.read_by(sent + 0.02, 1, None);
    rig.to(sent + 3.5);
    assert!(rig.s.card(id).is_some());
}

#[test]
fn aria_cells_options_and_tabs_are_activating() {
    for aria in ["gridcell", "option", "tab", "treeitem", "columnheader"] {
        let cell = ElementInfo {
            aria_role: aria.into(),
            framework: "Chrome".into(),
            ..el(UIA_GroupControlTypeId)
        };
        let page = ElementInfo {
            framework: "Chrome".into(),
            ..el(UIA_DocumentControlTypeId)
        };
        let s = snapshot(target("chrome.exe", false), vec![cell, page], None);
        assert!(spot_from_snapshot(&s, AT).activating, "{aria}");
    }
}

// MARK: Round 5: piles in rich composers, windowless readers, uncertain outcomes

/// The composer from the owner's log: T3 Code (Electron), a ProseMirror contenteditable.
fn composer() -> Seen {
    let mut spot = nothing("T3 Code (Beta).exe");
    spot.role = "AXTextArea".into();
    spot.editable_hit = true;
    spot.editable = true;
    spot.chromium = true;
    spot.field_role = Some("AXTextArea".into());
    Seen::Spot(Box::new(spot))
}

/// Double-click on the composer and run up to the first card's Ctrl+V.
fn paste_pile_into_composer(rig: &mut Rig) -> f64 {
    rig.double_click();
    rig.answer(0.17, composer());
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.staged_reply(0.41);
    assert_eq!(rig.e.sent, 1, "{:#?}", rig.e.log);
    0.41
}

/// The reads of the owner's log after each Ctrl+V: CopyQ first, then a reader without a
/// window (the Electron app itself).
fn copyq_then_windowless(rig: &mut Rig, sent: f64, total_before: u32) {
    rig.read_by(sent + 0.025, total_before + 1, Some(4242));
    rig.read_by(sent + 0.05, total_before + 2, None);
}

/// Item 2: the owner's log. Both cards of the pile leave; the windowless read is logged as
/// "probably the target".
#[test]
fn a_pile_of_two_into_the_composer_is_delivered() {
    let mut rig = Rig::new(2);
    let ids = rig.ids();
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    rig.images = Some(1);
    copyq_then_windowless(&mut rig, sent, 0);
    rig.to(sent + 0.3);
    assert!(rig.s.card(ids[0]).is_none(), "{:#?}", rig.e.log);
    // Card 2: its own focus check and "before" fingerprint, then its keys.
    rig.to(sent + 0.45 + 0.4);
    assert_eq!(rig.e.stages.len(), 2, "{:#?}", rig.e.log);
    let second = rig.now;
    rig.staged_reply(second);
    assert_eq!(rig.e.sent, 2);
    rig.images = Some(2);
    // The second staging starts a new render list.
    copyq_then_windowless(&mut rig, second, 0);
    rig.to(second + 0.3);
    assert!(rig.s.card(ids[1]).is_none(), "{:#?}", rig.e.log);
    assert_eq!(rig.s.hint(), Some("pasted 2"));
    assert!(rig.e.logged("probably the target"));
    // Two focus checks, one per card.
    assert_eq!(
        rig.e
            .log
            .iter()
            .filter(|l| l.contains("checking the keyboard focus"))
            .count(),
        2
    );
}

/// Item 1: only the probe proves these pastes (the target's own read is hidden): the second
/// and the third image in the same field are each a change.
#[test]
fn a_second_and_third_image_in_one_field_are_each_a_change() {
    let mut rig = Rig::new(3);
    let ids = rig.ids();
    rig.images = Some(0);
    let mut sent = paste_pile_into_composer(&mut rig);
    for (n, id) in ids.iter().enumerate() {
        if n > 0 {
            rig.to(sent + 0.45 + 0.4);
            assert_eq!(rig.e.stages.len(), n + 1, "{:#?}", rig.e.log);
            sent = rig.now;
            rig.staged_reply(sent);
        }
        // The image lands; only CopyQ is seen reading.
        rig.images = Some(n as u32 + 1);
        rig.read_by(sent + 0.025, 1, Some(4242));
        rig.to(sent + 0.5);
        assert!(rig.s.card(*id).is_none(), "card {n}: {:#?}", rig.e.log);
    }
    assert_eq!(
        rig.e
            .log
            .iter()
            .filter(|l| l.contains("probe: the field changed"))
            .count(),
        3
    );
}

/// Item 1: a field that already holds 64 images: the 65th is still a change (the count is
/// exact).
#[test]
fn a_field_with_64_images_still_sees_the_next_one() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.images = Some(64);
    let sent = paste_pile_into_composer(&mut rig);
    rig.images = Some(65);
    rig.read_by(sent + 0.025, 1, Some(4242));
    rig.to(sent + 0.5);
    assert!(rig.s.card(id).is_none(), "{:#?}", rig.e.log);
}

/// Item 2: no probe change, but the windowless read after the send in a Chromium app is
/// probably the target: accepted, and logged as such.
#[test]
fn a_windowless_read_after_the_send_is_probably_the_target() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    copyq_then_windowless(&mut rig, sent, 0);
    rig.to(sent + 0.3);
    assert!(rig.s.card(id).is_none(), "{:#?}", rig.e.log);
    assert!(rig.e.logged("probably the target 1"));
}

/// Item 3: an uncertain card keeps its card, says "check the paste" and stops the pile.
#[test]
fn an_uncertain_card_stops_the_pile() {
    let mut rig = Rig::new(2);
    let ids = rig.ids();
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    rig.read_by(sent + 0.025, 1, Some(4242));
    rig.to(sent + 3.2);
    assert_eq!(rig.s.hint(), Some(HINT_UNCERTAIN));
    assert!(rig.s.card(ids[0]).is_some() && rig.s.card(ids[1]).is_some());
    rig.to(sent + 6.0);
    assert_eq!(rig.e.stages.len(), 1, "the pile stops");
}

/// Item 5: the user's own press stops a delivery after its keys went out: the card ends
/// uncertain, never "this field doesn't take images".
#[test]
fn a_stopped_delivery_never_reports_a_refusal() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    let sent = paste_into_single_line(&mut rig, "query");
    rig.read(sent + 0.02, 1);
    rig.down(sent + 0.5, 1);
    rig.up(sent + 0.55);
    rig.to(sent + 3.2);
    assert_eq!(rig.s.hint(), Some(HINT_UNCERTAIN), "{:#?}", rig.e.log);
    assert!(rig.s.card(id).is_some());
    assert!(!rig.e.logged("this field doesn't take images"));
}

/// Item 4: the composer of the log is multi-line; a plain input stays single-line.
#[test]
fn a_prosemirror_composer_is_multi_line_and_an_input_single_line() {
    let page = || ElementInfo {
        framework: "Chrome".into(),
        ..el(UIA_DocumentControlTypeId)
    };
    let composer = ElementInfo {
        class_name: "tiptap ProseMirror composer-tiptap".into(),
        name: "Message".into(),
        aria_role: "textbox".into(),
        framework: "Chrome".into(),
        value_pattern: true,
        value_read_only: Some(false),
        text_edit_pattern: true,
        ..el(UIA_EditControlTypeId)
    };
    let s = snapshot(
        target("T3 Code (Beta).exe", false),
        vec![composer, page()],
        None,
    );
    let spot = spot_from_snapshot(&s, AT);
    assert_eq!(spot.field_role.as_deref(), Some("AXTextArea"));
    assert_eq!(lines(spot.field_role.as_deref()), "multi-line");
    assert!(
        spot.describe().contains("multi-line"),
        "{}",
        spot.describe()
    );

    let input = ElementInfo {
        class_name: "search-input".into(),
        aria_role: "textbox".into(),
        framework: "Chrome".into(),
        value_pattern: true,
        value_read_only: Some(false),
        ..el(UIA_EditControlTypeId)
    };
    let s = snapshot(target("chrome.exe", false), vec![input, page()], None);
    let spot = spot_from_snapshot(&s, AT);
    assert_eq!(lines(spot.field_role.as_deref()), "single-line");
}

// MARK: Saving into a folder (Explorer and the desktop)

const PICTURES: &str = r"C:\Users\me\Pictures";

/// A shell spot as the router gets it: the folder view's background when `drop_spot`.
fn shell_spot(shell: Shell, drop_spot: bool) -> Seen {
    let mut t = target("explorer.exe", false);
    t.shell = Some(shell);
    t.class_name = match shell {
        Shell::Explorer => "CabinetWClass".into(),
        Shell::Desktop => "Progman".into(),
    };
    let mut spot = Spot::new(t);
    spot.role = "AXList".into();
    spot.drop_spot = drop_spot;
    Seen::Spot(Box::new(spot))
}

fn png(folder: &str, name: &str) -> PathBuf {
    Path::new(folder).join(name)
}

impl Rig {
    /// Double-click on the background of a folder view, then the release.
    fn save_double_click(&mut self, shell: Shell) {
        self.double_click();
        self.answer(0.17, shell_spot(shell, true));
        self.up(0.25);
    }

    /// Explorer answers the newest folder lookup.
    fn folder_answer(&mut self, t: f64, outcome: FolderOutcome) {
        self.to(t);
        let id = self.e.folders.last().expect("a folder lookup").0;
        self.r
            .folder_answered(id, outcome, t, &mut self.s, &mut self.e);
    }

    /// The saver finishes the newest write.
    fn written(&mut self, t: f64, result: Result<PathBuf, SaveFail>) {
        self.to(t);
        let job = self.e.writes.last().expect("a write").0;
        self.r
            .folder_saved(job, result, t, &mut self.s, &mut self.e);
    }

    fn in_pictures(&mut self, t: f64) {
        self.folder_answer(t, FolderOutcome::Folder(PathBuf::from(PICTURES)));
    }
}

fn fail(problem: SaveProblem) -> SaveFail {
    SaveFail {
        problem,
        why: "os error".into(),
    }
}

#[test]
fn a_double_click_on_the_folder_background_saves_the_card_and_reveals_it() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.save_double_click(Shell::Explorer);
    assert_eq!(
        rig.e.folders,
        vec![(2, FolderRequest::Window(0x10), FOLDER_DEADLINE)]
    );
    assert!(rig.r.busy());
    rig.in_pictures(0.3);
    assert_eq!(rig.e.writes.len(), 1);
    assert_eq!(rig.e.writes[0].1, id);
    assert_eq!(rig.e.writes[0].2, PathBuf::from(PICTURES));
    assert_eq!(rig.e.writes[0].3, None);
    // The card leaves only once the file exists.
    assert!(rig.s.card(id).is_some());
    let file = png(PICTURES, "Screenshot 1970-01-01 000000.png");
    rig.written(0.4, Ok(file.clone()));
    assert!(rig.s.card(id).is_none());
    assert_eq!(rig.s.hint(), Some("saved to Pictures"));
    assert_eq!(rig.e.reveals, vec![(0x10, file)]);
    assert!(!rig.r.busy());
    // Nothing touched the clipboard or the keyboard.
    assert!(rig.e.stages.is_empty() && rig.e.pressed.is_empty() && rig.e.sent == 0);
    for line in [
        "drop-spot=yes",
        "→ SaveToFinder",
        "asking Explorer for the folder of Window(16) (request 2, 0.50s)",
        r"folder (request 2, 130 ms): C:\Users\me\Pictures",
        "(1/1): writing Screenshot ",
        r"saved C:\Users\me\Pictures\Screenshot 1970-01-01 000000.png",
        "delivered 1 card(s) → saved to Pictures",
        "reveal C:",
        "save over",
    ] {
        assert!(rig.e.logged(line), "{line}: {:#?}", rig.e.log);
    }
}

#[test]
fn a_double_click_on_the_empty_desktop_saves_into_the_desktop_folder() {
    let mut rig = Rig::new(1);
    rig.save_double_click(Shell::Desktop);
    assert_eq!(rig.e.folders[0].1, FolderRequest::Desktop);
    let desktop = r"C:\Users\me\OneDrive\Desktop";
    rig.folder_answer(0.3, FolderOutcome::Folder(PathBuf::from(desktop)));
    assert_eq!(rig.e.writes[0].2, PathBuf::from(desktop));
    let file = png(desktop, "Screenshot.png");
    rig.written(0.4, Ok(file.clone()));
    assert_eq!(rig.s.hint(), Some("saved to Desktop"));
    assert!(!rig.s.is_holding());
    assert_eq!(rig.e.reveals, vec![(0x10, file)]);
}

/// Swift `saveToFinder`: `let pile = state.deliverable` (top first), each saved in turn,
/// then one `deliver(ids: saved…, hint: "saved N to <folder>")`.
#[test]
fn a_pile_is_saved_top_first_one_file_at_a_time_and_leaves_together() {
    let mut rig = Rig::new(3);
    let ids = rig.ids();
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    let mut files = Vec::new();
    for (k, id) in ids.iter().enumerate() {
        // One write at a time, in pile order.
        assert_eq!(rig.e.writes.len(), k + 1);
        assert_eq!(rig.e.writes[k].1, *id);
        assert!(rig.s.card(*id).is_some());
        let file = png(PICTURES, &format!("Screenshot ({}).png", k + 1));
        files.push(file.clone());
        rig.written(0.4 + k as f64 * 0.1, Ok(file));
    }
    assert!(ids.iter().all(|id| rig.s.card(*id).is_none()));
    assert_eq!(rig.s.hint(), Some("saved 3 to Pictures"));
    // The last file written is the one selected.
    assert_eq!(rig.e.reveals, vec![(0x10, files[2].clone())]);
}

#[test]
fn a_recording_is_copied_as_its_movie_file() {
    let mut rig = Rig::with_movie();
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    assert_eq!(
        rig.e.writes[0].3,
        Some(PathBuf::from(r"C:\Videos\clip one.mp4"))
    );
    rig.written(0.4, Ok(png(PICTURES, "clip one.mp4")));
    assert_eq!(rig.s.hint(), Some("saved to Pictures"));

    // Already in that folder (`FinderTarget.copy`): nothing is copied.
    let mut rig = Rig::with_movie();
    rig.save_double_click(Shell::Explorer);
    rig.folder_answer(0.3, FolderOutcome::Folder(PathBuf::from(r"c:\videos\")));
    rig.to(1.0);
    assert!(rig.e.writes.is_empty());
    assert_eq!(rig.s.hint(), Some(HINT_ALREADY_SAVED));
    assert!(!rig.s.is_holding());
    assert_eq!(
        rig.e.reveals,
        vec![(0x10, PathBuf::from(r"C:\Videos\clip one.mp4"))]
    );
}

#[test]
fn places_that_are_not_folders_and_silent_explorers_keep_the_card() {
    let cases = [
        (
            FolderOutcome::NotAFolder {
                display_name: "This PC".into(),
            },
            "can't save in This PC · still holding it",
        ),
        (
            FolderOutcome::NotAFolder {
                display_name: "Recycle Bin".into(),
            },
            "can't save in Recycle Bin · still holding it",
        ),
        (FolderOutcome::TimedOut, HINT_EXPLORER_SLOW),
        (FolderOutcome::Failed("RPC".into()), HINT_SAVE_FAILED),
    ];
    for (outcome, hint) in cases {
        let mut rig = Rig::new(1);
        rig.save_double_click(Shell::Explorer);
        rig.folder_answer(0.3, outcome);
        rig.to(3.0);
        assert!(rig.e.writes.is_empty(), "{hint}");
        assert_eq!(rig.s.hint(), Some(hint));
        assert_eq!(rig.s.reject_tick(), 1, "{hint}: the card wobbles");
        assert!(rig.s.is_holding());
        assert!(!rig.r.busy());
    }
    // No answer at all (the lookup service is gone): given up after the grace.
    let mut rig = Rig::new(1);
    rig.save_double_click(Shell::Explorer);
    rig.to(0.15 + FOLDER_DEADLINE + LOOKUP_GRACE - 0.01);
    assert!(rig.r.busy());
    rig.to(3.0);
    assert_eq!(rig.s.hint(), Some(HINT_EXPLORER_SLOW));
    assert!(rig.s.is_holding() && !rig.r.busy());
}

#[test]
fn a_write_error_keeps_the_card_with_a_hint_that_names_it() {
    for (problem, hint) in [
        (SaveProblem::ReadOnly, HINT_READ_ONLY),
        (SaveProblem::DiskFull, HINT_DISK_FULL),
        (SaveProblem::Other, HINT_SAVE_FAILED),
    ] {
        let mut rig = Rig::new(1);
        rig.save_double_click(Shell::Explorer);
        rig.in_pictures(0.3);
        rig.written(0.4, Err(fail(problem)));
        assert_eq!(rig.s.hint(), Some(hint));
        assert!(rig.s.is_holding());
        assert!(rig.e.reveals.is_empty());
        assert!(!rig.r.busy());
    }
    // The saver does not take the job: the same, at once.
    let mut rig = Rig::new(1);
    rig.e.saver_busy = true;
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    assert_eq!(rig.s.hint(), Some(HINT_SAVE_FAILED));
    assert!(rig.s.is_holding() && !rig.r.busy());
    // A write that never answers is given up.
    let mut rig = Rig::new(1);
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    rig.to(0.3 + SAVE_TIMEOUT + 0.1);
    assert_eq!(rig.s.hint(), Some(HINT_SAVE_FAILED));
    assert!(rig.s.is_holding() && !rig.r.busy());
}

#[test]
fn a_write_error_in_a_pile_delivers_what_was_saved_then_names_the_problem() {
    let mut rig = Rig::new(3);
    let ids = rig.ids();
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    let first = png(PICTURES, "a.png");
    rig.written(0.4, Ok(first.clone()));
    rig.written(0.5, Err(fail(SaveProblem::DiskFull)));
    // The rest is not tried.
    assert_eq!(rig.e.writes.len(), 2);
    assert!(rig.s.card(ids[0]).is_none());
    assert!(rig.s.card(ids[1]).is_some() && rig.s.card(ids[2]).is_some());
    assert_eq!(rig.s.hint(), Some("saved to Pictures"));
    assert_eq!(rig.e.reveals, vec![(0x10, first)]);
    rig.to(0.5 + SAVE_FAIL_STEP + 0.01);
    assert_eq!(rig.s.hint(), Some(HINT_DISK_FULL));
    assert!(!rig.r.busy());
}

#[test]
fn a_press_or_a_key_before_the_save_starts_cancels_it() {
    // A press while Explorer is asked.
    let mut rig = Rig::new(1);
    rig.save_double_click(Shell::Explorer);
    rig.down(0.28, 1);
    rig.in_pictures(0.3);
    rig.to(3.0);
    assert!(rig.e.logged("another press → the save is dropped"));
    assert!(rig.e.writes.is_empty() && rig.s.is_holding());
    // A key while the button is still down after the answer.
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, shell_spot(Shell::Explorer, true));
    rig.in_pictures(0.2);
    rig.e.keys += 1;
    rig.up(0.25);
    rig.to(3.0);
    assert!(rig.e.logged("the user typed → the save is dropped"));
    assert!(rig.e.writes.is_empty() && rig.s.is_holding());
    assert!(!rig.r.busy());
}

#[test]
fn once_the_file_is_being_written_the_save_finishes() {
    let mut rig = Rig::new(2);
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    rig.down(0.32, 1);
    rig.e.keys += 1;
    assert!(rig.e.logged("another press → the save goes on"));
    rig.written(0.4, Ok(png(PICTURES, "a.png")));
    rig.written(0.5, Ok(png(PICTURES, "b.png")));
    assert!(!rig.s.is_holding());
    assert_eq!(rig.s.hint(), Some("saved 2 to Pictures"));
}

#[test]
fn a_held_button_is_a_drag_and_saves_nothing() {
    let mut rig = Rig::new(1);
    rig.double_click();
    rig.answer(0.17, shell_spot(Shell::Explorer, true));
    rig.in_pictures(0.2);
    // The button stays down past MOUSE_UP_TIMEOUT.
    rig.to(3.0);
    assert!(rig.e.logged("the button is still down (a drag) → no save"));
    assert!(rig.e.writes.is_empty() && rig.s.is_holding() && !rig.r.busy());
    assert_eq!(rig.s.reject_tick(), 0, "quietly");
}

#[test]
fn a_card_shaken_off_before_its_save_is_skipped() {
    let mut rig = Rig::new(2);
    let ids = rig.ids();
    rig.save_double_click(Shell::Explorer);
    // The top card is shaken off while Explorer is asked.
    rig.s.dissolve(0.27);
    rig.in_pictures(0.3);
    assert_eq!(rig.e.writes.len(), 1);
    assert_eq!(rig.e.writes[0].1, ids[1]);
    rig.written(0.4, Ok(png(PICTURES, "b.png")));
    assert_eq!(rig.s.hint(), Some("saved to Pictures"));
    assert!(rig.e.logged(&format!(
        "card {} left the pile meanwhile → skipped",
        ids[0].0
    )));
    // Everything gone before the save: nothing to do, quietly.
    let mut rig = Rig::new(1);
    rig.save_double_click(Shell::Explorer);
    rig.s.discard_all(0.27);
    rig.in_pictures(0.3);
    rig.to(2.0);
    assert!(rig.e.writes.is_empty() && !rig.r.busy());
    assert!(rig.e.reveals.is_empty());
}

#[test]
fn a_capture_landing_during_the_save_stays_on_the_pile() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    rig.s.hold(
        image(),
        1.0,
        SystemTime::UNIX_EPOCH,
        false,
        None,
        None,
        0.35,
    );
    rig.written(0.4, Ok(png(PICTURES, "a.png")));
    rig.to(2.0);
    assert_eq!(rig.e.writes.len(), 1);
    assert!(rig.s.card(id).is_none());
    assert_eq!(rig.s.deliverable().len(), 1);
}

#[test]
fn a_second_double_click_during_a_save_is_ignored() {
    let mut rig = Rig::new(2);
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    rig.double_click_at(0.5);
    assert!(rig.e.logged("busy=yes"));
    assert_eq!(rig.e.folders.len(), 1);
    assert_eq!(rig.e.lookups.len(), 1);
    rig.written(0.6, Ok(png(PICTURES, "a.png")));
    rig.written(0.7, Ok(png(PICTURES, "b.png")));
    assert!(!rig.s.is_holding());
}

/// Quit while a file is written: the saver thread finishes that file (it writes a temp
/// file and renames it, and `LiveEnv::finish` gives it a second); the router starts no more
/// writes, delivers nothing and ignores the late answer.
#[test]
fn quit_during_a_save_starts_no_more_writes() {
    let mut rig = Rig::new(2);
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    assert_eq!(rig.r.quit(0.35, &mut rig.e), None);
    assert!(
        rig.e
            .logged("quit: the file being written is finished by the saver")
    );
    assert!(!rig.r.busy());
    rig.written(0.4, Ok(png(PICTURES, "a.png")));
    rig.to(3.0);
    assert_eq!(rig.e.writes.len(), 1);
    assert_eq!(rig.s.deliverable().len(), 2);
    assert!(rig.e.reveals.is_empty());
    // While Explorer is asked: nothing at all is written.
    let mut rig = Rig::new(1);
    rig.save_double_click(Shell::Explorer);
    assert_eq!(rig.r.quit(0.27, &mut rig.e), None);
    rig.in_pictures(0.3);
    assert!(rig.e.writes.is_empty());
}

#[test]
fn presses_on_two_windows_or_an_answer_about_another_window_save_nothing() {
    let mut rig = Rig::new(1);
    rig.down(0.0, 1);
    rig.up(0.08);
    rig.mouse(
        MouseEvent::Down {
            button: Button::Left,
            x: AT.0,
            y: AT.1,
            time: 0.15,
            click_count: 2,
            injected: false,
            window: 0x20,
            touch: false,
        },
        0.15,
    );
    assert!(rig.e.lookups.is_empty() && rig.e.folders.is_empty());
    // The lookup found another window than the one pressed.
    let mut rig = Rig::new(1);
    rig.double_click();
    let Seen::Spot(mut spot) = shell_spot(Shell::Explorer, true) else {
        unreachable!()
    };
    spot.target.window = 0x30;
    rig.answer(0.17, Seen::Spot(spot));
    assert!(rig.e.folders.is_empty());
}

#[test]
fn not_a_drop_spot_in_explorer_is_ignored_without_a_folder_lookup() {
    for shell in [Shell::Explorer, Shell::Desktop] {
        let mut rig = Rig::new(1);
        rig.double_click();
        rig.answer(0.17, shell_spot(shell, false));
        rig.up(0.25);
        rig.to(3.0);
        assert!(rig.e.logged("drop-spot=no"));
        assert!(rig.e.folders.is_empty() && rig.e.lookups.len() == 1);
        assert!(rig.s.is_holding() && rig.s.reject_tick() == 0);
    }
}

#[test]
fn hover_says_save_only_over_the_folder_background() {
    let mut rig = Rig::new(1);
    rig.moved(0.0, AT.0, AT.1);
    rig.to(0.23);
    rig.answer(0.24, shell_spot(Shell::Explorer, true));
    assert_eq!(rig.s.hint(), Some(HINT_HOVER_SAVE));
    // Onto the navigation pane: the hint goes.
    rig.moved(0.5, AT.0, AT.1);
    rig.to(0.75);
    let s = explorer_snapshot(vec![nav_pane(), proper_tree_host()]);
    rig.answer(0.76, Seen::Spot(Box::new(spot_from_snapshot(&s, AT))));
    assert_eq!(rig.s.hint(), None);
    assert_eq!(
        hover_hint(&shell_spot(Shell::Desktop, true)),
        Some(HINT_HOVER_SAVE)
    );
    assert_eq!(hover_hint(&shell_spot(Shell::Explorer, false)), None);
}

// Windows 11 Explorer, as UI Automation shows it (read from a live window, build 26300):
// the top-level `CabinetWClass` window holds a `TITLE_BAR_SCAFFOLDING_WINDOW_CLASS` pane, a
// XAML header (`Microsoft.UI.Content.DesktopChildSiteBridge` > `InputSiteWindowClass`, with
// the tabs, the command bar, the breadcrumb bar, the address and search boxes), and the
// active tab (`ShellTabWindowClass` > `DUIViewWndClassName`) with the status bar, the
// navigation pane (`ProperTreeHost` > `SysTreeView32`, a Pane) and the folder view
// (`DUIListView`, the `SHELLDLL_DefView` window, > the `UIItemsView` list, the
// `DirectUIHWND` window, > `UIItem` list items > `UIProperty` edits for names and columns).

fn named(control: UIA_CONTROLTYPE_ID, class: &str) -> ElementInfo {
    ElementInfo {
        class_name: class.into(),
        ..el(control)
    }
}

fn cabinet() -> ElementInfo {
    ElementInfo {
        class_name: "CabinetWClass".into(),
        ..window()
    }
}

fn tab_panes() -> Vec<ElementInfo> {
    vec![
        named(UIA_PaneControlTypeId, "DUIViewWndClassName"),
        named(UIA_PaneControlTypeId, "ShellTabWindowClass"),
        cabinet(),
    ]
}

fn items_view() -> Vec<ElementInfo> {
    let mut chain = vec![
        named(UIA_ListControlTypeId, "UIItemsView"),
        named(UIA_PaneControlTypeId, "DUIListView"),
    ];
    chain.extend(tab_panes());
    chain
}

fn nav_pane() -> ElementInfo {
    named(UIA_PaneControlTypeId, "SysTreeView32")
}

fn proper_tree_host() -> ElementInfo {
    named(UIA_PaneControlTypeId, "ProperTreeHost")
}

fn header_site() -> Vec<ElementInfo> {
    vec![
        named(UIA_PaneControlTypeId, "InputSiteWindowClass"),
        named(
            UIA_PaneControlTypeId,
            "Microsoft.UI.Content.DesktopChildSiteBridge",
        ),
        cabinet(),
    ]
}

/// A snapshot of Explorer; a chain that does not reach the window is put in the tab.
fn explorer_snapshot(hit_and_up: Vec<ElementInfo>) -> Snapshot {
    let mut t = target("explorer.exe", false);
    t.shell = Some(Shell::Explorer);
    t.class_name = "CabinetWClass".into();
    let mut chain = hit_and_up;
    if !chain.last().is_some_and(|e| e.top_level) {
        chain.extend(tab_panes());
    }
    snapshot(t, chain, None)
}

fn drop_spot_of(chain: Vec<ElementInfo>) -> bool {
    spot_from_snapshot(&explorer_snapshot(chain), AT).drop_spot
}

fn with_view(hit: Vec<ElementInfo>) -> Vec<ElementInfo> {
    let mut chain = hit;
    chain.extend(items_view());
    chain
}

#[test]
fn only_the_items_views_background_is_a_drop_spot_in_explorer() {
    // Icons or details view, the area beside and below the last item: the list itself.
    assert!(drop_spot_of(items_view()));
    // "This folder is empty.": a text right in the list.
    assert!(drop_spot_of(with_view(vec![el(UIA_TextControlTypeId)])));
    // An item, its name, a cell of the details view.
    assert!(!drop_spot_of(with_view(vec![named(
        UIA_ListItemControlTypeId,
        "UIItem"
    )])));
    assert!(!drop_spot_of(with_view(vec![
        named(UIA_EditControlTypeId, "UIProperty"),
        named(UIA_ListItemControlTypeId, "UIItem"),
    ])));
    assert!(!drop_spot_of(with_view(vec![
        named(UIA_TextControlTypeId, "UIProperty"),
        named(UIA_ListItemControlTypeId, "UIItem"),
    ])));
    // The details view's column header: an item, and its free space past the last column.
    assert!(!drop_spot_of(with_view(vec![
        named(UIA_HeaderItemControlTypeId, "UIColumnHeader"),
        named(UIA_HeaderControlTypeId, "UIColumnHeader"),
    ])));
    assert!(!drop_spot_of(with_view(vec![named(
        UIA_HeaderControlTypeId,
        "UIColumnHeader"
    )])));
    // The navigation pane's free space and its items.
    assert!(!drop_spot_of(vec![nav_pane(), proper_tree_host()]));
    assert!(!drop_spot_of(vec![
        el(UIA_TreeItemControlTypeId),
        nav_pane(),
        proper_tree_host()
    ]));
    // The status bar, and the details or preview pane (a pane of the tab, not the view).
    assert!(!drop_spot_of(vec![named(
        UIA_StatusBarControlTypeId,
        "StatusBarModuleInner"
    )]));
    assert!(!drop_spot_of(vec![named(
        UIA_PaneControlTypeId,
        "PreviewPane"
    )]));
    // The title bar area and the XAML header: the breadcrumb bar's free space, the address
    // and search boxes, the header's free space, the command bar.
    assert!(!drop_spot_of(vec![
        named(UIA_PaneControlTypeId, "TITLE_BAR_SCAFFOLDING_WINDOW_CLASS"),
        cabinet(),
    ]));
    let mut breadcrumb = vec![named(UIA_GroupControlTypeId, "LandmarkTarget")];
    breadcrumb.extend(header_site());
    assert!(!drop_spot_of(breadcrumb));
    let address = ElementInfo {
        value_pattern: true,
        value_read_only: Some(false),
        ..named(UIA_EditControlTypeId, "TextBox")
    };
    let mut address_bar = vec![address, named(UIA_GroupControlTypeId, "AutoSuggestBox")];
    address_bar.extend(header_site());
    assert!(!drop_spot_of(address_bar));
    assert!(!drop_spot_of(header_site()));
    let mut command_bar = vec![named(UIA_AppBarControlTypeId, "ApplicationBar")];
    command_bar.extend(header_site());
    assert!(!drop_spot_of(command_bar));
}

#[test]
fn the_desktops_empty_area_is_a_drop_spot_and_an_icon_is_not() {
    let mut t = target("explorer.exe", false);
    t.shell = Some(Shell::Desktop);
    t.class_name = "Progman".into();
    let progman = ElementInfo {
        class_name: "Progman".into(),
        ..window()
    };
    let view = named(UIA_PaneControlTypeId, "SHELLDLL_DefView");
    let list = named(UIA_ListControlTypeId, "SysListView32");
    let icon = named(UIA_ListItemControlTypeId, "");
    let spot = |chain: Vec<ElementInfo>| spot_from_snapshot(&snapshot(t.clone(), chain, None), AT);
    assert!(spot(vec![list.clone(), view.clone(), progman.clone()]).drop_spot);
    // Desktop icons hidden: the list is hidden and the view is the hit.
    assert!(spot(vec![view.clone(), progman.clone()]).drop_spot);
    assert!(
        !spot(vec![
            icon.clone(),
            list.clone(),
            view.clone(),
            progman.clone()
        ])
        .drop_spot
    );
    assert!(
        !spot(vec![
            el(UIA_EditControlTypeId),
            icon,
            list,
            view,
            progman.clone()
        ])
        .drop_spot
    );
    // Not inside the desktop's view at all.
    assert!(!spot(vec![progman]).drop_spot);
}

#[test]
fn save_errors_are_told_apart_by_their_os_code() {
    use igorio_app::saver::SaveError;
    use std::io::Error;
    let of = |code: i32| SaveFail::of(&SaveError::Io(Error::from_raw_os_error(code))).problem;
    assert_eq!(of(5), SaveProblem::ReadOnly);
    assert_eq!(of(112), SaveProblem::DiskFull);
    assert_eq!(of(39), SaveProblem::DiskFull);
    assert_eq!(of(3), SaveProblem::Other);
    assert_eq!(
        SaveFail::of(&SaveError::Encode("x".into())).problem,
        SaveProblem::Other
    );
}

#[test]
fn saved_hints_name_the_folder() {
    assert_eq!(saved_hint(1, Path::new(PICTURES)), "saved to Pictures");
    assert_eq!(saved_hint(2, Path::new(PICTURES)), "saved 2 to Pictures");
    assert_eq!(saved_hint(1, Path::new(r"D:\")), r"saved to D:\");
    assert!(same_folder(
        Some(Path::new(r"C:\Videos")),
        Path::new(r"c:\videos\")
    ));
    assert!(!same_folder(
        Some(Path::new(r"C:\Videos\a")),
        Path::new(r"C:\Videos")
    ));
    assert!(!same_folder(None, Path::new(r"C:\Videos")));
}

#[test]
fn anything_inside_an_item_is_not_background() {
    // A group (a thumbnail's frame, a tile's text block) inside an item: the core's role
    // rule alone takes a group for background.
    assert!(!drop_spot_of(with_view(vec![
        el(UIA_GroupControlTypeId),
        named(UIA_ListItemControlTypeId, "UIItem"),
    ])));
}

#[test]
fn an_answer_for_another_write_is_ignored() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.save_double_click(Shell::Explorer);
    rig.in_pictures(0.3);
    let job = rig.e.writes[0].0;
    rig.r.folder_saved(
        job + 99,
        Ok(png(PICTURES, "other.png")),
        0.35,
        &mut rig.s,
        &mut rig.e,
    );
    assert!(rig.s.card(id).is_some() && rig.r.busy());
    rig.written(0.4, Ok(png(PICTURES, "a.png")));
    assert!(rig.s.card(id).is_none());
}

// MARK: Notes

/// A note window under `AT`.
fn note_rig(cards: usize) -> Rig {
    let mut rig = Rig::new(cards);
    rig.e.note = Some(Rect::new(400.0, 300.0, 304.0, 224.0));
    rig
}

#[test]
fn a_double_click_over_a_note_gives_it_the_cards_and_they_leave_on_its_reply() {
    let mut rig = note_rig(2);
    let ids = rig.ids();
    rig.double_click();
    assert_eq!(rig.e.gifts, vec![(AT, ids.clone())], "{:#?}", rig.e.log);
    assert!(rig.e.logged("over a note → give it 2 card(s)"));
    // No lookup, no clipboard: the note takes the pile itself.
    assert!(rig.e.lookups.is_empty());
    assert!(rig.e.snapshots.is_empty());
    // The cards stay until the note answers.
    rig.up(0.2);
    rig.to(1.0);
    assert_eq!(rig.ids(), ids);
    assert!(rig.r.busy());
    rig.r
        .note_took(&ids, "added 2 to note", 1.0, &mut rig.s, &mut rig.e);
    assert!(rig.ids().is_empty());
    assert_eq!(rig.s.hint(), Some("added 2 to note"));
    assert!(!rig.r.busy());
    assert!(rig.r.take_state_changed());
}

#[test]
fn a_note_that_never_answers_leaves_the_cards_on_the_pile_and_says_so() {
    let mut rig = note_rig(1);
    let ids = rig.ids();
    rig.double_click();
    let tick = rig.s.reject_tick();
    rig.to(0.15 + NOTE_TIMEOUT - 0.01);
    assert!(rig.r.busy());
    rig.to(0.15 + NOTE_TIMEOUT + 0.01);
    assert!(!rig.r.busy());
    assert_eq!(rig.ids(), ids);
    assert_eq!(rig.s.hint(), Some(HINT_NOTE_SLOW));
    assert_eq!(rig.s.reject_tick(), tick + 1);
    assert!(rig.e.logged("the note did not answer"));
    // A late answer still lets go of the cards it took: they are in the note.
    rig.r
        .note_took(&ids, "added to note", 12.0, &mut rig.s, &mut rig.e);
    assert!(rig.ids().is_empty());
}

#[test]
fn a_note_that_fails_keeps_the_cards_at_once() {
    let mut rig = note_rig(1);
    let ids = rig.ids();
    rig.double_click();
    assert!(rig.r.note_failed(0.5, &mut rig.s, &mut rig.e));
    assert!(!rig.r.busy());
    assert_eq!(rig.ids(), ids);
    assert_eq!(rig.s.hint(), Some(HINT_NOTE_FAILED));
    // An error with no gift waiting is not the router's.
    assert!(!rig.r.note_failed(0.6, &mut rig.s, &mut rig.e));
}

#[test]
fn without_cards_a_double_click_over_a_note_does_nothing() {
    let mut rig = note_rig(0);
    rig.double_click();
    rig.up(0.2);
    rig.to(3.0);
    assert!(rig.e.gifts.is_empty());
    assert!(rig.e.lookups.is_empty());
    assert!(!rig.r.busy());
}

#[test]
fn a_double_click_over_a_note_never_pastes_into_what_is_behind_it() {
    let mut rig = note_rig(1);
    // Whatever is behind the note would take a paste.
    rig.focus = Some(focused_field(false, ""));
    rig.double_click();
    rig.up(0.2);
    rig.to(NOTE_TIMEOUT + 1.0);
    assert!(rig.e.lookups.is_empty());
    assert!(rig.e.stages.is_empty());
    assert!(rig.e.pressed.is_empty());
    assert_eq!(rig.e.sent, 0);
    assert!(rig.e.checks.borrow().is_empty());
    // Outside the note the same double-click is an ordinary paste.
    let mut rig = Rig::new(1);
    rig.e.note = Some(Rect::new(0.0, 0.0, 100.0, 100.0));
    paste_into_field(&mut rig);
    assert!(rig.e.gifts.is_empty());
}

#[test]
fn typing_in_the_note_while_it_takes_the_cards_does_not_stop_it() {
    let mut rig = note_rig(1);
    let ids = rig.ids();
    rig.double_click();
    rig.e.keys += 3;
    rig.to(1.0);
    assert!(rig.r.busy());
    rig.r
        .note_took(&ids, "added to note", 1.0, &mut rig.s, &mut rig.e);
    assert!(rig.ids().is_empty());
}

#[test]
fn resting_over_a_note_shows_its_hint_without_a_lookup() {
    let mut rig = note_rig(1);
    rig.moved(0.0, AT.0, AT.1);
    rig.to(0.2);
    assert_ne!(rig.s.hint(), Some(igorio_app::notes::window::DROP_HINT));
    rig.to(0.23);
    assert_eq!(rig.s.hint(), Some(igorio_app::notes::window::DROP_HINT));
    assert!(rig.e.lookups.is_empty());
    // Off the note: the usual lookup.
    rig.moved(0.3, 10, 10);
    rig.to(0.6);
    assert_eq!(rig.e.lookups.len(), 1);
    // Nothing held: no hint.
    let mut rig = note_rig(0);
    rig.moved(0.0, AT.0, AT.1);
    rig.to(1.0);
    assert_eq!(rig.s.hint(), None);
}

#[test]
fn a_delivery_in_progress_ignores_a_double_click_over_a_note() {
    let mut rig = Rig::new(2);
    paste_into_field(&mut rig);
    rig.e.note = Some(Rect::new(400.0, 300.0, 304.0, 224.0));
    rig.double_click_at(0.6);
    assert!(rig.e.gifts.is_empty(), "{:#?}", rig.e.log);
    assert!(rig.e.logged("busy=yes"));
}

// MARK: Recordings: a file is only taken when the field shows it

/// Holds a recording on top of the pile (it was captured last).
fn hold_recording(rig: &mut Rig) {
    rig.s.hold(
        image(),
        1.0,
        SystemTime::UNIX_EPOCH,
        true,
        Some(PathBuf::from(r"C:\Videos\clip one.mp4")),
        Some(3.0),
        -5.0,
    );
}

/// The owner's log: the composer reads the file (windowless, probably the target) and shows
/// nothing. The card stays, with the file hint.
#[test]
fn a_recording_the_composer_reads_but_does_not_show_keeps_its_card() {
    let mut rig = Rig::with_movie();
    let id = rig.ids()[0];
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    assert!(matches!(rig.e.stages[0].1, Payload::File(_)));
    rig.read_by(sent + 0.03, 1, None);
    rig.to(sent + 0.5);
    assert!(
        rig.s.card(id).is_some(),
        "a read is no proof: {:#?}",
        rig.e.log
    );
    rig.to(sent + 3.2);
    assert!(rig.s.card(id).is_some());
    assert_eq!(rig.s.hint(), Some(HINT_NO_FILES));
    // The probe ran although the composer is multi-line.
    assert!(rig.e.lookups.iter().filter(|l| l.4).count() >= 5);
}

/// A file chip appears in the composer: the recording was taken.
#[test]
fn a_recording_that_shows_a_file_chip_is_delivered() {
    let mut rig = Rig::with_movie();
    let id = rig.ids()[0];
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    rig.chips = 1;
    rig.read_by(sent + 0.03, 1, None);
    rig.to(sent + 0.6);
    assert!(rig.s.card(id).is_none(), "{:#?}", rig.e.log);
    assert!(rig.e.logged("probe: the field changed"));
}

/// Read, but no probe answers: nothing can prove it either way.
#[test]
fn a_recording_whose_probe_cannot_answer_says_check_the_paste() {
    let mut rig = Rig::with_movie();
    let id = rig.ids()[0];
    rig.after = None;
    rig.double_click();
    rig.answer(0.17, composer());
    rig.remembered_arrives(0.18);
    rig.up(0.25);
    rig.to(0.40);
    rig.staged_reply(0.41);
    rig.read_by(0.44, 1, None);
    rig.to(3.6);
    assert!(rig.s.card(id).is_some());
    assert_eq!(rig.s.hint(), Some(HINT_UNCERTAIN), "{:#?}", rig.e.log);
}

/// Nothing reads the file: the keys went nowhere.
#[test]
fn a_recording_nothing_reads_says_couldnt_paste() {
    let mut rig = Rig::with_movie();
    let id = rig.ids()[0];
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    rig.to(sent + 1.7);
    assert!(rig.s.card(id).is_some());
    assert_eq!(rig.s.hint(), Some(HINT_PASTE_FAILED));
}

/// An image into the same composer still follows the image rule: a read by the target is
/// enough there.
#[test]
fn an_image_into_the_composer_still_follows_the_image_rule() {
    let mut rig = Rig::new(1);
    let id = rig.ids()[0];
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    rig.read_by(sent + 0.03, 1, None);
    rig.to(sent + 0.3);
    assert!(rig.s.card(id).is_none(), "{:#?}", rig.e.log);
}

/// A mixed pile: each card is judged by its own payload, and a card that is not proved
/// stops the pile.
#[test]
fn a_mixed_pile_judges_each_card_by_its_payload() {
    // Recording on top, refused: it stays, the screenshot under it is not tried.
    let mut rig = Rig::new(1);
    hold_recording(&mut rig);
    let ids = rig.ids();
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    rig.read_by(sent + 0.03, 1, None);
    rig.to(sent + 4.0);
    assert!(rig.s.card(ids[0]).is_some() && rig.s.card(ids[1]).is_some());
    assert_eq!(rig.s.hint(), Some(HINT_NO_FILES));
    assert_eq!(rig.e.stages.len(), 1);

    // Recording on top, taken (a chip): then the screenshot follows the image rule.
    let mut rig = Rig::new(1);
    hold_recording(&mut rig);
    let ids = rig.ids();
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    rig.chips = 1;
    rig.read_by(sent + 0.03, 1, None);
    rig.to(sent + 0.6);
    assert!(rig.s.card(ids[0]).is_none(), "{:#?}", rig.e.log);
    rig.to(sent + 0.6 + 0.45 + 0.4);
    assert_eq!(rig.e.stages.len(), 2, "{:#?}", rig.e.log);
    let second = rig.now;
    rig.staged_reply(second);
    rig.read_by(second + 0.03, 1, None);
    rig.to(second + 0.3);
    assert!(rig.s.card(ids[1]).is_none(), "{:#?}", rig.e.log);

    // Screenshot on top, then a recording the composer ignores: the screenshot leaves, the
    // recording stays with the file hint.
    let mut rig = Rig::with_movie();
    rig.s.hold(
        image(),
        1.0,
        SystemTime::UNIX_EPOCH,
        false,
        None,
        None,
        -5.0,
    );
    let ids = rig.ids();
    rig.images = Some(0);
    let sent = paste_pile_into_composer(&mut rig);
    rig.images = Some(1);
    rig.read_by(sent + 0.03, 1, None);
    rig.to(sent + 0.3);
    assert!(rig.s.card(ids[0]).is_none(), "{:#?}", rig.e.log);
    rig.to(sent + 0.3 + 0.45 + 0.4);
    let second = rig.now;
    rig.staged_reply(second);
    rig.read_by(second + 0.03, 1, None);
    rig.to(second + 3.5);
    assert!(rig.s.card(ids[1]).is_some(), "{:#?}", rig.e.log);
    assert_eq!(rig.s.hint(), Some(HINT_NO_FILES));
}
