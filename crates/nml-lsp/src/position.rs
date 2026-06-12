//! Conversions between LSP positions (UTF-16 code units) and byte offsets.
//!
//! The LSP protocol expresses `Position.character` in UTF-16 code units,
//! while Rust strings and `nml_core` spans are indexed in UTF-8 bytes.
//! Mixing the two corrupts positions on any line containing non-ASCII text
//! and can panic when byte-slicing mid-character. All position arithmetic
//! in this crate must go through these helpers.

use nml_core::span::Span;
use tower_lsp::lsp_types::{Position, Range};

/// Convert a UTF-16 column to a byte offset within `line`.
///
/// Never panics: columns past the end of the line clamp to `line.len()`,
/// and the result always lies on a `char` boundary, so it is safe to slice
/// with. A column landing inside a surrogate pair resolves to the boundary
/// after that character.
pub fn utf16_to_byte(line: &str, utf16_col: u32) -> usize {
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        if units >= utf16_col {
            return i;
        }
        units += c.len_utf16() as u32;
    }
    line.len()
}

/// Convert a byte offset within `line` to a UTF-16 column.
///
/// Never panics: offsets past the end of the line clamp to its UTF-16
/// length, and offsets inside a multi-byte character floor to that
/// character's start.
pub fn byte_to_utf16(line: &str, byte_col: usize) -> u32 {
    let byte_col = byte_col.min(line.len());
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        if i + c.len_utf8() > byte_col {
            break;
        }
        units += c.len_utf16() as u32;
    }
    units
}

/// The text of the 0-indexed `line` in `source` (newline excluded), if present.
pub fn line_at(source: &str, line: u32) -> Option<&str> {
    source.lines().nth(line as usize)
}

/// Byte-offset ↔ LSP `Position` mapping for one document.
///
/// This replaces direct use of `nml_core::span::SourceMap` inside the LSP:
/// the core source map reports *byte* columns, while the protocol requires
/// UTF-16 columns.
pub struct LineIndex<'a> {
    source: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(source: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self {
            source,
            line_starts,
        }
    }

    /// Convert a byte offset in the document to an LSP position (UTF-16
    /// column). Offsets past the end of the document are clamped.
    pub fn position(&self, offset: usize) -> Position {
        let offset = offset.min(self.source.len());
        let line = match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(insertion) => insertion - 1,
        };
        let line_start = self.line_starts[line];
        let character = byte_to_utf16(&self.source[line_start..], offset - line_start);
        Position::new(line as u32, character)
    }

    /// Convert an LSP position (UTF-16 column) to a byte offset in the
    /// document, clamping out-of-range lines and columns.
    pub fn offset(&self, pos: Position) -> usize {
        let line = (pos.line as usize).min(self.line_starts.len() - 1);
        let start = self.line_starts[line];
        let end = self
            .line_starts
            .get(line + 1)
            .map(|next| next - 1)
            .unwrap_or(self.source.len());
        let line_text = &self.source[start..end];
        let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);
        start + utf16_to_byte(line_text, pos.character)
    }

    /// Convert an `nml_core` byte span to an LSP range.
    pub fn range(&self, span: Span) -> Range {
        Range {
            start: self.position(span.start),
            end: self.position(span.end),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // "a😀b": 'a' 1 byte / 1 unit, '😀' 4 bytes / 2 units, 'b' 1 byte / 1 unit.
    const EMOJI_LINE: &str = "a😀b";

    #[test]
    fn utf16_to_byte_ascii() {
        assert_eq!(utf16_to_byte("hello", 0), 0);
        assert_eq!(utf16_to_byte("hello", 3), 3);
        assert_eq!(utf16_to_byte("hello", 5), 5);
    }

    #[test]
    fn utf16_to_byte_clamps_past_end() {
        assert_eq!(utf16_to_byte("ab", 99), 2);
        assert_eq!(utf16_to_byte("", 5), 0);
    }

    #[test]
    fn utf16_to_byte_emoji() {
        assert_eq!(utf16_to_byte(EMOJI_LINE, 1), 1);
        // Past the full surrogate pair.
        assert_eq!(utf16_to_byte(EMOJI_LINE, 3), 5);
        assert_eq!(utf16_to_byte(EMOJI_LINE, 4), 6);
    }

    #[test]
    fn utf16_to_byte_mid_surrogate_lands_on_boundary() {
        // Column 2 splits the surrogate pair; must still be a char boundary.
        let byte = utf16_to_byte(EMOJI_LINE, 2);
        assert!(EMOJI_LINE.is_char_boundary(byte));
        assert_eq!(byte, 5);
    }

    #[test]
    fn utf16_to_byte_cjk() {
        // Each CJK char: 3 bytes, 1 UTF-16 unit.
        assert_eq!(utf16_to_byte("日本語", 0), 0);
        assert_eq!(utf16_to_byte("日本語", 1), 3);
        assert_eq!(utf16_to_byte("日本語", 2), 6);
        assert_eq!(utf16_to_byte("日本語", 3), 9);
    }

    #[test]
    fn utf16_to_byte_combining_chars() {
        // "e" + COMBINING ACUTE ACCENT (2 bytes, 1 unit) + "x".
        let s = "e\u{0301}x";
        assert_eq!(utf16_to_byte(s, 1), 1);
        assert_eq!(utf16_to_byte(s, 2), 3);
        assert_eq!(utf16_to_byte(s, 3), 4);
    }

    #[test]
    fn byte_to_utf16_ascii() {
        assert_eq!(byte_to_utf16("hello", 0), 0);
        assert_eq!(byte_to_utf16("hello", 4), 4);
    }

    #[test]
    fn byte_to_utf16_emoji() {
        assert_eq!(byte_to_utf16(EMOJI_LINE, 1), 1);
        assert_eq!(byte_to_utf16(EMOJI_LINE, 5), 3);
        assert_eq!(byte_to_utf16(EMOJI_LINE, 6), 4);
    }

    #[test]
    fn byte_to_utf16_mid_char_floors() {
        // Byte 3 is inside the emoji; floors to the emoji's start.
        assert_eq!(byte_to_utf16(EMOJI_LINE, 3), 1);
    }

    #[test]
    fn byte_to_utf16_clamps_past_end() {
        assert_eq!(byte_to_utf16("日本", 999), 2);
    }

    #[test]
    fn roundtrip_on_boundaries() {
        let line = "café 😀 日本語 test";
        for (byte, _) in line.char_indices() {
            let units = byte_to_utf16(line, byte);
            assert_eq!(utf16_to_byte(line, units), byte);
        }
    }

    #[test]
    fn line_at_returns_lines() {
        let source = "first\nsecond\nthird";
        assert_eq!(line_at(source, 0), Some("first"));
        assert_eq!(line_at(source, 2), Some("third"));
        assert_eq!(line_at(source, 3), None);
    }

    #[test]
    fn line_index_position_ascii() {
        let index = LineIndex::new("hello\nworld");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(6), Position::new(1, 0));
        assert_eq!(index.position(8), Position::new(1, 2));
    }

    #[test]
    fn line_index_position_multibyte() {
        // Line 0: "x = \"😀\"" — emoji starts at byte 5, takes 4 bytes.
        let source = "x = \"😀\" y\nref = 日本";
        let index = LineIndex::new(source);
        // 'y' is at byte 11; UTF-16 column: x,sp,=,sp,'"' = 5, emoji = 2, '"' + sp = 2 → 9.
        assert_eq!(index.position(11), Position::new(0, 9));
        // Line 1 starts at byte 13; 本 starts at byte 13 + 6 + 3 = 22; column 7.
        assert_eq!(index.position(22), Position::new(1, 7));
    }

    #[test]
    fn line_index_position_clamps() {
        let index = LineIndex::new("ab");
        assert_eq!(index.position(999), Position::new(0, 2));
    }

    #[test]
    fn line_index_offset_multibyte() {
        let source = "x = \"😀\" y\nref = 日本";
        let index = LineIndex::new(source);
        assert_eq!(index.offset(Position::new(0, 9)), 11);
        assert_eq!(index.offset(Position::new(1, 7)), 22);
        // Clamps out-of-range line (to the start of the last line) and column.
        assert_eq!(
            index.offset(Position::new(99, 0)),
            source.find("ref").unwrap()
        );
        assert_eq!(index.offset(Position::new(1, 99)), source.len());
    }

    #[test]
    fn line_index_offset_position_roundtrip() {
        let source = "café 😀\n日本語 = \"x\"\nplain";
        let index = LineIndex::new(source);
        for (byte, _) in source.char_indices() {
            let pos = index.position(byte);
            assert_eq!(index.offset(pos), byte, "roundtrip failed at byte {byte}");
        }
    }

    #[test]
    fn line_index_range_multibyte() {
        let source = "tag = 日本語";
        let index = LineIndex::new(source);
        // Span over 日本語: bytes 6..15, UTF-16 columns 6..9.
        let range = index.range(Span::new(6, 15));
        assert_eq!(range.start, Position::new(0, 6));
        assert_eq!(range.end, Position::new(0, 9));
    }

    #[test]
    fn line_index_handles_crlf() {
        let source = "ab\r\ncd";
        let index = LineIndex::new(source);
        assert_eq!(index.position(4), Position::new(1, 0));
        assert_eq!(index.offset(Position::new(0, 99)), 2);
    }
}
