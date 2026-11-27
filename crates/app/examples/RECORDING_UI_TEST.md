# Recording UI: manual test

Run `cargo run -p igorio-app --example recording_ui_demo --release -- --chords --debug` in
a console you can see. Nothing is recorded; the blue outline, the pill and the ink window
appear on the monitor under the cursor. "Zoom" below means: hold Alt and turn the wheel
(add `--ctrl-alt` to the command for step 13). Tools: Ctrl+Shift+7 pen, Ctrl+Shift+8
highlighter (or type `pen` / `hl` and Enter in the console if those chords are taken). Quit
with `q` and Enter.

1. **Start.** Must: a thin blue outline around the whole monitor and a pill at the top centre
   that fades in, reads `0:00 Alt + wheel to zoom · Ctrl+Shift+7 pen · …` and counts
   seconds. Leave it alone for 10 s: Task Manager shows the demo near 0% CPU.
2. **Zoom in Notepad.** Click into a Notepad with text and a menu bar. Zoom in: the outline
   shrinks smoothly around the cursor; the text does not scroll. Wheel without Alt: the
   text scrolls normally, the outline does not change. Let go of Alt: the menu bar is NOT
   active (type a letter: it is typed).
3. **Hold to move, let go to stay.** Zoomed in, hold Alt and move the mouse: the outline
   glides after the cursor. Release Alt and move: the outline stays.
4. **Double tap.** Tap Alt twice quickly: the outline eases back to the whole screen; the
   Notepad menu bar does not flicker or stay active. Tap twice slowly (about half a second
   apart): nothing happens.
5. **Browser.** In Chrome or Edge on a long page: zoom does not scroll the page and does not
   zoom the page; plain wheel scrolls; Ctrl + wheel still zooms the page (not ours).
6. **Explorer and VS Code.** In File Explorer (folder view) and in VS Code (editor): zoom
   works, the window does not scroll, and releasing Alt shows no key tips, no menu focus.
   Alt+Tab and Alt+F4 still work while the demo runs.
7. **Pen latency.** Ctrl+Shift+7: the pill says `pen`, the cursor becomes a blue dot. Draw
   fast circles and a signature: the line stays attached to the cursor tip, with no gaps at
   fast flicks and no visible lag. The window below keeps its active title bar.
8. **Clicks go nowhere.** With the pen in hand, drawing over a button or a link does not
   press it; the app underneath keeps the focus and its caret.
9. **Drawing keys.** In Notepad with the pen in hand: Ctrl+Z removes the last stroke and
   Notepad's text does not change; Backspace and Delete clear all ink, Notepad's text does
   not change; Esc puts the pen down and the ink fades away in about a quarter second.
   After that, Ctrl+Z, Backspace and Esc reach Notepad again.
10. **Highlighter.** Ctrl+Shift+8 over text: a soft yellow band, text readable through it;
    pen strokes drawn after it sit on top. Ctrl+Shift+8 again: put down, the ink fades.
11. **Tool switch.** Pen in hand, press Ctrl+Shift+8: the highlighter is in hand and the
    pen strokes stay. Press Ctrl+Shift+8 again: put down.
12. **Second monitor** (if any). Quit, move the cursor to the other monitor, start the demo
    again: the outline, the pill and the ink cover that monitor only, at its own scale
    (try a monitor at a different %); zoom and drawing work there; nothing shows on the
    first monitor.
13. **Ctrl+Alt mode.** Start with `--ctrl-alt`: Alt + wheel scrolls as usual, Ctrl+Alt +
    wheel zooms. On a layout with AltGr (German, French), AltGr + wheel does not zoom.
14. **Quit and kill.** Type `q`: everything fades out, `bye (clean)`. Start again and close
    the console window instead: no key stays down (type in another app; Alt+Tab works),
    the wheel scrolls everywhere, Ctrl+Shift+7 and 8 type normally in other apps again.
