//! Position encoding negotiation and byte-canonical coordinates.
//!
//! `meta-ast` reports byte offsets and byte columns (tree-sitter points).
//! LSP reports columns in the negotiated encoding. This module converts
//! between the two at the protocol boundary. Byte offsets stay the internal
//! currency everywhere else.

use lsp_types::{ClientCapabilities, Position, PositionEncodingKind, Range};
use meta_ast::model::SourceRange;

/// Column encoding agreed with the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

impl Encoding {
    pub fn as_lsp(self) -> PositionEncodingKind {
        match self {
            Encoding::Utf8 => PositionEncodingKind::UTF8,
            Encoding::Utf16 => PositionEncodingKind::UTF16,
        }
    }
}

/// Pick an encoding from the client list.
pub fn negotiate(caps: &ClientCapabilities) -> Encoding {
    let offered = caps
        .general
        .as_ref()
        .and_then(|general| general.position_encodings.as_deref())
        .unwrap_or(&[]);
    if offered.contains(&PositionEncodingKind::UTF8) {
        Encoding::Utf8
    } else {
        Encoding::Utf16
    }
}

fn units_of(ch: char, encoding: Encoding) -> usize {
    match encoding {
        Encoding::Utf8 => ch.len_utf8(),
        Encoding::Utf16 => ch.len_utf16(),
    }
}

fn clamp_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// Precomputed line starts for one text buffer.
///
/// Building the index costs one scan of the text. Each conversion after that
/// costs O(log lines) plus the characters on one line.
#[derive(Debug)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    /// Scan the text once and record the byte offset of every line start.
    pub fn new(text: &str) -> Self {
        let mut starts = Vec::new();
        starts.push(0);
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(index + 1);
            }
        }
        Self { starts }
    }

    fn line_start(&self, line: u32) -> Option<usize> {
        self.starts.get(line as usize).copied()
    }

    fn line_end(&self, text: &str, start: usize) -> usize {
        match text[start..].find('\n') {
            Some(offset) => start + offset,
            None => text.len(),
        }
    }

    /// Convert an LSP position to a byte offset.
    pub fn to_byte_offset(&self, text: &str, pos: Position, encoding: Encoding) -> Option<usize> {
        let start = self.line_start(pos.line)?;
        let end = self.line_end(text, start);
        let line = &text[start..end];
        let target = pos.character as usize;
        let mut units = 0usize;
        for (index, ch) in line.char_indices() {
            if units >= target {
                return Some(start + index);
            }
            let next = units + units_of(ch, encoding);
            if next > target {
                return Some(start + index);
            }
            units = next;
        }
        Some(end)
    }

    /// Convert a byte offset to an LSP position.
    pub fn to_position(&self, text: &str, byte: usize, encoding: Encoding) -> Position {
        let byte = clamp_boundary(text, byte);
        let line = self
            .starts
            .partition_point(|&start| start <= byte)
            .saturating_sub(1);
        let start = self.starts[line];
        let character = text[start..byte]
            .chars()
            .map(|ch| units_of(ch, encoding))
            .sum::<usize>();
        Position {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(character).unwrap_or(u32::MAX),
        }
    }

    /// Convert a meta-ast byte range to an LSP range.
    pub fn range(&self, text: &str, range: &SourceRange, encoding: Encoding) -> Range {
        Range {
            start: self.to_position(text, range.byte_start, encoding),
            end: self.to_position(text, range.byte_end, encoding),
        }
    }
}

/// Source text plus its line index.
#[derive(Debug)]
pub struct SourceFile {
    text: String,
    lines: LineIndex,
}

impl SourceFile {
    /// Index the text once at construction.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let lines = LineIndex::new(&text);
        Self { text, lines }
    }

    /// Borrow the source text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Convert an LSP position to a byte offset.
    pub fn to_byte_offset(&self, pos: Position, encoding: Encoding) -> Option<usize> {
        self.lines.to_byte_offset(&self.text, pos, encoding)
    }

    /// Convert a meta-ast byte range to an LSP range.
    pub fn range(&self, range: &SourceRange, encoding: Encoding) -> Range {
        self.lines.range(&self.text, range, encoding)
    }
}

/// Fallback range when source text is unavailable: byte columns pass through.
pub fn range_without_text(range: &SourceRange) -> Range {
    Range {
        start: Position {
            line: u32::try_from(range.start.line).unwrap_or(u32::MAX),
            character: u32::try_from(range.start.column).unwrap_or(u32::MAX),
        },
        end: Position {
            line: u32::try_from(range.end.line).unwrap_or(u32::MAX),
            character: u32::try_from(range.end.column).unwrap_or(u32::MAX),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{GeneralClientCapabilities, Position, PositionEncodingKind};

    fn caps(offered: Option<Vec<PositionEncodingKind>>) -> ClientCapabilities {
        ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: offered,
                ..GeneralClientCapabilities::default()
            }),
            ..ClientCapabilities::default()
        }
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn position(text: &str, byte: usize, encoding: Encoding) -> Position {
        LineIndex::new(text).to_position(text, byte, encoding)
    }

    #[test]
    fn negotiate_prefers_utf8_then_falls_back() {
        assert_eq!(negotiate(&ClientCapabilities::default()), Encoding::Utf16);
        assert_eq!(negotiate(&caps(Some(Vec::new()))), Encoding::Utf16);
        assert_eq!(
            negotiate(&caps(Some(vec![PositionEncodingKind::UTF16]))),
            Encoding::Utf16
        );
        assert_eq!(
            negotiate(&caps(Some(vec![
                PositionEncodingKind::UTF8,
                PositionEncodingKind::UTF16
            ]))),
            Encoding::Utf8
        );
        assert_eq!(
            negotiate(&caps(Some(vec![
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8
            ]))),
            Encoding::Utf8
        );
    }

    #[test]
    fn ascii_offsets_are_stable() {
        let text = "abc\ndef\n";
        let index = LineIndex::new(text);
        assert_eq!(
            index.to_byte_offset(text, pos(0, 2), Encoding::Utf16),
            Some(2)
        );
        assert_eq!(
            index.to_byte_offset(text, pos(1, 1), Encoding::Utf16),
            Some(5)
        );
        assert_eq!(index.to_position(text, 5, Encoding::Utf16), pos(1, 1));
    }

    #[test]
    fn utf16_columns_count_surrogate_pairs() {
        // "🐍" is 4 UTF-8 bytes and 2 UTF-16 code units.
        let text = "x = \"🐍\"\n";
        let snake = text.find('🐍').unwrap();
        assert_eq!(position(text, snake, Encoding::Utf8), pos(0, 5));
        assert_eq!(position(text, snake, Encoding::Utf16), pos(0, 5));
        let after = snake + '🐍'.len_utf8();
        assert_eq!(position(text, after, Encoding::Utf8), pos(0, 9));
        assert_eq!(position(text, after, Encoding::Utf16), pos(0, 7));
    }

    #[test]
    fn multibyte_round_trip() {
        let text = "é中🐍 = 1\n";
        let source = SourceFile::new(text);
        for encoding in [Encoding::Utf8, Encoding::Utf16] {
            for (byte, _) in text.char_indices() {
                let position = position(text, byte, encoding);
                assert_eq!(
                    source.to_byte_offset(position, encoding),
                    Some(byte),
                    "encoding {encoding:?} byte {byte}"
                );
            }
        }
    }

    #[test]
    fn column_past_line_end_clamps() {
        let text = "ab\ncd\n";
        let index = LineIndex::new(text);
        assert_eq!(
            index.to_byte_offset(text, pos(0, 99), Encoding::Utf16),
            Some(2)
        );
        assert_eq!(
            index.to_byte_offset(text, pos(1, 99), Encoding::Utf16),
            Some(5)
        );
    }

    #[test]
    fn missing_line_returns_none() {
        let text = "ab\n";
        let index = LineIndex::new(text);
        assert_eq!(index.to_byte_offset(text, pos(5, 0), Encoding::Utf16), None);
    }

    #[test]
    fn position_inside_character_snaps_to_start() {
        let text = "🐍\n";
        let index = LineIndex::new(text);
        // UTF-16 column 1 is inside the surrogate pair.
        assert_eq!(
            index.to_byte_offset(text, pos(0, 1), Encoding::Utf16),
            Some(0)
        );
    }

    #[test]
    fn line_index_locates_bytes_across_many_lines() {
        let mut text = String::new();
        for line in 0..500 {
            text.push_str(&format!("line {line}\n"));
        }
        let index = LineIndex::new(&text);
        let target = text.find("line 321").expect("line 321");
        assert_eq!(
            index.to_position(&text, target, Encoding::Utf16),
            pos(321, 0)
        );
        assert_eq!(
            index.to_byte_offset(&text, pos(321, 5), Encoding::Utf16),
            Some(target + 5)
        );
    }

    #[test]
    fn range_uses_bytes() {
        let text = "x = \"🐍\"\n";
        let source_range = SourceRange {
            byte_start: 5,
            byte_end: 9,
            start: meta_ast::model::LineColumn { line: 0, column: 5 },
            end: meta_ast::model::LineColumn { line: 0, column: 9 },
        };
        assert_eq!(
            LineIndex::new(text).range(text, &source_range, Encoding::Utf16),
            Range {
                start: pos(0, 5),
                end: pos(0, 7)
            }
        );
    }

    #[test]
    fn range_without_text_uses_byte_columns() {
        let source_range = SourceRange {
            byte_start: 0,
            byte_end: 3,
            start: meta_ast::model::LineColumn { line: 2, column: 4 },
            end: meta_ast::model::LineColumn { line: 2, column: 7 },
        };
        assert_eq!(
            range_without_text(&source_range),
            Range {
                start: pos(2, 4),
                end: pos(2, 7)
            }
        );
    }
}
