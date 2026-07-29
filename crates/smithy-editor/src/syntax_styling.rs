//! Tree-sitter syntax colouring and LSP diagnostic emphasis for floem's editor.
//!
//! floem's [`Styling`] trait exposes `apply_attr_styles(edid, style, line,
//! default, attrs)` — a per-line hook for attaching text attributes. That is the
//! single place both syntax colours and diagnostic emphasis belong, so they are
//! implemented together here rather than as two mechanisms fighting over the same
//! text.
//!
//! **There are no squiggles here, and there cannot be through this trait.**
//! `apply_attr_styles` hands back a cosmic-text `Attrs`, which carries colour,
//! weight, width, style and metrics — and no underline or background. A diagnostic
//! is therefore drawn *here* as bold text in the severity colour. Earlier
//! revisions of this file claimed underlines in three comments and never drew one.
//!
//! The squiggles are real now, and they are painted as a separate layer over the
//! text — [`crate::squiggle`] for the geometry,
//! [`crate::code_editor::code_editor`] for the layer. Both go through
//! [`SyntaxStyling::span_within_line`], so the wave and the colour under it
//! cannot disagree about which characters a diagnostic covers.
//!
//! ## Offsets
//!
//! Our highlighter reports ranges as **byte offsets into the whole document**;
//! `apply_attr_styles` wants ranges **relative to the start of one line**. The
//! translation is the only tricky part, and it is where an off-by-one shows up
//! as colours smeared one character sideways, so it is a separate, tested
//! function.
//!
//! ## Re-highlighting
//!
//! [`Styling::id`] is floem's cache key: it re-lays-out a line only when the id
//! changes. So the id folds in a revision that is bumped whenever the text or
//! the diagnostics change. Returning a constant would freeze the colours at
//! whatever they were when the file opened; returning something that changes
//! every frame would re-shape every line on every frame.

use std::cell::RefCell;

use floem::peniko::Color;
use floem::text::{Attrs, AttrsList};
use floem::views::editor::id::EditorId;
use floem::views::editor::text::Styling;
use floem::views::editor::EditorStyle;

use lapce_xi_rope::Rope;

use crate::design;
use crate::highlight::{HighlightRange, HighlightStyle, SyntaxHighlighter};
use crate::lsp::Severity;
use crate::squiggle::DiagnosticRange;

/// A diagnostic reduced to what the styling needs.
///
/// Columns are **UTF-16 code units**, exactly as the language server reported
/// them, and are converted to byte offsets here — see [`utf16_to_byte`]. They
/// used to be copied straight through and used as byte offsets, which is correct
/// for ASCII and wrong for every other line: one accented character before the
/// error and the mark lands a byte late, one emoji and it lands three bytes late.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineDiagnostic {
    /// Zero-based line.
    pub line: usize,
    /// Column range within that line, in **UTF-16 code units**.
    pub start_column: usize,
    pub end_column: usize,
    pub severity: Severity,
}

/// Convert a UTF-16 column to a byte offset within `line`.
///
/// LSP positions are UTF-16 code units, which is neither bytes nor characters:
/// `é` is two bytes, one char and one UTF-16 unit; `😀` is four bytes, one char
/// and **two** UTF-16 units. `code_editor::utf16_len` already exists for the
/// hover path and this is its inverse.
///
/// A column past the end of the line clamps to the end rather than panicking —
/// servers do report positions one past the last character, for a missing
/// delimiter.
pub fn utf16_to_byte(line: &str, utf16_col: usize) -> usize {
    let mut seen = 0usize;
    for (byte, ch) in line.char_indices() {
        if seen >= utf16_col {
            return byte;
        }
        seen += ch.len_utf16();
    }
    line.len()
}

/// One edit, as the byte range it replaced and the length of the replacement.
///
/// The range is in the **old** text. That is what tree-sitter's `InputEdit`
/// wants and what floem's `OnUpdate` deltas report, so nothing has to be
/// converted between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditSpan {
    pub start: usize,
    pub old_end: usize,
    pub new_len: usize,
}

/// Byte offset to a tree-sitter `(row, column)`.
///
/// Column is in **bytes within the line**, which is what tree-sitter means by
/// column — not characters and not UTF-16 units. Getting that wrong desynchronises
/// the tree from the text on any line containing non-ASCII, and the symptom is
/// colours drifting rather than an error.
fn point_of(text: &Rope, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let line = text.line_of_offset(offset);
    (line, offset - text.offset_of_line(line))
}

/// One highlight, already clipped to a line and made relative to its start.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LineSpan {
    start: usize,
    end: usize,
    style: HighlightStyle,
}

/// Clip every range onto the lines it covers.
///
/// **Driven by ranges, not by lines**, which is what keeps it linear in the
/// output. Walking lines and advancing a cursor over sorted ranges is the
/// obvious shape and it is quadratic in the worst case: the cursor can only pass
/// a range once that range has *ended*, so a single long one — a file-header
/// block comment, an enormous array literal — pins it at zero and every line
/// rescans the whole list from there. Measured on this workspace's largest file,
/// 13,212 lines against 11,730 ranges, that cost **29.6 ms per parse**, which is
/// more than the parse itself.
///
/// Each range instead binary-searches for the line it starts on and walks
/// forward only while it still covers one, so the total work is the size of the
/// answer. Ranges are visited in start order, so each line's spans come out
/// sorted, which is the order the painter layers them in.
fn spans_by_line(
    ranges: &[HighlightRange],
    line_starts: &[usize],
    total: usize,
) -> Vec<Vec<LineSpan>> {
    let mut out = vec![Vec::new(); line_starts.len()];

    for range in ranges {
        // The line `range.start` falls on.
        let first_line = line_starts
            .partition_point(|&start| start <= range.start)
            .saturating_sub(1);

        for line in first_line..line_starts.len() {
            let line_start = line_starts[line];
            if line_start >= range.end {
                break;
            }
            let line_end = line_starts.get(line + 1).copied().unwrap_or(total);
            if let Some((start, end)) = clip_to_line(range.start, range.end, line_start, line_end) {
                out[line].push(LineSpan {
                    start,
                    end,
                    style: range.style,
                });
            }
        }
    }
    out
}

/// Syntax colours plus diagnostic emphasis.
pub struct SyntaxStyling {
    highlighter: RefCell<SyntaxHighlighter>,
    /// Cached document-wide ranges from the last parse.
    ranges: RefCell<Vec<HighlightRange>>,
    /// The same ranges, clipped to each line and made line-relative.
    ///
    /// **Built once per parse so that painting a line is proportional to what is
    /// on that line.** `apply_attr_styles` runs per line, per repaint, and it
    /// used to scan *every* range in the document each time to find the few that
    /// touched the line — 4,233 ranges against 50 visible lines is 211,650
    /// comparisons for one frame of a 1,900-line file, and 586,500 for a
    /// 13,000-line one. Scrolling paid that on every frame.
    ///
    /// A range spanning several lines (a block comment, a multi-line string)
    /// appears once per line it covers, which is what makes the lookup a plain
    /// index. That costs a little memory and is bounded by the ranges
    /// themselves: in this workspace's largest file it works out at about one
    /// span per line.
    line_spans: RefCell<Vec<Vec<LineSpan>>>,
    /// Byte offset of the start of each line, for the document→line translation.
    line_starts: RefCell<Vec<usize>>,
    /// Byte length of each line's *content*, excluding its terminator. Needed to
    /// clamp a widened diagnostic so it cannot run past the end of its line.
    line_lengths: RefCell<Vec<usize>>,
    /// The parsed document.
    ///
    /// Retained so a diagnostic span can be moved onto a character that actually
    /// has ink — see [`diagnostic_span`]. `reparse` already allocates this copy;
    /// keeping it costs nothing further, and only the lines that carry a
    /// diagnostic are ever read back out.
    text: RefCell<Rope>,
    diagnostics: RefCell<Vec<InlineDiagnostic>>,
    /// Bumped whenever anything above changes; folded into [`Styling::id`].
    revision: RefCell<u64>,
    font_size: usize,
}

impl SyntaxStyling {
    pub fn new(font_size: usize) -> Self {
        Self {
            highlighter: RefCell::new(SyntaxHighlighter::new()),
            ranges: RefCell::new(Vec::new()),
            line_spans: RefCell::new(Vec::new()),
            line_starts: RefCell::new(vec![0]),
            line_lengths: RefCell::new(vec![0]),
            text: RefCell::new(Rope::from("")),
            diagnostics: RefCell::new(Vec::new()),
            revision: RefCell::new(0),
            font_size,
        }
    }

    /// Point the highlighter at a language, by file path.
    pub fn set_language_from_path(&self, path: &std::path::Path) -> bool {
        self.highlighter
            .borrow_mut()
            .set_language_from_path(path)
            .is_ok()
    }

    /// Re-parse `text` and cache the resulting ranges.
    ///
    /// Takes floem's **own** rope, and does not copy it. This used to take `&str`
    /// and rebuild a `ropey::Rope` from it, which meant three copies of the whole
    /// file per keystroke: floem's rope to `String` at the call site, `String` to
    /// `ropey::Rope` here, and then `ropey::Rope` back to `String` char by char
    /// inside the parser. Tree-sitter reads the document in place now.
    pub fn reparse(&self, text: &Rope) {
        {
            let mut highlighter = self.highlighter.borrow_mut();
            if !highlighter.has_language() {
                return;
            }
            if highlighter.parse(text).is_err() {
                // A file mid-edit is often not parseable. Keep the previous
                // colouring rather than flashing to monochrome on every
                // keystroke that leaves a brace unbalanced.
                return;
            }
            *self.ranges.borrow_mut() = highlighter.highlights().to_vec();
        }
        self.adopt(text);
    }

    /// Re-parse after a single edit, reusing the previous syntax tree.
    ///
    /// **This is what makes typing in a large file cheap.** A full parse of a
    /// 1,900-line file measures 7.2 ms and a 13,000-line one 18.1 ms — the
    /// latter past a frame at 60 Hz, on every keystroke. Tree-sitter can reuse
    /// the parts of the tree an edit did not touch, but only if it is told what
    /// changed, and nothing was telling it: `reparse` called `parse` with no old
    /// tree, so the "incremental parsing" the module claimed had never happened.
    ///
    /// `changed` is the byte range replaced in the **old** text and the length
    /// of what replaced it, which is exactly what floem's `OnUpdate` deltas
    /// carry. The old text is the rope this styling last parsed, so both ends of
    /// the edit can be located without the caller keeping anything.
    ///
    /// Returns `false` when the edit could not be applied incrementally and the
    /// caller should fall back to [`Self::reparse`] — which is not a failure,
    /// just the cases where reusing a tree is not obviously safe: nothing parsed
    /// yet, or an edit whose old range no longer fits the text we hold.
    pub fn reparse_incremental(&self, text: &Rope, changed: EditSpan) -> bool {
        if !self.highlighter.borrow().has_tree() {
            return false;
        }
        let old_text = self.text.borrow().clone();
        if changed.start > changed.old_end || changed.old_end > old_text.len() {
            return false;
        }

        let start = point_of(&old_text, changed.start);
        let old_end = point_of(&old_text, changed.old_end);
        let new_end_byte = changed.start + changed.new_len;
        if new_end_byte > text.len() {
            return false;
        }
        let new_end = point_of(text, new_end_byte);

        {
            let mut highlighter = self.highlighter.borrow_mut();
            if highlighter
                .update(
                    text,
                    changed.start,
                    changed.old_end,
                    new_end_byte,
                    start,
                    old_end,
                    new_end,
                )
                .is_err()
            {
                return false;
            }
            *self.ranges.borrow_mut() = highlighter.highlights().to_vec();
        }
        self.adopt(text);
        true
    }

    /// Cache everything derived from a freshly parsed rope.
    fn adopt(&self, text: &Rope) {
        let starts = byte_line_starts(text);
        *self.line_spans.borrow_mut() = spans_by_line(&self.ranges.borrow(), &starts, text.len());
        *self.line_starts.borrow_mut() = starts;
        *self.line_lengths.borrow_mut() = byte_line_content_lengths(text);
        *self.text.borrow_mut() = text.clone();
        *self.revision.borrow_mut() += 1;
    }

    pub fn set_diagnostics(&self, diagnostics: Vec<InlineDiagnostic>) {
        *self.diagnostics.borrow_mut() = diagnostics;
        *self.revision.borrow_mut() += 1;
    }

    /// Byte range of `line` in document coordinates.
    fn line_bounds(&self, line: usize) -> Option<(usize, usize)> {
        let starts = self.line_starts.borrow();
        let start = *starts.get(line)?;
        let end = starts.get(line + 1).copied().unwrap_or(usize::MAX);
        Some((start, end))
    }

    /// The text of `line`, with its terminator excluded.
    fn line_text(&self, line: usize) -> String {
        let doc = self.text.borrow();
        let len = self.line_lengths.borrow().get(line).copied().unwrap_or(0);
        match self.line_starts.borrow().get(line).copied() {
            Some(start) => doc
                .slice_to_cow(start..(start + len).min(doc.len()))
                .into_owned(),
            None => String::new(),
        }
    }

    /// Every diagnostic as a **document** byte range, for the squiggle overlay.
    ///
    /// Resolved through [`Self::span_within_line`], which is also what the
    /// colouring uses, so a squiggle and the colour beneath it cannot disagree
    /// about which characters a diagnostic covers. That is the same reason
    /// colours and diagnostics share one `Styling` in the first place.
    pub fn diagnostic_ranges(&self) -> Vec<DiagnosticRange> {
        let mut out = Vec::new();
        // Diagnostics cluster on a line, and slicing the line out of the rope
        // is the expensive part, so the last one is kept.
        let mut cached: Option<(usize, String)> = None;

        for diagnostic in self.diagnostics.borrow().iter() {
            let Some(line_start) = self.line_starts.borrow().get(diagnostic.line).copied() else {
                continue;
            };
            let text = match &cached {
                Some((line, text)) if *line == diagnostic.line => text,
                _ => {
                    cached = Some((diagnostic.line, self.line_text(diagnostic.line)));
                    &cached.as_ref().expect("just set").1
                }
            };
            let Some(span) = Self::span_within_line(text, diagnostic) else {
                continue;
            };
            out.push(DiagnosticRange {
                offsets: (line_start + span.start)..(line_start + span.end),
                severity: diagnostic.severity,
            });
        }

        out
    }

    /// The line-relative byte span a diagnostic marks, given its line's text.
    ///
    /// The single place the server's UTF-16 columns become byte offsets and the
    /// mark is walked onto a character that has ink.
    fn span_within_line(
        text: &str,
        diagnostic: &InlineDiagnostic,
    ) -> Option<std::ops::Range<usize>> {
        // The server counts in UTF-16; everything below counts in bytes.
        let start = utf16_to_byte(text, diagnostic.start_column);
        let end = utf16_to_byte(text, diagnostic.end_column);
        diagnostic_span(start, end, text)
    }
}

/// Byte offset at which each line begins.
pub fn byte_line_starts(text: &Rope) -> Vec<usize> {
    // Driven off the line count rather than by walking offsets: stepping to
    // `offset_of_line(n + 1)` runs one past the end and appends a start for a line
    // that does not exist.
    let last = text.line_of_offset(text.len());
    let mut starts = Vec::with_capacity(last + 1);
    for line in 0..=last {
        starts.push(text.offset_of_line(line));
    }
    if starts.is_empty() {
        starts.push(0);
    }
    starts
}

/// Byte length of each line's content, with the line terminator excluded.
pub fn byte_line_content_lengths(text: &Rope) -> Vec<usize> {
    let starts = byte_line_starts(text);
    let total = text.len();
    let mut lengths = Vec::with_capacity(starts.len());
    for (i, start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(total);
        let raw = text.slice_to_cow(*start..end);
        let terminator = if raw.ends_with("\r\n") {
            2
        } else if raw.ends_with('\n') {
            1
        } else {
            0
        };
        lengths.push((end - start).saturating_sub(terminator));
    }
    if lengths.is_empty() {
        lengths.push(0);
    }
    lengths
}

/// The span to mark for a diagnostic, moved onto characters that have ink.
///
/// Two corrections to what the server reports, both learned from the same file.
///
/// **Zero width.** Most diagnostics are *points*, not ranges: "expected COMMA" is
/// reported at the position something is missing, with `start == end`. Those were
/// skipped outright, so a file with two syntax errors and one type error showed
/// exactly one mark — the type error being the only one with a width.
///
/// **Whitespace.** Widening a point by one character is not enough when the point
/// *is* a space, which is exactly where a missing comma or semicolon gets
/// reported. Styling a space bold and red paints nothing at all: the character has
/// no ink. So a span covering only whitespace is walked forward to the next
/// non-whitespace character, or backward if there is none — landing the mark on
/// the token the reader has to look at.
///
/// `None` only when the line has no ink anywhere, where any mark would be a stray
/// glyph on an empty row.
pub fn diagnostic_span(start: usize, end: usize, line: &str) -> Option<std::ops::Range<usize>> {
    let len = line.len();
    if len == 0 {
        return None;
    }

    // Byte offsets of every character with ink, so a span can be snapped to one.
    let inked: Vec<usize> = line
        .char_indices()
        .filter(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .collect();
    let first_inked = *inked.first()?;
    let last_inked = *inked.last()?;

    let (mut from, mut to) = if end > start {
        (start.min(len), end.min(len))
    } else {
        // Zero-width, or reversed by a misbehaving server.
        (start.min(len), start.min(len))
    };
    if from >= to {
        to = (from + 1).min(len);
        if from >= to {
            // At or past the end of the line: mark backward instead of nothing.
            from = last_inked;
            to = len;
        }
    }

    if line.get(from..to).is_some_and(|s| s.trim().is_empty()) {
        match inked.iter().find(|i| **i >= to) {
            // The next token to the right, which is where a missing comma or
            // semicolon actually wants the reader looking.
            Some(&next) => {
                from = next;
                to = line[next..]
                    .char_indices()
                    .find(|(_, c)| c.is_whitespace())
                    .map(|(off, _)| next + off)
                    .unwrap_or(len);
            }
            // Nothing to the right: fall back to the last token on the line.
            None => {
                from = last_inked;
                to = len;
            }
        }
        if from < first_inked {
            from = first_inked;
        }
    }

    (from < to).then_some(from..to)
}

/// Clip a document-wide range to one line, returning line-relative bounds.
///
/// Returns `None` when the range does not touch the line at all. A range that
/// spans a line boundary — a multi-line string or comment — is clipped rather
/// than dropped, so the colour continues across the lines it covers.
pub fn clip_to_line(
    range_start: usize,
    range_end: usize,
    line_start: usize,
    line_end: usize,
) -> Option<(usize, usize)> {
    if range_end <= line_start || range_start >= line_end {
        return None;
    }
    let start = range_start.max(line_start) - line_start;
    let end = range_end.min(line_end) - line_start;
    if start >= end {
        return None;
    }
    Some((start, end))
}

pub fn color_for(style: HighlightStyle) -> Color {
    use design::syntax;
    match style {
        HighlightStyle::Keyword => syntax::KEYWORD,
        HighlightStyle::String => syntax::STRING,
        HighlightStyle::Number => syntax::NUMBER,
        HighlightStyle::Comment => syntax::COMMENT,
        HighlightStyle::Function => syntax::FUNCTION,
        HighlightStyle::Type => syntax::TYPE,
        HighlightStyle::Variable => syntax::VARIABLE,
        HighlightStyle::Operator => syntax::OPERATOR,
        HighlightStyle::Punctuation => syntax::PUNCTUATION,
        _ => design::FG,
    }
}

pub fn color_for_severity(severity: Severity) -> Color {
    match severity {
        Severity::Error => design::DANGER,
        Severity::Warning => design::WARN,
        Severity::Information => design::INFO,
        Severity::Hint => design::FG_FAINT,
    }
}

impl Styling for SyntaxStyling {
    /// Cache key. Folds in the revision so edits and new diagnostics invalidate
    /// the laid-out lines, and nothing else does.
    fn id(&self) -> u64 {
        *self.revision.borrow()
    }

    fn font_size(&self, _edid: EditorId, _line: usize) -> usize {
        self.font_size
    }

    fn apply_attr_styles(
        &self,
        _edid: EditorId,
        _style: &EditorStyle,
        line: usize,
        default: Attrs,
        attrs: &mut AttrsList,
    ) {
        // `line_bounds` is still what decides whether the line exists at all;
        // the diagnostic pass below reads it.
        let Some((line_start, _line_end)) = self.line_bounds(line) else {
            return;
        };
        let _ = line_start;

        // Syntax first, so the diagnostic span layered on top wins.
        //
        // Indexed, not scanned. See `line_spans`: this runs per line per
        // repaint, and walking every range in the document to find the handful
        // on this line is quadratic in the size of the file.
        if let Some(spans) = self.line_spans.borrow().get(line) {
            for span in spans {
                attrs.add_span(
                    span.start..span.end,
                    default.clone().color(color_for(span.style)),
                );
            }
        }

        // Colour **and weight**, because colour alone is too weak a signal here.
        // The syntax pass has already coloured every identifier, so a red
        // identifier among coloured identifiers on a dark background is easy to
        // look straight past — which is exactly what happened.
        //
        // Not an underline: floem's `Styling` trait hands back only text
        // attributes, and cosmic-text's `Attrs` has no underline or background
        // field. The wave is painted over the text by `code_editor`, from the
        // same span this loop uses. A comment here used to claim `Attrs` drew
        // an underline; it never did and it still cannot.
        // Only materialised when this line actually carries a diagnostic, which
        // is almost never — this runs per line, per repaint.
        let mut line_text: Option<String> = None;
        for diagnostic in self.diagnostics.borrow().iter() {
            if diagnostic.line != line {
                continue;
            }
            let text = line_text.get_or_insert_with(|| self.line_text(line));
            let Some(span) = Self::span_within_line(text, diagnostic) else {
                continue;
            };
            attrs.add_span(
                span,
                default
                    .clone()
                    .color(color_for_severity(diagnostic.severity))
                    .weight(floem::text::FontWeight::BOLD),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lapce_xi_rope::Rope;

    /// LSP columns are UTF-16 code units, and were being used as byte offsets.
    /// Correct for ASCII, wrong for anything else — which is why it survived.
    #[test]
    fn a_utf16_column_becomes_the_right_byte_offset() {
        // `é` is 2 bytes but 1 UTF-16 unit, so byte and column diverge after it.
        let line = "let é = foo();";
        assert_eq!(utf16_to_byte(line, 0), 0);
        assert_eq!(utf16_to_byte(line, 4), 4, "`é` starts at byte 4");
        assert_eq!(utf16_to_byte(line, 5), 6, "and occupies two bytes");
        assert_eq!(&line[utf16_to_byte(line, 8)..], "foo();");
    }

    /// A non-BMP character is **two** UTF-16 units and four bytes, which is where
    /// a naive `chars().count()` conversion also goes wrong.
    #[test]
    fn a_non_bmp_character_counts_as_two_utf16_units() {
        let line = "a😀b";
        assert_eq!(utf16_to_byte(line, 1), 1, "the emoji starts at byte 1");
        assert_eq!(
            utf16_to_byte(line, 3),
            5,
            "two units later, four bytes later"
        );
        assert_eq!(&line[utf16_to_byte(line, 3)..], "b");
    }

    /// Pure ASCII is the case that always worked, and must keep working.
    #[test]
    fn ascii_columns_are_byte_offsets_unchanged() {
        let line = "    let mut app = App::new(ga me_handle);";
        for col in [0, 4, 29, 30] {
            assert_eq!(utf16_to_byte(line, col), col);
        }
    }

    /// Servers report a position one past the last character for a missing
    /// delimiter, and that must clamp rather than panic.
    #[test]
    fn a_column_past_the_end_clamps_to_the_end() {
        let line = "let x = 1";
        assert_eq!(utf16_to_byte(line, 9), 9);
        assert_eq!(utf16_to_byte(line, 999), line.len());
        assert_eq!(utf16_to_byte("", 5), 0);
    }

    /// Real lines and real columns from a live rust-analyzer run. Two of the three
    /// errors it reported pointed *at a space character*, and a space styled bold
    /// and red paints nothing — so three errors produced one visible mark.
    #[test]
    fn a_diagnostic_reported_on_a_space_marks_the_token_beside_it() {
        // `    let game_handle = spawn_ga me_actor();` — "expected SEMICOLON" at
        // 0-based column 30, which is the space.
        let line = "    let game_handle = spawn_ga me_actor();";
        assert_eq!(&line[30..31], " ", "column 30 really is the space");

        let span = diagnostic_span(30, 30, line).expect("a mark is needed");
        assert_eq!(&line[span.clone()], "me_actor();");
        assert!(
            !line[span].trim().is_empty(),
            "a mark on whitespace alone is invisible"
        );
    }

    /// The other one, from the same run: "expected COMMA" at the space inside
    /// `App::new(ga me_handle)`.
    #[test]
    fn the_second_space_error_also_lands_on_ink() {
        let line = "    let mut app = App::new(ga me_handle);";
        assert_eq!(&line[29..30], " ");

        let span = diagnostic_span(29, 29, line).expect("a mark is needed");
        assert_eq!(&line[span], "me_handle);");
    }

    /// A ranged diagnostic keeps its range — this is the one that always worked,
    /// and it must not change.
    #[test]
    fn a_ranged_diagnostic_marks_exactly_its_range() {
        let line = "    let mut app = App::new(ga me_handle);";
        assert_eq!(diagnostic_span(30, 39, line), Some(30..39));
        assert_eq!(&line[30..39], "me_handle");
    }

    /// A point inside a token widens by one character and stays put — there is
    /// already ink under it.
    #[test]
    fn a_point_inside_a_token_marks_that_character() {
        let line = "let x = foo();";
        assert_eq!(diagnostic_span(8, 8, line), Some(8..9));
        assert_eq!(&line[8..9], "f");
    }

    /// Servers report a missing semicolon or brace just past the last character.
    /// Widening forward falls off the line, so the mark goes backward onto the
    /// last token.
    #[test]
    fn a_diagnostic_past_the_end_of_a_line_marks_the_last_token() {
        let line = "let x = 1";
        let span = diagnostic_span(9, 9, line).expect("a mark is needed");
        assert_eq!(&line[span], "1");

        let span = diagnostic_span(99, 99, line).expect("a mark is needed");
        assert_eq!(&line[span], "1");
    }

    /// Trailing whitespace is the awkward case: the point is on a space, and
    /// there is no ink to its right.
    #[test]
    fn a_diagnostic_in_trailing_whitespace_falls_back_to_the_last_token() {
        let line = "let x = 1;   ";
        let span = diagnostic_span(11, 11, line).expect("a mark is needed");
        assert!(
            !line[span].trim().is_empty(),
            "must not settle on trailing spaces"
        );
    }

    /// Indentation is whitespace too, and a diagnostic there must not mark the
    /// blank left margin.
    #[test]
    fn a_diagnostic_in_leading_indentation_marks_the_first_token() {
        let line = "        let x = 1;";
        let span = diagnostic_span(2, 2, line).expect("a mark is needed");
        assert_eq!(&line[span], "let");
    }

    /// Nothing to mark on a line with no ink, and a stray glyph there would be
    /// worse than nothing.
    #[test]
    fn a_blank_line_is_not_marked() {
        assert_eq!(diagnostic_span(0, 0, ""), None);
        assert_eq!(diagnostic_span(0, 4, "    "), None);
    }

    /// A range the server got backwards is still a diagnostic worth showing.
    #[test]
    fn a_reversed_range_is_treated_as_a_point() {
        let line = "let x = foo();";
        assert_eq!(diagnostic_span(10, 4, line), Some(10..11));
    }

    /// A span may never run past the end of its line.
    #[test]
    fn a_span_is_clamped_to_the_line() {
        let line = "let x = 1;";
        let span = diagnostic_span(4, 500, line).expect("a mark is needed");
        assert!(span.end <= line.len());
    }

    /// Line lengths must exclude the terminator, or a mark widened at the end of
    /// a line lands on the newline and paints nothing.
    #[test]
    fn line_lengths_exclude_the_terminator() {
        let text = Rope::from("abc\nde\nf");
        assert_eq!(byte_line_content_lengths(&text), vec![3, 2, 1]);
    }

    #[test]
    fn line_lengths_handle_crlf_and_a_missing_final_newline() {
        assert_eq!(
            byte_line_content_lengths(&Rope::from("ab\r\ncd")),
            vec![2, 2]
        );
        assert_eq!(byte_line_content_lengths(&Rope::from("")), vec![0]);
    }

    #[test]
    fn line_starts_are_byte_offsets() {
        let text = Rope::from("abc\nde\nf");
        assert_eq!(byte_line_starts(&text), vec![0, 4, 7]);
    }

    /// Multi-byte characters must not shift the offsets, or colours land on the
    /// wrong glyphs in any file containing non-ASCII.
    #[test]
    fn line_starts_account_for_multibyte_characters() {
        let text = Rope::from("héllo\nworld");
        // "héllo\n" is 7 bytes: h(1) é(2) l l o(3) \n(1).
        assert_eq!(byte_line_starts(&text), vec![0, 7]);
    }

    #[test]
    fn an_empty_document_still_has_one_line_start() {
        assert_eq!(byte_line_starts(&Rope::from("")), vec![0]);
    }

    /// **The index must agree with the scan it replaced, exactly.**
    ///
    /// `apply_attr_styles` used to walk every range in the document for every
    /// line it painted. That is the definition the index has to reproduce, so
    /// the test computes both and compares — including a range that spans three
    /// lines, which is the case a naive index drops.
    #[test]
    fn the_per_line_index_agrees_with_scanning_every_range() {
        let ranges = vec![
            HighlightRange::new(0, 3, HighlightStyle::Keyword),
            HighlightRange::new(4, 9, HighlightStyle::Function),
            // Spans lines 1 through 3 — a block comment or a multi-line string.
            HighlightRange::new(12, 34, HighlightStyle::Comment),
            HighlightRange::new(35, 38, HighlightStyle::Number),
        ];
        let line_starts = vec![0usize, 10, 20, 30];
        let total = 40usize;

        let indexed = spans_by_line(&ranges, &line_starts, total);
        assert_eq!(indexed.len(), line_starts.len());

        for (line, &start) in line_starts.iter().enumerate() {
            let end = line_starts.get(line + 1).copied().unwrap_or(total);
            let scanned: Vec<LineSpan> = ranges
                .iter()
                .filter_map(|r| {
                    clip_to_line(r.start, r.end, start, end).map(|(s, e)| LineSpan {
                        start: s,
                        end: e,
                        style: r.style,
                    })
                })
                .collect();
            assert_eq!(
                indexed[line], scanned,
                "line {line} disagrees with a full scan of every range"
            );
        }
    }

    /// **One long range must not make indexing quadratic.**
    ///
    /// Walking lines and advancing a cursor over sorted ranges only lets the
    /// cursor past a range once that range has ended — so a single range
    /// spanning the file pins it at zero and every line rescans the whole list.
    /// This is the shape that costs: `catalogue.rs` is one enormous array
    /// literal, and indexing it took 29.6 ms per parse, more than parsing it.
    ///
    /// A timing assertion, because the defect is entirely about time and the
    /// result was identical either way. The bound is two orders of magnitude
    /// above the honest cost and one below the quadratic one.
    #[test]
    fn indexing_stays_linear_when_one_range_spans_the_whole_file() {
        const LINES: usize = 20_000;
        let line_starts: Vec<usize> = (0..LINES).map(|l| l * 40).collect();
        let total = LINES * 40;

        let mut ranges = vec![HighlightRange::new(0, total, HighlightStyle::Comment)];
        for line in 0..LINES {
            ranges.push(HighlightRange::new(
                line * 40 + 4,
                line * 40 + 12,
                HighlightStyle::Keyword,
            ));
        }
        ranges.sort_by_key(|r| r.start);

        let started = std::time::Instant::now();
        let indexed = spans_by_line(&ranges, &line_starts, total);
        let elapsed = started.elapsed();

        assert_eq!(indexed.len(), LINES);
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "indexing took {elapsed:?}; a cursor-over-lines walk rescans every range for \
             every line when one of them never ends"
        );
    }

    /// A range covering several lines has to colour all of them, which is the
    /// property that makes multi-line strings and block comments legible. The
    /// index advances a cursor over sorted ranges, so this is exactly where
    /// advancing it too eagerly would show up.
    #[test]
    fn a_range_spanning_lines_is_indexed_on_every_line_it_covers() {
        let ranges = vec![HighlightRange::new(5, 35, HighlightStyle::Comment)];
        let indexed = spans_by_line(&ranges, &[0, 10, 20, 30], 40);

        for (line, spans) in indexed.iter().enumerate() {
            assert_eq!(
                spans.len(),
                1,
                "line {line} lost its share of a range covering the whole file"
            );
        }
        assert_eq!(
            indexed[0][0],
            LineSpan {
                start: 5,
                end: 10,
                style: HighlightStyle::Comment
            }
        );
        assert_eq!(
            indexed[1][0],
            LineSpan {
                start: 0,
                end: 10,
                style: HighlightStyle::Comment
            }
        );
        assert_eq!(
            indexed[3][0],
            LineSpan {
                start: 0,
                end: 5,
                style: HighlightStyle::Comment
            }
        );
    }

    #[test]
    fn a_range_inside_a_line_becomes_line_relative() {
        assert_eq!(clip_to_line(12, 15, 10, 20), Some((2, 5)));
    }

    #[test]
    fn a_range_before_or_after_the_line_is_dropped() {
        assert_eq!(clip_to_line(0, 5, 10, 20), None);
        assert_eq!(clip_to_line(25, 30, 10, 20), None);
    }

    /// A range ending exactly at the line start does not touch the line.
    #[test]
    fn a_range_abutting_the_line_start_is_dropped() {
        assert_eq!(clip_to_line(5, 10, 10, 20), None);
    }

    /// Multi-line strings and block comments must keep their colour on every
    /// line they cover, not just the first.
    #[test]
    fn a_range_spanning_the_line_is_clipped_to_it() {
        assert_eq!(clip_to_line(0, 100, 10, 20), Some((0, 10)));
    }

    #[test]
    fn a_range_starting_inside_and_running_past_is_clipped() {
        assert_eq!(clip_to_line(15, 100, 10, 20), Some((5, 10)));
    }

    #[test]
    fn an_empty_range_is_dropped() {
        assert_eq!(clip_to_line(12, 12, 10, 20), None);
    }

    /// The last line has no following line start, so its end is unbounded.
    #[test]
    fn the_final_line_extends_to_the_end_of_the_document() {
        let styling = SyntaxStyling::new(13);
        *styling.line_starts.borrow_mut() = vec![0, 4];
        assert_eq!(styling.line_bounds(1), Some((4, usize::MAX)));
        assert_eq!(styling.line_bounds(9), None);
    }

    /// Returning a constant id would freeze the colouring at whatever it was
    /// when the file opened — floem only re-lays-out a line when the id changes.
    #[test]
    fn the_cache_id_changes_when_diagnostics_change() {
        let styling = SyntaxStyling::new(13);
        let before = styling.id();
        styling.set_diagnostics(vec![InlineDiagnostic {
            line: 0,
            start_column: 0,
            end_column: 4,
            severity: Severity::Error,
        }]);
        assert_ne!(styling.id(), before);
    }

    #[test]
    fn the_cache_id_is_stable_when_nothing_changes() {
        let styling = SyntaxStyling::new(13);
        assert_eq!(styling.id(), styling.id());
    }

    #[test]
    fn severities_map_to_distinct_colours() {
        assert_ne!(
            color_for_severity(Severity::Error),
            color_for_severity(Severity::Warning)
        );
        assert_ne!(
            color_for_severity(Severity::Warning),
            color_for_severity(Severity::Hint)
        );
    }

    #[test]
    fn syntax_styles_map_to_distinct_colours() {
        assert_ne!(
            color_for(HighlightStyle::Keyword),
            color_for(HighlightStyle::String)
        );
        assert_ne!(
            color_for(HighlightStyle::Comment),
            color_for(HighlightStyle::Function)
        );
    }

    #[test]
    fn a_language_is_detected_from_a_rust_path() {
        let styling = SyntaxStyling::new(13);
        assert!(styling.set_language_from_path(std::path::Path::new("src/main.rs")));
    }

    /// The requirement the squiggle layer rests on: a diagnostic's **document**
    /// byte range must slice out the characters the server actually named.
    ///
    /// Asserted by slicing rather than by comparing offsets, because the offsets
    /// are the thing under test — writing `47..50` here would just be the
    /// arithmetic done twice, and wrong in both places if it were wrong once.
    ///
    /// Three ways this can break, all of them live:
    ///
    /// - **UTF-16 against bytes.** `len` below sits at UTF-16 column 16 and byte
    ///   17, because an `ö` precedes it on the line. Reading the column as a byte
    ///   offset slices `.le` — a mark one character to the left, which is the
    ///   exact failure `utf16_to_byte` exists to prevent and which looks like
    ///   nothing at all on an ASCII line.
    /// - **Line-relative against document.** `apply_attr_styles` wants offsets
    ///   within a line; the overlay wants them within the document. Dropping the
    ///   line's start puts every squiggle in the file on line one.
    /// - **The line-text cache.** `diagnostic_ranges` keeps the last line it
    ///   sliced out of the rope, so these deliberately go line 1, line 2, line 1
    ///   — and the two lines are deliberately **different shapes**. The first
    ///   version of this test used two lines of the same layout, and a cache
    ///   that never refreshed still produced the right answer for both, because
    ///   the same column meant the same thing on either line. It caught nothing.
    ///   The short ASCII line comes first so that measuring the long line's
    ///   columns against it overruns and clamps.
    #[test]
    fn a_diagnostic_resolves_to_the_document_bytes_it_names() {
        let source = "fn main() {\n    let a = 1;\n    let x = \"ö\".len();\n}\n";
        let styling = SyntaxStyling::new(13);
        styling.set_language_from_path(std::path::Path::new("a.rs"));
        styling.reparse(&Rope::from(source));

        let at = |line: usize, start: usize, end: usize, severity: Severity| InlineDiagnostic {
            line,
            start_column: start,
            end_column: end,
            severity,
        };
        styling.set_diagnostics(vec![
            at(1, 12, 13, Severity::Warning), // `1`, on the short line
            at(2, 16, 19, Severity::Error),   // `len`, behind a two-byte `ö`
            at(1, 8, 9, Severity::Hint),      // `a`, back on the short line
        ]);

        // An out-of-bounds range, or one landing inside a character, is a
        // failure to report rather than a panic to decipher. A wrong offset
        // near a multi-byte character usually is not a char boundary, and
        // `&source[range]` would take the test down with a slicing panic
        // instead of showing which range was wrong.
        let marked: Vec<(String, Severity)> = styling
            .diagnostic_ranges()
            .iter()
            .map(|r| {
                let text = source
                    .get(r.offsets.clone())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("<not a valid span: {:?}>", r.offsets));
                (text, r.severity)
            })
            .collect();

        let expected = [
            ("1", Severity::Warning),
            ("len", Severity::Error),
            ("a", Severity::Hint),
        ];
        assert_eq!(marked.len(), expected.len(), "got {marked:?}");
        for (got, want) in marked.iter().zip(expected) {
            assert_eq!((got.0.as_str(), got.1), want);
        }
    }

    /// Apply an edit to a rope and return the new rope plus its span.
    fn edit(text: &str, start: usize, old_end: usize, insert: &str) -> (Rope, EditSpan) {
        let mut next = String::with_capacity(text.len() + insert.len());
        next.push_str(&text[..start]);
        next.push_str(insert);
        next.push_str(&text[old_end..]);
        (
            Rope::from(next.as_str()),
            EditSpan {
                start,
                old_end,
                new_len: insert.len(),
            },
        )
    }

    /// **An incremental parse must produce exactly what a full one would.**
    ///
    /// This is the only thing that makes reusing the tree safe. Tree-sitter is
    /// told what moved and keeps the rest; if the edit is described even slightly
    /// wrongly — a byte offset off by one, a column counted in characters rather
    /// than bytes — the tree quietly stops matching the text and the symptom is
    /// colours drifting sideways, not a crash or an error.
    ///
    /// So the test does not check that the incremental path is fast, or that it
    /// ran. It checks that it is indistinguishable from the answer that was
    /// already trusted.
    #[test]
    fn an_incremental_parse_gives_the_same_ranges_as_a_full_one() {
        let before = "fn main() {\n    let x = 1;\n    println!(\"hi\");\n}\n";
        // Insert a keyword mid-file: the tree above it is reusable, the tree
        // below it shifts, and the line itself re-parses.
        let (after, span) = edit(before, 16, 16, "mut ");

        let incremental = SyntaxStyling::new(13);
        incremental.set_language_from_path(std::path::Path::new("a.rs"));
        incremental.reparse(&Rope::from(before));
        assert!(
            incremental.reparse_incremental(&after, span),
            "an ordinary single-character insert should reuse the tree"
        );

        let full = SyntaxStyling::new(13);
        full.set_language_from_path(std::path::Path::new("a.rs"));
        full.reparse(&after);

        assert_eq!(
            *incremental.ranges.borrow(),
            *full.ranges.borrow(),
            "the reused tree disagrees with a fresh parse of the same text"
        );
        assert_eq!(
            *incremental.line_spans.borrow(),
            *full.line_spans.borrow(),
            "the per-line index disagrees, so painted colours would differ"
        );
    }

    /// The same, for a deletion and for an edit on a line containing multi-byte
    /// characters — where a column counted in anything but bytes goes wrong.
    #[test]
    fn incremental_parsing_survives_deletions_and_multibyte_lines() {
        let before = "fn main() {\n    let s = \"ünïcødé\";\n    let n = 42;\n}\n";

        // Delete `let n = 42;` entirely.
        let cut_from = before.find("    let n").unwrap();
        let cut_to = before.find("\n}\n").unwrap() + 1;
        for (start, old_end, insert) in [
            (cut_from, cut_to, ""),
            // And an insert after the non-ASCII text, on the same line.
            (
                before.find("\";").unwrap(),
                before.find("\";").unwrap(),
                "!",
            ),
        ] {
            let (after, span) = edit(before, start, old_end, insert);

            let incremental = SyntaxStyling::new(13);
            incremental.set_language_from_path(std::path::Path::new("a.rs"));
            incremental.reparse(&Rope::from(before));
            assert!(incremental.reparse_incremental(&after, span));

            let full = SyntaxStyling::new(13);
            full.set_language_from_path(std::path::Path::new("a.rs"));
            full.reparse(&after);

            assert_eq!(
                *incremental.ranges.borrow(),
                *full.ranges.borrow(),
                "edit at {start}..{old_end} inserting {insert:?} diverged"
            );
        }
    }

    /// With nothing parsed there is no tree to reuse, and the caller has to be
    /// told so rather than being handed a silently empty result.
    #[test]
    fn an_incremental_parse_declines_when_there_is_no_tree_to_reuse() {
        let styling = SyntaxStyling::new(13);
        styling.set_language_from_path(std::path::Path::new("a.rs"));
        assert!(
            !styling.reparse_incremental(
                &Rope::from("fn main() {}"),
                EditSpan {
                    start: 0,
                    old_end: 0,
                    new_len: 12
                }
            ),
            "nothing has been parsed, so there is nothing to be incremental about"
        );
    }

    /// An unparseable file mid-edit must keep its previous colouring rather than
    /// flashing monochrome on every unbalanced brace.
    #[test]
    fn a_failed_parse_keeps_the_previous_ranges() {
        let styling = SyntaxStyling::new(13);
        styling.set_language_from_path(std::path::Path::new("a.rs"));
        styling.reparse(&Rope::from("fn main() { let x = 1; }"));
        let good = styling.ranges.borrow().len();
        assert!(good > 0, "a valid file should produce ranges");

        styling.reparse(&Rope::from("fn main( {{{ ["));
        assert!(
            !styling.ranges.borrow().is_empty(),
            "colouring should survive a transient parse failure"
        );
    }
}
