//! Pure Markdown note text: styling spans, list continuation and screenshot insertion.
//!
//! The expected spans are written out so each scanner's language is visible: anchors, greedy and
//! lazy parts, what may or may not cross a line break, and the escapes in the Swift patterns.

use std::time::Instant;

use igorio_app::notes::markdown::{
    Kind, Look, MARK, Span, Weight, insert_screenshot, look, on_return, style, utf16_range,
    utf8_range,
};
use igorio_kit::theme;

type ExpectedSpan = (usize, usize, Kind, bool);
type StyleCase<'a> = (&'a str, &'a [ExpectedSpan]);

fn span(start: usize, end: usize, kind: Kind, marker: bool) -> Span {
    Span {
        range: start..end,
        kind,
        marker,
    }
}

/// Sorted, non-overlapping, on char boundaries and covering the whole text.
fn check_invariants(text: &str, spans: &[Span]) {
    let mut at = 0;
    for span in spans {
        assert!(
            span.range.start == at,
            "spans are not contiguous at {} (input {text:?})",
            span.range.start
        );
        assert!(
            span.range.start < span.range.end,
            "empty span at {} (input {text:?})",
            span.range.start
        );
        assert!(
            text.is_char_boundary(span.range.start) && text.is_char_boundary(span.range.end),
            "span off a char boundary: {:?} (input {text:?})",
            span.range
        );
        at = span.range.end;
    }
    assert_eq!(at, text.len(), "spans do not cover the text {text:?}");
    for pair in spans.windows(2) {
        assert!(
            pair[0].range.end <= pair[1].range.start,
            "spans overlap: {:?} and {:?}",
            pair[0].range,
            pair[1].range
        );
    }
}

#[track_caller]
fn expect_style(text: &str, expected: &[ExpectedSpan]) {
    let got = style(text);
    check_invariants(text, &got);
    let want: Vec<Span> = expected
        .iter()
        .map(|&(start, end, kind, marker)| span(start, end, kind, marker))
        .collect();
    assert_eq!(got, want, "styling {text:?}");
}

#[test]
fn style_table() {
    let cases: &[StyleCase] = &[
        ("", &[]),
        ("plain text", &[(0, 10, Kind::Body, false)]),
        // Headings: `#{1,3}` plus whitespace, `(.+)` is the rest of the line.
        ("# One", &[(0, 2, Kind::Heading(1), true), (2, 5, Kind::Heading(1), false)]),
        ("## Two", &[(0, 3, Kind::Heading(2), true), (3, 6, Kind::Heading(2), false)]),
        (
            "### Three",
            &[(0, 4, Kind::Heading(3), true), (4, 9, Kind::Heading(3), false)],
        ),
        // `#{1,3}` cannot take a fourth hash, so this is body text.
        ("#### Four", &[(0, 9, Kind::Body, false)]),
        // The trailing space is all `(.+)` can match after the greedy whitespace gives one back.
        (
            "##  ",
            &[(0, 3, Kind::Heading(2), true), (3, 4, Kind::Heading(2), false)],
        ),
        // Bold delimiters stay visible and dimmed; `__` is the same pattern's alternative.
        (
            "**bold**",
            &[
                (0, 2, Kind::Bold, true),
                (2, 6, Kind::Bold, false),
                (6, 8, Kind::Bold, true),
            ],
        ),
        (
            "__bold__",
            &[
                (0, 2, Kind::Bold, true),
                (2, 6, Kind::Bold, false),
                (6, 8, Kind::Bold, true),
            ],
        ),
        (
            "a **bold** b",
            &[
                (0, 2, Kind::Body, false),
                (2, 4, Kind::Bold, true),
                (4, 8, Kind::Bold, false),
                (8, 10, Kind::Bold, true),
                (10, 12, Kind::Body, false),
            ],
        ),
        // Italic with either delimiter.
        (
            "*italic*",
            &[
                (0, 1, Kind::Italic, true),
                (1, 7, Kind::Italic, false),
                (7, 8, Kind::Italic, true),
            ],
        ),
        (
            "_italic_",
            &[
                (0, 1, Kind::Italic, true),
                (1, 7, Kind::Italic, false),
                (7, 8, Kind::Italic, true),
            ],
        ),
        (
            "a *b* c",
            &[
                (0, 2, Kind::Body, false),
                (2, 3, Kind::Italic, true),
                (3, 4, Kind::Italic, false),
                (4, 5, Kind::Italic, true),
                (5, 7, Kind::Body, false),
            ],
        ),
        // `(?<![*\w])` and `(?![*\w])` keep words and multiplication out of italic.
        ("snake_case_name", &[(0, 15, Kind::Body, false)]),
        ("2 * 3 * 4", &[(0, 9, Kind::Body, false)]),
        ("a*b*", &[(0, 4, Kind::Body, false)]),
        ("*a*_", &[(0, 4, Kind::Body, false)]),
        (
            "*a*.",
            &[
                (0, 1, Kind::Italic, true),
                (1, 2, Kind::Italic, false),
                (2, 3, Kind::Italic, true),
                (3, 4, Kind::Body, false),
            ],
        ),
        (
            "(*a*)",
            &[
                (0, 1, Kind::Body, false),
                (1, 2, Kind::Italic, true),
                (2, 3, Kind::Italic, false),
                (3, 4, Kind::Italic, true),
                (4, 5, Kind::Body, false),
            ],
        ),
        // Bold inside a heading keeps the heading's size (Swift's `embolden` does), so the run
        // stays a heading span; only its delimiters are marked.
        (
            "## **bold**",
            &[
                (0, 5, Kind::Heading(2), true),
                (5, 9, Kind::Heading(2), false),
                (9, 11, Kind::Heading(2), true),
            ],
        ),
        (
            "### a **b** c",
            &[
                (0, 4, Kind::Heading(3), true),
                (4, 6, Kind::Heading(3), false),
                (6, 8, Kind::Heading(3), true),
                (8, 9, Kind::Heading(3), false),
                (9, 11, Kind::Heading(3), true),
                (11, 13, Kind::Heading(3), false),
            ],
        ),
        // Lazy `.+?` stops at the first closing pair that follows a non-space.
        (
            "**a** and **b**",
            &[
                (0, 2, Kind::Bold, true),
                (2, 3, Kind::Bold, false),
                (3, 5, Kind::Bold, true),
                (5, 10, Kind::Body, false),
                (10, 12, Kind::Bold, true),
                (12, 13, Kind::Bold, false),
                (13, 15, Kind::Bold, true),
            ],
        ),
        // `.` does not cross a line terminator, so this bold pair does not close.
        ("**a\rb**", &[(0, 7, Kind::Body, false)]),
        ("**a **", &[(0, 6, Kind::Body, false)]),
        // Four stars are a horizontal rule (`\*{3,}`), not two bold pairs.
        ("****", &[(0, 4, Kind::Body, true)]),
        // The backreference requires the same delimiter on both sides.
        ("__a**", &[(0, 5, Kind::Body, false)]),
        // Inline code; the class excludes a backtick and \n, but not \r.
        (
            "`code`",
            &[
                (0, 1, Kind::Code, true),
                (1, 5, Kind::Code, false),
                (5, 6, Kind::Code, true),
            ],
        ),
        (
            "a `b c` d",
            &[
                (0, 2, Kind::Body, false),
                (2, 3, Kind::Code, true),
                (3, 6, Kind::Code, false),
                (6, 7, Kind::Code, true),
                (7, 9, Kind::Body, false),
            ],
        ),
        ("`a\nb`", &[(0, 5, Kind::Body, false)]),
        (
            "`a\rb`",
            &[
                (0, 1, Kind::Code, true),
                (1, 4, Kind::Code, false),
                (4, 5, Kind::Code, true),
            ],
        ),
        // There is no fenced-code rule in the Swift styler: three backticks stay body text.
        ("```\ncode\n```", &[(0, 12, Kind::Body, false)]),
        // Links; the brackets and the target are markers, the label is link text.
        (
            "[go](https://e.co)",
            &[
                (0, 1, Kind::Link, true),
                (1, 3, Kind::Link, false),
                (3, 18, Kind::Link, true),
            ],
        ),
        // `+` in the link pattern: an empty label or target is not a link.
        ("[](u)", &[(0, 5, Kind::Body, false)]),
        ("[x]()", &[(0, 5, Kind::Body, false)]),
        // Images are dimmed whole (the kind set has no image kind; body marker is the closest).
        ("![](a.png)", &[(0, 10, Kind::Body, true)]),
        (
            "![screenshot](attachments/a.png)",
            &[(0, 32, Kind::Body, true)],
        ),
        // Bare URLs; a link target is protected by `(?<![(\[<])`.
        (
            "https://example.com",
            &[(0, 19, Kind::Link, false)],
        ),
        (
            "see https://e.co now",
            &[
                (0, 4, Kind::Body, false),
                (4, 16, Kind::Link, false),
                (16, 20, Kind::Body, false),
            ],
        ),
        ("(https://x)", &[(0, 11, Kind::Body, false)]),
        // Bullets and numbered items: only the marker is a list marker.
        (
            "- item",
            &[(0, 1, Kind::ListMarker, true), (1, 6, Kind::Body, false)],
        ),
        (
            "  - item",
            &[
                (0, 2, Kind::Body, false),
                (2, 3, Kind::ListMarker, true),
                (3, 8, Kind::Body, false),
            ],
        ),
        (
            "1. first",
            &[(0, 2, Kind::ListMarker, true), (2, 8, Kind::Body, false)],
        ),
        (
            "12) second",
            &[(0, 3, Kind::ListMarker, true), (3, 10, Kind::Body, false)],
        ),
        ("- ", &[(0, 1, Kind::ListMarker, true), (1, 2, Kind::Body, false)]),
        ("1.", &[(0, 2, Kind::Body, false)]),
        // Task items: the whole box is a marker; a checked box's text is struck through.
        (
            "- [ ] todo",
            &[(0, 5, Kind::ListMarker, true), (5, 10, Kind::Body, false)],
        ),
        (
            "- [x] done",
            &[
                (0, 5, Kind::ListMarker, true),
                (5, 6, Kind::Body, false),
                (6, 10, Kind::Strikethrough, false),
            ],
        ),
        // Quotes; the `>` and its optional space are marked.
        (
            "> quote",
            &[(0, 2, Kind::Quote, true), (2, 7, Kind::Quote, false)],
        ),
        (">", &[(0, 1, Kind::Quote, true)]),
        (
            ">no",
            &[(0, 1, Kind::Quote, true), (1, 3, Kind::Quote, false)],
        ),
        ("a > b", &[(0, 5, Kind::Body, false)]),
        // Strikethrough is `~~` with the same lazy body as bold.
        (
            "~~gone~~",
            &[
                (0, 2, Kind::Strikethrough, true),
                (2, 6, Kind::Strikethrough, false),
                (6, 8, Kind::Strikethrough, true),
            ],
        ),
        // Horizontal rules are dimmed whole; `---` and `***` never form a list or emphasis.
        ("---", &[(0, 3, Kind::Body, true)]),
        ("***", &[(0, 3, Kind::Body, true)]),
        ("  ---  ", &[(0, 7, Kind::Body, true)]),
        // CRLF: the `\r` is not part of the heading's `(.+)` or of its marker.
        (
            "# One\r\nbody",
            &[
                (0, 2, Kind::Heading(1), true),
                (2, 5, Kind::Heading(1), false),
                (5, 11, Kind::Body, false),
            ],
        ),
        // An emoji before a marker; ranges stay on UTF-8 char boundaries.
        (
            "😀 **bold**",
            &[
                (0, 5, Kind::Body, false),
                (5, 7, Kind::Bold, true),
                (7, 11, Kind::Bold, false),
                (11, 13, Kind::Bold, true),
            ],
        ),
        // A marker at the very start and end of the text.
        (
            "**x**",
            &[
                (0, 2, Kind::Bold, true),
                (2, 3, Kind::Bold, false),
                (3, 5, Kind::Bold, true),
            ],
        ),
    ];

    for &(text, expected) in cases {
        expect_style(text, expected);
    }
}

#[test]
fn looks_match_the_swift_values() {
    assert_eq!(
        look(Kind::Body),
        Look {
            weight: Weight::Regular,
            size: 14.0,
            colour: theme::TEXT,
            monospace: false,
        }
    );
    assert_eq!(
        look(Kind::Heading(1)),
        Look {
            weight: Weight::Semibold,
            size: 20.0,
            colour: theme::TEXT,
            monospace: false,
        }
    );
    assert_eq!(look(Kind::Heading(2)).size, 17.0);
    assert_eq!(look(Kind::Heading(3)).size, 15.5);
    assert_eq!(look(Kind::Heading(3)).weight, Weight::Semibold);
    assert_eq!(look(Kind::Bold).weight, Weight::Semibold);
    assert_eq!(look(Kind::Bold).size, 14.0);
    assert_eq!(look(Kind::Italic).weight, Weight::Regular);
    assert_eq!(
        look(Kind::Code),
        Look {
            weight: Weight::Regular,
            size: 12.5,
            colour: theme::TEXT,
            monospace: true,
        }
    );
    assert_eq!(look(Kind::Link).colour, theme::ACCENT);
    assert_eq!(look(Kind::ListMarker).colour, theme::MUTED);
    assert_eq!(look(Kind::ListMarker).weight, Weight::Semibold);
    assert_eq!(look(Kind::Quote).colour, theme::MUTED);
    assert_eq!(look(Kind::Strikethrough).size, 14.0);
    assert_eq!(MARK, [0x5a, 0x64, 0x78, 0x80]);
}

fn split_marked(marked: &str) -> (String, usize) {
    let mut cursor = None;
    for (at, ch) in marked.char_indices() {
        if ch == '|' {
            assert!(cursor.is_none(), "more than one cursor in {marked:?}");
            cursor = Some(at);
        }
    }
    let cursor = cursor.unwrap_or_else(|| panic!("no cursor in {marked:?}"));
    let mut text = marked.to_string();
    text.remove(cursor);
    (text, cursor)
}

#[track_caller]
fn expect_return(input: &str, expected: &str) {
    let (text, cursor) = split_marked(input);
    let edit = on_return(&text, cursor).unwrap_or_else(|| panic!("no edit for {input:?}"));
    let mut result = text.clone();
    result.replace_range(edit.range.clone(), &edit.text);
    let (want_text, want_cursor) = split_marked(expected);
    assert_eq!(result, want_text, "text after Return from {input:?}");
    assert_eq!(edit.cursor, want_cursor, "cursor after Return from {input:?}");
}

#[track_caller]
fn expect_default(input: &str) {
    let (text, cursor) = split_marked(input);
    assert!(
        on_return(&text, cursor).is_none(),
        "expected the default Return for {input:?}"
    );
}

#[test]
fn return_table() {
    // Bullets continue with the same character.
    expect_return("- one|", "- one\n- |");
    expect_return("* one|", "* one\n* |");
    expect_return("+ one|", "+ one\n+ |");
    // Numbers increase and keep their delimiter.
    expect_return("1. one|", "1. one\n2. |");
    expect_return("9) one|", "9) one\n10) |");
    expect_return("007. one|", "007. one\n8. |");
    expect_return("10. a|", "10. a\n11. |");
    expect_return("1) a|", "1) a\n2) |");
    // `Int(...) ?? 0` makes an overflowing number start again at 1.
    expect_return(
        "9223372036854775808. x|",
        "9223372036854775808. x\n1. |",
    );
    // Indentation is kept.
    expect_return("  - one|", "  - one\n  - |");
    expect_return("\t- one|", "\t- one\n\t- |");
    expect_return("  * [ ] x|", "  * [ ] x\n  * [ ] |");
    // A task continues unchecked, including after a checked one.
    expect_return("- [ ] todo|", "- [ ] todo\n- [ ] |");
    expect_return("- [x] done|", "- [x] done\n- [ ] |");
    expect_return("- [X] done|", "- [X] done\n- [ ] |");
    expect_return("- [ ] x|", "- [ ] x\n- [ ] |");
    // Return in the middle of an item splits it; the tail stays after the new marker.
    expect_return("- o|ne", "- o\n- |ne");
    expect_return("- [ ] t|odo", "- [ ] t\n- [ ] |odo");
    // Empty items remove the marker and do not add a line.
    expect_return("- |", "|");
    expect_return("-  |", "|");
    expect_return("- [ ] |", "|");
    expect_return("- [ ]  |", "|");
    expect_return("  - |", "|");
    expect_return("  - [ ] |", "|");
    expect_return("1. |", "|");
    // The head stops at the cursor, so an item with text only after the cursor looks empty.
    expect_return("- |x", "|x");
    // A box with no trailing space is not a task item for this pattern; it is a plain bullet.
    expect_return("- [ ]|", "- [ ]\n- |");
    expect_return("- [ ]x|", "- [ ]x\n- |");
    // CRLF.
    expect_return("a\r\n- x|", "a\r\n- x\n- |");
    expect_return("- |\r\nz", "|\r\nz");
    // No list item: let the widget insert its default newline.
    expect_default("text|");
    expect_default("|");
    expect_default("|text");
    expect_default("-|");
    expect_default("1.|");
    expect_default("- a\n|");
    expect_default("> quote|");
    expect_default("```|");
}

#[test]
fn return_keeps_the_text_outside_the_replacement() {
    let text = "before\n  - [x] done\n  - [ ] tail";
    let cursor = text.find("tail").unwrap() + 2;
    let edit = on_return(text, cursor).unwrap();
    assert_eq!(edit.range, cursor..cursor);
    assert_eq!(edit.text, "\n  - [ ] ");
    assert_eq!(edit.cursor, cursor + edit.text.len());

    let empty = on_return("- [ ] ", 6).unwrap();
    assert_eq!(empty.range, 0..6);
    assert_eq!(empty.text, "");
    assert_eq!(empty.cursor, 0);
}

#[track_caller]
fn expect_screenshot(input: &str, markdown: &str, expected: &str) {
    let (text, cursor) = split_marked(input);
    let edit = insert_screenshot(&text, cursor, markdown);
    assert_eq!(edit.range, cursor..cursor, "replacement range for {input:?}");
    let mut result = text.clone();
    result.replace_range(edit.range.clone(), &edit.text);
    let (want_text, want_cursor) = split_marked(expected);
    assert_eq!(result, want_text, "text after inserting into {input:?}");
    assert_eq!(edit.cursor, want_cursor, "cursor after inserting into {input:?}");
}

#[test]
fn screenshot_table() {
    let one = "![screenshot](attachments/a.png)";
    // Images go on their own lines, with no extra newline at either end of the text.
    expect_screenshot("|", one, "![screenshot](attachments/a.png)|");
    expect_screenshot("abc|", one, "abc\n![screenshot](attachments/a.png)|");
    expect_screenshot("|abc", one, "![screenshot](attachments/a.png)\n|abc");
    expect_screenshot(
        "ab|c",
        one,
        "ab\n![screenshot](attachments/a.png)\n|c",
    );
    // Already next to a newline: no second one.
    expect_screenshot("abc\n|", one, "abc\n![screenshot](attachments/a.png)|");
    expect_screenshot("|\nabc", one, "![screenshot](attachments/a.png)|\nabc");
    expect_screenshot("abc\r\n|", one, "abc\r\n![screenshot](attachments/a.png)|");
    // A lone CR is not a newline for this check, exactly like `character(at:) != 10`.
    expect_screenshot("abc\r|", one, "abc\r\n![screenshot](attachments/a.png)|");
    // Emoji before the caret.
    expect_screenshot("😀|", one, "😀\n![screenshot](attachments/a.png)|");
    // Several screenshots are one Markdown block, wrapped once.
    let two = "![one](1)\n![two](2)";
    expect_screenshot("a|b", two, "a\n![one](1)\n![two](2)\n|b");
}

#[test]
fn utf16_ranges() {
    // é is two UTF-8 bytes and one UTF-16 unit; the rest is ASCII.
    assert_eq!(utf16_range("héllo", 0..6), 0..5);
    assert_eq!(utf16_range("héllo", 1..3), 1..2);
    assert_eq!(utf8_range("héllo", 0..5), 0..6);
    assert_eq!(utf8_range("héllo", 1..2), 1..3);

    // 😀 is four UTF-8 bytes and two UTF-16 units.
    assert_eq!(utf16_range("a😀b", 0..6), 0..4);
    assert_eq!(utf16_range("a😀b", 1..5), 1..3);
    assert_eq!(utf8_range("a😀b", 0..4), 0..6);
    assert_eq!(utf8_range("a😀b", 1..3), 1..5);
    assert_eq!(utf16_range("😀", 0..4), 0..2);
    assert_eq!(utf8_range("😀", 0..2), 0..4);

    // Byte ends inside a character are moved outwards; UTF-16 units inside a surrogate pair
    // map to the boundary after the whole character.
    assert_eq!(utf16_range("😀", 0..1), 0..2);
    assert_eq!(utf8_range("😀", 0..1), 0..4);
    assert_eq!(utf8_range("😀", 1..1), 4..4);
    assert_eq!(utf8_range("😀", 1..2), 4..4);

    // Empty and out-of-range inputs stay in the text.
    assert_eq!(utf16_range("", 0..0), 0..0);
    assert_eq!(utf8_range("", 0..0), 0..0);
    assert_eq!(utf16_range("a", 0..99), 0..1);
    assert_eq!(utf8_range("a", 5..9), 1..1);

    // A styled span round-trips through UTF-16.
    let text = "😀 **x**";
    let bold = style(text).into_iter().find(|span| span.kind == Kind::Bold).unwrap();
    let units = utf16_range(text, bold.range.clone());
    assert_eq!(units, 3..5);
    assert_eq!(utf8_range(text, units), bold.range);
}

#[test]
fn styles_twenty_thousand_characters() {
    let block = concat!(
        "# Heading one\n",
        "## Heading two\n",
        "### Heading three\n\n",
        "Some **bold**, *italic*, _under_ and ~~gone~~ text.\n\n",
        "- bullet\n",
        "* star\n",
        "+ plus\n",
        "1. first\n",
        "2) second\n\n",
        "- [ ] todo\n",
        "- [x] done\n\n",
        "> quoted line\n\n",
        "`inline code`\n\n",
        "[link](https://example.com/a)\n\n",
        "https://bare.example/b\n\n",
        "---\n\n",
        "![screenshot](attachments/shot-1.png)\n\n",
        "2 * 3 * 4 and snake_case_name\n\n",
    );
    let mut note = String::new();
    while note.len() < 20_000 {
        note.push_str(block);
    }

    let started = Instant::now();
    let spans = style(&note);
    let elapsed = started.elapsed();

    check_invariants(&note, &spans);
    println!(
        "styled {} bytes in {elapsed:?} into {} spans",
        note.len(),
        spans.len()
    );
}

// ---------------------------------------------------------------------------
// Blocks and live editing. Cases are written with `|` for the caret and `«…»` for a selection.
// ---------------------------------------------------------------------------

mod live {
    use std::ops::Range;

    use igorio_app::notes::markdown::{
        Block, BlockCommand, Change, Inline, SlashItem, apply_slash, backspace, blocks,
        change_between, enter, image_target, indent, link_at, list_numbers, paste, renumber,
        set_block, slash_matches, slash_query, toggle_inline, toggle_link, toggle_todo,
        type_char, word_at,
    };

    /// Text and selection from a case string.
    fn parse(case: &str) -> (String, Range<usize>) {
        if let Some(at) = case.find('|') {
            return (case.replacen('|', "", 1), at..at);
        }
        let start = case.find('«').expect("caret or selection");
        let text = case.replacen('«', "", 1);
        let end = text.find('»').expect("selection end");
        (text.replacen('»', "", 1), start..end)
    }

    /// The case string of a text and selection.
    fn show(text: &str, sel: Range<usize>) -> String {
        if sel.is_empty() {
            format!("{}|{}", &text[..sel.start], &text[sel.start..])
        } else {
            format!("{}«{}»{}", &text[..sel.start], &text[sel.clone()], &text[sel.end..])
        }
    }

    type Case<'a> = (&'a str, Option<&'a str>);

    fn table(cases: &[Case<'_>], edit: impl Fn(&str, Range<usize>) -> Option<Change>) {
        for (case, expected) in cases {
            let (text, sel) = parse(case);
            let got = edit(&text, sel).map(|change| show(&change.apply(&text), change.selection));
            assert_eq!(got.as_deref(), *expected, "case {case:?}");
        }
    }

    #[test]
    fn blocks_classify_each_line() {
        let text = "# Title\n## Two\n#### four\n- a\n  * b\n1. one\n10) ten\n- [ ] todo\n- [x] done\n> quote\n---\n***\n![s](attachments/a.png)\n```\n# not a heading\n```\nplain\n#tag";
        let kinds: Vec<Block> = blocks(text).iter().map(|line| line.block).collect();
        assert_eq!(
            kinds,
            [
                Block::Heading(1),
                Block::Heading(2),
                Block::Paragraph,
                Block::Bullet,
                Block::Bullet,
                Block::Numbered(1),
                Block::Numbered(10),
                Block::Todo(false),
                Block::Todo(true),
                Block::Quote,
                Block::Divider,
                Block::Divider,
                Block::Image,
                Block::Fence,
                Block::Code,
                Block::Fence,
                Block::Paragraph,
                Block::Paragraph,
            ]
        );
        let depths: Vec<u8> = blocks("- a\n  - b\n    - c\n  - d\ntext\n  - e")
            .iter()
            .map(|line| line.depth)
            .collect();
        assert_eq!(depths, [0, 1, 2, 1, 0, 0]);
        let todo = &blocks("  - [ ] task")[0];
        assert_eq!((todo.indent, todo.content), (2, 8));
        // "# " with nothing after it is already a heading (the Notion shortcut).
        assert_eq!(blocks("# ")[0].block, Block::Heading(1));
        assert_eq!(blocks("")[0].range, 0..0);
        assert_eq!(blocks("a\n").len(), 2);
    }

    #[test]
    fn images_and_links_are_found() {
        assert_eq!(
            image_target("![screenshot](attachments/shot-1.png)"),
            Some("attachments/shot-1.png")
        );
        assert_eq!(image_target("see ![s](a.png)"), None);
        let text = "go [home](https://a.example) or https://b.example now ![x](c.png)";
        assert_eq!(link_at(text, 5).as_deref(), Some("https://a.example"));
        assert_eq!(link_at(text, 35).as_deref(), Some("https://b.example"));
        assert_eq!(link_at(text, 0), None);
        assert_eq!(link_at(text, text.len() - 3), None);
    }

    #[test]
    fn enter_follows_notion() {
        table(
            &[
                ("- one|", Some("- one\n- |")),
                ("* one|", Some("* one\n* |")),
                ("- [x] done|", Some("- [x] done\n- [ ] |")),
                ("1. a|\n2. b", Some("1. a\n2. |\n3. b")),
                ("3) a|", Some("3) a\n4) |")),
                ("> said|", Some("> said\n> |")),
                ("  - nested|", Some("  - nested\n  - |")),
                // The text after the caret moves to the new item.
                ("- ab|cd", Some("- ab\n- |cd")),
                // An empty item leaves the list; a nested one moves out a level first.
                ("- a\n- |", Some("- a\n|")),
                ("- a\n  - |", Some("- a\n- |")),
                ("- [ ] |", Some("|")),
                ("> |", Some("|")),
                // A list split by text keeps its written numbers (valid Markdown: "3." starts
                // a list at 3); the editor draws the same numbers.
                ("1. a\n2. |\n3. c", Some("1. a\n|\n3. c")),
                // Headings, text and code take the control's plain line break.
                ("# Title|", None),
                ("text|", None),
                ("```\ncode|\n```", None),
                ("«- a»", None),
                ("|- a", None),
            ],
            enter,
        );
    }

    #[test]
    fn backspace_at_a_block_start_makes_text() {
        table(
            &[
                ("# |Title", Some("|Title")),
                ("## |", Some("|")),
                ("- |item", Some("|item")),
                ("  - [x] |done", Some("|done")),
                ("> |quote", Some("|quote")),
                ("1. a\n2. |b\n3. c", Some("1. a\n|b\n3. c")),
                ("- i|tem", None),
                ("|plain", None),
                ("pl|ain", None),
                ("« »- x", None),
            ],
            backspace,
        );
    }

    #[test]
    fn tab_nests_and_shift_tab_unnests() {
        table(
            &[
                ("- a\n- b|", Some("- a\n  - b|")),
                ("1. a\n2. b|", Some("1. a\n   1. b|")),
                ("- [ ] a\n- [ ] b|", Some("- [ ] a\n  - [ ] b|")),
                // The first item has nothing to nest under: handled, unchanged.
                ("- a|", Some("- a|")),
                ("- a\n«- b\n- c»", Some("- a\n  «- b\n  - c»")),
                ("text|", None),
            ],
            |text, sel| indent(text, sel, false),
        );
        table(
            &[
                ("- a\n  - b|", Some("- a\n- b|")),
                ("1. a\n2. b\n   1. c|", Some("1. a\n2. b\n3. c|")),
                ("- a|", Some("- a|")),
                ("text|", None),
            ],
            |text, sel| indent(text, sel, true),
        );
    }

    #[test]
    fn numbered_lists_count_again() {
        assert_eq!(renumber("1. a\n1. b\n1. c"), "1. a\n2. b\n3. c");
        assert_eq!(renumber("3. a\n9. b"), "3. a\n4. b");
        assert_eq!(
            renumber("1. a\n   1. x\n   5. y\n2. b"),
            "1. a\n   1. x\n   2. y\n2. b"
        );
        assert_eq!(renumber("1. a\n\n5. b"), "1. a\n\n5. b");
        assert_eq!(renumber("1) a\n7) b"), "1) a\n2) b");
        assert_eq!(
            list_numbers("- x\n1. a\n1. b\ntext"),
            [None, Some(1), Some(2), None]
        );
    }

    #[test]
    fn typed_shortcuts_rewrite_text() {
        let typing = |ch: char| move |text: &str, sel: Range<usize>| type_char(text, sel, ch);
        table(
            &[
                ("[]|", Some("- [ ] |")),
                ("[ ]|", Some("- [ ] |")),
                ("a\n[x]|", Some("a\n- [x] |")),
                ("x[]|", None),
                ("```\n[]|\n```", None),
            ],
            typing(' '),
        );
        table(
            &[("``|", Some("```\n|\n```")), ("``|x", None), ("a``|", None)],
            typing('`'),
        );
        table(
            &[("--|", Some("---\n|")), ("--|x", None), ("- -|", None)],
            typing('-'),
        );
        table(&[("# |", None), ("- |", None)], typing('x'));
    }

    #[test]
    fn formatting_keys_toggle() {
        let key = |mark: Inline| {
            move |text: &str, sel: Range<usize>| Some(toggle_inline(text, sel, mark))
        };
        table(
            &[
                ("a «word» b", Some("a **«word»** b")),
                ("a **«word»** b", Some("a «word» b")),
                ("a «**word**» b", Some("a «word» b")),
                ("wo|rd", Some("**wo|rd**")),
                ("**wo|rd**", Some("wo|rd")),
                ("a | b", Some("a **|** b")),
                ("«one\ntwo»", Some("**«one**\n**two»**")),
                ("*«it»*", Some("***«it»***")),
            ],
            key(Inline::Bold),
        );
        table(
            &[
                ("«word»", Some("*«word»*")),
                ("*«word»*", Some("«word»")),
                // Bold is not italic: Ctrl+I on bold text adds italic.
                ("**«word»**", Some("***«word»***")),
                ("***«word»***", Some("**«word»**")),
            ],
            key(Inline::Italic),
        );
        table(
            &[("«x»", Some("`«x»`")), ("`«x»`", Some("«x»"))],
            key(Inline::Code),
        );
        table(
            &[("«x»", Some("~~«x»~~")), ("~~«x»~~", Some("«x»"))],
            key(Inline::Strike),
        );
    }

    #[test]
    fn link_key_wraps_and_unwraps() {
        table(
            &[
                ("see «docs» now", Some("see [docs](|) now")),
                ("do|cs", Some("[docs](|)")),
                ("a | b", Some("a [|]() b")),
                ("[d|ocs](https://x.example)", Some("«docs»")),
            ],
            |text, sel| Some(toggle_link(text, sel)),
        );
    }

    #[test]
    fn block_keys_set_and_toggle() {
        let key = |command: BlockCommand| {
            move |text: &str, sel: Range<usize>| Some(set_block(text, sel, command))
        };
        table(
            &[
                ("ti|tle", Some("# ti|tle")),
                ("# ti|tle", Some("ti|tle")),
                ("## ti|tle", Some("# ti|tle")),
                ("- it|em", Some("# it|em")),
            ],
            key(BlockCommand::Heading(1)),
        );
        table(
            &[
                // The selection keeps covering the words, after the new markers.
                ("«a\nb»", Some("- «a\n- b»")),
                ("- |a", Some("|a")),
                ("1. |a", Some("- |a")),
            ],
            key(BlockCommand::Bullet),
        );
        table(
            &[("«a\nb\nc»", Some("1. «a\n2. b\n3. c»"))],
            key(BlockCommand::Numbered),
        );
        table(
            &[
                ("buy| milk", Some("- [ ] buy| milk")),
                ("- [x] |x", Some("|x")),
            ],
            key(BlockCommand::Todo),
        );
        table(
            &[("> |q", Some("|q")), ("|q", Some("> |q"))],
            key(BlockCommand::Quote),
        );
        table(
            &[("# |T", Some("|T")), ("- |x", Some("|x"))],
            key(BlockCommand::Text),
        );
        table(
            &[
                ("|", Some("```\n|\n```")),
                ("«a\nb»", Some("```\n«a\nb»\n```")),
            ],
            key(BlockCommand::Code),
        );
        table(
            &[
                ("|", Some("---\n|")),
                ("te|xt", Some("text\n***\n|")),
                ("# H|", Some("# H\n---\n|")),
            ],
            key(BlockCommand::Divider),
        );
    }

    #[test]
    fn todos_toggle_in_place() {
        table(
            &[
                ("- [ ] a|", Some("- [x] a|")),
                ("- [x] a|", Some("- [ ] a|")),
                ("  * [X] |b", Some("  * [ ] |b")),
                ("- a|", None),
            ],
            |text, sel| toggle_todo(text, sel.start, sel),
        );
    }

    #[test]
    fn slash_menu_finds_filters_and_applies() {
        assert_eq!(slash_query("/he", 3), Some((0..3, "he")));
        assert_eq!(slash_query("word /h", 7), Some((5..7, "h")));
        assert_eq!(slash_query("- /to", 5), Some((2..5, "to")));
        assert_eq!(slash_query("a/b", 3), None);
        assert_eq!(slash_query("/a b", 4), None);
        assert_eq!(slash_query("```\n/x\n```", 6), None);
        assert_eq!(slash_matches("").len(), 10);
        assert_eq!(
            slash_matches("he"),
            [SlashItem::Heading1, SlashItem::Heading2, SlashItem::Heading3]
        );
        assert_eq!(slash_matches("LIST"), [SlashItem::Bullet, SlashItem::Numbered]);
        assert_eq!(slash_matches("h2"), [SlashItem::Heading2]);
        assert_eq!(slash_matches("to"), [SlashItem::Todo]);
        assert_eq!(slash_matches("zzz"), []);
        let pick = |item: SlashItem| {
            move |text: &str, sel: Range<usize>| {
                let (range, _) = slash_query(text, sel.start)?;
                Some(apply_slash(text, range, item))
            }
        };
        table(
            &[("/he|", Some("# |")), ("idea /h1|", Some("# idea |"))],
            pick(SlashItem::Heading1),
        );
        table(&[("/to|", Some("- [ ] |"))], pick(SlashItem::Todo));
        table(&[("- a /|", Some("- a |"))], pick(SlashItem::Bullet));
        table(&[("/co|", Some("```\n|\n```"))], pick(SlashItem::Code));
        table(&[("/div|", Some("---\n|"))], pick(SlashItem::Divider));
        table(&[("# x /t|", Some("x |"))], pick(SlashItem::Text));
    }

    #[test]
    fn paste_is_plain_and_links_selections() {
        let pasting = |pasted: &'static str| {
            move |text: &str, sel: Range<usize>| Some(paste(text, sel, pasted))
        };
        table(
            &[
                ("see «docs»", Some("see [docs](https://x.example)|")),
                ("a |", Some("a https://x.example|")),
                ("«two\nlines»", Some("https://x.example|")),
            ],
            pasting("https://x.example"),
        );
        table(&[("|", Some("a\nb\nc|"))], pasting("a\r\nb\rc"));
    }

    #[test]
    fn small_helpers() {
        assert_eq!(word_at("one two", 5), 4..7);
        assert_eq!(word_at("one two", 3), 0..3);
        assert_eq!(word_at("a  b", 2), 2..2);
        assert_eq!(word_at("héllo", 3), 0..6);
        let change = change_between("abc", "aXc", 2..2);
        assert_eq!((change.range, change.text.as_str()), (1..2, "X"));
        let change = change_between("aaa", "aaaa", 4..4);
        assert_eq!((change.range, change.text.as_str()), (3..3, "a"));
        assert_eq!(change_between("é", "è", 0..0).apply("é"), "è");
    }
}
