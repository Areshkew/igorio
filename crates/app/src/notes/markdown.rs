//! Pure text logic of the macOS Markdown note editor (`MarkdownEditor.swift`), ported to Rust.
//!
//! Styling is a second pass: [`style`] returns byte-range spans a widget can paint. List
//! continuation, screenshot insertion and UTF-16/UTF-8 range conversion are plain functions too.
//! Nothing here knows about a text widget, Windows, or the note store.

use std::ops::Range;

use igorio_kit::theme::{self, Rgba};

/// The colour of Markdown punctuation: `MUTED` at 50%, the Swift `markColor`.
pub const MARK: Rgba = [0x5a, 0x64, 0x78, 0x80];

/// What a span is, with exactly the kinds the Swift styler distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Body,
    /// `#`, `##` or `###`; the level is 1 to 3 (`Heading(3)` also covers higher levels).
    Heading(u8),
    Bold,
    Italic,
    Code,
    Link,
    ListMarker,
    Quote,
    Strikethrough,
}

/// A styled byte range. Ranges are sorted, do not overlap and cover the whole text.
///
/// `marker` is true for the Markdown punctuation itself (the `#` prefix, the `**`, the brackets
/// around a link, ...). Draw marker spans with [`MARK`] instead of the look's colour; the look's
/// size, weight and monospace flag still apply. [`Kind::ListMarker`] is punctuation by definition
/// and its look already has the muted colour the Swift code uses, so keep that colour for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub kind: Kind,
    pub marker: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
}

/// Concrete visual values, taken from `MarkdownStyler` in the Swift file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub weight: Weight,
    /// Logical pixels, as the Swift point sizes.
    pub size: f32,
    pub colour: Rgba,
    pub monospace: bool,
}

/// Fonts and colours for one kind. A widget applies these without parsing Markdown.
pub fn look(kind: Kind) -> Look {
    match kind {
        Kind::Body => Look {
            weight: Weight::Regular,
            size: 14.0,
            colour: theme::TEXT,
            monospace: false,
        },
        // Swift: [20, 17, 15.5][level - 1], semibold.
        Kind::Heading(1) => Look {
            weight: Weight::Semibold,
            size: 20.0,
            colour: theme::TEXT,
            monospace: false,
        },
        Kind::Heading(2) => Look {
            weight: Weight::Semibold,
            size: 17.0,
            colour: theme::TEXT,
            monospace: false,
        },
        Kind::Heading(_) => Look {
            weight: Weight::Semibold,
            size: 15.5,
            colour: theme::TEXT,
            monospace: false,
        },
        // `embolden`: semibold at the run's existing size; body text is 14.
        Kind::Bold => Look {
            weight: Weight::Semibold,
            size: 14.0,
            colour: theme::TEXT,
            monospace: false,
        },
        // Obliqueness 0.14 keeps the regular font; the kind tells the widget to slant it.
        Kind::Italic => Look {
            weight: Weight::Regular,
            size: 14.0,
            colour: theme::TEXT,
            monospace: false,
        },
        // Monospaced, size - 1.5, body size 14.
        Kind::Code => Look {
            weight: Weight::Regular,
            size: 12.5,
            colour: theme::TEXT,
            monospace: true,
        },
        // linkTextAttributes: accent, underlined; the kind tells the widget to underline.
        Kind::Link => Look {
            weight: Weight::Regular,
            size: 14.0,
            colour: theme::ACCENT,
            monospace: false,
        },
        // Bullet marker: muted and semibold. Task markers become medium in Swift, which one
        // kind cannot also express; see the module notes in the report.
        Kind::ListMarker => Look {
            weight: Weight::Semibold,
            size: 14.0,
            colour: theme::MUTED,
            monospace: false,
        },
        // Muted, obliqueness 0.12.
        Kind::Quote => Look {
            weight: Weight::Regular,
            size: 14.0,
            colour: theme::MUTED,
            monospace: false,
        },
        Kind::Strikethrough => Look {
            weight: Weight::Regular,
            size: 14.0,
            colour: theme::TEXT,
            monospace: false,
        },
    }
}

/// A replacement for an edit command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// Byte range in the old text to replace.
    pub range: Range<usize>,
    /// Text that goes in its place.
    pub text: String,
    /// Caret position after the edit, in the new text.
    pub cursor: usize,
}

// ---------------------------------------------------------------------------
// Styling
// ---------------------------------------------------------------------------

/// Styles the whole note. Cost is linear in the text length: every pattern is scanned once and
/// each byte takes part in a bounded number of attribute passes.
pub fn style(text: &str) -> Vec<Span> {
    let mut cells = vec![Cell::BODY; text.len()];

    for m in scan_heading(text) {
        let (Some(prefix), Some(content)) = (m.group(1), m.group(2)) else {
            continue;
        };
        let level = text[prefix.0..prefix.1].bytes().filter(|b| *b == b'#').count() as u8;
        paint(&mut cells, prefix, Kind::Heading(level), Mark::Set);
        paint(&mut cells, content, Kind::Heading(level), Mark::Clear);
    }
    for m in scan_quote(text) {
        if let Some(prefix) = m.group(1) {
            paint(&mut cells, prefix, Kind::Quote, Mark::Set);
        }
        if let Some(content) = m.group(2) {
            paint(&mut cells, content, Kind::Quote, Mark::Clear);
        }
    }
    for m in scan_bullet(text) {
        if let Some(marker) = m.group(1) {
            paint(&mut cells, marker, Kind::ListMarker, Mark::Set);
        }
    }
    for m in scan_task(text) {
        if let Some(marker) = m.group(1) {
            paint(&mut cells, marker, Kind::ListMarker, Mark::Set);
        }
        let checked = m
            .group(2)
            .is_some_and(|done| &text[done.0..done.1] != " ");
        if let Some(content) = m.group(3).filter(|_| checked) {
            // Swift: a checked box's content is muted and struck through.
            paint(&mut cells, content, Kind::Strikethrough, Mark::Clear);
        }
    }
    for m in scan_rule(text) {
        paint(&mut cells, m.whole, Kind::Body, Mark::Set);
    }

    for m in scan_bold(text) {
        if let Some(inner) = m.group(2) {
            paint(&mut cells, inner, Kind::Bold, Mark::Keep);
        }
        for delimiter in [1, 3] {
            if let Some(marker) = m.group(delimiter) {
                paint(&mut cells, marker, Kind::Bold, Mark::Set);
            }
        }
    }
    for m in scan_italic(text) {
        if let Some(inner) = m.group(2) {
            paint(&mut cells, inner, Kind::Italic, Mark::Keep);
        }
        for delimiter in [1, 3] {
            if let Some(marker) = m.group(delimiter) {
                paint(&mut cells, marker, Kind::Italic, Mark::Set);
            }
        }
    }
    for m in scan_strike(text) {
        if let Some(inner) = m.group(2) {
            paint(&mut cells, inner, Kind::Strikethrough, Mark::Keep);
        }
        for delimiter in [1, 3] {
            if let Some(marker) = m.group(delimiter) {
                paint(&mut cells, marker, Kind::Strikethrough, Mark::Set);
            }
        }
    }
    for m in scan_code(text) {
        if let Some(inner) = m.group(2) {
            paint(&mut cells, inner, Kind::Code, Mark::Keep);
        }
        for delimiter in [1, 3] {
            if let Some(marker) = m.group(delimiter) {
                paint(&mut cells, marker, Kind::Code, Mark::Set);
            }
        }
    }
    for m in scan_link(text) {
        if let Some(inner) = m.group(2) {
            paint(&mut cells, inner, Kind::Link, Mark::Clear);
        }
        for delimiter in [1, 3] {
            if let Some(marker) = m.group(delimiter) {
                paint(&mut cells, marker, Kind::Link, Mark::Set);
            }
        }
    }
    // Images are dimmed wholesale and lose the link the `link` pass put inside them. The kind
    // list has no image kind, so this is body text drawn as a marker (dimmed).
    for m in scan_image(text) {
        paint(&mut cells, m.whole, Kind::Body, Mark::Set);
    }
    for m in scan_bare_url(text) {
        paint(&mut cells, m.whole, Kind::Link, Mark::Clear);
    }

    cells_to_spans(text, &cells)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    kind: Kind,
    marker: bool,
}

impl Cell {
    const BODY: Cell = Cell {
        kind: Kind::Body,
        marker: false,
    };
}

#[derive(Clone, Copy)]
enum Mark {
    Set,
    Clear,
    Keep,
}

fn paint(cells: &mut [Cell], (start, end): (usize, usize), kind: Kind, mark: Mark) {
    for cell in &mut cells[start..end] {
        // Swift's embolden keeps a heading's size, and headings are already semibold, so a bold
        // run inside one looks the same as the heading; keeping the heading kind preserves the
        // size instead of shrinking it to body size. The same reasoning holds for italic/strike
        // inside a heading, where only the slant/strike would be lost (the kind set cannot stack).
        let keep_heading = matches!(cell.kind, Kind::Heading(_))
            && matches!(
                kind,
                Kind::Bold | Kind::Italic | Kind::Strikethrough
            );
        if !keep_heading {
            cell.kind = kind;
        }
        match mark {
            Mark::Set => cell.marker = true,
            Mark::Clear => cell.marker = false,
            Mark::Keep => {}
        }
    }
}

fn cells_to_spans(text: &str, cells: &[Cell]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (start, ch) in text.char_indices() {
        let end = start + ch.len_utf8();
        let cell = cells[start];
        if let Some(last) = spans.last_mut().filter(|last| {
            last.kind == cell.kind && last.marker == cell.marker && last.range.end == start
        }) {
            last.range.end = end;
            continue;
        }
        spans.push(Span {
            range: start..end,
            kind: cell.kind,
            marker: cell.marker,
        });
    }
    spans
}

// ---------------------------------------------------------------------------
// Scanners
//
// Each scanner accepts the language of one NSRegularExpression in `MarkdownStyler`. The Swift
// pattern is quoted above its scanner. `^` and `$` follow `.anchorsMatchLines`; `.` and `\S`
// follow ICU defaults (dot does not cross a line terminator). Matches are enumerated leftmost
// first and non-overlapping, like `enumerateMatches`.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct Found {
    whole: (usize, usize),
    /// Captures 1 to 6, zero-based.
    groups: [Option<(usize, usize)>; 6],
}

impl Found {
    fn group(&self, one_based: usize) -> Option<(usize, usize)> {
        self.groups.get(one_based - 1).copied().flatten()
    }
}

const NO_GROUPS: [Option<(usize, usize)>; 6] = [None; 6];

fn found(start: usize, end: usize, groups: [Option<(usize, usize)>; 6]) -> Found {
    Found {
        whole: (start, end),
        groups,
    }
}

/// `^(#{1,3}[ \t]+)(.+)$`
fn scan_heading(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    for (line_start, line_end) in lines(text) {
        let line = &text[line_start..line_end];
        let bytes = line.as_bytes();
        let mut hashes = 0;
        while hashes < 3 && bytes.get(hashes) == Some(&b'#') {
            hashes += 1;
        }
        if hashes == 0 {
            continue;
        }
        if !matches!(bytes.get(hashes), Some(b' ' | b'\t')) {
            continue;
        }
        let mut ws_end = hashes;
        while matches!(bytes.get(ws_end), Some(b' ' | b'\t')) {
            ws_end += 1;
        }
        let rest = bytes.len() - ws_end;
        let prefix_end = if rest > 0 {
            ws_end
        } else if ws_end - hashes >= 2 {
            // `[ \t]+` gives one character back so that `(.+)` has something to match.
            ws_end - 1
        } else {
            continue;
        };
        out.push(found(
            line_start,
            line_end,
            [
                Some((line_start, line_start + prefix_end)),
                Some((line_start + prefix_end, line_end)),
                None,
                None,
                None,
                None,
            ],
        ));
    }
    out
}

/// `^(>[ \t]?)(.*)$`
fn scan_quote(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    for (line_start, line_end) in lines(text) {
        let line = &text[line_start..line_end];
        if !line.starts_with('>') {
            continue;
        }
        let prefix_end = if line.as_bytes().get(1) == Some(&b' ')
            || line.as_bytes().get(1) == Some(&b'\t')
        {
            2
        } else {
            1
        };
        out.push(found(
            line_start,
            line_end,
            [
                Some((line_start, line_start + prefix_end)),
                Some((line_start + prefix_end, line_end)),
                None,
                None,
                None,
                None,
            ],
        ));
    }
    out
}

/// `^([ \t]*[-*+][ \t]+\[([ xX])\])[ \t]+(.*)$`
fn scan_task(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    for (line_start, line_end) in lines(text) {
        let line = &text[line_start..line_end];
        let bytes = line.as_bytes();
        let mut i = 0;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        if !matches!(bytes.get(i), Some(b'-' | b'*' | b'+')) {
            continue;
        }
        i += 1;
        let ws_start = i;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        if i == ws_start {
            continue;
        }
        if bytes.get(i) != Some(&b'[') {
            continue;
        }
        let Some(checked) = bytes.get(i + 1).copied() else {
            continue;
        };
        if !matches!(checked, b' ' | b'x' | b'X') || bytes.get(i + 2) != Some(&b']') {
            continue;
        }
        let marker_end = i + 3;
        let mut content_start = marker_end;
        while matches!(bytes.get(content_start), Some(b' ' | b'\t')) {
            content_start += 1;
        }
        if content_start == marker_end {
            continue;
        }
        out.push(found(
            line_start,
            line_end,
            [
                Some((line_start, line_start + marker_end)),
                Some((line_start + i + 1, line_start + i + 2)),
                Some((line_start + content_start, line_end)),
                None,
                None,
                None,
            ],
        ));
    }
    out
}

/// `^[ \t]*([-*+]|\d+[.)])[ \t]+`
fn scan_bullet(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    for (line_start, line_end) in lines(text) {
        let line = &text[line_start..line_end];
        let bytes = line.as_bytes();
        let mut i = 0;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        let marker_start = i;
        let marker_end = if matches!(bytes.get(i), Some(b'-' | b'*' | b'+')) {
            i + 1
        } else {
            let mut j = i;
            while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            if j == i || !matches!(bytes.get(j), Some(b'.' | b')')) {
                continue;
            }
            j + 1
        };
        let mut ws_end = marker_end;
        while matches!(bytes.get(ws_end), Some(b' ' | b'\t')) {
            ws_end += 1;
        }
        if ws_end == marker_end {
            continue;
        }
        out.push(found(
            line_start,
            line_start + ws_end,
            [
                Some((line_start + marker_start, line_start + marker_end)),
                None,
                None,
                None,
                None,
                None,
            ],
        ));
    }
    out
}

/// `^[ \t]*(?:-{3,}|\*{3,})[ \t]*$`
fn scan_rule(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    for (line_start, line_end) in lines(text) {
        let line = &text[line_start..line_end];
        let bytes = line.as_bytes();
        let mut i = 0;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        let Some(&ch) = bytes.get(i) else {
            continue;
        };
        if ch != b'-' && ch != b'*' {
            continue;
        }
        let mut j = i;
        while bytes.get(j) == Some(&ch) {
            j += 1;
        }
        if j - i < 3 {
            continue;
        }
        while matches!(bytes.get(j), Some(b' ' | b'\t')) {
            j += 1;
        }
        if j == bytes.len() {
            out.push(found(line_start, line_end, NO_GROUPS));
        }
    }
    out
}

/// ``(`)([^`\n]+)(`)``
fn scan_code(text: &str) -> Vec<Found> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i = next_boundary(text, i);
            continue;
        }
        let inner_start = i + 1;
        let mut q = inner_start;
        let mut close = None;
        while q < bytes.len() {
            match bytes[q] {
                b'`' => {
                    close = Some(q);
                    break;
                }
                b'\n' => break,
                _ => q += 1,
            }
        }
        if let Some(close) = close.filter(|close| *close > inner_start) {
            out.push(found(
                i,
                close + 1,
                [
                    Some((i, i + 1)),
                    Some((inner_start, close)),
                    Some((close, close + 1)),
                    None,
                    None,
                    None,
                ],
            ));
            i = close + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// `(\*\*|__)(?=\S)(.+?)(?<=\S)(\1)`
fn scan_bold(text: &str) -> Vec<Found> {
    scan_paired(text, &["**", "__"])
}

/// `(~~)(?=\S)(.+?)(?<=\S)(~~)`
fn scan_strike(text: &str) -> Vec<Found> {
    scan_paired(text, &["~~"])
}

fn scan_paired(text: &str, delimiters: &[&str]) -> Vec<Found> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let mut matched = None;
        for delimiter in delimiters {
            if text[i..].starts_with(delimiter) {
                matched = pair_at(text, i, delimiter);
                if matched.is_some() {
                    break;
                }
            }
        }
        if let Some(m) = matched {
            i = m.whole.1;
            out.push(m);
        } else {
            i = next_boundary(text, i);
        }
    }
    out
}

fn pair_at(text: &str, start: usize, delimiter: &str) -> Option<Found> {
    let inner_start = start + delimiter.len();
    let opening = char_at(text, inner_start)?;
    if is_whitespace(opening) || is_line_terminator(opening) {
        return None;
    }
    let mut q = inner_start;
    while q < text.len() {
        if q > inner_start && text[q..].starts_with(delimiter) {
            let before = prev_char(text, q)?;
            if !is_whitespace(before) {
                let end = q + delimiter.len();
                return Some(found(
                    start,
                    end,
                    [
                        Some((start, inner_start)),
                        Some((inner_start, q)),
                        Some((q, end)),
                        None,
                        None,
                        None,
                    ],
                ));
            }
        }
        let ch = char_at(text, q)?;
        if is_line_terminator(ch) {
            return None;
        }
        q += ch.len_utf8();
    }
    None
}

/// `(?<![*\w])([*_])(?=[^\s*_])([^*_\n]+?)(?<=\S)(\1)(?![*\w])`
fn scan_italic(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(m) = italic_at(text, i) {
            i = m.whole.1;
            out.push(m);
        } else {
            i = next_boundary(text, i);
        }
    }
    out
}

fn italic_at(text: &str, start: usize) -> Option<Found> {
    let opener = char_at(text, start)?;
    if opener != '*' && opener != '_' {
        return None;
    }
    if prev_char(text, start).is_some_and(|before| before == '*' || is_word(before)) {
        return None;
    }
    let inner_start = start + opener.len_utf8();
    let first = char_at(text, inner_start)?;
    if is_whitespace(first) || first == '*' || first == '_' {
        return None;
    }
    let mut q = inner_start;
    let mut before = None;
    while q < text.len() {
        let ch = char_at(text, q)?;
        if ch == '\n' || ch == '*' || ch == '_' {
            if ch == opener && q > inner_start {
                let before_ok = before.is_some_and(|before| !is_whitespace(before));
                let end = q + ch.len_utf8();
                let after_ok = char_at(text, end)
                    .is_none_or(|next| next != '*' && !is_word(next));
                if before_ok && after_ok {
                    return Some(found(
                        start,
                        end,
                        [
                            Some((start, inner_start)),
                            Some((inner_start, q)),
                            Some((q, end)),
                            None,
                            None,
                            None,
                        ],
                    ));
                }
            }
            return None;
        }
        before = Some(ch);
        q += ch.len_utf8();
    }
    None
}

/// `(\[)([^\]\n]+)(\]\(([^)\s]+)\))`
fn scan_link(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if char_at(text, i) != Some('[') {
            i = next_boundary(text, i);
            continue;
        }
        let alt_start = i + 1;
        let mut q = alt_start;
        while q < text.len() {
            let Some(ch) = char_at(text, q) else { break };
            if ch == ']' || ch == '\n' {
                break;
            }
            q += ch.len_utf8();
        }
        let mut matched = None;
        if q > alt_start && char_at(text, q) == Some(']') && text[q + 1..].starts_with('(') {
            let target_start = q + 2;
            let mut r = target_start;
            while r < text.len() {
                let Some(ch) = char_at(text, r) else { break };
                if ch == ')' || is_whitespace(ch) {
                    break;
                }
                r += ch.len_utf8();
            }
            if r > target_start && char_at(text, r) == Some(')') {
                matched = Some(found(
                    i,
                    r + 1,
                    [
                        Some((i, alt_start)),
                        Some((alt_start, q)),
                        Some((q, r + 1)),
                        Some((target_start, r)),
                        None,
                        None,
                    ],
                ));
            }
        }
        if let Some(m) = matched {
            i = m.whole.1;
            out.push(m);
        } else {
            i = next_boundary(text, i);
        }
    }
    out
}

/// `!\[[^\]\n]*\]\(([^)\n]+)\)`
fn scan_image(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if !text[i..].starts_with("![") {
            i = next_boundary(text, i);
            continue;
        }
        let alt_start = i + 2;
        let mut q = alt_start;
        while q < text.len() {
            let Some(ch) = char_at(text, q) else { break };
            if ch == ']' || ch == '\n' {
                break;
            }
            q += ch.len_utf8();
        }
        let mut matched = None;
        if char_at(text, q) == Some(']') && text[q + 1..].starts_with('(') {
            let target_start = q + 2;
            let mut r = target_start;
            while r < text.len() {
                let Some(ch) = char_at(text, r) else { break };
                if ch == ')' || ch == '\n' {
                    break;
                }
                r += ch.len_utf8();
            }
            if r > target_start && char_at(text, r) == Some(')') {
                matched = Some(found(
                    i,
                    r + 1,
                    [
                        Some((target_start, r)),
                        None,
                        None,
                        None,
                        None,
                        None,
                    ],
                ));
            }
        }
        if let Some(m) = matched {
            i = m.whole.1;
            out.push(m);
        } else {
            i = next_boundary(text, i);
        }
    }
    out
}

/// `(?<![(\[<])\bhttps?://[^\s)>\]]+`
fn scan_bare_url(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let previous_ok = match prev_char(text, i) {
            Some(before) => {
                before != '(' && before != '[' && before != '<' && !is_word(before)
            }
            None => true,
        };
        if previous_ok {
            let prefix = if text[i..].starts_with("https://") {
                8
            } else if text[i..].starts_with("http://") {
                7
            } else {
                0
            };
            if prefix > 0 {
                let mut end = i + prefix;
                while end < text.len() {
                    let Some(ch) = char_at(text, end) else { break };
                    if is_whitespace(ch) || ch == ')' || ch == '>' || ch == ']' {
                        break;
                    }
                    end += ch.len_utf8();
                }
                if end > i + prefix {
                    out.push(found(i, end, NO_GROUPS));
                    i = end;
                    continue;
                }
            }
        }
        i = next_boundary(text, i);
    }
    out
}

// ---------------------------------------------------------------------------
// List continuation
// ---------------------------------------------------------------------------

/// Reproduces `continueList` for an empty selection at `cursor`.
///
/// A list item gets `\n` plus the same indent and the next marker; a task item starts unchecked;
/// numbers increase. An item whose text before the cursor is empty loses its marker and does not
/// add a line (the Swift code replaces the prefix with `""`). Returns `None` when Swift lets the
/// default Return happen, including when the selection is not empty (the Swift guard bails out).
pub fn on_return(text: &str, cursor: usize) -> Option<Edit> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = line_start(text, cursor);
    let head = &text[line_start..cursor];
    // `$` can match before a final CR; the capture before it is what Swift sees as group 6.
    let head = head.strip_suffix('\r').unwrap_or(head);
    let item = list_item(head)?;

    let content = &head[item.content_start..];
    if content.trim().is_empty() {
        return Some(Edit {
            range: line_start..cursor,
            text: String::new(),
            cursor: line_start,
        });
    }

    let marker = match item.marker {
        ListMarker::Task(bullet) => format!("{} [ ] ", bullet as char),
        ListMarker::Bullet(bullet) => format!("{} ", bullet as char),
        ListMarker::Numbered(number, delimiter) => {
            format!("{}{} ", number.saturating_add(1), delimiter as char)
        }
    };
    let inserted = format!("\n{}{marker}", &head[..item.indent_end]);
    let new_cursor = cursor + inserted.len();
    Some(Edit {
        range: cursor..cursor,
        text: inserted,
        cursor: new_cursor,
    })
}

struct ListItem {
    indent_end: usize,
    marker: ListMarker,
    content_start: usize,
}

enum ListMarker {
    Task(u8),
    Bullet(u8),
    Numbered(i64, u8),
}

/// `^([ \t]*)(?:([-*+])[ \t]+\[[ xX]\]|([-*+])|(\d+)([.)]))[ \t]+(.*)$`
fn list_item(head: &str) -> Option<ListItem> {
    let bytes = head.as_bytes();
    let mut i = 0;
    while matches!(bytes.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    let indent_end = i;
    let &first = bytes.get(i)?;

    if matches!(first, b'-' | b'*' | b'+') {
        let bullet = first;
        i += 1;
        let ws_start = i;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        if i == ws_start {
            return None;
        }
        let after_ws = i;

        // Try the task alternative first, including its own required trailing whitespace.
        if bytes.get(i) == Some(&b'[')
            && matches!(bytes.get(i + 1), Some(b' ' | b'x' | b'X'))
            && bytes.get(i + 2) == Some(&b']')
        {
            let mut content_start = i + 3;
            let after_box = content_start;
            while matches!(bytes.get(content_start), Some(b' ' | b'\t')) {
                content_start += 1;
            }
            if content_start > after_box {
                return Some(ListItem {
                    indent_end,
                    marker: ListMarker::Task(bullet),
                    content_start,
                });
            }
        }

        return Some(ListItem {
            indent_end,
            marker: ListMarker::Bullet(bullet),
            content_start: after_ws,
        });
    }

    if first.is_ascii_digit() {
        let number_start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let delimiter = *bytes.get(i)?;
        if delimiter != b'.' && delimiter != b')' {
            return None;
        }
        i += 1;
        let number = head[number_start..i - 1].parse::<i64>().unwrap_or(0);
        let ws_start = i;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        if i == ws_start {
            return None;
        }
        return Some(ListItem {
            indent_end,
            marker: ListMarker::Numbered(number, delimiter),
            content_start: i,
        });
    }

    None
}

fn line_start(text: &str, cursor: usize) -> usize {
    let mut start = 0;
    for (i, ch) in text[..cursor].char_indices() {
        if is_line_terminator(ch) {
            start = i + ch.len_utf8();
        }
    }
    start
}

// ---------------------------------------------------------------------------
// Screenshots
// ---------------------------------------------------------------------------

/// The Markdown a double-click inserts when the fairy carries screenshots (`NoteTextView.mouseDown`).
///
/// Images go on their own lines: a newline is added before unless the caret is at the start of the
/// text or after a newline, and after unless the caret is at the end or before a newline. The caret
/// ends after everything inserted. `markdown` may hold several images separated by newlines.
pub fn insert_screenshot(text: &str, cursor: usize, markdown: &str) -> Edit {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = match prev_char(text, cursor) {
        Some('\n') | None => "",
        Some(_) => "\n",
    };
    let after = match char_at(text, cursor) {
        Some('\n') | None => "",
        Some(_) => "\n",
    };
    let inserted = format!("{before}{markdown}{after}");
    Edit {
        range: cursor..cursor,
        cursor: cursor + inserted.len(),
        text: inserted,
    }
}

// ---------------------------------------------------------------------------
// UTF-16 ranges
// ---------------------------------------------------------------------------

/// Converts a UTF-8 byte range to the UTF-16 code-unit range a Windows text widget uses.
///
/// Ends that are not on char boundaries are moved inwards first.
pub fn utf16_range(text: &str, bytes: Range<usize>) -> Range<usize> {
    let start = floor_boundary(text, bytes.start.min(text.len()));
    let end = ceil_boundary(text, bytes.end.min(text.len())).max(start);
    utf16_len(&text[..start])..utf16_len(&text[..end])
}

/// Converts a UTF-16 code-unit range back to UTF-8 bytes; the inverse of [`utf16_range`].
///
/// A unit inside a surrogate pair maps to the boundary after that character.
pub fn utf8_range(text: &str, units: Range<usize>) -> Range<usize> {
    let mut start = None;
    let mut end = text.len();
    let mut at = 0;
    for (byte, ch) in text.char_indices() {
        if start.is_none() && at >= units.start {
            start = Some(byte);
        }
        if at >= units.end {
            end = byte;
            break;
        }
        at += ch.len_utf16();
    }
    let start = start.unwrap_or(text.len());
    let end = end.max(start);
    start..end
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn floor_boundary(text: &str, mut byte: usize) -> usize {
    byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn ceil_boundary(text: &str, mut byte: usize) -> usize {
    byte = byte.min(text.len());
    while byte < text.len() && !text.is_char_boundary(byte) {
        byte += 1;
    }
    byte
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Lines without their terminators, as `anchorsMatchLines` sees them. CRLF is one terminator.
fn lines(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < text.len() {
        let Some(ch) = char_at(text, i) else {
            break;
        };
        if ch == '\r' {
            out.push((start, i));
            i += 1;
            if text.as_bytes().get(i) == Some(&b'\n') {
                i += 1;
            }
            start = i;
        } else if is_line_terminator(ch) {
            out.push((start, i));
            i += ch.len_utf8();
            start = i;
        } else {
            i += ch.len_utf8();
        }
    }
    out.push((start, text.len()));
    out
}

fn char_at(text: &str, byte: usize) -> Option<char> {
    text.get(byte..)?.chars().next()
}

fn prev_char(text: &str, byte: usize) -> Option<char> {
    text.get(..byte)?.chars().next_back()
}

fn next_boundary(text: &str, byte: usize) -> usize {
    match char_at(text, byte) {
        Some(ch) => byte + ch.len_utf8(),
        None => text.len(),
    }
}

/// ICU `.` with `.anchorsMatchLines` does not match any of these.
fn is_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

fn is_whitespace(ch: char) -> bool {
    ch.is_whitespace()
}

/// The approximation of ICU `\w` used by the lookarounds and `\b`.
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '\u{200c}' || ch == '\u{200d}'
}

// ---------------------------------------------------------------------------
// Blocks and live editing
//
// The note editor shows Markdown the way Notion does: one block per line, block markers hidden
// and drawn (bullets, checkboxes, dividers), and Markdown shortcuts converted as they are typed.
// The file stays plain Markdown. Everything below works on text with `\n` line breaks only (the
// editor converts the control's `\r` before calling in) and on byte offsets.
// ---------------------------------------------------------------------------

/// What one line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph,
    /// `#`, `##` or `###`, followed by a space; the content may be empty.
    Heading(u8),
    Bullet,
    /// The number as written.
    Numbered(u32),
    /// `- [ ] ` or `- [x] `; true when checked.
    Todo(bool),
    Quote,
    /// A ```` ``` ```` line that opens or closes a code block.
    Fence,
    /// A line inside a fenced code block.
    Code,
    /// `---`, `***` or longer on a line of its own.
    Divider,
    /// A line that holds one image link and nothing else.
    Image,
}

impl Block {
    pub fn is_list(self) -> bool {
        matches!(self, Block::Bullet | Block::Numbered(_) | Block::Todo(_))
    }

    /// Blocks that Backspace at their start turns into text.
    fn has_prefix(self) -> bool {
        matches!(
            self,
            Block::Heading(_) | Block::Bullet | Block::Numbered(_) | Block::Todo(_) | Block::Quote
        )
    }
}

/// One line of the note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The line without its `\n`.
    pub range: Range<usize>,
    pub block: Block,
    /// Bytes of leading spaces and tabs.
    pub indent: usize,
    /// Nesting depth of a list item (0 at the top level); 0 for other blocks.
    pub depth: u8,
    /// Where the content starts, after the indent and the block marker.
    pub content: usize,
}

impl Line {
    /// The indent and marker, which the editor hides and draws instead.
    pub fn prefix(&self) -> Range<usize> {
        self.range.start..self.content
    }
}

/// Splits the note into lines and tells what each one is.
pub fn blocks(text: &str) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut in_code = false;
    // Indents of the open list levels, outermost first.
    let mut stack: Vec<usize> = Vec::new();
    let mut start = 0;
    loop {
        let end = text[start..].find('\n').map_or(text.len(), |at| start + at);
        let line = &text[start..end];
        let (block, indent, content) = if is_fence(line) {
            in_code = !in_code;
            (Block::Fence, 0, start)
        } else if in_code {
            (Block::Code, 0, start)
        } else {
            let (block, indent, content) = classify(line);
            (block, indent, start + content)
        };
        let depth = if block.is_list() {
            while stack.last().is_some_and(|&top| top > indent) {
                stack.pop();
            }
            if stack.last() != Some(&indent) {
                stack.push(indent);
            }
            (stack.len() - 1).min(8) as u8
        } else {
            stack.clear();
            0
        };
        out.push(Line {
            range: start..end,
            block,
            indent,
            depth,
            content,
        });
        if end == text.len() {
            break;
        }
        start = end + 1;
    }
    out
}

fn is_fence(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    line.len() - trimmed.len() <= 3 && trimmed.starts_with("```")
}

/// The block, the indent and the content offset of one line outside code.
fn classify(line: &str) -> (Block, usize, usize) {
    let bytes = line.as_bytes();
    let mut indent = 0;
    while matches!(bytes.get(indent), Some(b' ' | b'\t')) {
        indent += 1;
    }
    let skip_ws = |mut i: usize| {
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        i
    };
    if !scan_rule(line).is_empty() {
        return (Block::Divider, indent, line.len());
    }
    if indent == 0 && bytes.first() == Some(&b'#') {
        let mut hashes = 0;
        while bytes.get(hashes) == Some(&b'#') {
            hashes += 1;
        }
        if hashes <= 3 && matches!(bytes.get(hashes), Some(b' ' | b'\t')) {
            return (Block::Heading(hashes as u8), 0, skip_ws(hashes));
        }
    }
    if indent == 0 && bytes.first() == Some(&b'>') {
        let content = if matches!(bytes.get(1), Some(b' ' | b'\t')) { 2 } else { 1 };
        return (Block::Quote, 0, content);
    }
    if matches!(bytes.get(indent), Some(b'-' | b'*' | b'+'))
        && matches!(bytes.get(indent + 1), Some(b' ' | b'\t'))
    {
        let after = skip_ws(indent + 1);
        if bytes.get(after) == Some(&b'[')
            && matches!(bytes.get(after + 1), Some(b' ' | b'x' | b'X'))
            && bytes.get(after + 2) == Some(&b']')
            && (after + 3 == bytes.len() || matches!(bytes.get(after + 3), Some(b' ' | b'\t')))
        {
            let checked = bytes[after + 1] != b' ';
            return (Block::Todo(checked), indent, skip_ws(after + 3));
        }
        return (Block::Bullet, indent, after);
    }
    let mut digits = indent;
    while bytes.get(digits).is_some_and(u8::is_ascii_digit) {
        digits += 1;
    }
    if digits > indent
        && digits - indent <= 9
        && matches!(bytes.get(digits), Some(b'.' | b')'))
        && matches!(bytes.get(digits + 1), Some(b' ' | b'\t'))
    {
        let number = line[indent..digits].parse().unwrap_or(1);
        return (Block::Numbered(number), indent, skip_ws(digits + 1));
    }
    let trimmed = line.trim();
    if trimmed.starts_with("![")
        && let [image] = scan_image(trimmed).as_slice()
        && image.whole == (0, trimmed.len())
    {
        return (Block::Image, indent, indent);
    }
    (Block::Paragraph, indent, 0)
}

/// The index of the line holding `byte` (a byte on a `\n` belongs to the line it ends).
pub fn line_at(lines: &[Line], byte: usize) -> usize {
    lines
        .iter()
        .position(|line| byte <= line.range.end)
        .unwrap_or(lines.len().saturating_sub(1))
}

/// The image path of an image line (`![...](path)`), as written.
pub fn image_target(line: &str) -> Option<&str> {
    let lead = line.len() - line.trim_start().len();
    let trimmed = line.trim();
    let images = scan_image(trimmed);
    let [image] = images.as_slice() else {
        return None;
    };
    if image.whole != (0, trimmed.len()) {
        return None;
    }
    let (a, b) = image.group(1)?;
    Some(&line[lead + a..lead + b])
}

/// The link target under `byte`: a `[text](url)` link or a bare `http(s)://` address. Image
/// links are not links here.
pub fn link_at(text: &str, byte: usize) -> Option<String> {
    let images = scan_image(text);
    let in_image = |at: usize| images.iter().any(|m| m.whole.0 <= at && at < m.whole.1);
    for m in scan_link(text) {
        if m.whole.0 <= byte && byte < m.whole.1 && !in_image(m.whole.0) {
            let (a, b) = m.group(4)?;
            return Some(text[a..b].to_owned());
        }
    }
    scan_bare_url(text)
        .into_iter()
        .find(|m| m.whole.0 <= byte && byte < m.whole.1)
        .map(|m| text[m.whole.0..m.whole.1].to_owned())
}

/// A text change with the selection to show afterwards. Offsets are bytes; `range` is in the
/// old text, `selection` in the new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub range: Range<usize>,
    pub text: String,
    pub selection: Range<usize>,
}

impl Change {
    /// The text after the change.
    pub fn apply(&self, text: &str) -> String {
        replace(text, self.range.clone(), &self.text)
    }

    /// True when the change leaves the text as it was (the key was handled; nothing to do).
    pub fn is_empty(&self) -> bool {
        self.range.is_empty() && self.text.is_empty()
    }
}

/// The smallest change that turns `old` into `new`, with a selection in `new`.
pub fn change_between(old: &str, new: &str, selection: Range<usize>) -> Change {
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let room = old.len().min(new.len()) - prefix;
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix)
    {
        suffix -= 1;
    }
    Change {
        range: prefix..old.len() - suffix,
        text: new[prefix..new.len() - suffix].to_owned(),
        selection,
    }
}

fn replace(text: &str, range: Range<usize>, with: &str) -> String {
    let mut out = String::with_capacity(text.len() + with.len());
    out.push_str(&text[..range.start]);
    out.push_str(with);
    out.push_str(&text[range.end..]);
    out
}

/// The marker the next item after `line` starts with (after the indent).
fn next_marker(text: &str, line: &Line) -> String {
    let marker = &text[line.range.start + line.indent..line.content];
    let bullet = marker.chars().next().unwrap_or('-');
    match line.block {
        Block::Bullet => format!("{bullet} "),
        Block::Todo(_) => format!("{bullet} [ ] "),
        Block::Numbered(n) => {
            let delimiter = if marker.contains(')') { ')' } else { '.' };
            format!("{}{delimiter} ", n.saturating_add(1))
        }
        Block::Quote => "> ".to_owned(),
        _ => String::new(),
    }
}

/// Enter with the selection `sel` (Notion rules). A list item, to-do or quote continues on the
/// next line and takes the text after the caret with it; a to-do starts unchecked. Enter on an
/// empty item leaves the list (a nested item moves out one level first). Numbered lists are
/// renumbered. `None` lets the control insert a plain line break: headings (the next line is
/// text), paragraphs, code, and any selection.
pub fn enter(text: &str, sel: Range<usize>) -> Option<Change> {
    if !sel.is_empty() || sel.end > text.len() {
        return None;
    }
    let cursor = sel.start;
    let lines = blocks(text);
    let line = &lines[line_at(&lines, cursor)];
    if !matches!(
        line.block,
        Block::Bullet | Block::Numbered(_) | Block::Todo(_) | Block::Quote
    ) || cursor < line.content
    {
        return None;
    }
    if text[line.content..line.range.end].trim().is_empty() {
        if line.depth > 0 {
            return indent(text, cursor..cursor, true);
        }
        let new = renumber(&replace(text, line.prefix(), ""));
        let at = line.range.start;
        return Some(change_between(text, &new, at..at));
    }
    let inserted = format!(
        "\n{}{}",
        &text[line.range.start..line.range.start + line.indent],
        next_marker(text, line)
    );
    let new = replace(text, cursor..cursor, &inserted);
    let caret = cursor + inserted.len();
    let renumbered = renumber(&new);
    let caret = moved_by_renumber(&new, &renumbered, caret);
    Some(change_between(text, &renumbered, caret..caret))
}

/// Where `byte` in `before` lands in `after`, when the two differ only in list numbers.
fn moved_by_renumber(before: &str, after: &str, byte: usize) -> usize {
    if before.len() == after.len() {
        return byte;
    }
    let old_lines = blocks(before);
    let new_lines = blocks(after);
    let index = line_at(&old_lines, byte);
    let (Some(old), Some(new)) = (old_lines.get(index), new_lines.get(index)) else {
        return byte.min(after.len());
    };
    if byte >= old.content {
        (new.content + (byte - old.content)).min(new.range.end)
    } else {
        (new.range.start + (byte - old.range.start)).min(new.range.end)
    }
}

/// Backspace with the selection `sel`: at the very start of a heading, list item, to-do or
/// quote (right after its hidden marker) the line turns into plain text and keeps its words.
/// `None` lets the control delete a character.
pub fn backspace(text: &str, sel: Range<usize>) -> Option<Change> {
    if !sel.is_empty() || sel.end > text.len() {
        return None;
    }
    let cursor = sel.start;
    let lines = blocks(text);
    let line = &lines[line_at(&lines, cursor)];
    if !line.block.has_prefix() || cursor != line.content || line.content == line.range.start {
        return None;
    }
    let new = renumber(&replace(text, line.prefix(), ""));
    let at = line.range.start;
    Some(change_between(text, &new, at..at))
}

/// Tab (`outdent` false) or Shift+Tab (true) on the list items in the selection. Tab moves an
/// item under the item above it at its level; Shift+Tab moves it out to its parent's level. An
/// item that cannot move gives an empty change (the key is still handled, as in Notion); `None`
/// when no line in the selection is a list item.
pub fn indent(text: &str, sel: Range<usize>, outdent: bool) -> Option<Change> {
    let lines = blocks(text);
    let first = line_at(&lines, sel.start);
    let last = line_at(&lines, sel.end.max(sel.start));
    if !lines[first..=last].iter().any(|line| line.block.is_list()) {
        return None;
    }
    let mut new = text.to_owned();
    let mut selection = sel.clone();
    // Top-down; offsets in later lines move with each edit, so lines are read again each time.
    for index in first..=last {
        let current = blocks(&new);
        let line = current[index].clone();
        if !line.block.is_list() {
            continue;
        }
        let above = || {
            current[..index]
                .iter()
                .rev()
                .take_while(|line| line.block.is_list())
        };
        let target = if outdent {
            above()
                .find(|other| other.indent < line.indent)
                .map(|parent| parent.indent)
                .or((line.indent > 0).then_some(0))
        } else {
            above()
                .take_while(|other| other.indent >= line.indent)
                .find(|other| other.indent == line.indent)
                .map(|sibling| sibling.indent + marker_width(&new, sibling))
        };
        let Some(target) = target else { continue };
        let start = line.range.start;
        let old = line.indent;
        new = replace(&new, start..start + old, &" ".repeat(target));
        let shift = |at: usize| {
            if at >= start + old {
                at + target - old
            } else if at > start {
                start + target.min(at - start)
            } else {
                at
            }
        };
        selection = shift(selection.start)..shift(selection.end);
    }
    let renumbered = renumber(&new);
    let selection = moved_by_renumber(&new, &renumbered, selection.start)
        ..moved_by_renumber(&new, &renumbered, selection.end);
    Some(change_between(text, &renumbered, selection))
}

/// The width of a list item's marker with its space ("- " is 2, "10. " is 4; a to-do counts
/// as its bullet, so nested items line up under the box).
fn marker_width(text: &str, line: &Line) -> usize {
    let marker = &text[line.range.start + line.indent..line.content];
    match line.block {
        Block::Todo(_) => marker.find('[').unwrap_or(2),
        _ => marker.len(),
    }
}

/// Numbered lists counted again: in each run of list items, numbered siblings at one level
/// count up from the first one's number. Only those numbers change.
pub fn renumber(text: &str) -> String {
    let lines = blocks(text);
    let numbers = list_numbers_of(&lines);
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (line, number) in lines.iter().zip(numbers) {
        let (Block::Numbered(written), Some(number)) = (line.block, number) else {
            continue;
        };
        if written == number {
            continue;
        }
        let digits = line.range.start + line.indent;
        let digits_end = digits + text[digits..].bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&text[copied..digits]);
        out.push_str(&number.to_string());
        copied = digits_end;
    }
    out.push_str(&text[copied..]);
    out
}

/// The number each numbered item shows (None for other lines), counted as [`renumber`] does.
pub fn list_numbers(text: &str) -> Vec<Option<u32>> {
    list_numbers_of(&blocks(text))
}

fn list_numbers_of(lines: &[Line]) -> Vec<Option<u32>> {
    let mut out = Vec::with_capacity(lines.len());
    // (indent, next number) for each open level.
    let mut counters: Vec<(usize, Option<u32>)> = Vec::new();
    for line in lines {
        if !line.block.is_list() {
            counters.clear();
            out.push(None);
            continue;
        }
        while counters.last().is_some_and(|&(indent, _)| indent > line.indent) {
            counters.pop();
        }
        if counters.last().is_none_or(|&(indent, _)| indent < line.indent) {
            counters.push((line.indent, None));
        }
        let nested = counters.len() > 1;
        let slot = counters.last_mut().expect("pushed above");
        if let Block::Numbered(written) = line.block {
            let number = slot.1.unwrap_or(if nested { 1 } else { written });
            slot.1 = Some(number.saturating_add(1));
            out.push(Some(number));
        } else {
            // A bullet between numbered siblings ends their count.
            slot.1 = None;
            out.push(None);
        }
    }
    out
}

/// Typing `ch` over the selection `sel`, for the shortcuts that rewrite text: `[] ` or `[ ] `
/// at the start of a line becomes a to-do, ```` ``` ```` opens a code block with the caret
/// inside, and `---` becomes a divider with the caret on a new line below. Other shortcuts
/// (`# `, `- `, `1. `, `> `, `**bold**`...) are Markdown already and need no rewrite. The
/// change includes the typed character.
pub fn type_char(text: &str, sel: Range<usize>, ch: char) -> Option<Change> {
    if !sel.is_empty() || sel.end > text.len() {
        return None;
    }
    let cursor = sel.start;
    let lines = blocks(text);
    let line = &lines[line_at(&lines, cursor)];
    if matches!(line.block, Block::Code | Block::Fence) {
        return None;
    }
    let start = line.range.start;
    let head = &text[start..cursor];
    let rest_empty = cursor == line.range.end;
    let (inserted, caret) = match ch {
        ' ' if matches!(head, "[]" | "[ ]") => ("- [ ] ", 6),
        ' ' if matches!(head, "[x]" | "[X]") => ("- [x] ", 6),
        '`' if head == "``" && rest_empty => ("```\n\n```", 4),
        '-' if head == "--" && rest_empty => ("---\n", 4),
        _ => return None,
    };
    let new = replace(text, start..cursor, inserted);
    let caret = start + caret;
    Some(change_between(text, &new, caret..caret))
}

/// The word around `cursor` (letters, digits and `_`), or an empty range at the cursor.
pub fn word_at(text: &str, cursor: usize) -> Range<usize> {
    let cursor = floor_boundary(text, cursor);
    let start = text[..cursor]
        .char_indices()
        .rev()
        .take_while(|(_, ch)| is_word(*ch))
        .last()
        .map_or(cursor, |(at, _)| at);
    let end = text[cursor..]
        .char_indices()
        .take_while(|(_, ch)| is_word(*ch))
        .last()
        .map_or(cursor, |(at, ch)| cursor + at + ch.len_utf8());
    start..end
}

/// The inline marks the formatting keys toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline {
    Bold,
    Italic,
    Code,
    Strike,
}

impl Inline {
    pub fn marker(self) -> &'static str {
        match self {
            Inline::Bold => "**",
            Inline::Italic => "*",
            Inline::Code => "`",
            Inline::Strike => "~~",
        }
    }

    /// Is a range wrapped in this mark? `left` and `right` are the runs of the marker's
    /// character right outside it.
    fn wraps(self, left: usize, right: usize) -> bool {
        match self {
            // One star is italic, three are bold italic; two are only bold.
            Inline::Italic => left % 2 == 1 && right % 2 == 1,
            Inline::Bold | Inline::Strike => left >= 2 && right >= 2,
            Inline::Code => left >= 1 && right >= 1,
        }
    }
}

/// Ctrl+B and friends: toggles a mark on the selection, or on the word at the caret. With no
/// word, inserts the two markers with the caret between them. A selection over several lines
/// is marked line by line (blank lines are skipped). Marking bold text bold removes the mark.
pub fn toggle_inline(text: &str, sel: Range<usize>, mark: Inline) -> Change {
    let marker = mark.marker();
    let m = marker.len();
    let target = if sel.is_empty() { word_at(text, sel.start) } else { sel.clone() };
    if target.is_empty() {
        let new = replace(text, target.clone(), &format!("{marker}{marker}"));
        let caret = target.start + m;
        return change_between(text, &new, caret..caret);
    }
    let mut pieces: Vec<Range<usize>> = Vec::new();
    let mut at = target.start;
    for part in text[target.clone()].split('\n') {
        let lead = part.len() - part.trim_start().len();
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            pieces.push(at + lead..at + lead + trimmed.len());
        }
        at += part.len() + 1;
    }
    if pieces.is_empty() {
        return change_between(text, text, sel);
    }
    let ch = marker.as_bytes()[0];
    let bytes = text.as_bytes();
    let left_run = |at: usize| bytes[..at].iter().rev().take_while(|b| **b == ch).count();
    let right_run = |at: usize| bytes[at..].iter().take_while(|b| **b == ch).count();
    let outside = pieces
        .iter()
        .all(|piece| mark.wraps(left_run(piece.start), right_run(piece.end)));
    let inside = !outside
        && pieces.iter().all(|piece| {
            let s = &text[piece.clone()];
            s.len() >= 2 * m
                && s.starts_with(marker)
                && s.ends_with(marker)
                && mark.wraps(
                    s.bytes().take_while(|b| *b == ch).count(),
                    s.bytes().rev().take_while(|b| *b == ch).count(),
                )
        });
    let mut new = text.to_owned();
    for piece in pieces.iter().rev() {
        if outside {
            new = replace(&new, piece.end..piece.end + m, "");
            new = replace(&new, piece.start - m..piece.start, "");
        } else if inside {
            new = replace(&new, piece.end - m..piece.end, "");
            new = replace(&new, piece.start..piece.start + m, "");
        } else {
            new = replace(&new, piece.end..piece.end, marker);
            new = replace(&new, piece.start..piece.start, marker);
        }
    }
    let count = pieces.len();
    let first = pieces[0].start;
    let last = pieces[count - 1].end;
    let selection = if outside {
        first - m..last - (2 * count - 1) * m
    } else if inside {
        first..last - 2 * count * m
    } else {
        first + m..last + (2 * count - 1) * m
    };
    let selection = if sel.is_empty() {
        // The caret keeps its place within the word.
        let caret = (selection.start + (sel.start - target.start)).min(selection.end);
        caret..caret
    } else {
        selection
    };
    change_between(text, &new, selection)
}

/// Ctrl+K: wraps the selection (or the word at the caret) as `[text]()` with the caret where
/// the address goes. Inside an existing link, removes the link and keeps its text selected.
pub fn toggle_link(text: &str, sel: Range<usize>) -> Change {
    let images = scan_image(text);
    for m in scan_link(text) {
        let in_image = images.iter().any(|i| i.whole.0 <= m.whole.0 && m.whole.0 < i.whole.1);
        if !in_image && m.whole.0 <= sel.start && sel.end <= m.whole.1 {
            let (a, b) = m.group(2).unwrap_or((m.whole.0, m.whole.0));
            let label = text[a..b].to_owned();
            let new = replace(text, m.whole.0..m.whole.1, &label);
            return change_between(text, &new, m.whole.0..m.whole.0 + label.len());
        }
    }
    let target = if sel.is_empty() { word_at(text, sel.start) } else { sel };
    if target.is_empty() || text[target.clone()].contains('\n') {
        let new = replace(text, target.start..target.start, "[]()");
        let caret = target.start + 1;
        return change_between(text, &new, caret..caret);
    }
    let wrapped = format!("[{}]()", &text[target.clone()]);
    let new = replace(text, target.clone(), &wrapped);
    let caret = target.start + wrapped.len() - 1;
    change_between(text, &new, caret..caret)
}

/// What a block command (the slash menu, Ctrl+Alt+1 and the others) turns lines into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockCommand {
    Text,
    Heading(u8),
    Bullet,
    Numbered,
    Todo,
    Quote,
    Code,
    Divider,
}

impl BlockCommand {
    fn prefix(self) -> &'static str {
        match self {
            BlockCommand::Heading(1) => "# ",
            BlockCommand::Heading(2) => "## ",
            BlockCommand::Heading(_) => "### ",
            BlockCommand::Bullet => "- ",
            BlockCommand::Numbered => "1. ",
            BlockCommand::Todo => "- [ ] ",
            BlockCommand::Quote => "> ",
            BlockCommand::Text | BlockCommand::Code | BlockCommand::Divider => "",
        }
    }

    fn is(self, block: Block) -> bool {
        match (self, block) {
            (BlockCommand::Heading(a), Block::Heading(b)) => a == b,
            (BlockCommand::Bullet, Block::Bullet)
            | (BlockCommand::Numbered, Block::Numbered(_))
            | (BlockCommand::Todo, Block::Todo(_))
            | (BlockCommand::Quote, Block::Quote)
            | (BlockCommand::Text, Block::Paragraph) => true,
            _ => false,
        }
    }
}

/// Turns the lines in the selection into a block type. When every line already is that type,
/// they go back to plain text (the keys toggle). Code wraps the lines in a fenced block; a
/// divider goes on its own line below the caret's line. The caret keeps its place in the words.
pub fn set_block(text: &str, sel: Range<usize>, command: BlockCommand) -> Change {
    let lines = blocks(text);
    let first = line_at(&lines, sel.start);
    let last = line_at(&lines, sel.end.max(sel.start));
    match command {
        BlockCommand::Divider => return insert_divider(text, &lines, last),
        BlockCommand::Code => return wrap_code(text, &lines, first, last, sel),
        _ => {}
    }
    let chosen = &lines[first..=last];
    let command = if command != BlockCommand::Text
        && chosen.iter().all(|line| command.is(line.block))
    {
        BlockCommand::Text
    } else {
        command
    };
    let mut new = text.to_owned();
    let mut selection = sel;
    for line in chosen.iter().rev() {
        if matches!(line.block, Block::Code | Block::Fence | Block::Divider | Block::Image) {
            continue;
        }
        // List items keep their nesting when they stay list items.
        let keep_indent = line.block.is_list()
            && matches!(
                command,
                BlockCommand::Bullet | BlockCommand::Numbered | BlockCommand::Todo
            );
        let indent = if keep_indent { line.indent } else { 0 };
        let old = line.prefix();
        let prefix = format!(
            "{}{}",
            &text[line.range.start..line.range.start + indent],
            command.prefix()
        );
        new = replace(&new, old.clone(), &prefix);
        let shift = |at: usize| {
            if at >= old.end {
                at + prefix.len() - old.len()
            } else if at >= old.start {
                old.start + prefix.len()
            } else {
                at
            }
        };
        selection = shift(selection.start)..shift(selection.end);
    }
    let renumbered = renumber(&new);
    let selection = moved_by_renumber(&new, &renumbered, selection.start)
        ..moved_by_renumber(&new, &renumbered, selection.end);
    change_between(text, &renumbered, selection)
}

fn insert_divider(text: &str, lines: &[Line], index: usize) -> Change {
    let line = &lines[index];
    // `---` right under a line of text makes that line a heading in other Markdown apps, so a
    // divider after text is written `***` (the Swift styler takes both).
    let under_text = |above: Option<&Line>| {
        above.is_some_and(|above| {
            above.block == Block::Paragraph && !text[above.range.clone()].trim().is_empty()
        })
    };
    if text[line.range.clone()].trim().is_empty() {
        let rule = if under_text(index.checked_sub(1).map(|i| &lines[i])) { "***" } else { "---" };
        let inserted = format!("{rule}\n");
        let new = replace(text, line.range.clone(), &inserted);
        let caret = line.range.start + inserted.len();
        return change_between(text, &new, caret..caret);
    }
    let rule = if under_text(Some(line)) { "***" } else { "---" };
    let inserted = format!("\n{rule}\n");
    let new = replace(text, line.range.end..line.range.end, &inserted);
    let caret = line.range.end + inserted.len();
    change_between(text, &new, caret..caret)
}

fn wrap_code(text: &str, lines: &[Line], first: usize, last: usize, sel: Range<usize>) -> Change {
    let start = lines[first].range.start;
    let end = lines[last].range.end;
    let body = &text[start..end];
    if body.trim().is_empty() {
        let new = replace(text, start..end, "```\n\n```");
        return change_between(text, &new, start + 4..start + 4);
    }
    let new = replace(text, start..end, &format!("```\n{body}\n```"));
    change_between(text, &new, sel.start + 4..sel.end + 4)
}

/// Ctrl+Enter, or a click on a to-do's box: checks or unchecks the to-do on the line holding
/// `byte`. The selection stays where it was.
pub fn toggle_todo(text: &str, byte: usize, sel: Range<usize>) -> Option<Change> {
    let lines = blocks(text);
    let line = &lines[line_at(&lines, byte)];
    let Block::Todo(checked) = line.block else {
        return None;
    };
    let open = line.range.start + text[line.prefix()].find('[')?;
    let new = replace(text, open + 1..open + 2, if checked { " " } else { "x" });
    Some(change_between(text, &new, sel))
}

/// The entries of the slash menu, in the order shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashItem {
    Text,
    Heading1,
    Heading2,
    Heading3,
    Bullet,
    Numbered,
    Todo,
    Quote,
    Code,
    Divider,
}

impl SlashItem {
    pub const ALL: [SlashItem; 10] = [
        SlashItem::Text,
        SlashItem::Heading1,
        SlashItem::Heading2,
        SlashItem::Heading3,
        SlashItem::Bullet,
        SlashItem::Numbered,
        SlashItem::Todo,
        SlashItem::Quote,
        SlashItem::Code,
        SlashItem::Divider,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SlashItem::Text => "Text",
            SlashItem::Heading1 => "Heading 1",
            SlashItem::Heading2 => "Heading 2",
            SlashItem::Heading3 => "Heading 3",
            SlashItem::Bullet => "Bulleted list",
            SlashItem::Numbered => "Numbered list",
            SlashItem::Todo => "To-do",
            SlashItem::Quote => "Quote",
            SlashItem::Code => "Code",
            SlashItem::Divider => "Divider",
        }
    }

    /// Other words the filter matches.
    fn keywords(self) -> &'static [&'static str] {
        match self {
            SlashItem::Text => &["plain", "paragraph"],
            SlashItem::Heading1 => &["h1", "title"],
            SlashItem::Heading2 => &["h2", "subtitle"],
            SlashItem::Heading3 => &["h3"],
            SlashItem::Bullet => &["ul", "bullet", "unordered"],
            SlashItem::Numbered => &["ol", "ordered"],
            SlashItem::Todo => &["todo", "task", "checkbox", "check"],
            SlashItem::Quote => &["blockquote"],
            SlashItem::Code => &["pre", "snippet"],
            SlashItem::Divider => &["hr", "line", "rule", "separator"],
        }
    }

    pub fn command(self) -> BlockCommand {
        match self {
            SlashItem::Text => BlockCommand::Text,
            SlashItem::Heading1 => BlockCommand::Heading(1),
            SlashItem::Heading2 => BlockCommand::Heading(2),
            SlashItem::Heading3 => BlockCommand::Heading(3),
            SlashItem::Bullet => BlockCommand::Bullet,
            SlashItem::Numbered => BlockCommand::Numbered,
            SlashItem::Todo => BlockCommand::Todo,
            SlashItem::Quote => BlockCommand::Quote,
            SlashItem::Code => BlockCommand::Code,
            SlashItem::Divider => BlockCommand::Divider,
        }
    }
}

/// The `/command` being typed before the caret: its byte range (from the slash to the caret)
/// and the query after the slash. A slash counts at the start of a line's content or after a
/// space; the query has no spaces and is at most 24 bytes.
pub fn slash_query(text: &str, cursor: usize) -> Option<(Range<usize>, &str)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let lines = blocks(text);
    let line = &lines[line_at(&lines, cursor)];
    let content = line.content.max(line.range.start + line.indent);
    if matches!(line.block, Block::Code | Block::Fence) || cursor < content {
        return None;
    }
    let head = &text[content..cursor];
    let slash = head.rfind('/')?;
    let query = &head[slash + 1..];
    if query.len() > 24 || query.chars().any(char::is_whitespace) {
        return None;
    }
    if head[..slash].chars().next_back().is_some_and(|ch| !ch.is_whitespace()) {
        return None;
    }
    Some((content + slash..cursor, query))
}

/// The menu entries matching a query, in menu order: the label, a word of it, or a keyword
/// starts with the query (ignoring case).
pub fn slash_matches(query: &str) -> Vec<SlashItem> {
    let query = query.to_lowercase();
    SlashItem::ALL
        .into_iter()
        .filter(|item| {
            let label = item.label().to_lowercase();
            query.is_empty()
                || label.starts_with(&query)
                || label.split(' ').any(|word| word.starts_with(&query))
                || item.keywords().iter().any(|word| word.starts_with(&query))
        })
        .collect()
}

/// Picking a slash menu entry: the `/query` text goes and the line becomes that block (the menu
/// applies what it says; it does not toggle back to text).
pub fn apply_slash(text: &str, slash: Range<usize>, item: SlashItem) -> Change {
    let removed = replace(text, slash.clone(), "");
    let caret = slash.start;
    let lines = blocks(&removed);
    let line = &lines[line_at(&lines, caret)];
    let command = item.command();
    let after = if command.is(line.block) {
        removed.clone()
    } else {
        set_block(&removed, caret..caret, command).apply(&removed)
    };
    let selection = if command.is(line.block) {
        caret..caret
    } else {
        set_block(&removed, caret..caret, command).selection
    };
    change_between(text, &after, selection)
}

/// Pasting `pasted` over `sel`: line breaks become `\n`; an address pasted over selected words
/// on one line links them (`[words](address)`). The caret ends after the pasted text.
pub fn paste(text: &str, sel: Range<usize>, pasted: &str) -> Change {
    let pasted = pasted.replace("\r\n", "\n").replace('\r', "\n");
    let address = pasted.trim();
    let is_url = (address.starts_with("https://") || address.starts_with("http://"))
        && address.len() > 8
        && !address.chars().any(char::is_whitespace);
    let selected = &text[sel.clone()];
    if is_url && !sel.is_empty() && !selected.contains('\n') && !selected.contains("](") {
        let wrapped = format!("[{selected}]({address})");
        let new = replace(text, sel.clone(), &wrapped);
        let caret = sel.start + wrapped.len();
        return change_between(text, &new, caret..caret);
    }
    let new = replace(text, sel.clone(), &pasted);
    let caret = sel.start + pasted.len();
    change_between(text, &new, caret..caret)
}
