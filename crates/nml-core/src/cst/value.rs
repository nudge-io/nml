//! Value-decode layer (RFC 0004 P3).
//!
//! Interprets a *syntactic* `Value` / `ArrayValue` / `Fallback` CST node into a
//! *semantic* [`SpannedValue`] — the inverse of the parser's "syntax only"
//! discipline. All the interpretation the lexer/parser deliberately deferred
//! lives here: string-escape decoding, multiline resolution (the text-block
//! order — see [`decode_multiline`]), number range-checking, money parsing,
//! `$ENV` namespace validation, `true`/`false`, and template strings. Money
//! and template parsing **reuse** the existing `money`/`template` modules.
//! This module is the single source of truth for value interpretation; its
//! behavior is spec-defined (spec/syntax.md: Source text, Strings) and
//! pinned by the `cst::tests` unit suite plus the fuzz batteries there.

use crate::cst::syntax::{
    SyntaxKind, SyntaxNode, SyntaxToken, content_span, node_span, text_offset,
};

/// Decode a bare string-literal token (`"…"`) to its value — used where the
/// grammar guarantees a string (oneof discriminator/arm values) so there is no
/// surrounding `Value` node to go through [`decode_value`].
pub(super) fn decode_string_token(tok: &SyntaxToken) -> Result<String, NmlError> {
    decode_string(tok.text(), text_offset(tok.text_range().start()))
}
use crate::error::NmlError;
use crate::span::Span;
use crate::types::{Number, SpannedValue, Value};
use crate::{money, template};

/// Variable-reference namespaces recognized after `$`.
pub(crate) const KNOWN_NAMESPACES: &[&str] = &["ENV"];

/// Decode a value node (`Value`, `ArrayValue`, or `Fallback`) into a
/// [`SpannedValue`]. Returns the first semantic error encountered.
pub fn decode_value(node: &SyntaxNode) -> Result<SpannedValue, NmlError> {
    match node.kind() {
        SyntaxKind::Value => decode_scalar(node),
        SyntaxKind::ArrayValue => decode_array(node),
        SyntaxKind::Fallback => decode_fallback(node),
        other => Err(NmlError::syntax(
            crate::error::ParseErrorKind::Expected {
                expected: vec![crate::error::ExpectedItem::Desc("a value node")],
                found: Some(crate::error::FoundToken {
                    kind: other,
                    text: String::new(),
                }),
                context: None,
            },
            node_span(node),
        )),
    }
}

fn decode_scalar(node: &SyntaxNode) -> Result<SpannedValue, NmlError> {
    let span = content_span(node);
    let toks = sig_tokens(node);
    // Decode is *semantic* validation; an empty value node is a syntactic failure
    // the parser already reported, so it yields a placeholder rather than a
    // (redundant) error.
    let Some(first) = toks.first() else {
        return Ok(SpannedValue::new(Value::String(String::new()), span));
    };

    let value = match first.kind() {
        SyntaxKind::String => {
            // `span.start` is the string token's start (it is the only
            // significant token, so `content_span` begins there).
            let decoded = decode_string(first.text(), span.start)?;
            // Template detection reads the RAW token text: templates are
            // syntax, escapes are content, so `\u{7B}\u{7B}` means a
            // literal `{{` — the escape hatch for the collision — and an
            // escape can never smuggle a template past review. (No escape
            // could produce a brace before `\u{…}` existed, so this is
            // observationally identical for every prior document.)
            if first.text().contains("{{") {
                Value::TemplateString(template::parse_template_string(&decoded, span.start))
            } else {
                Value::String(decoded)
            }
        }
        SyntaxKind::Number => number_or_money(&toks[..], span, false)?,
        SyntaxKind::Dash => number_or_money(&toks[..], span, true)?,
        SyntaxKind::Role => {
            // RFC 0014: a role-conjunction expression (`Role (& Role)*`)
            // lowers to ONE `Value::Role` carrying the canonical
            // `" & "`-joined text — single-spaced regardless of source
            // spacing. This exact form is the cross-repo contract consumers
            // (nudge) parse and display; the formatter re-renders from this
            // value, so canonicalization is automatic.
            let mut text = first.text().to_string();
            for tok in toks.iter().skip(1).filter(|t| t.kind() == SyntaxKind::Role) {
                text.push_str(" & ");
                text.push_str(tok.text());
            }
            Value::Role(text)
        }
        SyntaxKind::Secret => {
            validate_secret(first.text(), span)?;
            Value::Secret(first.text().to_string())
        }
        SyntaxKind::Ident => match first.text() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            other => Value::Reference(other.to_string()),
        },
        other => {
            return Err(NmlError::syntax(
                crate::error::ParseErrorKind::Expected {
                    expected: vec![crate::error::ExpectedItem::Desc("a value")],
                    found: Some(crate::error::FoundToken {
                        kind: other,
                        text: String::new(),
                    }),
                    context: None,
                },
                span,
            ));
        }
    };
    Ok(SpannedValue::new(value, span))
}

/// `Number (currency)?` or, when `negative`, `- Number (currency)?`.
fn number_or_money(toks: &[SyntaxToken], span: Span, negative: bool) -> Result<Value, NmlError> {
    let num_idx = usize::from(negative);
    // Incomplete (`-` with no number) is a syntactic failure the parser reported;
    // yield a placeholder rather than a redundant decode error.
    let Some(number) = toks.get(num_idx).filter(|t| t.kind() == SyntaxKind::Number) else {
        return Ok(Value::Number(Number::Int(0)));
    };
    let raw = if negative {
        format!("-{}", number.text())
    } else {
        number.text().to_string()
    };

    match toks.get(num_idx + 1) {
        Some(cur) if cur.kind() == SyntaxKind::Ident => {
            Ok(Value::Money(money::parse_money(&raw, cur.text(), span)?))
        }
        _ => Ok(Value::Number(parse_number(&raw, span)?)),
    }
}

fn decode_array(node: &SyntaxNode) -> Result<SpannedValue, NmlError> {
    let items = value_children(node)
        .map(|c| decode_value(&c))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SpannedValue::new(Value::Array(items), content_span(node)))
}

/// `a | b | c` decodes right-associatively to `Fallback(a, Fallback(b, c))`
/// — the resolver tries left-to-right, so the chain nests rightward.
fn decode_fallback(node: &SyntaxNode) -> Result<SpannedValue, NmlError> {
    let mut values = value_children(node)
        .map(|c| decode_value(&c))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .rev();
    let mut acc = values.next().ok_or_else(|| {
        NmlError::syntax(
            crate::error::ParseErrorKind::Expected {
                expected: vec![crate::error::ExpectedItem::Desc("a fallback arm")],
                found: Some(crate::error::FoundToken {
                    kind: SyntaxKind::Fallback,
                    text: String::new(),
                }),
                context: None,
            },
            node_span(node),
        )
    })?;
    for v in values {
        let span = v.span.merge(acc.span);
        acc = SpannedValue::new(Value::Fallback(Box::new(v), Box::new(acc)), span);
    }
    Ok(acc)
}

// ── scalar decoders (spec: Strings; pinned by cst::tests + the fuzz batteries) ──

/// Decode a string token's raw text (`"…"` or `"""…"""`) into its value: strip
/// the delimiters, process escapes, and (triple-quoted) resolve the body in
/// the text-block order — see [`decode_multiline`]. `tok_start` is the
/// token's source offset, so escape errors get char-precise spans.
fn decode_string(raw: &str, tok_start: usize) -> Result<String, NmlError> {
    if let Some(body) = raw.strip_prefix("\"\"\"") {
        let body = body.strip_suffix("\"\"\"").unwrap_or(body);
        decode_multiline(body, tok_start + 3)
    } else if let Some(body) = raw.strip_prefix('"') {
        let body = body.strip_suffix('"').unwrap_or(body);
        decode_escapes(body, tok_start + 1)
    } else {
        decode_escapes(raw, tok_start)
    }
}

/// [`decode_escapes_into`], collected — for the single-line callers.
fn decode_escapes(inner: &str, inner_start: usize) -> Result<String, NmlError> {
    let mut out = String::with_capacity(inner.len());
    decode_escapes_into(&mut out, inner, inner_start)?;
    Ok(out)
}

/// Decode `\" \\ \n \t \r \u{…}` into `out`; `inner_start` is the source
/// offset of `inner`'s first byte, so errors point at the exact escape.
/// Content only, by construction: single-line tokens cannot span lines and
/// the multiline splitter owns line terminators, so none reach this loop.
fn decode_escapes_into(out: &mut String, inner: &str, inner_start: usize) -> Result<(), NmlError> {
    let mut chars = inner.char_indices();
    while let Some((i, c)) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some((_, '"')) => out.push('"'),
            Some((_, '\\')) => out.push('\\'),
            Some((_, 'n')) => out.push('\n'),
            Some((_, 't')) => out.push('\t'),
            Some((_, 'r')) => out.push('\r'),
            Some((_, 'u')) => out.push(decode_unicode_escape(&mut chars, inner, inner_start, i)?),
            Some((j, other)) => {
                // Span the `\` (at `i`) through the escape char (ending at `j + len`).
                let span = Span::new(inner_start + i, inner_start + j + other.len_utf8());
                return Err(NmlError::syntax(
                    crate::error::ParseErrorKind::InvalidEscape {
                        escape: Some(other),
                    },
                    span,
                ));
            }
            None => {
                let span = Span::new(inner_start + i, inner_start + inner.len());
                return Err(NmlError::syntax(
                    crate::error::ParseErrorKind::InvalidEscape { escape: None },
                    span,
                ));
            }
        }
    }
    Ok(())
}

/// Decode the braced tail of a `\u{…}` escape (Rust/Swift syntax: 1–6 hex
/// digits naming a Unicode scalar), `chars` positioned just past the `u`.
/// `backslash` is the escape's start within `inner`, so every error spans
/// from the `\` through the offending character — the same precision as
/// [`decode_escapes`]' simple arms.
fn decode_unicode_escape(
    chars: &mut std::str::CharIndices<'_>,
    inner: &str,
    inner_start: usize,
    backslash: usize,
) -> Result<char, NmlError> {
    use crate::error::UnicodeEscapeIssue;
    let err = |issue: UnicodeEscapeIssue, end: usize| {
        NmlError::syntax(
            crate::error::ParseErrorKind::InvalidUnicodeEscape { issue },
            Span::new(inner_start + backslash, inner_start + end),
        )
    };
    match chars.next() {
        Some((_, '{')) => {}
        Some((j, c)) => return Err(err(UnicodeEscapeIssue::MissingBrace, j + c.len_utf8())),
        None => return Err(err(UnicodeEscapeIssue::MissingBrace, inner.len())),
    }
    let mut value: u32 = 0;
    let mut digits = 0usize;
    loop {
        match chars.next() {
            Some((j, '}')) => {
                let end = j + 1;
                return if digits == 0 {
                    Err(err(UnicodeEscapeIssue::Empty, end))
                } else if digits > 6 {
                    Err(err(UnicodeEscapeIssue::Overlong, end))
                } else {
                    char::from_u32(value)
                        .ok_or_else(|| err(UnicodeEscapeIssue::NotAScalar(value), end))
                };
            }
            Some((j, c)) => match c.to_digit(16) {
                Some(d) => {
                    digits += 1;
                    // Stop accumulating past 6 digits: the escape is already
                    // Overlong, and an unbounded run must not overflow.
                    if digits <= 6 {
                        value = value * 16 + d;
                    }
                }
                None => return Err(err(UnicodeEscapeIssue::BadDigit(c), j + c.len_utf8())),
            },
            None => return Err(err(UnicodeEscapeIssue::Unterminated, inner.len())),
        }
    }
}

/// Decode a triple-quoted body in the text-block order (spec: Strings; the
/// order Java's JEP 378 specifies): **transport first** — split RAW lines
/// (a CRLF's CR belongs to the terminator), trim the blank first/last
/// lines, compute min-indent — **then content**: decode escapes per
/// retained line slice at its true source offset, so escape errors span
/// exact bytes with no remapping. Content therefore can never steer
/// transport interpretation, and escaped whitespace survives dedent
/// ("protected space" — the capability Java added `\s` for).
fn decode_multiline(body: &str, body_start: usize) -> Result<String, NmlError> {
    let bytes = body.as_bytes();
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut start = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            lines.push((start, &body[start..end]));
            start = i + 1;
        }
    }
    lines.push((start, &body[start..]));

    let is_blank = |l: &str| l.chars().all(char::is_whitespace);

    // NML0019 (the Swift/Java rule): content begins on a NEW line — the
    // opening line is the one remaining place raw text could steer
    // min-indent. Whitespace-only is harmless (blank lines are filtered
    // from the indent computation) and stays legal, as does `""""""`.
    if let Some((off, first)) = lines.first().copied() {
        if !is_blank(first) {
            let content = off + (first.len() - first.trim_start().len());
            return Err(NmlError::syntax(
                crate::error::ParseErrorKind::MultilineOpeningContent,
                Span::new(body_start + content, body_start + off + first.len()),
            ));
        }
    }

    let mut s = 0;
    let mut e = lines.len();
    if lines.first().is_some_and(|(_, l)| is_blank(l)) {
        s = 1;
    }
    if e > s && is_blank(lines[e - 1].1) {
        e -= 1;
    }
    let lines = &lines[s..e];
    if lines.is_empty() {
        return Ok(String::new());
    }

    let min_indent = lines
        .iter()
        .filter(|(_, l)| !is_blank(l))
        .map(|(_, l)| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0);

    let mut out = String::new();
    for (idx, (off, l)) in lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let stripped = if l.len() >= min_indent && l.chars().take(min_indent).all(|c| c == ' ') {
            min_indent
        } else {
            0
        };
        decode_escapes_into(&mut out, &l[stripped..], body_start + off + stripped)?;
    }
    Ok(out)
}

/// Parse a number literal. Integers without a decimal point are exact `i64`
/// (out-of-range is an error, never a silently rounded float), matching legacy.
fn parse_number(raw: &str, span: Span) -> Result<Number, NmlError> {
    if raw.contains('.') {
        raw.parse().map(Number::Float).map_err(|_| {
            NmlError::syntax(
                crate::error::ParseErrorKind::InvalidNumber {
                    raw: crate::error::echo_capture(raw),
                },
                span,
            )
        })
    } else {
        raw.parse().map(Number::Int).map_err(|_| {
            NmlError::syntax(
                crate::error::ParseErrorKind::NumberOutOfRange {
                    raw: crate::error::echo_capture(raw),
                },
                span,
            )
        })
    }
}

/// Validate a `$NS.key` reference: the namespace must be known and a key must
/// follow (relocated from the legacy lexer's `read_secret_ref`).
fn validate_secret(text: &str, span: Span) -> Result<(), NmlError> {
    let body = text.strip_prefix('$').unwrap_or(text);
    let (ns, key) = body.split_once('.').ok_or_else(|| {
        NmlError::syntax(
            crate::error::ParseErrorKind::BadSecretRef {
                reason: crate::error::SecretRefIssue::MissingDot,
            },
            span,
        )
    })?;
    if !KNOWN_NAMESPACES.contains(&ns) {
        return Err(NmlError::syntax(
            crate::error::ParseErrorKind::BadSecretRef {
                reason: crate::error::SecretRefIssue::UnknownNamespace(crate::error::echo_capture(
                    ns,
                )),
            },
            span,
        ));
    }
    if key.is_empty() {
        return Err(NmlError::syntax(
            crate::error::ParseErrorKind::BadSecretRef {
                reason: crate::error::SecretRefIssue::EmptyKey(crate::error::echo_capture(ns)),
            },
            span,
        ));
    }
    Ok(())
}

// ── node-reading helpers ──────────────────────────────────────────────────

/// Direct, non-trivia token children of a node, in order.
fn sig_tokens(node: &SyntaxNode) -> Vec<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
        .collect()
}

/// Child nodes that are themselves values (array elements / fallback arms).
fn value_children(node: &SyntaxNode) -> impl Iterator<Item = SyntaxNode> + '_ {
    node.children().filter(|n| {
        matches!(
            n.kind(),
            SyntaxKind::Value | SyntaxKind::ArrayValue | SyntaxKind::Fallback
        )
    })
}

// `content_span` now lives in `cst::syntax` (shared with the lowering and the value
// layer) since spans drive comment placement and template offsets, not just
// diagnostics.
