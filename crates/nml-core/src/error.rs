//! Error types for NML parsing and validation.

use crate::span::Span;
use thiserror::Error;

/// All errors that can occur during NML parsing, lexing, or validation.
///
/// Each variant carries a human-readable message and a [`Span`] pointing
/// to the location in source where the error occurred.
#[derive(Debug, Clone, Error)]
pub enum NmlError {
    /// A syntax error during parsing.
    #[error("{message}")]
    Parse { message: String, span: Span },

    /// A tokenization error during lexing.
    #[error("{message}")]
    Lex { message: String, span: Span },

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

    /// Returns the human-readable error message.
    pub fn message(&self) -> &str {
        match self {
            NmlError::Parse { message, .. }
            | NmlError::Lex { message, .. }
            | NmlError::Validation { message, .. }
            | NmlError::InvalidMoney { message, .. } => message,
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
            _ => diag,
        }
    }

    pub fn lex(message: impl Into<String>, span: Span) -> Self {
        NmlError::Lex {
            message: message.into(),
            span,
        }
    }

    pub fn parse(message: impl Into<String>, span: Span) -> Self {
        NmlError::Parse {
            message: message.into(),
            span,
        }
    }
}

/// Convenience type alias for results with [`NmlError`].
pub type NmlResult<T> = Result<T, NmlError>;
