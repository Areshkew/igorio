# Tool menu at the cursor: manual test

Run `cargo run -p igorio-app --example summon_demo --release` in a console you can see.
It offers one tool, New note, as the app does today. Add `-- --tools note,record` for two
rows (steps 3 and 4), `-- --modifier ctrl-alt` for step 18. Every state change, lookup,
swallowed click and injected key is printed (`FAIRY_DEBUG ...`), and every chosen tool as
`TOOL <id> at x,y: label | detail`. "Menu" means the small rounded list of tools that opens
under the cursor. It opens anywhere except over a text field, a password field or a
terminal.

## The menu

1. **One row, default under the cursor.** Minimise windows, hold Alt over the desktop
   (between icons) for half a second. Must: a menu with one row, "New note", opens with
   its row under the cursor (the arrow sits in the row's left gutter, before the icon), the
   row is highlighted; `menu open ... with [note]`, `injected mask key vkE8` and `gate armed`
   are printed. The cursor did not have to move. Click without moving. Must: the row
   flashes briefly and the menu fades; `swallowed press`, `TOOL note at ... Desktop`; no
   selection rectangle starts on the desktop. Release Alt.
2. **Releasing Alt.** Hold Alt over the desktop until the menu opens, release without
   clicking. Must: the menu fades out, no `TOOL`, nothing else happens.
3. **Two rows** (`--tools note,record`). Hold Alt over the desktop. Must: "New note" under
   the cursor, "Record screen" below it with key chips `Ctrl` `Shift` `6` on the right;
   New note has no chips. Move down onto Record screen: the highlight slides there. Click.
   Must: `TOOL record at x,y` where x,y is where the menu opened. Repeat and click New note
   without moving: `TOOL note`.
4. **Pointer over the menu.** With two rows, hold Alt and move the pointer all over the
   menu, slowly and fast, including its edges. Must: the menu stays open, no `lookup` line
   while the pointer is inside it, the highlight follows the row under the pointer.
5. **A click outside.** Hold Alt until the menu opens, move off the menu onto empty desktop
   and click. Must: the menu closes, no `TOOL` line, no `swallowed press` line, and the
   desktop gets the click (it deselects its icons; press and drag instead and a selection
   rectangle starts, so the app got the press and, on letting go, the release). Repeat in a
   Notepad window: hold Alt over its status bar until the menu opens, then
   click into the text: the caret moves there. Also click just outside the menu's rounded
   edge, in its shadow: that click passes too. A click on the card's padding above the first
   row or below the last runs that row.
6. **Near each screen edge.** Hold Alt with the cursor a few pixels from the left, right,
   top and bottom edges of the work area (just above the taskbar), and in each corner, on
   every monitor you have (and one at 125% or 150% if you can). Must: the whole menu is on
   screen, the first row is under the cursor every time; near the bottom edge the menu
   opens upward (`upward` in the log) and New note is the bottom row, still under the
   cursor.
7. **Look.** In light and dark app mode (Settings > Personalization > Colors): the menu
   matches the settings window and the notes' slash menu (white or dark card, soft shadow,
   blue highlight, grey chips). With Settings > Accessibility > Visual effects >
   Animation effects off, it opens, moves its highlight and closes without animation.

## The gesture rules (unchanged)

8. **Notepad (classic menu bar), menu bar check.** In a Notepad with a menu bar, hold Alt
   over the menu bar or the status bar until the menu opens, release without clicking. Must: the menu bar is NOT active afterwards (typing a letter types it, no menu
   item is highlighted). Then tap Alt quickly. Must: the menu bar IS active as usual (`Esc`
   to leave), and the demo printed no `injected` and no `gate armed` line for that tap.
9. **Notepad text.** Hold Alt over the text for a second. Must: no menu, `allowed false`.
   Release: the menu bar activates as usual (this was an ordinary Alt use). Notepad keeps
   its caret and focus whenever the menu showed. Same over a terminal window (Windows
   Terminal, cmd) and a password field: no menu.
10. **Chromium browser page.** Hold Alt over a page margin, then over page text, a link, a
    button and the tab strip. Must: the menu every time; a click on New note over the page
    makes `TOOL note ... <host>` with the URL in the detail. Over a text field (a search box,
    the address bar): no menu. Hold over the page until the menu opens, release without a
    click. Must: the browser menu button is not focused, the address bar does not take focus.
11. **Alt+click while no menu.** In the browser, Alt+click a link quickly (before the menu
    could open, under 0.3 s). Must: the browser does its own
    Alt+click (download the link); no `swallowed` line.
12. **Move onto a text field.** Hold Alt over page text until the menu opens, move off the
    menu onto a text field: the menu closes (`gate disarmed`). Click into the field now.
    Must: the field gets the click (no `swallowed` line, no `TOOL`). Move off the menu onto a
    link or a button instead: the menu stays. Back from a text field onto page text: the
    menu opens again there.
13. **File Explorer.** Hold Alt over the empty part of a folder's file list. Must: the menu;
    New note makes `TOOL note ... File Explorer` with the folder in the detail; no item is
    selected. Over a file or the command bar: the menu too. Over the address bar or the
    search box: no menu. Hold until the menu opens, release: the command bar's
    key tips / menu do not appear.
14. **Office (if installed)**, Word or Excel: hold Alt over the ribbon or the grey area
    around the page until the menu opens, release. Must: no key tips appear. A
    quick Alt tap still shows the key tips.
15. **Visual Studio Code.** Hold Alt over an empty editor area (no file open) until the menu
    opens, release. Must: the menu bar does not toggle or take focus. A quick Alt tap still
    behaves as before.
16. **Alt+Tab, Alt+F4, access keys, Alt+drag.** Alt+Tab (quick, and slow after holding Alt
    over the desktop until the menu opens): switching works both times, the menu closes, no
    `TOOL`. Alt+F4 on a spare Notepad: it closes. In a dialog, Alt+underlined letter works.
    Press the mouse button, then hold Alt and drag: no menu.
17. **AltGr** (on a layout with AltGr, e.g. German or Polish): hold AltGr over the desktop.
    Must: no menu, no `injected` line; AltGr+key still types its character.
18. **Ctrl+Alt mode** (`--modifier ctrl-alt`): plain Alt does nothing (no menu, menus as
    usual); holding Ctrl+Alt over the desktop opens the menu, a click on New note makes a
    note, no `injected` line ever; releasing either key closes the menu.
19. **Stopping.** Choose a tool, then stop the demo with Ctrl+C. Must: `stopped (summoner
    thread ended: true ...)`; afterwards Alt and Ctrl are not stuck (type in Notepad: plain
    letters, no menu). Start it again, hold Alt over the desktop until the menu opens, and
    with Alt still held end the demo from Task Manager. Must: the menu is gone, and after
    releasing Alt the keyboard and the mouse behave normally (the next click reaches the
    app: the gate lived in the killed process).

## In the app

20. Run the app with notes on. Hold Alt over the desktop: the menu has one row, New note
    (Record screen is not built yet). A click opens a note there. The New note shortcut (if
    one is set in Settings > Shortcuts) still opens a note at the cursor directly, with no
    menu; once set, its chips show on the New note row the next time the menu opens.

Not covered here: elevated windows (Run as administrator: no menu over them, by design;
when an elevated window is in front, Windows hides the keys from our hook and the gesture
does not start), pen and touch (a tap while the menu shows must not run a tool), and
`Summoner::open_menu_at_cursor` (no shortcut opens it yet; its rules are unit-tested).
