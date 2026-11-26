# Note gesture: manual test

Run `cargo run -p igorio-app --example summon_demo --release` in a console you can see
(add `-- --modifier ctrl-alt` for step 14). Every state change, lookup, swallowed click
and injected key is printed (`FAIRY_DEBUG ...`), and every note as `SUMMON at x,y: label | detail`.
"Pill" means the small rounded plate around the cursor.

1. **Desktop, hold and click.** Minimise windows, hold Alt over empty desktop (not on an
   icon) for half a second. Must: the pill appears around the cursor and follows it;
   `injected mask key vkE8` and `gate armed` are printed. Click. Must: `swallowed press`,
   `SUMMON ... Desktop`; no selection rectangle starts on the desktop. Release Alt.
2. **Desktop, hold and let go.** Hold Alt over the desktop until the pill shows, release
   without clicking. Must: the pill goes, no `SUMMON`, nothing else happens.
3. **Notepad (classic menu bar), menu check.** In a Notepad with a menu bar, hold Alt over
   a blank part of the window that is not text (for example the empty area right of the
   menu items or the status bar gap, if it shows a pill; over the text there must be no
   pill). If the pill showed, release without clicking. Must: the menu bar is NOT active
   afterwards (typing a letter types it, no menu item is highlighted). Then tap Alt
   quickly. Must: the menu bar IS active as usual (`Esc` to leave), and the demo printed
   no `injected` and no `gate armed` line for that tap.
4. **Notepad text.** Hold Alt over the text for a second. Must: no pill, `empty false`.
   Release: the menu bar activates as usual (this was an ordinary Alt use).
5. **Chromium browser page.** Hold Alt over a page margin. Must: pill; click makes
   `SUMMON ... <host>` with the URL in the detail. Over a link, a text field or the tab
   strip: no pill. Then hold Alt over the margin until the pill shows, release without a
   click. Must: the browser menu button is not focused, the address bar does not take focus.
6. **Alt+click while no pill.** In the browser, Alt+click a link quickly (before the pill
   could show, or over the link itself where none shows). Must: the browser does its own
   Alt+click (download the link); no `swallowed` line.
7. **Leave empty space.** Hold Alt over a page margin until the pill shows, move onto a
   link: the pill goes (`gate disarmed`). Alt+click the link now. Must: the browser gets
   the click (no `swallowed` line, no `SUMMON`).
8. **File Explorer.** Hold Alt over the empty part of a folder's file list. Must: pill;
   click makes `SUMMON ... File Explorer` with the folder in the detail; no item is
   selected. Over a file: no pill. Hold until the pill shows, release: the command bar's
   key tips / menu do not appear.
9. **Office (if installed)**, Word or Excel: hold Alt over the grey area around the page
   or below the sheet until a pill shows (if one shows), release. Must: no key tips appear.
   A quick Alt tap still shows the key tips.
10. **Visual Studio Code.** Hold Alt over an empty editor area (no file open) until the pill
    shows, release. Must: the menu bar does not toggle or take focus. A quick Alt tap
    still behaves as before.
11. **Alt+Tab, Alt+F4, access keys, Alt+drag.** Alt+Tab (quick, and slow after holding Alt
    over the desktop until the pill shows): switching works both times, no `SUMMON`.
    Alt+F4 on a spare Notepad: it closes. In a dialog, Alt+underlined letter works.
    Press the mouse button, then hold Alt and drag: no pill.
12. **AltGr** (on a layout with AltGr, e.g. German or Polish): hold AltGr over the desktop.
    Must: no pill, no `injected` line; AltGr+key still types its character.
13. **Stopping.** Make a note, then stop the demo with Ctrl+C. Must: `stopped (summoner
    thread ended: true ...)`; afterwards Alt and Ctrl are not stuck (type in Notepad: plain
    letters, no menu). Start it again, hold Alt over the desktop until the pill shows, and
    with Alt still held end the demo from Task Manager. Must: the pill is gone, and after
    releasing Alt the keyboard and mouse behave normally.
14. **Ctrl+Alt mode** (`--modifier ctrl-alt`): plain Alt does nothing (no pill, menus as
    usual); holding Ctrl+Alt over the desktop shows the pill, click makes a note, no
    `injected` line ever.

Not covered here: elevated windows (Run as administrator: no pill over them, by design;
when an elevated window is in front, Windows hides the keys from our hook and the gesture
does not start), pen and touch (a tap while the pill shows must not make a note).
