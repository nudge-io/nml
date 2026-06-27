//! Value-decode layer (RFC 0004 P3).
//!
//! Interprets a *syntactic* `Value` / `ArrayValue` / `Fallback` CST node into a
//! *semantic* [`SpannedValue`] — the inverse of the parser's "syntax only"
//! discipline. All the interpretation the lexer/parser deliberately deferred
//! lives here: string-escape decoding, multiline dedent, number range-checking,
//! money parsing, `$ENV` namespace validation, `true`/`false`, and template
//! strings. Money and template parsing **reuse** the existing `money`/`template`
//! modules; the escape/dedent/number logic mirrors the legacy lexer exactly and
//! is pinned to it by a differential test (`cst::tests`), so capability is
//! preserved, not reinvented. (At P7 the legacy lexer's copies are deleted,
//! leaving this the single source of truth.)

use crate::cst::syntax::{content_span, node_span, text_offset, SyntaxKind, SyntaxNode, SyntaxToken};

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

/// Variable-reference namespaces recognized after `$` (mirrors the legacy lexer).
const KNOWN_NAMESPACES: &[&str] = &["ENV"];

/// Decode a value node (`Value`, `ArrayValue`, or `Fallback`) into a
/// [`SpannedValue`]. Returns the first semantic error encountered.
pub fn decode_value(node: &SyntaxNode) -> Result<SpannedValue, NmlError> {
    match node.kind() {
        SyntaxKind::Value => decode_scalar(node),
        SyntaxKind::ArrayValue => decode_array(node),
        SyntaxKind::Fallback => decode_fallback(node),
        other => Err(NmlError::parse(
            format!("expected a value node, found {other:?}"),
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
            if decoded.contains("{{") {
                Value::TemplateString(template::parse_template_string(&decoded, span.start))
            } else {
                Value::String(decoded)
            }
        }
        SyntaxKind::Number => number_or_money(&toks[..], span, false)?,
        SyntaxKind::Dash => number_or_money(&toks[..], span, true)?,
        SyntaxKind::Role => Value::Role(first.text().to_string()),
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
            return Err(NmlError::parse(
                format!("cannot decode value starting with {other:?}"),
                span,
            ))
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

/// `a | b | c` decodes right-associatively to `Fallback(a, Fallback(b, c))`,
/// matching the legacy recursive `parse_value_or_fallback`.
fn decode_fallback(node: &SyntaxNode) -> Result<SpannedValue, NmlError> {
    let mut values = value_children(node)
        .map(|c| decode_value(&c))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .rev();
    let mut acc = values
        .next()
        .ok_or_else(|| NmlError::parse("empty fallback chain", node_span(node)))?;
    for v in values {
        let span = v.span.merge(acc.span);
        acc = SpannedValue::new(Value::Fallback(Box::new(v), Box::new(acc)), span);
    }
    Ok(acc)
}

// ── scalar decoders (mirror the legacy lexer; pinned by a differential test) ──

/// Decode a string token's raw text (`"…"` or `"""…"""`) into its value: strip
/// the delimiters, process escapes, and dedent triple-quoted bodies. `tok_start`
/// is the token's source offset, so escape errors get a span covering exactly
/// the offending `\x` (matching the legacy lexer's precision).
fn decode_string(raw: &str, tok_start: usize) -> Result<String, NmlError> {
    if let Some(body) = raw.strip_prefix("\"\"\"") {
        let body = body.strip_suffix("\"\"\"").unwrap_or(body);
        Ok(dedent_multiline(&decode_escapes(body, tok_start + 3)?))
    } else if let Some(body) = raw.strip_prefix('"') {
        let body = body.strip_suffix('"').unwrap_or(body);
        decode_escapes(body, tok_start + 1)
    } else {
        decode_escapes(raw, tok_start)
    }
}

/// Decode `\" \\ \n \t`; `inner_start` is the source offset of `inner`'s first
/// byte, used to point errors at the exact escape.
fn decode_escapes(inner: &str, inner_start: usize) -> Result<String, NmlError> {
    let mut out = String::with_capacity(inner.len());
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
            Some((j, other)) => {
                // Span the `\` (at `i`) through the escape char (ending at `j + len`).
                let span = Span::new(inner_start + i, inner_start + j + other.len_utf8());
                return Err(NmlError::parse(
                    format!("unknown escape sequence: '\\{other}'"),
                    span,
                ));
            }
            None => {
                let span = Span::new(inner_start + i, inner_start + inner.len());
                return Err(NmlError::parse("unexpected end of string", span));
            }
        }
    }
    Ok(out)
}

/// Strip the common leading-space indent from a triple-quoted body and trim the
/// blank first/last lines (mirrors the legacy `dedent_multiline_string`).
fn dedent_multiline(raw: &str) -> String {
    let mut lines: Vec<&str> = raw.split('\n').collect();
    if lines.first().is_some_and(|l| l.chars().all(char::is_whitespace)) {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.chars().all(char::is_whitespace)) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    let min_indent = lines
        .iter()
        .filter(|l| !l.chars().all(char::is_whitespace))
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.len() >= min_indent && l.chars().take(min_indent).all(|c| c == ' ') {
                &l[min_indent..]
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a number literal. Integers without a decimal point are exact `i64`
/// (out-of-range is an error, never a silently rounded float), matching legacy.
fn parse_number(raw: &str, span: Span) -> Result<Number, NmlError> {
    if raw.contains('.') {
        raw.parse()
            .map(Number::Float)
            .map_err(|_| NmlError::parse(format!("invalid number: \"{raw}\""), span))
    } else {
        raw.parse().map(Number::Int).map_err(|_| {
            NmlError::parse(format!("integer \"{raw}\" out of range for 64-bit integer"), span)
        })
    }
}

/// Validate a `$NS.key` reference: the namespace must be known and a key must
/// follow (relocated from the legacy lexer's `read_secret_ref`).
fn validate_secret(text: &str, span: Span) -> Result<(), NmlError> {
    let body = text.strip_prefix('$').unwrap_or(text);
    let (ns, key) = body.split_once('.').ok_or_else(|| {
        NmlError::parse("expected '.' after the variable namespace (e.g. $ENV.MY_VAR)", span)
    })?;
    if !KNOWN_NAMESPACES.contains(&ns) {
        return Err(NmlError::parse(
            format!(
                "unknown variable source '{ns}'. Valid sources: {}",
                KNOWN_NAMESPACES.join(", ")
            ),
            span,
        ));
    }
    if key.is_empty() {
        return Err(NmlError::parse(
            format!("expected a variable name after ${ns}."),
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
