//! Error types for NML parsing and validation.

use crate::span::Span;
use thiserror::Error;

/// The classified payload of a syntax error (RFC 0009): message, stable
/// code, and machine-applicable suggestion all **derive from the payload**,
/// so prose and taxonomy cannot drift — there is one channel per site, and
/// no prose carrier exists: a new emission site cannot compile without a
/// kind, a code decision, and (via the docs guard) an index section.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseErrorKind {
    /// Removed syntax with a mechanical replacement — the stability policy's
    /// "breaking changes ship with fixers" engine. One entry per migration
    /// (`=>`→`->`, `&&`→`&`, …); the suggestion replaces `old` with `new`
    /// in place.
    ReplacedSyntax {
        old: &'static str,
        new: &'static str,
    },
    /// The workhorse: "expected …, found …". `expected` mixes concrete
    /// tokens (rendered via `SyntaxKind::describe`) and grammar classes
    /// ("a type"); `context` preserves positional prose ("after a shared
    /// property"); `found` is the offending token, echoed bounded.
    Expected {
        expected: Vec<ExpectedItem>,
        found: Option<FoundToken>,
        context: Option<&'static str>,
    },
    /// A string missing its closing delimiter. `open` spans the opening
    /// quote (related-info anchor); multi-line strings surface the failure
    /// at end-of-input, far from the `"""` that caused it.
    UnterminatedString { open: Span, multiline: bool },
    /// A byte no token starts with.
    UnexpectedCharacter { ch: char },
    /// A tab in indentation (spec: spaces only).
    TabInIndent,
    /// A carriage return not followed by a line feed. Line endings are LF or
    /// CRLF (spec: Source text); a bare CR is invisible in most tools and is
    /// either corruption or content smuggling, never intent.
    BareCarriageReturn {
        /// Inside a string literal (content, whose fix is the `\\r` escape)
        /// or in token position (transport, whose fix is deletion).
        in_string: bool,
    },
    /// A raw control character (any Unicode Cc — C0, DEL, or the C1
    /// range — other than tab and line endings) anywhere in source.
    /// Control characters are content, and content belongs in escapes
    /// (`\u{1B}`), where review can see it.
    ForbiddenControlCharacter {
        ch: char,
        /// Inside a string literal, where the character is CONTENT and a
        /// value-preserving machine repair exists (see [`Repairs`]); in
        /// token position no repair is sound.
        in_string: bool,
    },
    /// An invisible character that can make source display differently than
    /// it parses: a bidirectional control (Trojan Source, CVE-2021-42574),
    /// an interior U+FEFF, a U+2028/U+2029 line/paragraph separator, or a
    /// Unicode tag character (U+E0000–U+E007F).
    /// The `\u{…}` escape is the sanctioned spelling.
    InvisibleCharacter {
        ch: char,
        /// Inside a string literal, where the resolution space is
        /// enumerable (see [`Repairs`]); in token position no repair is
        /// sound.
        in_string: bool,
        /// Deletion provably removes JUST this character — the
        /// sentinel judgment plus the delete-splice lex-integrity
        /// check (`cst::value::RepairJudge::remove_preserves_value`).
        /// Only then is the *remove* alternative offered; where
        /// deletion would disturb the string's structure (a line
        /// flipping blank, quote runs merging, a CR gluing to an LF)
        /// the escape stands alone. Always `false` in token position.
        remove_sound: bool,
    },
    /// Content on a multi-line string's opening line (the Swift/Java rule:
    /// content begins on a new line). Text there would participate in
    /// dedent's min-indent — the one way transport interpretation could
    /// still be steered by where content sits.
    MultilineOpeningContent,
    /// A fallback chain (`a | b`) in a list position. Elements are single
    /// values: an anonymous chain has no stable identity for set
    /// uniqueness or reload diffing. The chain belongs at a property, or
    /// behind a `const` name referenced from the list.
    FallbackInListItem,
    /// An own-line closing `"""` whose indentation differs from the
    /// content's min-indent. Alignment makes the delimiter-anchored reading
    /// and the min-indent reading provably agree, so neither can be
    /// misread. Machine-fixable: moving the delimiter is value-preserving
    /// (its line is edge-trimmed either way).
    MultilineClosingMisaligned { expected: usize, found: usize },
    /// A dedent to a column matching no enclosing block. `valid` lists the
    /// open indentation levels — the offside rule, taught at the site.
    BadDedent { found: usize, valid: Vec<usize> },
    /// A deliberate resource bound was hit (`what` names the axis). The
    /// bound is a DoS defense on untrusted input, documented in the index.
    NestingLimit { what: &'static str },
    /// `set<a, b>` — the map-habit typo; elements are alternatives.
    /// Machine-fixable: the comma becomes `|`.
    SetSeparator,
    /// `map` is reserved for a future map type.
    ReservedTypeKeyword,
    /// An identifier takes type arguments but is not a known constructor
    /// (only `set` is). Carries the found name for the did-you-mean.
    UnknownTypeConstructor { found: String },
    /// A `#directive` key repeated on one field.
    DuplicateDirective,
    /// An unknown (`Some`) or unterminated (`None`) string escape.
    InvalidEscape { escape: Option<char> },
    /// A malformed `\u{…}` escape; the payload names the precise failure so
    /// the message can teach the exact fix.
    InvalidUnicodeEscape { issue: UnicodeEscapeIssue },
    /// A numeric literal no number parses from (e.g. `1.2.3`).
    InvalidNumber { raw: String },
    /// A numeric literal with a trailing decimal point (`1299.`). Shares
    /// `INVALID_NUMBER`'s code (same malformed-literal class) but carries
    /// its own signal so the fix — deleting the dot — is machine-applicable.
    NumberTrailingDot { raw: String },
    /// A misplaced `_` digit separator (`1__0`, `1_`, `1_.5`) — separators
    /// are legal only between two digits (one spelling per grouping,
    /// stricter than Rust). Shares `INVALID_NUMBER`'s code (same
    /// malformed-literal class). `stripped` is the producer's whole-anchor
    /// replacement with separators removed — provably value-preserving
    /// (separators are spelling, never value) — carried only when the
    /// literal is short enough to capture completely
    /// (64 bytes); a truncated replacement would corrupt the
    /// file, so a pathological literal gets the message without the fix.
    NumberBadSeparator {
        raw: String,
        stripped: Option<String>,
    },
    /// A number outside the exact decimal128 domain (RFC 0016): numbers
    /// are exact by design, never silently rounded, so >34 significant
    /// digits or an out-of-range magnitude is an error. The structured
    /// payload carries the counts; deliberately no `raw` echo — the
    /// offending literals are long by definition, the 32-char echo would
    /// truncate them into noise, and the span already locates the site.
    NumberOutOfRange {
        issue: crate::decimal::NumberRangeIssue,
    },
    /// A malformed `$NS.key` reference.
    BadSecretRef { reason: SecretRefIssue },
    /// `&&` written for `&` — a C-family habit, never valid NML. Shares
    /// `REPLACED_SYNTAX`'s code (same fix pattern: mechanical replacement)
    /// but keeps its own teaching prose — nothing was "replaced".
    DoubleAmp,
}

/// One alternative in an [`ParseErrorKind::Expected`] set: a concrete token
/// or a grammar class ("a value").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedItem {
    Kind(crate::cst::SyntaxKind),
    Desc(&'static str),
}

/// The offending token at an [`ParseErrorKind::Expected`] site. `text` is
/// captured bounded (`echo_capture`) and rendered bounded (`MAX_ECHO`);
/// control characters are escaped by the shared renderer, never trusted
/// raw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundToken {
    pub kind: crate::cst::SyntaxKind,
    pub text: String,
}

/// Why a `\u{…}` escape failed to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnicodeEscapeIssue {
    /// `\u` not followed by `{`.
    MissingBrace,
    /// No closing `}` before the string ended.
    Unterminated,
    /// `\u{}` — zero digits.
    Empty,
    /// More than 6 hex digits.
    Overlong,
    /// A non-hex character between the braces; carried for the message.
    BadDigit(char),
    /// Hex parsed but is a surrogate or above U+10FFFF; carries the value.
    NotAScalar(u32),
}

/// Why a `$NS.key` reference failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRefIssue {
    /// No `.` after the namespace (`$ENV` alone).
    MissingDot,
    /// The namespace is not a known source; carries it for the suggestion.
    UnknownNamespace(String),
    /// Nothing after `$NS.`.
    EmptyKey(String),
}

/// Echoed-source bound: enough to recognize the token, too little to flood
/// a terminal (long strings render via their kind's description instead).
pub(crate) const MAX_ECHO: usize = 32;

/// Bound on a machine-fix replacement carried **whole** in an error
/// payload (the separator-strip fix): unlike an echo, a replacement cannot
/// be truncated — a partial rewrite would corrupt the file — so past this
/// bound the fix is omitted rather than clipped. Generous for any literal
/// a human wrote; a stingy cap on what a hostile file can amplify.
pub(crate) const MAX_FIX_CAPTURE: usize = 64;

/// The separator-strip machine fix, whole-or-none: the replacement
/// substitutes the producer's entire anchor span, so it must never be
/// truncated — past [`MAX_FIX_CAPTURE`] the diagnostic ships without it.
pub(crate) fn strip_separators_fix(raw: &str) -> Option<String> {
    (raw.len() <= MAX_FIX_CAPTURE).then(|| raw.chars().filter(|c| *c != '_').collect())
}

pub(crate) fn echo(text: &str) -> String {
    if text.chars().count() <= MAX_ECHO {
        text.to_string()
    } else {
        let cut: String = text.chars().take(MAX_ECHO).collect();
        format!("{cut}…")
    }
}

/// Bound an echoed capture at its source: one char past [`MAX_ECHO`], so
/// [`echo`] still knows to append the ellipsis, but a pathological
/// multi-megabyte token can never be cloned wholesale into an error
/// payload (memory-amplification hardening — the error list is bounded in
/// *count* by `MAX_ERRORS`; this bounds each entry's *size*).
pub(crate) fn echo_capture(text: &str) -> String {
    text.chars().take(MAX_ECHO + 1).collect()
}

impl ExpectedItem {
    fn describe(&self) -> String {
        match self {
            ExpectedItem::Kind(k) => k.describe().to_string(),
            ExpectedItem::Desc(d) => (*d).to_string(),
        }
    }
}

/// The teaching tail for the character classes whose generic escape
/// advice alone would mislead — one exhaustive classifier, so a
/// character can never collect two tails. The line breaks (NEL, LS,
/// PS) are the ones renderers actually display as line breaks, where
/// the author almost always meant `\n` (VT/FF are controls no renderer
/// breaks on; they keep the generic message). The tag block is an
/// invisible ASCII mirror: its raw form is a hidden-text channel, and
/// its one legitimate modern use — emoji tag sequences — is content
/// that belongs in escapes.
fn char_class_hint(ch: char) -> &'static str {
    match ch {
        // NEL sits in BOTH ambiguity classes — a Unicode mandatory
        // line break AND the CP-1252 ellipsis byte (0x85) — so its
        // hint teaches all three readings its repair enumerates, in
        // the same order.
        '\u{85}' => {
            " — this is a Unicode line break: write `\\n` for a line break, \
             the escape to keep the character, or `…` if the byte is \
             Windows-1252 mojibake"
        }
        '\u{2028}' | '\u{2029}' => {
            " — this is a Unicode line break: write `\\n` for a line break, \
             or the escape to keep the character"
        }
        '\u{E0000}'..='\u{E007F}' => {
            " — tag characters mirror ASCII invisibly (a hidden-text \
             channel); for an emoji tag sequence, write the escapes"
        }
        _ => "",
    }
}

impl ParseErrorKind {
    /// The human-facing message, derived from the payload. Echoed source
    /// text is length-bounded here and control-escaped by the renderer.
    pub fn message(&self) -> String {
        use ParseErrorKind::*;
        match self {
            ReplacedSyntax { old, new } => format!("'{old}' was replaced by '{new}'"),
            Expected {
                expected,
                found,
                context,
            } => {
                let mut msg = String::from("expected ");
                let items: Vec<String> = expected.iter().map(ExpectedItem::describe).collect();
                match items.as_slice() {
                    [one] => msg.push_str(one),
                    [head @ .., last] => {
                        msg.push_str(&head.join(", "));
                        msg.push_str(" or ");
                        msg.push_str(last);
                    }
                    [] => msg.push_str("something else"),
                }
                if let Some(ctx) = context {
                    msg.push(' ');
                    msg.push_str(ctx);
                }
                match found {
                    Some(tok) if tok.text.is_empty() => {
                        msg.push_str(&format!(", found {}", tok.kind.describe()))
                    }
                    Some(tok) => msg.push_str(&format!(", found `{}`", echo(&tok.text))),
                    None => msg.push_str(", found end of file"),
                }
                msg
            }
            UnterminatedString {
                multiline: true, ..
            } => "unterminated multi-line string: missing closing `\"\"\"`".to_string(),
            UnterminatedString { .. } => "unterminated string: missing closing `\"`".to_string(),
            UnexpectedCharacter { ch } => {
                format!("unexpected character `{}`", echo(&ch.to_string()))
            }
            TabInIndent => "tabs are not permitted in indentation; use spaces".to_string(),
            BareCarriageReturn { .. } => "bare carriage return (a CR with no following LF); \
                                   line endings are LF or CRLF — for a literal CR in a \
                                   string, write `\\r`"
                .to_string(),
            // The advised spelling IS the shared one (`unicode_escape`)
            // — the same string the machine repair inserts and the
            // formatter emits, so advice and tooling can never drift.
            ForbiddenControlCharacter { ch, .. } => format!(
                "raw control character U+{:04X} is not permitted in source; \
                 write it as `{}` inside a string{}",
                *ch as u32,
                crate::source_policy::unicode_escape(*ch),
                char_class_hint(*ch)
            ),
            InvisibleCharacter { ch, .. } => format!(
                "invisible character U+{:04X} can make source display differently \
                 than it parses; write it as `{}` inside a string{}",
                *ch as u32,
                crate::source_policy::unicode_escape(*ch),
                char_class_hint(*ch)
            ),
            MultilineOpeningContent => "multi-line string content must begin on the \
                                        line after the opening `\"\"\"` (text on the \
                                        opening line would steer indentation stripping)"
                .to_string(),
            FallbackInListItem => "a fallback chain cannot be a list element — each \
                                    element is a single value (name the chain with a \
                                    `const` and reference it, or use a property)"
                .to_string(),
            MultilineClosingMisaligned { expected, found } => format!(
                "the closing `\"\"\"` must align with the content's indentation \
                 (content is at column {expected}, the closing quotes at {found})"
            ),
            BadDedent { found, valid } => {
                let levels = valid
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "indentation of {found} matches no enclosing block \
                     (open blocks are at columns {levels})"
                )
            }
            NestingLimit { what } => format!("maximum {what} nesting depth exceeded"),
            SetSeparator => "set elements are alternatives separated by '|', not ','".to_string(),
            ReservedTypeKeyword => {
                "'map' is reserved for a future map type — only 'set' takes type arguments today"
                    .to_string()
            }
            UnknownTypeConstructor { found } => format!(
                "unknown type constructor `{}` (only 'set' takes type arguments)",
                echo(found)
            ),
            DuplicateDirective => {
                "duplicate directive — each directive may appear once per field".to_string()
            }
            InvalidEscape { escape: Some(ch) } => format!(
                "unknown escape sequence '\\{}' (valid escapes: \\\" \\\\ \\n \\t \\r \\s \\u{{…}})",
                echo(&ch.to_string())
            ),
            InvalidEscape { escape: None } => {
                "unexpected end of string inside an escape sequence".to_string()
            }
            InvalidUnicodeEscape { issue } => match issue {
                UnicodeEscapeIssue::MissingBrace => {
                    "`\\u` must be followed by `{` (e.g. `\\u{1F600}`)".to_string()
                }
                UnicodeEscapeIssue::Unterminated => {
                    "unterminated `\\u{…}` escape: missing closing `}`".to_string()
                }
                UnicodeEscapeIssue::Empty => {
                    "empty `\\u{}` escape: expected 1–6 hex digits".to_string()
                }
                UnicodeEscapeIssue::Overlong => {
                    "overlong `\\u{…}` escape: at most 6 hex digits".to_string()
                }
                UnicodeEscapeIssue::BadDigit(ch) => format!(
                    "invalid character `{}` in `\\u{{…}}` escape: expected a hex digit",
                    echo(&ch.to_string())
                ),
                UnicodeEscapeIssue::NotAScalar(cp) => format!(
                    "`\\u{{{cp:X}}}` is not a Unicode scalar value \
                     (surrogates and code points above 10FFFF cannot be written)"
                ),
            },
            InvalidNumber { raw } => format!("invalid number: \"{}\"", echo(raw)),
            NumberTrailingDot { raw } => format!(
                "number \"{}\" ends with a decimal point; remove the \".\" or add \
                 fraction digits",
                echo(raw)
            ),
            NumberBadSeparator { raw, .. } => format!(
                "misplaced digit separator in \"{}\": `_` is allowed only between \
                 two digits (1_000, never 1__000 or 1_)",
                echo(raw)
            ),
            // The normative RFC 0016 texts; counts come from the payload,
            // never from the (truncated) echo.
            NumberOutOfRange { issue } => issue.to_string(),
            DoubleAmp => "'&' is the conjunction operator; '&&' is not needed".to_string(),
            BadSecretRef { reason } => match reason {
                SecretRefIssue::MissingDot => {
                    "expected '.' after the variable namespace (e.g. $ENV.MY_VAR)".to_string()
                }
                SecretRefIssue::UnknownNamespace(ns) => format!(
                    "unknown variable source '{}'. Valid sources: {}",
                    echo(ns),
                    crate::cst::KNOWN_NAMESPACES.join(", ")
                ),
                SecretRefIssue::EmptyKey(ns) => {
                    format!("expected a variable name after ${}.", echo(ns))
                }
            },
        }
    }

    /// The stable code, when this kind has one (exhaustive by construction:
    /// a new kind cannot compile without deciding).
    pub fn code(&self) -> Option<crate::diagnostic::Code> {
        use crate::diagnostic::codes;
        use ParseErrorKind::*;
        Some(match self {
            ReplacedSyntax { .. } => codes::REPLACED_SYNTAX,
            Expected { .. } => codes::UNEXPECTED_TOKEN,
            UnterminatedString { .. } => codes::UNTERMINATED_STRING,
            UnexpectedCharacter { .. } => codes::UNEXPECTED_CHARACTER,
            TabInIndent => codes::TAB_IN_INDENT,
            BareCarriageReturn { .. } => codes::BARE_CARRIAGE_RETURN,
            ForbiddenControlCharacter { .. } => codes::FORBIDDEN_CONTROL,
            InvisibleCharacter { .. } => codes::INVISIBLE_CHARACTER,
            MultilineOpeningContent => codes::MULTILINE_OPENING_CONTENT,
            FallbackInListItem => codes::FALLBACK_IN_LIST_ITEM,
            MultilineClosingMisaligned { .. } => codes::MULTILINE_CLOSING_MISALIGNED,
            BadDedent { .. } => codes::BAD_DEDENT,
            NestingLimit { .. } => codes::NESTING_LIMIT,
            SetSeparator => codes::SET_SEPARATOR,
            ReservedTypeKeyword => codes::RESERVED_TYPE_KEYWORD,
            UnknownTypeConstructor { .. } => codes::UNKNOWN_TYPE_CONSTRUCTOR,
            DuplicateDirective => codes::DUPLICATE_DIRECTIVE,
            InvalidEscape { .. } => codes::INVALID_ESCAPE,
            InvalidUnicodeEscape { .. } => codes::INVALID_ESCAPE,
            InvalidNumber { .. } => codes::INVALID_NUMBER,
            NumberTrailingDot { .. } => codes::INVALID_NUMBER,
            NumberBadSeparator { .. } => codes::INVALID_NUMBER,
            NumberOutOfRange { .. } => codes::NUMBER_OUT_OF_RANGE,
            BadSecretRef { .. } => codes::BAD_SECRET_REF,
            DoubleAmp => codes::REPLACED_SYNTAX,
        })
    }

    /// The escape spelling the in-string repair inserts for this
    /// kind's character — and the SAME string the reclassifier's
    /// soundness judge splices before granting the in-string reading
    /// (`cst::value`'s decode-equality gate), so the judged and the
    /// applied spellings can never diverge. `None` for kinds with no
    /// in-string escape repair.
    pub(crate) fn in_string_escape(&self) -> Option<String> {
        use ParseErrorKind::*;
        match self {
            BareCarriageReturn { .. } => Some("\\r".to_string()),
            ForbiddenControlCharacter { ch, .. } | InvisibleCharacter { ch, .. } => {
                Some(crate::source_policy::unicode_escape(*ch))
            }
            _ => None,
        }
    }

    /// The machine-repair space, derived from the payload — see
    /// [`Repairs`] for the applier contract. `span` is the error's
    /// anchor. Repairs stay conservative: a singular [`Repairs::Fix`]
    /// only where the rewrite provably preserves intent; genuine
    /// ambiguity is enumerated as [`Repairs::Alternatives`], which no
    /// applier ever picks from.
    pub fn repairs(&self, span: Span) -> Repairs {
        use crate::source_policy::{unicode_escape, windows_1252_repair};
        use ParseErrorKind::*;
        match self {
            ReplacedSyntax { old, new } => Repairs::DidYouMean(
                (*new).to_string(),
                Span::new(span.start, span.start + old.len()),
            ),
            // `&&` → `&`, over the span the emission anchored on both amps.
            DoubleAmp => Repairs::DidYouMean("&".to_string(), span),
            // A bare CR in token position has NO machine repair: on a
            // CR-terminated ("old Mac") file every CR IS a line ending,
            // so deleting it glues lines together, and the one
            // value-preserving repair (a line break) is exactly the
            // control character the shared injection guard refuses.
            // INSIDE a string literal the CR is content, and the
            // value-preserving fix is its escape.
            BareCarriageReturn { in_string: false } => Repairs::None,
            BareCarriageReturn { in_string: true } => {
                Repairs::Fix(self.in_string_escape().expect("carries one"), span)
            }
            // A raw control INSIDE a string is content; which repair is
            // value-preserving depends on the character's ambiguity class.
            ForbiddenControlCharacter {
                ch,
                in_string: true,
            } => match *ch {
                // NEL is the one character in BOTH ambiguity classes — a
                // Unicode mandatory line break AND a CP-1252 mojibake
                // artifact (0x85 displays as `…`) — hence uniquely three
                // readings: the line break meant, the byte kept, the
                // ellipsis the author's editor actually showed.
                '\u{85}' => Repairs::Alternatives(vec![
                    ("\\n".to_string(), span),
                    (unicode_escape('\u{85}'), span),
                    ("…".to_string(), span),
                ]),
                // A mapped C1 byte is either deliberate content (keep it,
                // escaped and visible) or the classic double-decode
                // (repair to what the CP-1252 author typed) — the reader
                // must choose.
                ch => match windows_1252_repair(ch) {
                    Some(repair) => Repairs::Alternatives(vec![
                        (unicode_escape(ch), span),
                        (repair.to_string(), span),
                    ]),
                    // C0, DEL, and the five unmapped C1 bytes: the escape
                    // is the ONE value-preserving reading — singular and
                    // auto-appliable, exactly like NML0016's in-string
                    // `\r`.
                    None => Repairs::Fix(self.in_string_escape().expect("carries one"), span),
                },
            },
            // An invisible INSIDE a string: the resolution space is
            // enumerable — and offered whole only where every entry is
            // sound.
            InvisibleCharacter {
                ch,
                in_string: true,
                remove_sound,
            } => match *ch {
                // LS/PS: a line break was meant, or the separator
                // itself — deletion is never offered, so `remove_sound`
                // is deliberately unread here (the reclassifier still
                // judges it rather than duplicate this class knowledge;
                // one bounded judgment wasted on a rare char).
                '\u{2028}' | '\u{2029}' => Repairs::Alternatives(vec![
                    ("\\n".to_string(), span),
                    (unicode_escape(*ch), span),
                ]),
                // Bidi controls, interior FEFF, tag characters: nearly
                // always pasted or hostile — remove it, or keep it
                // visibly (escaped). The remove arm is offered only
                // where deletion is PROVEN to remove just this
                // character (`remove_sound` — the sentinel judgment +
                // the delete-splice relex); where it is not, the set
                // COLLAPSES to the singular escape [`Repairs::Fix`] —
                // never a one-entry `Alternatives`, which would
                // auto-apply under the sole-candidate rule while
                // violating the ≥ 2 contract the variant documents.
                other if *remove_sound => Repairs::Alternatives(vec![
                    (String::new(), span),
                    (unicode_escape(other), span),
                ]),
                _ => Repairs::Fix(self.in_string_escape().expect("carries one"), span),
            },
            // In TOKEN position neither class has a sound repair: the
            // character is structure, and any rewrite is a guess about
            // what the structure should have been.
            ForbiddenControlCharacter {
                in_string: false, ..
            }
            | InvisibleCharacter {
                in_string: false, ..
            } => Repairs::None,
            // Rewriting the closing line's indent is provably
            // value-preserving: the line is edge-trimmed either way.
            MultilineClosingMisaligned { expected, .. } => {
                Repairs::Fix(" ".repeat(*expected), span)
            }
            // Deleting the trailing dot provably preserves the value
            // (`1299.` → `1299`). The dot is the literal's final byte, so
            // the fix needs no (possibly truncated) raw text.
            NumberTrailingDot { .. } if span.end > span.start => {
                Repairs::Fix(String::new(), Span::new(span.end - 1, span.end))
            }
            // Stripping separators provably preserves the value (spelling,
            // never value); the producer captured the whole replacement or
            // none (MAX_FIX_CAPTURE — a truncated rewrite would corrupt).
            NumberBadSeparator {
                stripped: Some(s), ..
            } => Repairs::DidYouMean(s.clone(), span),
            // The comma becomes the alternative separator, in place.
            SetSeparator => Repairs::DidYouMean("|".to_string(), span),
            UnknownTypeConstructor { found } => crate::suggest::suggest(found, ["set"])
                .map(|s| Repairs::DidYouMean(s.to_string(), span))
                .unwrap_or(Repairs::None),
            BadSecretRef {
                reason: SecretRefIssue::UnknownNamespace(ns),
            } => {
                // The namespace sub-span: after `$`, before `.`.
                crate::suggest::suggest(ns, crate::cst::KNOWN_NAMESPACES.iter().copied())
                    .map(|s| {
                        Repairs::DidYouMean(
                            s.to_string(),
                            Span::new(span.start + 1, span.start + 1 + ns.len()),
                        )
                    })
                    .unwrap_or(Repairs::None)
            }
            _ => Repairs::None,
        }
    }
}

/// The machine-repair space of a classified syntax error (RFC 0023
/// follow-on D1) — not merely what replacement text exists, but what an
/// applier may DO with it. Each variant is a semantic contract:
///
/// * [`Repairs::DidYouMean`] — a near-miss respelling of what the author
///   typed (`=>` → `->`, a misspelled namespace); singular, and
///   machine-applicable when it is the diagnostic's sole candidate.
/// * [`Repairs::Fix`] — the single provably intent-preserving rewrite
///   (an indent, a value-preserving escape); auto-applied by `nml fix`
///   under the same sole-candidate rule.
/// * [`Repairs::Alternatives`] — the enumerated resolution space where
///   the author's intent is genuinely ambiguous (a pasted NEL: line
///   break, kept byte, or mojibake ellipsis?). Structurally never
///   auto-applied: every applier's sole-candidate filter matches exactly
///   one suggestion, and N ≥ 2 alternatives can never be one — the
///   editor presents each as a separate action instead.
/// * [`Repairs::None`] — no machine repair is sound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repairs {
    /// No sound machine repair exists.
    None,
    /// A near-miss respelling: `(replacement, span)`.
    DidYouMean(String, Span),
    /// The single provably intent-preserving rewrite: `(replacement, span)`.
    Fix(String, Span),
    /// The enumerated resolution space, in presentation order (most
    /// likely intent first). Always ≥ 2 entries — a singular repair is
    /// [`Repairs::Fix`] — and never auto-applied.
    Alternatives(Vec<(String, Span)>),
}

#[cfg(test)]
mod repair_tests {
    use super::*;

    /// The soundness judge splices `in_string_escape()`; the repair
    /// inserts what `repairs()` constructs. This pin makes the coupling
    /// structural: for every in-string policy kind, the judged spelling
    /// IS the applied spelling (the singular fix equals it; every
    /// alternatives set contains it as the keep-the-character arm) — an
    /// edit that lets them diverge auto-applies an UNJUDGED replacement
    /// and fails here.
    #[test]
    fn the_judged_escape_is_the_applied_escape_for_every_policy_kind() {
        let span = Span::new(10, 11);
        let cases: Vec<ParseErrorKind> = vec![
            ParseErrorKind::BareCarriageReturn { in_string: true },
            // Singular-fix class (C0), mapped C1, NEL, LS, bidi, tag.
            ParseErrorKind::ForbiddenControlCharacter {
                ch: '\u{1}',
                in_string: true,
            },
            ParseErrorKind::ForbiddenControlCharacter {
                ch: '\u{93}',
                in_string: true,
            },
            ParseErrorKind::ForbiddenControlCharacter {
                ch: '\u{85}',
                in_string: true,
            },
            ParseErrorKind::InvisibleCharacter {
                ch: '\u{2028}',
                in_string: true,
                remove_sound: false,
            },
            // The remove-granted forms keep the judged escape as the
            // keep-the-character arm…
            ParseErrorKind::InvisibleCharacter {
                ch: '\u{202E}',
                in_string: true,
                remove_sound: true,
            },
            ParseErrorKind::InvisibleCharacter {
                ch: '\u{E0067}',
                in_string: true,
                remove_sound: true,
            },
            // …and the COLLAPSED forms (remove refused) surface it as
            // the singular Fix — covered by the Fix arm below.
            ParseErrorKind::InvisibleCharacter {
                ch: '\u{202E}',
                in_string: true,
                remove_sound: false,
            },
            ParseErrorKind::InvisibleCharacter {
                ch: '\u{FEFF}',
                in_string: true,
                remove_sound: false,
            },
        ];
        for kind in cases {
            let judged = kind.in_string_escape().expect("policy kinds carry one");
            match kind.repairs(span) {
                Repairs::Fix(replacement, s) => {
                    assert_eq!(replacement, judged, "{kind:?}");
                    assert_eq!(s, span);
                }
                Repairs::Alternatives(alts) => {
                    assert!(
                        alts.iter().any(|(r, s)| *r == judged && *s == span),
                        "{kind:?}: alternatives must contain the judged escape: {alts:?}"
                    );
                }
                other => panic!("{kind:?}: expected a repair, got {other:?}"),
            }
        }
        // The collapse, pinned shape-exactly (D-C): a remove-refused
        // bidi/FEFF/tag character carries the singular Fix whose
        // replacement IS the judged escape — never a one-entry
        // Alternatives (which would auto-apply while violating the
        // variant's ≥ 2 contract), and never the remove arm.
        let collapsed = ParseErrorKind::InvisibleCharacter {
            ch: '\u{202E}',
            in_string: true,
            remove_sound: false,
        };
        match collapsed.repairs(span) {
            Repairs::Fix(replacement, s) => {
                assert_eq!(
                    replacement,
                    collapsed.in_string_escape().expect("carries one")
                );
                assert_eq!(s, span);
            }
            other => panic!("the collapse must be the singular Fix, got {other:?}"),
        }
        // And the granted form still enumerates remove + escape, ≥ 2.
        let granted = ParseErrorKind::InvisibleCharacter {
            ch: '\u{202E}',
            in_string: true,
            remove_sound: true,
        };
        match granted.repairs(span) {
            Repairs::Alternatives(alts) => {
                assert_eq!(alts.len(), 2, "{alts:?}");
                assert!(alts.iter().any(|(r, _)| r.is_empty()), "{alts:?}");
            }
            other => panic!("the granted form stays enumerated, got {other:?}"),
        }
        // And kinds with no in-string escape judge nothing.
        assert_eq!(ParseErrorKind::DoubleAmp.in_string_escape(), None);
    }
}

#[derive(Debug, Clone, Error)]
pub enum NmlError {
    /// A syntax error (lexing or parsing — the phase distinction carried no
    /// information the kind's code doesn't; RFC 0009 merged the variants).
    #[error("{}", kind.message())]
    Syntax { kind: ParseErrorKind, span: Span },

    /// An invalid money literal — fully structured (RFC 0009 D13): message,
    /// code, and the ISO-4217 did-you-mean all derive from the kind.
    #[error("invalid money value: {}", kind.message())]
    Money {
        kind: crate::money::MoneyErrorKind,
        span: Span,
    },

    /// An invalid duration literal (RFC 0017) — money's exact analogue:
    /// message, code, and any machine-applicable fix (nearest unit,
    /// finer-unit respelling) all derive from the kind.
    #[error("{}", kind.message())]
    Duration {
        kind: crate::duration::DurationErrorKind,
        span: Span,
    },
}

impl NmlError {
    /// Returns the source span where this error occurred.
    pub fn span(&self) -> Span {
        match self {
            NmlError::Syntax { span, .. }
            | NmlError::Money { span, .. }
            | NmlError::Duration { span, .. } => *span,
        }
    }

    /// Returns the human-readable error message. Owned: syntax-error
    /// messages derive from their [`ParseErrorKind`] payload (RFC 0009).
    pub fn message(&self) -> String {
        match self {
            NmlError::Syntax { kind, .. } => kind.message(),
            NmlError::Money { kind, .. } => kind.message(),
            NmlError::Duration { kind, .. } => kind.message(),
        }
    }

    /// Lower this abort error into the unified findings model (RFC 0008) —
    /// the single `NmlError` → [`Diagnostic`](crate::diagnostic::Diagnostic)
    /// bridge, replacing the three
    /// hand-rolled converters that previously lived in the loader, the LSP,
    /// and the CLI. Unknown-currency errors attach an ISO-4217 did-you-mean
    /// from the structurally captured code — never from message text.
    pub fn to_diagnostic(&self) -> crate::diagnostic::Diagnostic {
        use crate::diagnostic::Diagnostic;
        let diag = Diagnostic::error(self.to_string()).with_span(self.span());
        match self {
            NmlError::Money { kind, .. } => {
                let diag = diag.with_code(kind.code());
                match kind {
                    crate::money::MoneyErrorKind::UnknownCurrency { code, code_span } => {
                        match crate::suggest::suggest(code, crate::money::currency_codes()) {
                            Some(s) => diag.with_suggestion(s, *code_span),
                            None => diag,
                        }
                    }
                    _ => diag,
                }
            }
            NmlError::Duration { kind, span } => {
                use crate::duration::{DurationErrorKind, DurationUnit};
                let diag = diag.with_code(kind.code());
                match kind {
                    // Nearest-unit did-you-mean over the suffix's own span
                    // (`30S` → `30s`); case-insensitive exact matches win in
                    // the shared engine, so casing typos always get the fix.
                    DurationErrorKind::UnknownUnit { unit, unit_span } => {
                        match crate::suggest::suggest(
                            unit,
                            DurationUnit::ALL.iter().map(|u| u.suffix()),
                        ) {
                            Some(s) => diag.with_suggestion(s, *unit_span),
                            None => diag,
                        }
                    }
                    // The granularity-preserving respelling replaces the
                    // whole literal (`1.5h` → `1h30m`, `30.5s` →
                    // `30s500ms`) — value-preserving by construction, so
                    // machine-applicable.
                    DurationErrorKind::FractionalMagnitude {
                        equivalent: Some(equivalent),
                        ..
                    } => diag.with_suggestion(equivalent, *span),
                    // The merged form replaces the whole literal (`1h2h` →
                    // `3h`) — value-preserving by construction.
                    DurationErrorKind::DuplicateUnit { merged } => {
                        diag.with_suggestion(merged, *span)
                    }
                    // No machine fix (completing or deleting the dangling
                    // magnitude would change the value); the related span
                    // points at the exact break.
                    DurationErrorKind::MalformedCompound { break_span } => {
                        diag.with_related(*break_span, "this magnitude has no unit")
                    }
                    _ => diag,
                }
            }
            NmlError::Syntax { kind, span } => {
                let diag = match kind.code() {
                    Some(code) => diag.with_code(code),
                    None => diag,
                };
                // The kind DECLARES its repair class (RFC 0023 D1) —
                // no textual heuristic decides fix-vs-did-you-mean.
                let diag = match kind.repairs(*span) {
                    Repairs::None => diag,
                    Repairs::DidYouMean(replacement, s) => diag.with_suggestion(replacement, s),
                    Repairs::Fix(replacement, s) => diag.with_fix(replacement, s),
                    // Each alternative is a Fix-kind suggestion: the
                    // renderer previews them capped, the editor offers
                    // each as its own action, and the sole-candidate rule
                    // keeps every applier's hands off (N ≥ 2, never one).
                    Repairs::Alternatives(alts) => alts
                        .into_iter()
                        .fold(diag, |d, (replacement, s)| d.with_fix(replacement, s)),
                };
                // Related info (RFC 0009): an unterminated string's failure
                // can surface far from its opening delimiter — label it.
                match kind {
                    ParseErrorKind::UnterminatedString { open, .. } => {
                        diag.with_related(*open, "string opened here")
                    }
                    _ => diag,
                }
            }
        }
    }

    /// A classified syntax error (RFC 0009): code, message, and any fix
    /// derive from the kind's payload. There is no prose constructor — the
    /// taxonomy is closed by construction.
    pub fn syntax(kind: ParseErrorKind, span: Span) -> Self {
        NmlError::Syntax { kind, span }
    }
}

/// Convenience type alias for results with [`NmlError`].
pub type NmlResult<T> = Result<T, NmlError>;
