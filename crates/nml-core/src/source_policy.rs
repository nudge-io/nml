//! The source-character policy (spec: Source text) — the character-level
//! contract every NML document meets before structure is even considered.
//! The governing principle: **raw is transport, escaped is content.**
//!
//! * **Line endings are LF or CRLF.** A bare CR (no following LF) is either
//!   corruption or content smuggling; it is diagnosed, never silently
//!   normalized. (CRLF→LF *value* normalization is the value layer's job —
//!   `cst::value` — because the lossless CST must preserve transport bytes.)
//! * **Raw source is printable.** Every Unicode CONTROL character
//!   (general category Cc — C0, DEL, and the C1 range U+0080–U+009F)
//!   other than tab and NML's line endings is content, and content
//!   belongs in `\u{…}` escapes, where review can see it: a raw ESC in
//!   a value is a terminal-injection primitive, C1's CSI (U+009B) is
//!   the same primitive in one byte, and a raw NUL truncates C strings
//!   downstream. (Raw C1 inside valid UTF-8 usually marks Windows-1252
//!   double-decoding — the error catches real corruption too.)
//! * **Nothing invisible may steer the reader.** The class is
//!   structure-steering invisibles: the explicit bidirectional controls
//!   (Trojan Source, CVE-2021-42574), interior U+FEFF, and the two
//!   non-control mandatory line breaks U+2028/U+2029 (UAX #14) — so
//!   every Unicode line-boundary character outside LF/CRLF is diagnosed
//!   (NEL/VT/FF as controls, LS/PS here, bare CR by its own rule). The
//!   bidi set matches rustc's Trojan-Source lints; the whole set is a
//!   strict superset of rustc's (rustc reads LS/PS as ordinary
//!   whitespace and allows raw C1 in literals) — NML values are echoed
//!   into terminals, logs, and generated configs, so the display-vs-
//!   parse guidance of UTS #55 (Unicode Source Code Handling) is
//!   applied at the language level. The Unicode tag block
//!   U+E0000–U+E007F (128 scalars, a deprecated invisible mirror of
//!   ASCII) is in the class too: a raw tag sequence is a hidden-text
//!   channel — invisible instructions or payloads riding inside what
//!   displays as ordinary text — with no raw-source use (its one
//!   modern purpose, emoji tag sequences, is content and belongs in
//!   escapes). The implicit bidi marks
//!   (LRM/RLM/ALM) stay legal: ordinary RTL content that reorders only
//!   neighboring weak characters, never tokens — rustc's line too.
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
            '\r' => ParseErrorKind::BareCarriageReturn { in_string: false },
            '\u{FEFF}' if i == 0 => continue, // the byte-order mark
            _ if is_banned_control(ch) => ParseErrorKind::ForbiddenControlCharacter {
                ch,
                in_string: false,
            },
            _ if is_invisible(ch) => ParseErrorKind::InvisibleCharacter {
                ch,
                in_string: false,
                // Token position: no repair is sound, removal included.
                // The reclassifier upgrades both fields together where
                // the judgments pass.
                remove_sound: false,
            },
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

/// The canonical `\u{…}` escape spelling of a character — uppercase
/// hex, no zero padding — shared by diagnostic messages, machine-fix
/// replacement text, and the formatter's value emitter, so the spelling
/// a diagnostic *advises* and the spelling tooling *writes* can never
/// drift apart.
pub fn unicode_escape(ch: char) -> String {
    format!("\\u{{{:X}}}", ch as u32)
}

/// The Windows-1252 reading of a C1 code point (U+0080–U+009F): the
/// character a CP-1252 byte displays as, which a double-decode (bytes
/// read as Latin-1, then re-encoded as UTF-8) turns into the raw C1
/// control NML0017 catches. `Some` is the mojibake *repair* — what the
/// author's editor actually showed (the ftfy class of fix); `None` for
/// the five bytes 0x81/0x8D/0x8F/0x90/0x9D that CP-1252 leaves
/// undefined, where no repair reading exists.
pub(crate) fn windows_1252_repair(ch: char) -> Option<char> {
    Some(match ch {
        '\u{80}' => '€',
        '\u{82}' => '‚',
        '\u{83}' => 'ƒ',
        '\u{84}' => '„',
        '\u{85}' => '…',
        '\u{86}' => '†',
        '\u{87}' => '‡',
        '\u{88}' => 'ˆ',
        '\u{89}' => '‰',
        '\u{8A}' => 'Š',
        '\u{8B}' => '‹',
        '\u{8C}' => 'Œ',
        '\u{8E}' => 'Ž',
        '\u{91}' => '\u{2018}',
        '\u{92}' => '\u{2019}',
        '\u{93}' => '\u{201C}',
        '\u{94}' => '\u{201D}',
        '\u{95}' => '•',
        '\u{96}' => '–',
        '\u{97}' => '—',
        '\u{98}' => '˜',
        '\u{99}' => '™',
        '\u{9A}' => 'š',
        '\u{9B}' => '›',
        '\u{9C}' => 'œ',
        '\u{9E}' => 'ž',
        '\u{9F}' => 'Ÿ',
        _ => return None,
    })
}

/// Every Unicode control (general category Cc — exactly
/// `char::is_control`: C0, DEL, and C1) minus tab and the line-ending
/// characters. Derived from the category, not a hand list, so the class
/// is closed by construction. CR is excluded here because it has its
/// own contextual rule — CRLF is a line ending, bare CR errors — and
/// NEL (U+0085), a Unicode mandatory line break, lands here as the C1
/// control it is.
fn is_banned_control(ch: char) -> bool {
    ch.is_control() && !matches!(ch, '\t' | '\n' | '\r')
}

/// The structure-steering invisibles: the EXPLICIT bidi controls
/// (rustc's Trojan-Source lint set — embeddings, overrides, isolates),
/// U+FEFF (a BOM only at offset zero), the two non-control mandatory
/// line breaks LS/PS (UAX #14), and the Unicode tag block
/// U+E0000–U+E007F (an invisible ASCII mirror — raw, it is a hidden
/// payload channel; emoji tag sequences are content and take escapes)
/// — characters that change how source STRUCTURE reads or carry text
/// the reader cannot see. The implicit bidi marks (LRM/RLM/ALM) are
/// deliberately NOT here: ordinary RTL string content, reordering only
/// neighboring weak characters (rustc excludes them too).
fn is_invisible(ch: char) -> bool {
    matches!(
        ch,
        '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{E0000}'..='\u{E007F}'
    )
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
                    ParseErrorKind::BareCarriageReturn { .. } => 16,
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
        // The C1 range is Cc too: NEL (a Unicode mandatory line break)
        // and CSI (the one-byte ESC-[) are banned raw everywhere.
        assert_eq!(codes_of("a = \"x\u{85}y\""), vec![17]);
        assert_eq!(codes_of("// \u{9B}"), vec![17]);
        assert_eq!(codes_of("a = \u{85}1"), vec![17]);
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
    fn line_and_paragraph_separators_are_invisible_steering() {
        // U+2028/U+2029 — the only mandatory line breaks (UAX #14) that
        // are not control characters: banned raw in strings, comments,
        // and token position alike; no BOM-style offset-0 carve-out.
        assert_eq!(codes_of("a = \"x\u{2028}y\""), vec![18]);
        assert_eq!(codes_of("// c\u{2029}x = 1"), vec![18]);
        assert_eq!(codes_of("\u{2028}a = 1\n"), vec![18]);
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
        assert!(must_escape('\u{85}'), "NEL is a C1 control");
        assert!(must_escape('\u{9B}'), "CSI is a C1 control");
        assert!(must_escape('\u{2028}'));
        assert!(must_escape('\u{2029}'));
        // The closed class: every C1 code point, no hand-list drift.
        assert!(('\u{80}'..='\u{9F}').all(must_escape));
        // Legal raw content stays literal.
        assert!(!must_escape('\t'));
        assert!(!must_escape('\n'));
        assert!(!must_escape('é'));
        // The implicit bidi marks are ORDINARY RTL content — outside
        // the steering class by decision, not oversight (they reorder
        // only neighboring weak characters; rustc excludes them too).
        assert!(!must_escape('\u{200E}'), "LRM stays legal");
        assert!(!must_escape('\u{200F}'), "RLM stays legal");
        assert!(!must_escape('\u{061C}'), "ALM stays legal");
        // Zero-width joiners are legitimate content (emoji, Persian text).
        assert!(!must_escape('\u{200D}'));
        // The tag block is closed end-to-end: 128 scalars, neighbors out.
        // (membership continues below)
        assert!(('\u{E0000}'..='\u{E007F}').all(must_escape));
        assert!(must_escape('\u{E0067}'), "TAG LATIN SMALL LETTER G");
        assert!(!must_escape('\u{E0080}'), "past the tag block");
        assert!(!must_escape('\u{DFFFF}'), "before the tag block");
    }

    #[test]
    fn the_escape_spelling_is_uppercase_hex_unpadded() {
        assert_eq!(unicode_escape('\u{1}'), "\\u{1}");
        assert_eq!(unicode_escape('\u{9B}'), "\\u{9B}");
        assert_eq!(unicode_escape('\u{202E}'), "\\u{202E}");
        assert_eq!(unicode_escape('\u{E0067}'), "\\u{E0067}");
    }

    #[test]
    fn the_1252_table_covers_exactly_the_27_mapped_c1_bytes() {
        let mapped = ('\u{80}'..='\u{9F}')
            .filter(|&c| windows_1252_repair(c).is_some())
            .count();
        assert_eq!(mapped, 27);
        for hole in ['\u{81}', '\u{8D}', '\u{8F}', '\u{90}', '\u{9D}'] {
            assert_eq!(
                windows_1252_repair(hole),
                None,
                "{hole:?} is undefined in CP-1252"
            );
        }
        // Spot the classic mojibake trio: NEL=ellipsis, smart quotes.
        assert_eq!(windows_1252_repair('\u{85}'), Some('…'));
        assert_eq!(windows_1252_repair('\u{93}'), Some('\u{201C}'));
        assert_eq!(windows_1252_repair('\u{92}'), Some('\u{2019}'));
        // Every repair passes the RESOLVER'S OWN injection guard
        // (`needs_escape` — a strict superset of `must_escape`): a fix
        // the guard would refuse is an action that silently does
        // nothing, and a character the policy re-diagnoses is worse.
        for c in '\u{80}'..='\u{9F}' {
            if let Some(r) = windows_1252_repair(c) {
                assert!(
                    !crate::diagnostic::needs_escape(r),
                    "repair {r:?} must pass the injection guard"
                );
            }
        }
        // And outside C1 there is no table at all.
        assert_eq!(windows_1252_repair('a'), None);
        assert_eq!(windows_1252_repair('\u{2028}'), None);
    }
}
