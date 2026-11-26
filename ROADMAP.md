# Roadmap: fairy cursor for Windows, in Rust

Source: https://github.com/Rrrima/fairy-cursor at commit `3d5f655` (MIT).
About 8,500 lines of Swift plus 1,500 lines of tests. Its `ausm` dependency
(screenshot and cursor monitoring) is mac-only and is replaced, not translated.

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
8. Set Per-Monitor V2 DPI awareness in the manifest only. Get DPI with
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

Chosen design: **`uiAccess="true"` in the release manifest.**

- The app runs as the normal user. No UAC prompt at start, no admin rights at run time.
- Windows lets a `uiAccess` process read and drive elevated windows.
- Requirements: the exe is signed, the certificate is trusted on the machine, and the
  exe is installed under `C:\Program Files`. So the installer needs admin once.
- For a personal build: a self-signed certificate added to the machine's Trusted Root
  store works. For public release: a real code-signing certificate.
- Autostart: normal `Run` registry key works, because the process is not elevated.

Dev builds (`cargo run`) have no `uiAccess` manifest, because Windows refuses to start
an unsigned `uiAccess` exe. In dev builds elevated windows are ignored: when the window
under the cursor is elevated, the overlay shows a "can't reach this window" hint.

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

Check: switching a capability off stops its hooks; settings survive a restart; after
changing the captures folder, the next kept screenshot lands in the new folder; changing
the card size resizes the pile on screen at once.

### M6. Packaging

Manifest (DPI awareness v2, `uiAccess` in release), signing script, installer to
Program Files, autostart.

Check: clean install on a second user account; starts with no prompt; pastes into an
elevated window.

### M7. Notes

Summoner (hold `Alt` on empty space, click), note windows, markdown editor, store,
recent notes menu, drop screenshots into a note.

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
