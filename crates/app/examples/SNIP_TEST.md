# Test Windows screenshot detection

Use a PowerShell window. Give each capture time to finish before starting the next. Save the console output if a line is missing or has a different source. The watcher reads the clipboard and prints event metadata.

1. Run $env:FAIRY_DEBUG='1' followed by cargo run -p igorio-app --example snip_watch --release. Expect START detector, Ctrl+C to stop and a LOG line containing FOLDER watching. The known folder can be missing until the first saved screenshot.
2. Press Win+Shift+S, then drag a rectangle. Expect CaptureUiShown, CaptureUiGone, and one Captured source=Snip. The two overlay events can surround Captured in either order.
3. Press Win+Shift+S again and press Esc. Expect CaptureUiShown, CaptureUiGone and no Captured line. Immediately copy a bitmap selection in Paint. Expect Ignored reason="image without recent capture evidence".
4. Turn off “Use the Print screen key to open screen capture” under Settings, Accessibility, Keyboard. Press PrtScn. Expect one Captured source=PrintScreen and no CaptureUiShown for that key.
5. Turn the setting on. Press PrtScn and drag a rectangle. Expect CaptureUiShown, CaptureUiGone, and one Captured source=Snip.
6. Focus an ordinary window and press Alt+PrtScn. Expect one Captured source=WindowPrintScreen, with the window’s dimensions.
7. Press Win+PrtScn. Expect one Captured source=FullScreenFile with file=Some(...) and a LOG line containing FILE path=. If this machine also changes the clipboard to the same-size image, expect LOG containing PAIR matched and still only one Captured line. If there is no clipboard update, the paired case is covered by cargo test -p igorio-app pairs_file_and_clipboard_once_in_either_order.
8. After five seconds, right-click a picture in a browser and choose Copy image. Expect Ignored reason="image accompanied by HTML or text" when the browser includes HTML or text, or Ignored reason="image without recent capture evidence" for a bitmap-only copy.
9. In File Explorer, select a PNG file and press Ctrl+C. Expect Ignored reason="file-transfer format".
10. In Paint, select pixels and press Ctrl+C. Then copy ordinary text from a text editor. Expect Ignored reason="image without recent capture evidence" for an image-only copy and Ignored reason="no supported image format" for the text.
11. Run cargo test -p igorio-app screenshot_reader_only_observes_new_foreign_sequences in a second terminal. Expect screenshot_reader_only_observes_new_foreign_sequences ... ok. After UI integration, paste a held image through Igorio and let it restore the prior clipboard. Expect zero new Captured events and zero foreign clipboard signal lines in Igorio’s debug log for its stage, render and restore. The standalone watcher belongs to a separate process.
12. Press Ctrl+C in the watcher and expect STOP detector. Restore the original Print Screen setting.
