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

/// Byte offset of the first character of `line`. None when the line is absent.
fn line_start(text: &str, line: u32) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    let mut seen = 0u32;
    for (index, _) in text.match_indices('\n') {
        seen += 1;
        if seen == line {
            return Some(index + 1);
        }
    }
    None
}

/// Line content without the trailing newline.
fn line_content(text: &str, start: usize) -> &str {
    let rest = &text[start..];
    match rest.find('\n') {
        Some(end) => &rest[..end],
        None => rest,
    }
}

fn clamp_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// Convert an LSP position to a byte offset.
pub fn to_byte_offset(text: &str, pos: Position, encoding: Encoding) -> Option<usize> {
    let start = line_start(text, pos.line)?;
    let line = line_content(text, start);
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
    Some(start + line.len())
}

/// Convert a byte offset to an LSP position.
fn to_position(text: &str, byte: usize, encoding: Encoding) -> Position {
    let byte = clamp_boundary(text, byte);
    let line = text[..byte].matches('\n').count() as u32;
    let start = line_start(text, line).unwrap_or(0);
    let character = text[start..byte]
        .chars()
        .map(|ch| units_of(ch, encoding))
        .sum::<usize>();
    Position {
        line,
        character: u32::try_from(character).unwrap_or(u32::MAX),
    }
}

/// Convert a meta-ast byte range to an LSP range.
pub fn range(text: Option<&str>, range: &SourceRange, encoding: Encoding) -> Range {
    match text {
        Some(text) => Range {
            start: to_position(text, range.byte_start, encoding),
            end: to_position(text, range.byte_end, encoding),
        },
        None => Range {
            start: Position {
                line: u32::try_from(range.start.line).unwrap_or(u32::MAX),
                character: u32::try_from(range.start.column).unwrap_or(u32::MAX),
            },
            end: Position {
                line: u32::try_from(range.end.line).unwrap_or(u32::MAX),
                character: u32::try_from(range.end.column).unwrap_or(u32::MAX),
            },
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
        assert_eq!(to_byte_offset(text, pos(0, 2), Encoding::Utf16), Some(2));
        assert_eq!(to_byte_offset(text, pos(1, 1), Encoding::Utf16), Some(5));
        assert_eq!(to_position(text, 5, Encoding::Utf16), pos(1, 1));
    }

    #[test]
    fn utf16_columns_count_surrogate_pairs() {
        // "🐍" is 4 UTF-8 bytes and 2 UTF-16 code units.
        let text = "x = \"🐍\"\n";
        let snake = text.find('🐍').unwrap();
        assert_eq!(to_position(text, snake, Encoding::Utf8), pos(0, 5));
        assert_eq!(to_position(text, snake, Encoding::Utf16), pos(0, 5));
        let after = snake + '🐍'.len_utf8();
        assert_eq!(to_position(text, after, Encoding::Utf8), pos(0, 9));
        assert_eq!(to_position(text, after, Encoding::Utf16), pos(0, 7));
    }

    #[test]
    fn multibyte_round_trip() {
        let text = "é中🐍 = 1\n";
        for encoding in [Encoding::Utf8, Encoding::Utf16] {
            for (byte, _) in text.char_indices() {
                let position = to_position(text, byte, encoding);
                assert_eq!(
                    to_byte_offset(text, position, encoding),
                    Some(byte),
                    "encoding {encoding:?} byte {byte}"
                );
            }
        }
    }

    #[test]
    fn column_past_line_end_clamps() {
        let text = "ab\ncd\n";
        assert_eq!(to_byte_offset(text, pos(0, 99), Encoding::Utf16), Some(2));
        assert_eq!(to_byte_offset(text, pos(1, 99), Encoding::Utf16), Some(5));
    }

    #[test]
    fn missing_line_returns_none() {
        let text = "ab\n";
        assert_eq!(to_byte_offset(text, pos(5, 0), Encoding::Utf16), None);
    }

    #[test]
    fn position_inside_character_snaps_to_start() {
        let text = "🐍\n";
        // UTF-16 column 1 is inside the surrogate pair.
        assert_eq!(to_byte_offset(text, pos(0, 1), Encoding::Utf16), Some(0));
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
            range(Some(text), &source_range, Encoding::Utf16),
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
            range(None, &source_range, Encoding::Utf16),
            Range {
                start: pos(2, 4),
                end: pos(2, 7)
            }
        );
    }
}
