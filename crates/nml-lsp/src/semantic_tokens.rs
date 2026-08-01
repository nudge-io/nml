//! Semantic token legend and encoding for duration literals.
//!
//! One semantic token per magnitude/unit token — never a span across
//! interior trivia, so the spaced compound form (`1h 30m`) styles its
//! tokens and leaves the whitespace alone. Magnitudes are `number`; unit
//! suffixes are the custom `durationUnit` type (declared in the VS Code
//! extension with `superType: number`, so unthemed clients degrade to
//! number coloring while themes may style units like `keyword.other.unit`).

use nml_core::cst::{self, Parse};
use nml_core::span::Span;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensRangeResult, SemanticTokensResult,
    SemanticTokensServerCapabilities,
};

use crate::position::LineIndex;

/// Legend index of the `number` token type (duration magnitudes).
const TYPE_NUMBER: u32 = 0;
/// Legend index of the custom `durationUnit` token type (unit suffixes).
const TYPE_DURATION_UNIT: u32 = 1;

pub fn server_capabilities() -> SemanticTokensServerCapabilities {
    SemanticTokensOptions {
        legend: legend(),
        range: Some(true),
        full: Some(tower_lsp::lsp_types::SemanticTokensFullOptions::Bool(true)),
        ..Default::default()
    }
    .into()
}

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::NUMBER,
            SemanticTokenType::new("durationUnit"),
        ],
        token_modifiers: vec![SemanticTokenModifier::new("duration")],
    }
}

pub fn encode_document(parse: &Parse, source: &str, range: Option<Span>) -> SemanticTokens {
    let line_index = LineIndex::new(source);
    let sweep = range.map_or_else(
        || cst::duration_literals_in(parse, Span::new(0, source.len())),
        |r| cst::duration_literals_in(parse, r),
    );
    let mut enc = DeltaEncoder::default();
    for at in sweep {
        for (mag, unit) in &at.components {
            enc.push(*mag, TYPE_NUMBER, &line_index);
            if let Some(u) = unit {
                enc.push(*u, TYPE_DURATION_UNIT, &line_index);
            }
        }
    }
    SemanticTokens {
        result_id: None,
        data: enc.data,
    }
}

/// LSP relative token encoding: each token's line/column is a delta from
/// the previous token's start. Modifier bit 0 = `duration` on every token
/// (see [`legend`]).
#[derive(Default)]
struct DeltaEncoder {
    data: Vec<SemanticToken>,
    prev_line: u32,
    prev_start: u32,
}

impl DeltaEncoder {
    fn push(&mut self, span: Span, token_type: u32, line_index: &LineIndex) {
        let start = line_index.position(span.start);
        let end = line_index.position(span.end);
        // Duration tokens are single-line by grammar (components never
        // cross a newline), so the UTF-16 length is a column difference.
        let length = end.character.saturating_sub(start.character);
        if length == 0 {
            return;
        }
        let delta_line = start.line.saturating_sub(self.prev_line);
        let delta_start = if delta_line == 0 {
            start.character.saturating_sub(self.prev_start)
        } else {
            start.character
        };
        self.data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: 1,
        });
        self.prev_line = start.line;
        self.prev_start = start.character;
    }
}

pub fn full(parse: &Parse, source: &str) -> SemanticTokensResult {
    SemanticTokensResult::Tokens(encode_document(parse, source, None))
}

pub fn range(parse: &Parse, source: &str, span: Span) -> SemanticTokensRangeResult {
    SemanticTokensRangeResult::Tokens(encode_document(parse, source, Some(span)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(source: &str) -> Vec<SemanticToken> {
        let parse = nml_core::cst::parse(source);
        encode_document(&parse, source, None).data
    }

    #[test]
    fn compound_literal_emits_one_token_per_component_token() {
        // `1h30m` = Number(1) Ident(h) Number(30) Ident(m).
        let data = tokens("service App:\n    t = 1h30m\n");
        assert_eq!(data.len(), 4, "{data:?}");
        assert_eq!(
            (data[0].delta_line, data[0].delta_start, data[0].length),
            (1, 8, 1),
            "first token: line 1, col 8 (`1`)"
        );
        assert!(data.iter().all(|t| t.token_modifiers_bitset == 1));
        // Magnitudes are `number`; unit suffixes are `durationUnit`.
        assert_eq!(
            data.iter().map(|t| t.token_type).collect::<Vec<_>>(),
            vec![0, 1, 0, 1],
            "{data:?}"
        );
    }

    #[test]
    fn spaced_compound_leaves_the_gap_unstyled() {
        // `1h 30m`: tokens end/start around the space — no token covers it.
        let data = tokens("service App:\n    t = 1h 30m\n");
        assert_eq!(data.len(), 4, "{data:?}");
        // `h` at col 11 (len 1); `30` starts at col 13 → delta_start 2.
        assert_eq!(data[2].delta_start, 2, "{data:?}");
    }

    #[test]
    fn same_line_literals_delta_encode() {
        let data = tokens("service App:\n    a = [1h, 5s]\n");
        assert_eq!(data.len(), 4, "{data:?}");
        // Second literal's magnitude is on the same line: delta_line 0.
        assert_eq!(data[2].delta_line, 0);
        assert!(data[2].delta_start > 0);
    }

    #[test]
    fn utf16_columns_after_non_ascii() {
        // The emoji is 2 UTF-16 units; the literal's column must count them.
        let src = "service App:\n    s = \"☕\"\n    t = 5s\n";
        let data = tokens(src);
        assert_eq!(data.len(), 2, "{data:?}");
        assert_eq!(data[0].delta_line, 2);
        assert_eq!(data[0].delta_start, 8, "col of `5` on its own line");
    }

    #[test]
    fn range_sweep_excludes_out_of_range_literals() {
        let src = "service App:\n    a = 1h\n    b = 5s\n";
        let parse = nml_core::cst::parse(src);
        let start = src.find("1h").unwrap();
        let data = encode_document(&parse, src, Some(Span::new(start, start + 2))).data;
        assert_eq!(data.len(), 2, "only the first literal's tokens: {data:?}");
    }
}
