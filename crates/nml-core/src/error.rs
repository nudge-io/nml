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
    BareCarriageReturn,
    /// A raw control character (C0 other than tab and line endings, or DEL)
    /// anywhere in source. Control characters are content, and content
    /// belongs in escapes (`\u{1B}`), where review can see it.
    ForbiddenControlCharacter { ch: char },
    /// An invisible character that can make source display differently than
    /// it parses: a bidirectional control (Trojan Source, CVE-2021-42574) or
    /// an interior U+FEFF. The `\u{…}` escape is the sanctioned spelling.
    InvisibleCharacter { ch: char },
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
    /// An integer outside `i64` — numbers are exact by design, never
    /// silently rounded.
    NumberOutOfRange { raw: String },
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
            BareCarriageReturn => "bare carriage return (a CR with no following LF); \
                                   line endings are LF or CRLF — for a literal CR in a \
                                   string, write `\\r`"
                .to_string(),
            ForbiddenControlCharacter { ch } => format!(
                "raw control character U+{:04X} is not permitted in source; \
                 write it as `\\u{{{:X}}}` inside a string",
                *ch as u32, *ch as u32
            ),
            InvisibleCharacter { ch } => format!(
                "invisible character U+{:04X} can make source display differently \
                 than it parses; write it as `\\u{{{:X}}}` inside a string",
                *ch as u32, *ch as u32
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
                "unknown escape sequence '\\{}' (valid escapes: \\\" \\\\ \\n \\t \\r \\u{{…}})",
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
            NumberOutOfRange { raw } => {
                format!("integer \"{}\" out of range for 64-bit integer", echo(raw))
            }
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
            BareCarriageReturn => codes::BARE_CARRIAGE_RETURN,
            ForbiddenControlCharacter { .. } => codes::FORBIDDEN_CONTROL,
            InvisibleCharacter { .. } => codes::INVISIBLE_CHARACTER,
            BadDedent { .. } => codes::BAD_DEDENT,
            NestingLimit { .. } => codes::NESTING_LIMIT,
            SetSeparator => codes::SET_SEPARATOR,
            ReservedTypeKeyword => codes::RESERVED_TYPE_KEYWORD,
            UnknownTypeConstructor { .. } => codes::UNKNOWN_TYPE_CONSTRUCTOR,
            DuplicateDirective => codes::DUPLICATE_DIRECTIVE,
            InvalidEscape { .. } => codes::INVALID_ESCAPE,
            InvalidUnicodeEscape { .. } => codes::INVALID_ESCAPE,
            InvalidNumber { .. } => codes::INVALID_NUMBER,
            NumberOutOfRange { .. } => codes::NUMBER_OUT_OF_RANGE,
            BadSecretRef { .. } => codes::BAD_SECRET_REF,
            DoubleAmp => codes::REPLACED_SYNTAX,
        })
    }

    /// A machine-applicable fix, derived from the payload. `span` is the
    /// error's anchor. Fixes stay conservative: only byte replacements that
    /// provably preserve intent.
    pub fn suggestion(&self, span: Span) -> Option<(String, Span)> {
        use ParseErrorKind::*;
        match self {
            ReplacedSyntax { old, new } => Some((
                (*new).to_string(),
                Span::new(span.start, span.start + old.len()),
            )),
            // `&&` → `&`, over the span the emission anchored on both amps.
            DoubleAmp => Some(("&".to_string(), span)),
            // Deleting the stray CR provably preserves intent: it is never
            // content (that spelling is `\r`) and never a line ending.
            BareCarriageReturn => Some((String::new(), span)),
            // The comma becomes the alternative separator, in place.
            SetSeparator => Some(("|".to_string(), span)),
            UnknownTypeConstructor { found } => {
                crate::suggest::suggest(found, ["set"]).map(|s| (s.to_string(), span))
            }
            BadSecretRef {
                reason: SecretRefIssue::UnknownNamespace(ns),
            } => {
                // The namespace sub-span: after `$`, before `.`.
                crate::suggest::suggest(ns, crate::cst::KNOWN_NAMESPACES.iter().copied()).map(|s| {
                    (
                        s.to_string(),
                        Span::new(span.start + 1, span.start + 1 + ns.len()),
                    )
                })
            }
            _ => None,
        }
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
}

impl NmlError {
    /// Returns the source span where this error occurred.
    pub fn span(&self) -> Span {
        match self {
            NmlError::Syntax { span, .. } | NmlError::Money { span, .. } => *span,
        }
    }

    /// Returns the human-readable error message. Owned: syntax-error
    /// messages derive from their [`ParseErrorKind`] payload (RFC 0009).
    pub fn message(&self) -> String {
        match self {
            NmlError::Syntax { kind, .. } => kind.message(),
            NmlError::Money { kind, .. } => kind.message(),
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
            NmlError::Syntax { kind, span } => {
                let diag = match kind.code() {
                    Some(code) => diag.with_code(code),
                    None => diag,
                };
                let diag = match kind.suggestion(*span) {
                    // An empty replacement is a deletion — structurally a
                    // fix ("remove"), never a did-you-mean (there is no
                    // near-miss spelling of nothing).
                    Some((replacement, fix_span)) if replacement.is_empty() => {
                        diag.with_fix(replacement, fix_span)
                    }
                    Some((replacement, fix_span)) => diag.with_suggestion(replacement, fix_span),
                    None => diag,
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
