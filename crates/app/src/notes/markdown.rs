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
