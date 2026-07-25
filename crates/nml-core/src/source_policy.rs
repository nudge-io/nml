//! The source-character policy (spec: Source text) — the character-level
//! contract every NML document meets before structure is even considered.
//! The governing principle: **raw is transport, escaped is content.**
//!
//! * **Line endings are LF or CRLF.** A bare CR (no following LF) is either
//!   corruption or content smuggling; it is diagnosed, never silently
//!   normalized. (CRLF→LF *value* normalization is the value layer's job —
//!   `cst::value` — because the lossless CST must preserve transport bytes.)
//! * **Raw source is printable.** C0 controls (other than tab and line
//!   endings) and DEL are content, and content belongs in `\u{…}` escapes,
//!   where review can see it: a raw ESC in a value is a terminal-injection
//!   primitive, a raw NUL truncates C strings downstream.
//! * **Nothing invisible may steer the reader.** Bidirectional controls
//!   (Trojan Source, CVE-2021-42574) and interior U+FEFF can make source
//!   display differently than it parses. The banned set matches rustc's.
//!
//! A *leading* U+FEFF is accepted as a byte-order mark (Windows-editor
//! interop; the lexer files it as trivia) — everywhere else it is an error.
//!
//! One scan, run by `parse_lowered` beside lexing, so every consumer of
//! every parse inherits the policy and no entry point can forget it. Errors
//! are bounded by the lexer's `MAX_ERRORS`/suppressed-count contract.

use crate::error::{NmlError, ParseErrorKind};
use crate::span::Span;

/// Scan `source` for policy violations: `(bounded errors, suppressed count)`.
pub(crate) fn check(source: &str) -> (Vec<NmlError>, usize) {
    let mut errors = Vec::new();
    let mut suppressed = 0usize;
    for (i, ch) in source.char_indices() {
        let kind = match ch {
            '\r' if source.as_bytes().get(i + 1) == Some(&b'\n') => continue,
            '\r' => ParseErrorKind::BareCarriageReturn,
            '\u{FEFF}' if i == 0 => continue, // the byte-order mark
            _ if is_banned_control(ch) => ParseErrorKind::ForbiddenControlCharacter { ch },
            _ if is_invisible(ch) => ParseErrorKind::InvisibleCharacter { ch },
            _ => continue,
        };
        if errors.len() < crate::cst::MAX_ERRORS {
            errors.push(NmlError::syntax(kind, Span::new(i, i + ch.len_utf8())));
        } else {
            suppressed += 1;
        }
    }
    (errors, suppressed)
}

/// Must a string **value** character be rendered as an escape when NML is
/// generated (the formatter, code emitters)? Exactly the characters raw
/// source may not carry as content: the policy set plus CR — a raw CR is
/// transport or an error, never content, so a literal CR round-trips as
/// `\r`. LF and tab are legal raw and stay literal.
pub fn must_escape(ch: char) -> bool {
    ch == '\r' || is_banned_control(ch) || is_invisible(ch)
}

/// C0 (minus tab and the line-ending characters) and DEL. CR is *not* here:
/// it has its own contextual rule — CRLF is a line ending, bare CR errors.
fn is_banned_control(ch: char) -> bool {
    (ch < '\u{20}' && !matches!(ch, '\t' | '\n' | '\r')) || ch == '\u{7F}'
}

/// rustc's Trojan-Source set (the explicit bidi embeddings, overrides, and
/// isolates) plus U+FEFF, which is a BOM only at offset zero.
fn is_invisible(ch: char) -> bool {
    matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes_of(src: &str) -> Vec<u16> {
        check(src)
            .0
            .iter()
            .map(|e| match e {
                NmlError::Syntax { kind, .. } => match kind {
                    ParseErrorKind::BareCarriageReturn => 16,
                    ParseErrorKind::ForbiddenControlCharacter { .. } => 17,
                    ParseErrorKind::InvisibleCharacter { .. } => 18,
                    other => panic!("unexpected policy kind: {other:?}"),
                },
                other => panic!("policy emits syntax errors only: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn clean_sources_pass() {
        // LF, CRLF, tabs in strings, and non-ASCII text are all legal raw.
        for src in [
            "service App:\n    port = 1\n",
            "service App:\r\n    port = 1\r\n",
            "a = \"tab\there\"",
            "a = \"héllo → 🎉\"",
            "",
        ] {
            assert_eq!(codes_of(src), Vec::<u16>::new(), "{src:?}");
        }
    }

    #[test]
    fn bare_cr_reported_with_precise_span() {
        // Old-Mac line endings: each bare CR is one error at its own offset.
        let (errors, _) = check("a = 1\rb = 2\r");
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].span(), Span::new(5, 6));
        assert_eq!(errors[1].span(), Span::new(11, 12));
        // A CR that is half of CRLF is a line ending, never reported.
        assert_eq!(codes_of("a = 1\r\n"), Vec::<u16>::new());
    }

    #[test]
    fn controls_and_del_reported_everywhere() {
        // In token position, string content, and comments alike — the scan
        // is structure-blind by design (comments are a Trojan Source vector).
        assert_eq!(codes_of("a = \u{1B}1"), vec![17]);
        assert_eq!(codes_of("a = \"\u{0}\""), vec![17]);
        assert_eq!(codes_of("// \u{7F}"), vec![17]);
    }

    #[test]
    fn bidi_and_interior_feff_reported_leading_bom_accepted() {
        assert_eq!(codes_of("a = \"x\u{202E}y\""), vec![18]);
        assert_eq!(codes_of("// \u{2066}"), vec![18]);
        assert_eq!(codes_of("a = \"x\u{FEFF}\""), vec![18]);
        // Offset zero is the byte-order mark: accepted.
        assert_eq!(codes_of("\u{FEFF}a = 1\n"), Vec::<u16>::new());
    }

    #[test]
    fn errors_bounded_with_suppressed_count() {
        let src = "\u{1}".repeat(crate::cst::MAX_ERRORS + 7);
        let (errors, suppressed) = check(&src);
        assert_eq!(errors.len(), crate::cst::MAX_ERRORS);
        assert_eq!(suppressed, 7);
    }

    #[test]
    fn escape_set_is_exactly_the_policy_plus_cr() {
        assert!(must_escape('\r'));
        assert!(must_escape('\u{0}'));
        assert!(must_escape('\u{1B}'));
        assert!(must_escape('\u{7F}'));
        assert!(must_escape('\u{202E}'));
        assert!(must_escape('\u{FEFF}'));
        // Legal raw content stays literal.
        assert!(!must_escape('\t'));
        assert!(!must_escape('\n'));
        assert!(!must_escape('é'));
        // Zero-width joiners are legitimate content (emoji, Persian text).
        assert!(!must_escape('\u{200D}'));
    }
}
