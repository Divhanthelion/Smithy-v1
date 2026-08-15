//! LSP types and conversions
//!
//! This module provides types for working with LSP data and conversions
//! between LSP's UTF-16 positions and Ropey's UTF-8 char offsets.

use ropey::Rope;
use thiserror::Error;

/// Position encoding used by the LSP server
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEncoding {
    /// UTF-16 code units (default LSP encoding)
    #[default]
    Utf16,
    /// UTF-32 code points (same as Rust chars)
    Utf32,
    /// UTF-8 byte offsets
    Utf8,
}

/// A position in a document (line and column)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentPosition {
    /// Zero-based line number
    pub line: u32,
    /// Zero-based column (in UTF-16 code units by default)
    pub column: u32,
}

impl DocumentPosition {
    pub fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// Convert from LSP Position
    pub fn from_lsp(pos: lsp_types::Position) -> Self {
        Self {
            line: pos.line,
            column: pos.character,
        }
    }

    /// Convert to LSP Position
    pub fn to_lsp(self) -> lsp_types::Position {
        lsp_types::Position {
            line: self.line,
            character: self.column,
        }
    }

    /// Convert LSP position (UTF-16) to Ropey char offset
    ///
    /// LSP uses UTF-16 code units for column positions, while Ropey uses
    /// Unicode scalar values (chars). This function handles the conversion.
    pub fn to_char_offset(
        self,
        rope: &Rope,
        encoding: PositionEncoding,
    ) -> Result<usize, PositionError> {
        let line_idx = self.line as usize;
        if line_idx >= rope.len_lines() {
            return Err(PositionError::LineOutOfRange {
                line: line_idx,
                max_lines: rope.len_lines(),
            });
        }

        let line_start = rope.line_to_char(line_idx);
        let line = rope.line(line_idx);

        match encoding {
            PositionEncoding::Utf32 => {
                // UTF-32: column is char offset directly
                let char_offset = self.column as usize;
                if char_offset > line.len_chars() {
                    return Err(PositionError::ColumnOutOfRange {
                        column: char_offset,
                        max_column: line.len_chars(),
                    });
                }
                Ok(line_start + char_offset)
            }
            PositionEncoding::Utf16 => {
                // UTF-16: convert code units to char offset
                let target_utf16 = self.column as usize;
                let mut utf16_count = 0;
                let mut char_count = 0;

                for ch in line.chars() {
                    if utf16_count >= target_utf16 {
                        break;
                    }
                    utf16_count += ch.len_utf16();
                    char_count += 1;
                }

                Ok(line_start + char_count)
            }
            PositionEncoding::Utf8 => {
                // UTF-8: convert byte offset to char offset
                let target_bytes = self.column as usize;
                let mut byte_count = 0;
                let mut char_count = 0;

                for ch in line.chars() {
                    if byte_count >= target_bytes {
                        break;
                    }
                    byte_count += ch.len_utf8();
                    char_count += 1;
                }

                Ok(line_start + char_count)
            }
        }
    }

    /// Convert Ropey char offset to LSP position (UTF-16)
    pub fn from_char_offset(
        rope: &Rope,
        offset: usize,
        encoding: PositionEncoding,
    ) -> Result<Self, PositionError> {
        if offset > rope.len_chars() {
            return Err(PositionError::OffsetOutOfRange {
                offset,
                max_offset: rope.len_chars(),
            });
        }

        let line = rope.char_to_line(offset);
        let line_start = rope.line_to_char(line);
        let char_offset_in_line = offset - line_start;

        let column = match encoding {
            PositionEncoding::Utf32 => char_offset_in_line as u32,
            PositionEncoding::Utf16 => {
                // Convert char offset to UTF-16 code units
                let line_text = rope.line(line);
                let mut utf16_count = 0;
                for (i, ch) in line_text.chars().enumerate() {
                    if i >= char_offset_in_line {
                        break;
                    }
                    utf16_count += ch.len_utf16();
                }
                utf16_count as u32
            }
            PositionEncoding::Utf8 => {
                // Convert char offset to byte offset
                let line_text = rope.line(line);
                let mut byte_count = 0;
                for (i, ch) in line_text.chars().enumerate() {
                    if i >= char_offset_in_line {
                        break;
                    }
                    byte_count += ch.len_utf8();
                }
                byte_count as u32
            }
        };

        Ok(Self {
            line: line as u32,
            column,
        })
    }
}

/// A range in a document
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentRange {
    pub start: DocumentPosition,
    pub end: DocumentPosition,
}

impl DocumentRange {
    pub fn new(start: DocumentPosition, end: DocumentPosition) -> Self {
        Self { start, end }
    }

    pub fn from_lsp(range: lsp_types::Range) -> Self {
        Self {
            start: DocumentPosition::from_lsp(range.start),
            end: DocumentPosition::from_lsp(range.end),
        }
    }

    pub fn to_lsp(self) -> lsp_types::Range {
        lsp_types::Range {
            start: self.start.to_lsp(),
            end: self.end.to_lsp(),
        }
    }
}

/// Diagnostic severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl From<lsp_types::DiagnosticSeverity> for Severity {
    fn from(severity: lsp_types::DiagnosticSeverity) -> Self {
        match severity {
            lsp_types::DiagnosticSeverity::ERROR => Severity::Error,
            lsp_types::DiagnosticSeverity::WARNING => Severity::Warning,
            lsp_types::DiagnosticSeverity::INFORMATION => Severity::Information,
            lsp_types::DiagnosticSeverity::HINT => Severity::Hint,
            _ => Severity::Information,
        }
    }
}

/// A diagnostic message from the language server
#[derive(Debug, Clone)]
pub struct LspDiagnostic {
    pub range: DocumentRange,
    pub severity: Severity,
    pub message: String,
    pub code: Option<String>,
    pub source: Option<String>,
}

impl From<lsp_types::Diagnostic> for LspDiagnostic {
    fn from(diag: lsp_types::Diagnostic) -> Self {
        Self {
            range: DocumentRange::from_lsp(diag.range),
            severity: diag
                .severity
                .map(Severity::from)
                .unwrap_or(Severity::Information),
            message: diag.message,
            code: diag.code.map(|c| match c {
                lsp_types::NumberOrString::Number(n) => n.to_string(),
                lsp_types::NumberOrString::String(s) => s,
            }),
            source: diag.source,
        }
    }
}

/// Hover information from the language server
#[derive(Debug, Clone)]
pub struct LspHover {
    pub contents: String,
    pub range: Option<DocumentRange>,
}

impl From<lsp_types::Hover> for LspHover {
    fn from(hover: lsp_types::Hover) -> Self {
        let contents = match hover.contents {
            lsp_types::HoverContents::Scalar(marked) => match marked {
                lsp_types::MarkedString::String(s) => s,
                lsp_types::MarkedString::LanguageString(ls) => {
                    format!("```{}\n{}\n```", ls.language, ls.value)
                }
            },
            lsp_types::HoverContents::Array(arr) => arr
                .into_iter()
                .map(|m| match m {
                    lsp_types::MarkedString::String(s) => s,
                    lsp_types::MarkedString::LanguageString(ls) => {
                        format!("```{}\n{}\n```", ls.language, ls.value)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            lsp_types::HoverContents::Markup(markup) => markup.value,
        };

        Self {
            contents,
            range: hover.range.map(DocumentRange::from_lsp),
        }
    }
}

/// Errors that can occur during position conversion
#[derive(Debug, Error)]
pub enum PositionError {
    #[error("Line {line} out of range (max {max_lines})")]
    LineOutOfRange { line: usize, max_lines: usize },

    #[error("Column {column} out of range (max {max_column})")]
    ColumnOutOfRange { column: usize, max_column: usize },

    #[error("Offset {offset} out of range (max {max_offset})")]
    OffsetOutOfRange { offset: usize, max_offset: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ascii_position_is_the_same_in_bytes_and_utf16() {
        let rope = Rope::from_str("hello world");
        let pos = DocumentPosition::new(0, 6);
        let offset = pos.to_char_offset(&rope, PositionEncoding::Utf16).unwrap();
        assert_eq!(offset, 6);

        let back = DocumentPosition::from_char_offset(&rope, 6, PositionEncoding::Utf16).unwrap();
        assert_eq!(back, pos);
    }

    #[test]
    fn a_non_bmp_character_counts_as_two_utf16_units() {
        // 😀 is 2 UTF-16 code units
        let rope = Rope::from_str("a😀b");

        // Position after 'a' (UTF-16 offset 1)
        let pos = DocumentPosition::new(0, 1);
        let offset = pos.to_char_offset(&rope, PositionEncoding::Utf16).unwrap();
        assert_eq!(offset, 1); // char index of 😀

        // Position after 😀 (UTF-16 offset 3 because 😀 is 2 code units)
        let pos = DocumentPosition::new(0, 3);
        let offset = pos.to_char_offset(&rope, PositionEncoding::Utf16).unwrap();
        assert_eq!(offset, 2); // char index of 'b'
    }

    #[test]
    fn a_position_on_a_later_line_accounts_for_the_ones_above() {
        let rope = Rope::from_str("line1\nline2\nline3");

        let pos = DocumentPosition::new(1, 2);
        let offset = pos.to_char_offset(&rope, PositionEncoding::Utf16).unwrap();
        assert_eq!(offset, 8); // "line1\n" = 6 chars, then "li" = 2 more

        let back = DocumentPosition::from_char_offset(&rope, 8, PositionEncoding::Utf16).unwrap();
        assert_eq!(back.line, 1);
        assert_eq!(back.column, 2);
    }

    #[test]
    fn a_position_past_the_end_is_refused_rather_than_clamped_silently() {
        let rope = Rope::from_str("hello");

        let pos = DocumentPosition::new(5, 0);
        assert!(pos.to_char_offset(&rope, PositionEncoding::Utf16).is_err());
    }
}
