# UI Automation paste spike

From this directory, run `cargo run --release -- inspect` to log a double-click, or
`cargo run --release -- paste` to log it and attempt delivery. Use a scratch
document, draft, or terminal prompt. Let the clipboard restore finish before
stopping paste mode with Ctrl+C.

## Verdict rule

A recognized terminal executable wins, even if UIA times out. On the desktop or
inside the active Explorer folder view, a container hit with no item or chrome
ancestor counts as background. The Shell supplies the filesystem path. Otherwise
the rule checks the hit and up to six ancestors, stopping at a window or document
boundary. It looks for a writable text pattern, a writable text control value,
or a focusable Edit control with no Value pattern. Read-only and password
controls are excluded. A same-process focused editable control can qualify
when its rectangle contains the click. Everything else is `CANVAS_OR_UNKNOWN`.

| macOS roles in the reference | Windows UIA facts used here |
| --- | --- |
| AXTextField, AXTextArea, AXSearchField | Edit; search fields commonly expose Edit |
| AXComboBox | ComboBox with writable Value, or an Edit child |
| AXSecureTextField | IsPassword |
| AXEditableAncestor, settable text value on a generic role | Writable ancestor, TextEdit, or Text with IsReadOnly=false |
| AXWindow, AXApplication, AXSheet, AXWebArea | Window and Document serve as traversal boundaries. UIA has no one-to-one mapping for application and sheet. |
| Finder AXScrollArea, AXGroup, AXList, AXOutline, AXTable, AXBrowser, AXSplitGroup, AXLayoutArea, AXGrid | Pane, Group, List, Table, DataGrid, or Custom in a confirmed shell view |
| AXRow, AXCell, toolbar and menu roles | ListItem, DataItem, TreeItem, HeaderItem, ToolBar, Menu and related controls block a background verdict |

## What ran

The release build completed without warnings. Six in-memory verdict tests passed.
The read-only self-test created IUIAutomation, printed the live hit, ancestors,
and focus, and round-tripped two in-memory HGLOBAL formats while skipping a
mock bitmap handle. The clipboard check used process memory exclusively. One
run saw a Chromium-backed web editor with Value, Text, and TextEdit enabled
and both read-only flags false. No accessibility settings were changed for that
run. The interactive modes and application results below are unverified.

## What the APIs suggest

Chrome 138 and later enable the native UIA provider by default. Chromium can
enable its web-content tree in response to UIA use. Electron documents automatic
accessibility activation for detected assistive technology and an app opt-in.
Whether the first plain UIA query exposes the Chrome page or Slack composer
on this desktop needs a manual check.
[Chrome](https://developer.chrome.com/blog/windows-uia-support-update) ·
[Chromium source](https://chromium.googlesource.com/chromium/src/+/c7d7fd807032908a70d2b29f58d730bbe1e304dc/content/browser/accessibility/browser_accessibility_state_impl_win.cc) ·
[Electron](https://www.electronjs.org/docs/latest/tutorial/accessibility)

The hook thread pumps messages and forwards every mouse event. UIA uses a
windowless MTA worker and cached properties. The controller stops waiting
after 250 ms. A blocked COM call can outlive that deadline and delay later
lookups. Explorer queries run on the controller in an STA and can take
longer than 250 ms.
The controller processes one gesture at a time, so double-clicks during a
pending paste or slow Shell query can be skipped.
[UIA threading](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-threading) ·
[Explorer folder lookup](https://devblogs.microsoft.com/oldnewthing/20040720-00/?p=38393)

Paste mode stages PNG and DIB, or a raw Unicode temp path for terminals.
It uses the 8-second fallback from PastePolicy because this clipboard path
cannot observe reads. A newer sequence cancels restore. The log lists any
opaque, owner-managed, or unreadable formats skipped. Clipboard contention or
forced exit can prevent restoration. A selected word can be replaced. Virtual
Explorer folders may lack a path. The unsigned spike skips higher-integrity
targets and reports access denial when Windows exposes it.
[Clipboard operations](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-operations)

## Manual checks

| App | Double-click location | Expected verdict | Expected paste result |
| --- | --- | --- | --- |
| Chrome | Rich web composer, then a plain input and a password field | EDITABLE, EDITABLE, CANVAS_OR_UNKNOWN | Image in rich composer, plain input may refuse it, password ignored |
| Slack | Empty message composer with placeholder | EDITABLE | Image draft appears |
| Word | Text in a writable document | EDITABLE | Checkerboard inserted |
| Notepad | Text editor | EDITABLE | No visible image where only text is accepted |
| Windows Terminal | Shell prompt or terminal text | TERMINAL | Path to an existing PNG is inserted, with no Enter |
| File Explorer | Blank space below the last folder item, then a file and the title bar | EXPLORER_FOLDER_BACKGROUND, then CANVAS_OR_UNKNOWN for the controls | Folder path in log, normal clicks still work |
| Desktop | Empty space, then an icon | DESKTOP, then CANVAS_OR_UNKNOWN | Desktop path in log, no paste |
| Elevated Notepad | Editor while spike runs at normal integrity | Higher-integrity or access-denied diagnostic if the click is observed | No attempted paste |

Copy sample text before a paste and verify it returns after eight seconds. Repeat
and copy different text during those eight seconds. Check that the newer text
survives and the log reports the sequence change.
