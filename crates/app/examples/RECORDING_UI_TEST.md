# Recording UI: manual test

Run `cargo run -p igorio-app --example recording_ui_demo --release -- --chords --debug` in
a console you can see. Nothing is recorded; the blue outline, the control strip and the ink
window appear on the monitor under the cursor. "Zoom" below means: hold Alt and turn the
wheel (add `--ctrl-alt` to the command for step 13). Tools: Ctrl+Shift+7 pen, Ctrl+Shift+8
highlighter, or the strip's buttons. Quit with `q` and Enter (the strip's stop button also
quits the demo).

1. **Start.** Must: a thin blue outline around the monitor and, at the top centre, a small
   strip that fades in: a red dot, `0:00` counting seconds, and three icon buttons (pen,
   highlighter, stop). No sentence of text. After about a second a hint fades in under it,
   "Hold [Alt] and scroll to zoom", and goes by itself a few seconds later. Leave it alone
   for 10 s: Task Manager shows the demo near 0% CPU.
2. **Zoom in Notepad.** Click into a Notepad with text and a menu bar. Zoom in: the outline
   shrinks smoothly around the cursor, the strip shows a `2.0x`-style badge after the time;
   the text does not scroll. Wheel without Alt: the text scrolls normally. Let go of Alt:
   the menu bar is NOT active (type a letter: it is typed).
3. **Badge and double tap.** Click the badge: the outline eases back to the whole screen and
   the badge goes. Zoom in again and tap Alt twice quickly: the same; the Notepad menu bar
   does not flicker. Tap twice slowly: nothing happens.
4. **The strip never takes the focus.** With Notepad active, click each strip button and
   the time: Notepad's title bar stays active, its caret stays, nothing is typed or clicked
   in Notepad under the strip.
5. **Tooltips.** Rest the pointer on each button for half a second: a small tip under it
   names the action with its keys as chips (Pen Ctrl Shift 7, Highlighter …, Stop recording
   Ctrl Shift 6; the badge: Whole screen · tap Alt twice). Moving away hides it. Buttons
   highlight softly on hover and darken while pressed.
6. **Pen from the strip.** Click the pen button: it shows selected, the cursor becomes a blue
   dot over the screen, and once per run a tip shows "Ctrl Z undo · Backspace clear · Esc
   done" for two seconds. Draw fast circles: the line stays attached to the tip.
7. **The strip with a tool in hand.** Pen in hand, move over the strip: the normal arrow
   shows, hover works, clicking its buttons works, and pressing on it never draws. Click
   the pen button again: the pen is put down and the ink fades.
8. **Drag.** Drag the dot or the time left and right: the strip slides along the top edge
   and stays on the monitor; it cannot be dragged down. Its tips follow it.
9. **Drawing keys.** In Notepad with the pen in hand: Ctrl+Z removes the last stroke and
   Notepad's text does not change; Backspace and Delete clear all ink; Esc puts the pen down.
   After that, Ctrl+Z, Backspace and Esc reach Notepad again.
10. **Highlighter.** Ctrl+Shift+8 over text: a soft yellow band, text readable through it;
    the highlighter button shows selected. Pen strokes drawn after it sit on top.
11. **Browser, Explorer, VS Code.** In each: zoom does not scroll or zoom the page; plain
    wheel scrolls; releasing Alt shows no key tips or menu focus; Alt+Tab and Alt+F4 work.
12. **Second monitor** (if any). Quit, move the cursor to the other monitor, start again: the
    outline, the strip and the ink cover that monitor only, at its own scale; the strip sits
    top centre there and is no wider than at 100% in DIPs.
13. **Ctrl+Alt mode.** Start with `--ctrl-alt`: the start hint reads "Hold [Ctrl] [Alt] and
    scroll to zoom"; Alt + wheel scrolls, Ctrl+Alt + wheel zooms; AltGr + wheel does not zoom.
14. **Stop and kill.** Click the stop button: the demo prints `stop button`, everything fades
    out, `bye (clean)`. Start again and close the console window instead: no key stays
    down, the wheel scrolls everywhere, Ctrl+Shift+7 and 8 type normally in other apps.
