# Windows 11 screenshot detection

## Proposed rule

The documented clipboard APIs expose the current data formats and owner window. There is no documented flag that says an image came from a system screenshot. Treat the result as a guess based on how the image arrived. [Microsoft clipboard formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats)

Require a bitmap, DIB or PNG without text, HTML or file-transfer formats. Then call it `SCREENSHOT` when a likely snipping overlay is visible or went away within 3 seconds, a candidate capture process owns the clipboard within 15 seconds of a capture shortcut, or a Print Screen shortcut was observed within 1.5 seconds. Otherwise call it `OTHER`. Use each set of capture signals once. The time windows and overlay size threshold are starting points for testing.

A new image in the known folder produces a `FILE` line. If a clipboard image and a file have the same pixel size and arrive within 4 seconds, the watcher links the nearest unpaired events with `MATCH same_event=likely`, in either order. The match includes the clipboard verdict so a missed classification is visible.

## Signals checked

| Signal | Status |
| --- | --- |
| Clipboard listener and sequence number | **Verified by running** listener registration and a read-only sequence dump. **Not verified** delivery and timing of updates from real captures. |
| Clipboard image, HTML, text and file-transfer formats | **Verified by running** a live format dump and classifier tests. The dump contained `PNG`, bitmap formats and file-transfer formats together. PNG and DIB size parsing passed on fixtures. **Not verified** the format sets or bitmap dimensions from each real capture and copy action. Windows can list synthesized formats, too. [Microsoft clipboard formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats) |
| Clipboard owner | **Verified by running** the owner window, PID and exe lookup. The existing item reported `svchost.exe`. **Not verified** whether captures report `SnippingTool.exe`, `ScreenClippingHost.exe`, a `Microsoft.ScreenSketch` package process, `explorer.exe`, or no owner on this PC. Explorer is logged and carries no weight on its own. |
| Foreground and overlay | **Verified by running** foreground lookup and hook registration. **Not verified** the capture window's class, process, visibility, focus and timing. The visibility check looks for a non-minimized, non-maximized, uncloaked capture-process window covering at least 90% of a monitor in both dimensions. [Microsoft foreground hook](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwineventhook) |
| Keyboard hook | **Verified by running** hook installation. **Not verified** which physical shortcut events Windows delivers on this PC, including Print Screen with the overlay setting on and off. The hook passes every keystroke through and logs only capture shortcuts. Injected events are logged but do not affect the verdict. [Microsoft keyboard hook](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc) |
| Clipboard history and cloud formats | **Not verified** whether captures carry `CanIncludeInClipboardHistory`, `CanUploadToCloudClipboard` or `ExcludeClipboardContentFromMonitorProcessing` on this PC. The first two are logged with their DWORD values when available and the third by presence. Microsoft documents these as settings any clipboard producer can supply, so they do not affect the verdict. [Microsoft clipboard history formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats#cloud-clipboard-and-clipboard-history-formats) |
| Screenshots folder and matching | **Verified by running** `ReadDirectoryChangesW` on the redirected OneDrive known folder, a real notification on a scratch PNG, and time/size matching tests. **Not verified** a file saved by either Windows capture path or a live clipboard/file pair. [Microsoft directory notifications](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw) |

An image-only copy can be misclassified if it lands just after a cancelled overlay or a Print Screen shortcut. A manual copy from Snipping Tool can also overlap a recent shortcut. Unrelated, same-size images can produce a false folder match. Genuine captures can be missed if Windows hides or renames the overlay, delivers a key late, supplies extra text/file formats, or the clipboard is busy or overwritten before inspection. File matching can miss a slow save, different dimensions, or a save redirected outside the known folder. If the manual tests show too many ambiguous cases, the roadmap's own capture hotkey gives the app a direct way to identify images it captures.

## Manual test

1. Run `cargo run --release` from this directory. Confirm `START` reports both hooks and `WATCH` shows the known folder. If the folder is absent, it can become active after the first saved screenshot.
2. Press Win+Shift+S and select a rectangle. Expect `KEY`, look for `FOREGROUND` and `OVERLAY`, then expect `CLIPBOARD ... verdict=SCREENSHOT`. Record the owner, class, formats and history/cloud fields.
3. Press Win+Shift+S and cancel with Esc. After 16 seconds copy an image. Expect `CLIPBOARD ... verdict=OTHER` for the copy.
4. With the Print Screen overlay setting off, press PrtScn. Expect a `KEY` line and `CLIPBOARD ... verdict=SCREENSHOT`.
5. With the Print Screen overlay setting on, press PrtScn and complete a snip. Expect a `KEY` line, overlay observations if detected, and `CLIPBOARD ... verdict=SCREENSHOT`.
6. Press Alt+PrtScn over an ordinary window. Expect `KEY shortcut=AltPrintScreen` and `CLIPBOARD ... verdict=SCREENSHOT` for a window-sized image.
7. Press Win+PrtScn. Expect `KEY shortcut=WinPrintScreen` and a `FILE` line. If Windows also writes the same-size image to the clipboard, expect `MATCH`.
8. Repeat Win+Shift+S with Snipping Tool auto-save enabled and pointed at the watched folder. Expect `CLIPBOARD`, `FILE` and `MATCH` with the same dimensions. [Microsoft Snipping Tool save settings](https://support.microsoft.com/en-us/windows/apps/use-snipping-tool-to-capture-screenshots.)
9. Away from any capture, copy a browser image, an image file in Explorer, some text, and a bitmap selection in an image editor. Expect `CLIPBOARD ... verdict=OTHER` in all four cases. Compare owner and formats with the captures.
10. Press Ctrl+C in the watcher console and expect `STOP`. Restore any settings you changed for the test.
