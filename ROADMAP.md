# Roadmap: fairy cursor for Windows, in Rust

Source: https://github.com/Rrrima/fairy-cursor at commit `3d5f655` (MIT).
About 8,500 lines of Swift plus 1,500 lines of tests. Its `ausm` dependency
(screenshot and cursor monitoring) is mac-only and is replaced, not translated.
The copy of the Swift source in `reference/` was removed once the port was done; its MIT
notice and the Barlow font licence are in `NOTICE`.

## Rules for the translation

- `sources/core` is pure logic. Translate it file by file, keep names, keep every test.
- Everything else is rewritten against Win32. Keep the same split: pure decision in
  `core`, live lookups and actions in `app`.
- Coordinates: macOS uses points with a bottom-left origin. The port uses physical
  pixels with a top-left origin everywhere, and converts sizes with the monitor DPI.
- All Swift timing constants (paste delays, shake thresholds) are copied first and
  tuned only after testing on Windows.
- Keyboard: `⌘` becomes `Ctrl`, `⌥` becomes `Alt`. Final shortcut choice is in M7/M8.

## Rules for the Win32 code

From the review of the M0 spikes. The spikes break some of these; do not copy them as is.

1. Low-level hooks run on their own thread. That thread only pumps messages and pushes
   small events to a channel: no I/O, logging, clipboard, COM or process queries. It
   always calls `CallNextHookEx`. Windows silently removes a hook that is too slow.
2. Every call that waits on another process (UI Automation, Shell, clipboard rendering)
   runs on a worker with a deadline and a cap on the number of calls. Stale requests
   are dropped.
3. No thread that owns windows waits for UI Automation. Hits inside our own process are
   handled before UI Automation is called.
4. Open the clipboard with a bounded retry, never on the hook or UI thread, and close it
   quickly. Ignore clipboard updates our own window caused. Read the sequence number
   after `CloseClipboard`.
5. Mark staged and restored clipboard data with
   `ExcludeClipboardContentFromMonitorProcessing`, so pastes stay out of `Win+V`
   history and cloud clipboard.
6. Save and restore only clipboard formats on a known list. Skip GDI, metafile, private
   (0x200 to 0x3FF) and OLE private formats, and cap the total size.
7. Right before `SendInput`, check the foreground window and the modifier keys again,
   and fill scan codes.
8. Declare Per-Monitor V2 DPI awareness in the manifest. Get DPI with
   `GetDpiForMonitor`. Keep one cached bitmap for drawing, and tick only while
   something moves.


## UI toolkit: native Win32

Decided after a prototype (branch `t3/prototype-settings-ui`, not merged). The settings
window, the first-run guide and the note windows use native Win32 windows and controls,
owner-drawn where the look needs it. The note editor is a RichEdit control.

- egui was tried for the settings window. It worked, but it added about 7 MB to the exe,
  about 300 packages, and 60 to 80 MB of memory that is not released when the window
  closes. Its text box has no spell checking, no Windows font fallback and incomplete
  right-to-left editing.
- RichEdit gives spell checking, IME, all scripts and UI Automation without extra code.
- These windows run on their own thread with their own message loop. A modal dialog (the
  folder picker) must never block the overlay loop.

## Workspace layout

```
Cargo.toml              workspace
crates/
  core/                 pure logic, no Windows API, unit tests   (sources/core)
  kit/                  Capability trait, Host trait, shared types (sources/kit)
  platform/             Win32 wrappers: hooks, UI Automation, clipboard, input, shell
  screenshots/          capability
  notes/                capability
  recording/            capability
  app/                  exe: overlay, tray, settings, guide, drop routing
assets/                 icon, Barlow fonts (OFL)
```

Crates: `windows`, `tiny-skia`, `fontdue`, `png`,
`serde`/`serde_json`, `windows-capture`.

## File map

### core (translate 1:1)

| Swift | Rust | Lines |
|---|---|---|
| `ShakeDetector.swift` | `core/src/shake.rs` | 203 |
| `HeldPile.swift` | `core/src/pile.rs` | 137 |
| `DropPolicy.swift` | `core/src/drop_policy.rs` | 198 |
| `PastePolicy.swift` | `core/src/paste_policy.rs` | 199 |
| `EditableResolver.swift` | `core/src/editable.rs` (trait `AccessibilityNode`) | 87 |
| `CaptureWatchdog.swift` | `core/src/capture_watchdog.rs` | 61 |
| `Ink.swift` | `core/src/ink.rs` | 110 |
| `Freehand.swift` | `core/src/freehand.rs` | 250 |
| `RecordingViewport.swift` | `core/src/viewport.rs` | 86 |
| `tests/core/*.swift` (11 files) | `core/tests/*.rs` | 1542 |

`DropPolicy` and `EditableResolver` use macOS role names (`AXTextArea`, `AXWebArea`).
Replace them with a small role enum in `core`, filled from UI Automation control types.

### kit

| Swift | Rust |
|---|---|
| `Capability.swift` | `kit/src/capability.rs`: `Capability` and `Host` traits, `Manifest`, `DropZone` |
| `HeldScreenshot.swift` | `kit/src/held.rs` |
| `Theme.swift`, `SettingsRows.swift`, `KeyChip.swift` | `kit/src/theme.rs`, row helpers for the settings window |
| `SystemScreenshotLocation.swift` | `kit/src/folders.rs` (`Pictures\Screenshots`) |
| `Finder.swift` | `platform/src/explorer.rs` |
| `WebAccessibility.swift` | not needed: Chromium builds its tree when a UI Automation client asks. Check in M0 |
| `Permissions.swift` | dropped (no permission prompts on Windows) |

### app

| Swift | Rust | Windows API |
|---|---|---|
| `main.swift`, `AppDelegate.swift` | `app/src/main.rs`, `host.rs` | message loop, tray |
| `FairyPanel.swift` | `app/src/overlay.rs` | layered click-through topmost window, `WDA_EXCLUDEFROMCAPTURE` |
| `FairyView.swift`, `FairyState.swift`, `LogoMark.swift` | `app/src/overlay_draw.rs`, `state.rs` | `tiny-skia` + `UpdateLayeredWindow` |
| `Follower.swift` | `app/src/follower.rs` | `GetCursorPos`, timer, monitor work area |
| `AXNode.swift` | `platform/src/uia.rs` | `IUIAutomation`, cached reads, timeout |
| `DropRouter.swift` | `app/src/drop_router.rs` | `WH_MOUSE_LL` double-click, UI Automation |
| `PasteTarget.swift` | `app/src/paste.rs` | clipboard, `GetClipboardSequenceNumber`, `SendInput` |
| `Keys.swift` | `platform/src/input.rs` | `SendInput` |
| `FinderTarget.swift` | `app/src/explorer_target.rs` | `IShellWindows`, `CabinetWClass`, desktop |
| `AppSettings.swift`, `CapabilityRegistry.swift` | `app/src/settings.rs`, `registry.rs` | JSON in `%APPDATA%` |
| `SettingsWindow.swift` | `app/src/settings_window.rs` | native Win32 controls, owner-drawn where the look needs it |
| `GuideWindow.swift`, `GuideScenes.swift` | `app/src/guide.rs` | native Win32 window; scenes simplified |
| `Relocation.swift` | dropped (macOS translocation only) |

### capabilities

| Swift | Rust | Windows API |
|---|---|---|
| `ScreenshotsCapability.swift` + ausm `ScreenshotMonitorKit` | `screenshots/src/lib.rs`, `detect.rs` | clipboard listener after `Win+Shift+S`, folder watch for `Win+PrtScn` |
| `SystemScreenshotTakeover.swift` | dropped (Windows has no thumbnail redirect) | |
| `ScreenshotSaver.swift` | `screenshots/src/saver.rs` | PNG encode off the UI thread |
| `NotesCapability.swift`, `NotesController.swift` | `notes/src/lib.rs`, `controller.rs` | |
| `NoteStore.swift` | `notes/src/store.rs` (mostly portable) | markdown files |
| `NoteContext.swift` | `notes/src/context.rs` | UI Automation |
| `NoteSummoner.swift` | `notes/src/summoner.rs` | `WH_KEYBOARD_LL` + `WH_MOUSE_LL` |
| `NoteView.swift`, `MarkdownEditor.swift` | `notes/src/view.rs`, `editor.rs` | Win32 note window hosting a RichEdit control; styling from `notes/markdown.rs` |
| `RecordingCapability.swift`, `RecordingInput.swift` | `recording/src/lib.rs`, `input.rs` | `RegisterHotKey`, low-level hooks |
| `ScreenRecorder.swift` | `recording/src/recorder.rs` | Windows.Graphics.Capture, Media Foundation H.264 (`.mp4`) |
| `RecordingFrame.swift` | `recording/src/frame.rs` | overlay excluded from capture |
| `InkOverlay.swift` | `recording/src/ink_overlay.rs` | overlay included in capture |
| `CursorScale.swift` | `recording/src/cursor.rs` | draw a larger cursor into the video |

## Administrator and prompts

Goal: no prompt when the app starts, and it still works over windows that run as
administrator.

Background: a normal process cannot read (UI Automation), send keys to, or hook input
for an elevated window. Windows blocks this (UIPI).

The installer uses an `asInvoker` manifest with `uiAccess="true"`. Windows requires a
trusted signature and a protected location, so Setup asks for admin once and installs
to Program Files. Igorio starts with no UAC prompt. When started by a user in the
Administrators group, it can read and paste into elevated windows on that desktop.

Setup creates a self-signed code-signing certificate for this machine, trusts the
public certificate, signs and verifies the exe, and deletes the private key. Uninstall
removes the certificate. If signing fails, Setup installs the regular build and shows
a message. The setup exe itself is unsigned, so SmartScreen can warn on download.
Later options are SignPath Foundation if the repo becomes public or Azure Trusted Signing.

Normal Cargo builds, including `--release`, embed `uiAccess="false"` and can run from
the build folder. The app checks its token at startup and shows "can't reach this
window" when Windows blocks access to an elevated target.

Rejected:

- `requireAdministrator`: a UAC prompt on every start, blocked for standard users,
  autostart needs a scheduled task, and files and child processes become elevated.
- Scheduled task with highest privileges: no prompt after install, but the whole app
  still runs elevated.

## Folders the user chooses

Set in the settings window, stored in `%APPDATA%\<app>\settings.json`. Each row has a
folder picker and an "open folder" button. A change applies at once, with no restart.

| Setting | What goes there | Default |
|---|---|---|
| Notes folder | one `.md` file per note | `Documents\Fairy Notes` |
| Captures folder | kept screenshots (`.png`) and recordings (`.mp4`) | `Pictures\Screenshots` |

- Screenshots pasted into a terminal as a path are saved in `<captures>\Pasted`. The
  Swift app uses a fixed `Pictures/Fairy Cursor` folder for this.
- Changing a folder does not move existing files. Notes in the old folder stop showing
  in "recent notes" until the user moves them.
- If a folder is missing or not writable, the app creates it, or falls back to the
  default and shows a hint.
- Capabilities never read the setting directly. They ask the host (`Host::captures_folder`),
  as in the Swift `FairyHost.capturesFolder`.

## Card size the user chooses

The pile should not get in the user's way. The default card is 80 x 60 logical px
(the macOS original uses 112 x 84). A setting in the settings window changes it:

| Choice | Card box (logical px) |
|---|---|
| Small (default) | 80 x 60 |
| Medium | 96 x 72 |
| Large (as on macOS) | 112 x 84 |

- Stored as `card_size` in `settings.json`. A change applies at once, with no restart.
- Today the size is the constant `layout::THUMB_MAX`. The setting replaces it with a
  value passed to the renderer (in `Scene`) and to the follower, which uses the card
  box to flip at screen edges. The hint text size does not change.

## Shortcuts the user chooses

Every action that has a keyboard shortcut can be changed, or left without one, in the
settings window. Stored as `shortcuts` in `settings.json`; a change applies at once.

| Action | Default | Available |
|---|---|---|
| Restore last screenshot | none | now |
| Discard top card (same as one shake) | none | now |
| Discard all cards | none | now |
| Pin top card | none | now |
| Open settings | none | now |
| New note at the cursor | none (the gesture is hold Alt and click) | with M7 |
| Start or stop recording | Ctrl+Shift+6 | with M8 |
| Pen | Ctrl+Shift+7 | with M8 |
| Highlighter | Ctrl+Shift+8 | with M8 |

- Taking a screenshot uses the Windows shortcuts (`Win+Shift+S`, `PrtScn`, `Alt+PrtScn`,
  `Win+PrtScn`). The app only notices them; they are changed in Windows, not here. The
  settings window says so next to the Screenshots row.
- A shortcut is one key with at least one of Ctrl, Alt, Shift, Win. Function keys may be
  used alone. Keys Windows reserves (for example `Win+L`, `Ctrl+Alt+Del`, `Alt+Tab`) and
  the screenshot keys are refused, with the reason shown.
- Two actions cannot share a shortcut. If Windows refuses to register one because another
  program holds it, the row shows that and the old shortcut stays.
- Each row has a recorder: click it, press the keys, and it is set; Esc cancels, Backspace
  clears.
- Shortcuts are global hotkeys (`RegisterHotKey`), owned by one thread with a message
  loop. The keys used while drawing (undo, clear, put the pen down) are only active
  during a recording and are not global.

## Tool menu at the cursor

Holding Alt does not open a note directly. It opens a small menu of tools under the
cursor, so users do not have to guess what the gesture does.

- The menu opens anywhere except over a text field, a password field or a terminal (they
  take typed text). The first version opened it only over empty space, as in Swift; that
  proved hard to find (holding Alt over a button or text did nothing), so it changed.
- The menu lists tools only: **New note**, **Record screen** and **Pin to screen** today. Everything else
  (restore, discard, settings, recent notes) stays in the tray menu.
- Each row shows the tool's keyboard shortcut, so users learn the direct way.
- The first tool sits under the cursor, so "hold Alt, click" still makes a note in one
  click. Moving to another row picks that tool. Releasing Alt closes the menu.
- A tool is listed only when it can run (Record screen appears when recording is built
  and switched on).
- Tools are entries in one list, each with a condition for when it is offered. This is
  where later, context-aware tools go.
- The settings window has a "Tool menu" page: one row per tool, dragged (or moved with
  Alt+Up and Alt+Down) into the user's order, and a switch per row to leave the tool out of
  the menu (the tool and its shortcut keep working). The first shown tool is the one under
  the cursor. With every tool hidden the menu never opens, and the page says so. A tool
  whose capability is off stays in the list, dimmed with a note. Stored as `tool_menu` in
  `settings.json`, `[{"id": "note", "shown": true}, {"id": "record", "shown": true},
  {"id": "pin", "shown": true}]`;
  read leniently (unknown ids dropped, missing ones added at the end, shown). A change
  applies at once.

Ideas for later, not planned yet:

- Tools offered from context, for example a grammar review when the user is typing in a
  text field.

## Pinned cards

Decided for v0.1.0. A carried card can be left on the screen as a picture, for example to
compare it with what the user is working on.

- **Pin to screen** in the tool menu (offered only while the user carries a card) or the "Pin
  top card" shortcut pins the top card where the menu opened (or at the cursor). It leaves
  the pile like a delivered card; "Restore last screenshot" can still bring it back.
- A pin is a borderless, topmost tool window on its own thread (`pins.rs`), with DWM rounded
  corners and shadow like a note. It shows at the card's natural size, made smaller to fit 40%
  of the work area, with a short grow-in (none with reduced motion), and never takes the
  foreground when it appears.
- Drag moves it, the wheel resizes it around the cursor (64 DIPs to the monitor), Ctrl+wheel
  changes its opacity (30% to 100%). Hovered, it shows a close button; Esc closes it while
  hovered or focused. A right-click opens a native menu: Carry again, Copy, Save as PNG…
  (starts in the captures folder), Close.
- A double-click carries it again (back on top of the pile). With cards in hand it is refused
  with the usual wobble: a pin takes no cards.
- A recording pins as its first frame with the length badge; no playback.
- Pins are in screenshots and recordings, are not restored after a restart, and own their
  picture: closing one frees it. At most 20 at once.

## Notes editor: Notion-like writing

The macOS app shows Markdown markers dimmed. This port goes further, as its owner asked:
writing a note should feel like Notion, while the file on disk stays plain Markdown.

- Live formatting: headings, bold, italic, code and links show formatted with their
  markers hidden. The markers reappear while the caret is inside the element.
- Markdown shortcuts convert as you type: `# `, `- `, `1. `, `[] `, `> `, three
  backticks, `---`, `**bold**`, `*italic*`, `` `code` ``, `~~strike~~`. One undo gives
  back the typed characters.
- Blocks behave: Enter continues a list, Enter on an empty item leaves it, Backspace at
  the start turns a block into text, Tab and Shift+Tab indent, numbered lists renumber.
- Clickable to-do checkboxes, drawn bullets and dividers, Ctrl+click opens a link.
- A slash menu (`/`) to pick a block type, with a filter.
- Formatting keys such as Ctrl+B, Ctrl+I, Ctrl+E, Ctrl+K.
- A dropped screenshot shows as an image block.
- Motion in the style of the settings window: short, and never while typing.

Out of scope: databases, pages in pages, drag handles, tables, columns, mentions.

Risk: this is built on the RichEdit control. If RichEdit cannot carry a part of it well,
that part is left out or done more simply; a custom text view is a separate, larger job.

## Hidden notes and the notes panel

Not in the macOS app; the owner asked for it.

- A note is shown, hidden or closed (put away). The header's hide button (left of delete)
  saves the note at once and fades it out like a put-away note. A hidden note stays open
  but off screen: it does not come back on the next start. A blank note is put away
  instead.
- The hidden state lives only in the app's windows file (`isHidden`, written only when
  true), so the note files stay as the Mac app writes them and older files read as shown.
- The tray's "Notes…" opens the notes panel next to the tray icon (the corner by the
  taskbar when the icon is in the overflow area). It is a popup of the notes thread
  (`notes/panel.rs`) in the settings window's style: every note, newest edit first, with
  its title, a preview, when it changed and its state. A filter field at the top keeps the
  notes whose title or text holds every typed word, ignoring case. Up and Down move, Enter
  shows the note and brings it to the front, Esc clears the filter or closes the panel,
  Ctrl+H shows or hides, Ctrl+Shift+Delete deletes (to the Recycle Bin, without asking,
  as the note's own delete). Each row has the same two actions; "Hide all" hides the shown
  notes and "Show all" shows the hidden ones (closed notes stay closed). The list follows
  changes while the panel is open. The panel closes when another window takes the
  foreground.
- A shown-again note grows in like a new note. The tray's "Recent Notes" lists the five
  notes changed last; choosing a hidden one shows it.

## Milestones

Each one ends with a check that must pass before the next starts.

### M0. Spikes (throwaway code)

1. Tell a snip (`Win+Shift+S`, `PrtScn`) from a normal image copy on the clipboard.
2. UI Automation: find the editable element under the cursor, then paste an image, in
   Chrome, Slack, Word, Notepad, Windows Terminal.
3. Overlay window: click-through, topmost, absent from a screenshot.
4. `uiAccess` build: signed with a self-signed cert, installed in Program Files, pastes
   into an elevated Notepad with no prompt at start.

Check: a short note per spike with what worked. If spike 1 fails, the fallback is our
own capture hotkey. If spike 4 fails, revisit the admin section.

### M1. Workspace and core

Create the workspace. Translate `core` and all 11 test files (recording files included,
they are small).

Check: `cargo test -p igorio-core` passes with the same cases as the Swift tests.

### M2. Overlay, follower, tray

Overlay window, pile drawing (cards, tilt, shadow, hint text), spring follower with
edge flipping, tray menu with Quit, `--demo N` flag that loads N test images.

Check: `app --demo 3` shows three cards following the cursor on two monitors with
different DPI, and they do not appear in a screenshot.

### M3. kit and screenshots capability

`Capability` and `Host` traits, registry, screenshot detection, "keep every
screenshot" saving.

Check: `Win+Shift+S` puts a card on the pile; copying an image in a browser does not.

### M4. Drops

Mouse hook and double-click, `uia.rs`, drop router, paste with clipboard restore,
terminal path paste, Explorer and desktop save, shake to drop, restore last screenshot.

Check: manual matrix. Paste into Notepad (rejected, wobble), Word, Chrome text area,
Slack, Windows Terminal (path); save into an Explorer folder and the desktop; shake
drops one, more shaking drops all; the user's clipboard is back afterwards.

This is the first usable version.

### M5. Settings and guide

Settings JSON, settings window (capability switches, the two folder pickers, card size,
shortcuts,
launch at login), first-run guide. The folder settings exist from M3 as JSON values; this adds
the pickers.

The guide (`guide.rs`, `guide/scenes.rs`) is a window in the settings window's style on its
own thread, not topmost, so the gestures work while it is open. It opens on start until
`guide_seen` is set in settings.json, and from the tray's "Guide…". After a welcome page,
each switched-on gesture gets a page with a looping picture, the user's own key chips and a
"Try it now" line that turns into "Done" when the UI loop reports the gesture.

Check: switching a capability off stops its hooks; settings survive a restart; after
changing the captures folder, the next kept screenshot lands in the new folder; changing
the card size resizes the pile on screen at once.

### M6. Packaging

The exe embeds a Per-Monitor V2 manifest, its Cargo version, and an icon made from
the tray drawing. A session mutex keeps Start menu and sign-in launches from running
two copies. Inno Setup installs to Program Files, adds a Start menu shortcut, and
offers launch at sign-in using the same `HKCU\Run` value as Settings. Uninstall keeps
the user's settings, captures and notes.

With the Windows SDK and Inno Setup 6 installed, run `./installer/build.ps1` from
the repo root. It builds the UI access and fallback exes under `target/dist` and writes
`target/dist/installer/Igorio-Setup-<version>.exe`.

Check: clean install on a second user account; starts with no prompt; pastes into an
elevated window.

### M7. Notes

Summoner (hold `Alt`, click), note windows, markdown editor, store, recent notes menu,
drop screenshots into a note.

The tool menu opens anywhere except over text fields. This was chosen after the Swift rule
(empty space only) proved hard to discover.

Open point: a lone `Alt` tap focuses the menu bar on Windows. The keyboard hook must
swallow it, or the modifier changes to `Ctrl+Alt`.

Check: create, edit, close, reopen a note; the `.md` file matches; `Alt` does not
open app menus when the pill was shown.

### M8. Recording

Hotkeys, capture and encode, viewport zoom and pan, blue frame, ink overlay, larger
cursor, finished recording goes on the pile.

Check: a recording with zoom and ink plays in the Windows media player; the blue frame
is not in the video; the ink is.

## Known limits

- The Snipping Tool notification cannot be turned off from code.
- App-specific paste rules from the Swift code (Google Docs, Figma, Slack) need
  retesting one by one on Windows.
- The macOS glass look is not reproduced; the port uses its own flat style.
