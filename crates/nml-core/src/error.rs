//! Error types for NML parsing and validation.

use crate::span::Span;
use thiserror::Error;

/// All errors that can occur during NML parsing, lexing, or validation.
///
/// Each variant carries a human-readable message and a [`Span`] pointing
/// to the location in source where the error occurred.
/// The classified payload of a syntax error (RFC 0009): message, stable
/// code, and machine-applicable suggestion all **derive from the payload**,
/// so prose and taxonomy cannot drift — there is one channel per site.
///
/// `Generic` is the staged-migration carrier: emission sites not yet
/// classified keep their legacy prose byte-identically (code `None`), and
/// each classification is a one-line local change at the site. Same partial-
/// coverage rule as RFC 0008's codes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseErrorKind {
    /// Unclassified legacy prose (transitional; no code).
    Generic(String),
    /// Removed syntax with a mechanical replacement — the stability policy's
    /// "breaking changes ship with fixers" engine. One entry per migration
    /// (`=>`→`->`, …); the suggestion replaces `old` with `new` in place.
    ReplacedSyntax {
        old: &'static str,
        new: &'static str,
    },
}

impl ParseErrorKind {
    /// The human-facing message, derived from the payload.
    pub fn message(&self) -> String {
        match self {
            ParseErrorKind::Generic(message) => message.clone(),
            ParseErrorKind::ReplacedSyntax { old, new } => {
                format!("'{old}' was replaced by '{new}'")
            }
        }
    }

    /// The stable code, when this kind has one (exhaustive by construction:
    /// a new kind cannot compile without deciding).
    pub fn code(&self) -> Option<crate::diagnostic::Code> {
        match self {
            ParseErrorKind::Generic(_) => None,
            ParseErrorKind::ReplacedSyntax { .. } => {
                Some(crate::diagnostic::codes::REPLACED_SYNTAX)
            }
        }
    }

    /// A machine-applicable fix, derived from the payload. `span` is the
    /// error's anchor; `ReplacedSyntax` replaces exactly the old token's
    /// bytes from that anchor.
    pub fn suggestion(&self, span: Span) -> Option<(String, Span)> {
        match self {
            ParseErrorKind::Generic(_) => None,
            ParseErrorKind::ReplacedSyntax { old, new } => Some((
                (*new).to_string(),
                Span::new(span.start, span.start + old.len()),
            )),
        }
    }
}

#[derive(Debug, Clone, Error)]
pub enum NmlError {
    /// A syntax error during parsing.
    #[error("{}", kind.message())]
    Parse { kind: ParseErrorKind, span: Span },

    /// A tokenization error during lexing.
    #[error("{}", kind.message())]
    Lex { kind: ParseErrorKind, span: Span },

    /// A semantic validation error (e.g., duplicate declarations).
    #[error("{message}")]
    Validation { message: String, span: Span },

    /// An invalid money literal (e.g., bad currency code).
    #[error("invalid money value: {message}")]
    InvalidMoney {
        message: String,
        span: Span,
        /// The offending currency code and its own sub-span, captured
        /// structurally at the parse site (RFC 0008) so the ISO-4217
        /// did-you-mean attaches without message parsing. `None` for money
        /// errors that aren't about the currency code.
        currency: Option<(String, Span)>,
    },
}

impl NmlError {
    /// Returns the source span where this error occurred.
    pub fn span(&self) -> Span {
        match self {
            NmlError::Parse { span, .. }
            | NmlError::Lex { span, .. }
            | NmlError::Validation { span, .. }
            | NmlError::InvalidMoney { span, .. } => *span,
        }
    }

    /// Returns the human-readable error message. Owned: syntax-error
    /// messages derive from their [`ParseErrorKind`] payload (RFC 0009).
    pub fn message(&self) -> String {
        match self {
            NmlError::Parse { kind, .. } | NmlError::Lex { kind, .. } => kind.message(),
            NmlError::Validation { message, .. } | NmlError::InvalidMoney { message, .. } => {
                message.clone()
            }
        }
    }

    /// Lower this abort error into the unified findings model (RFC 0008) —
    /// the single `NmlError` → [`Diagnostic`] bridge, replacing the three
    /// hand-rolled converters that previously lived in the loader, the LSP,
    /// and the CLI. Unknown-currency errors attach an ISO-4217 did-you-mean
    /// from the structurally captured code — never from message text.
    pub fn to_diagnostic(&self) -> crate::diagnostic::Diagnostic {
        use crate::diagnostic::{codes, Diagnostic};
        let diag = Diagnostic::error(self.to_string()).with_span(self.span());
        match self {
            NmlError::InvalidMoney {
                currency: Some((code, code_span)),
                ..
            } => {
                let diag = diag.with_code(codes::UNKNOWN_CURRENCY);
                match crate::suggest::suggest(code, crate::money::currency_codes()) {
                    Some(s) => diag.with_suggestion(s, *code_span),
                    None => diag,
                }
            }
            NmlError::InvalidMoney { .. } => diag.with_code(codes::INVALID_MONEY),
            NmlError::Parse { kind, span } | NmlError::Lex { kind, span } => {
                let diag = match kind.code() {
                    Some(code) => diag.with_code(code),
                    None => diag,
                };
                match kind.suggestion(*span) {
                    Some((replacement, fix_span)) => diag.with_suggestion(replacement, fix_span),
                    None => diag,
                }
            }
            _ => diag,
        }
    }

    /// Unclassified lex error (transitional prose carrier — see
    /// [`ParseErrorKind::Generic`]).
    pub fn lex(message: impl Into<String>, span: Span) -> Self {
        NmlError::Lex {
            kind: ParseErrorKind::Generic(message.into()),
            span,
        }
    }

    /// Unclassified parse error (transitional prose carrier).
    pub fn parse(message: impl Into<String>, span: Span) -> Self {
        NmlError::Parse {
            kind: ParseErrorKind::Generic(message.into()),
            span,
        }
    }

    /// A classified parse error (RFC 0009): code, message, and any fix
    /// derive from the kind's payload.
    pub fn parse_kind(kind: ParseErrorKind, span: Span) -> Self {
        NmlError::Parse { kind, span }
    }
}

/// Convenience type alias for results with [`NmlError`].
pub type NmlResult<T> = Result<T, NmlError>;
