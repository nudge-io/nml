//! Value-decode layer (RFC 0004 P3).
//!
//! Interprets a *syntactic* `Value` / `ArrayValue` / `Fallback` CST node into a
//! *semantic* [`SpannedValue`] — the inverse of the parser's "syntax only"
//! discipline. All the interpretation the lexer/parser deliberately deferred
//! lives here: string-escape decoding, multiline resolution (the text-block
//! order — see [`decode_multiline`]), number range-checking, money and
//! duration parsing, `$ENV` namespace validation, `true`/`false`, and
//! template strings. Money, duration, and template parsing **reuse** the
//! existing `money`/`duration`/`template` modules.
//! This module is the single source of truth for value interpretation; its
//! behavior is spec-defined (spec/syntax.md: Source text, Strings) and
//! pinned by the `cst::tests` unit suite plus the fuzz batteries there.

use crate::cst::syntax::{
    SyntaxKind, SyntaxNode, SyntaxToken, content_span, node_span, text_offset,
};

use crate::error::NmlError;
use crate::span::Span;
use crate::types::{Number, SpannedValue, Value};
use crate::{money, template};

/// Decode a bare string-literal token (`"…"`) **totally** — used where the
/// grammar guarantees a string (oneof discriminator/arm values) so there is no
/// surrounding `Value` node to go through [`decode_value_all`]. Every bad
/// escape lands in `sink` and decoding recovers with U+FFFD, mirroring the
/// value decoders' rustc-style all-errors contract.
pub(super) fn decode_string_token_all(tok: &SyntaxToken, sink: &mut ValueErrors) -> String {
    decode_string(tok.text(), text_offset(tok.text_range().start()), sink)
}

/// Strict view of [`decode_string_token_all`] (first error by source
/// position), derived from the total decoder like [`decode_value`] is.
pub(super) fn decode_string_token(tok: &SyntaxToken) -> Result<String, NmlError> {
    let mut sink = ValueErrors::default();
    let s = decode_string_token_all(tok, &mut sink);
    match sink.errors.into_iter().min_by_key(|e| e.span().start) {
        Some(e) => Err(e),
        None => Ok(s),
    }
}

/// Variable-reference namespaces recognized after `$`.
pub(crate) const KNOWN_NAMESPACES: &[&str] = &["ENV"];

/// Bounded error sink for the total value decoders ([`decode_value_all`]).
/// Collection caps at the parse pipeline's error bound with an exact
/// suppressed count — the same contract the lexer and lowering apply — so
/// a pathological value can never amplify memory through its error list.
#[derive(Debug, Default)]
pub struct ValueErrors {
    pub errors: Vec<NmlError>,
    pub suppressed: usize,
}

impl ValueErrors {
    fn push(&mut self, e: NmlError) {
        if self.errors.len() < crate::cst::MAX_ERRORS {
            self.errors.push(e);
        } else {
            self.suppressed += 1;
        }
    }
}

/// Decode a value node (`Value`, `ArrayValue`, or `Fallback`) into a
/// [`SpannedValue`], **totally**: every semantic error is collected into
/// `sink` (all bad escapes in a string at once — rustc-style, not
/// whack-a-mole) and decoding continues with best-effort recovery (a bad
/// escape yields U+FFFD; a malformed literal yields a placeholder), so
/// lenient surfaces (the LSP) get a value *and* the full findings list.
/// Strict surfaces derive from this — see [`decode_value`]. Errored
/// documents still fail strict parsing, so no recovered value can reach a
/// deployment path.
pub fn decode_value_all(node: &SyntaxNode, sink: &mut ValueErrors) -> SpannedValue {
    match node.kind() {
        SyntaxKind::Value => decode_scalar(node, sink),
        SyntaxKind::ArrayValue => decode_array(node, sink),
        SyntaxKind::Fallback => decode_fallback(node, sink),
        other => {
            sink.push(NmlError::syntax(
                crate::error::ParseErrorKind::Expected {
                    expected: vec![crate::error::ExpectedItem::Desc("a value node")],
                    found: Some(crate::error::FoundToken {
                        kind: other,
                        text: String::new(),
                    }),
                    context: None,
                },
                node_span(node),
            ));
            SpannedValue::new(Value::String(String::new()), node_span(node))
        }
    }
}

/// The strict single-error view, derived from [`decode_value_all`] (the
/// `parse_to_ast` / `parse_to_ast_all` pattern): the error earliest **in
/// source position** — collection order is close to source order but not
/// identical (a misaligned closing delimiter is detected before the
/// per-line scan), so the position-minimum is taken explicitly.
pub fn decode_value(node: &SyntaxNode) -> Result<SpannedValue, NmlError> {
    let mut sink = ValueErrors::default();
    let v = decode_value_all(node, &mut sink);
    match sink.errors.into_iter().min_by_key(|e| e.span().start) {
        Some(e) => Err(e),
        None => Ok(v),
    }
}

fn decode_scalar(node: &SyntaxNode, sink: &mut ValueErrors) -> SpannedValue {
    let span = content_span(node);
    let toks = sig_tokens(node);
    // Decode is *semantic* validation; an empty value node is a syntactic failure
    // the parser already reported, so it yields a placeholder rather than a
    // (redundant) error.
    let Some(first) = toks.first() else {
        return SpannedValue::new(Value::String(String::new()), span);
    };

    // A malformed non-string literal recovers to the same empty placeholder
    // lowering has always substituted — the error, not the placeholder, is
    // the contract.
    let placeholder = |sink: &mut ValueErrors, e: NmlError| {
        sink.push(e);
        Value::String(String::new())
    };

    let value = match first.kind() {
        SyntaxKind::String => {
            // `span.start` is the string token's start (it is the only
            // significant token, so `content_span` begins there).
            let decoded = decode_string(first.text(), span.start, sink);
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
        SyntaxKind::Number => match number_or_money(&toks[..], span, false) {
            Ok(v) => v,
            Err(e) => placeholder(sink, e),
        },
        SyntaxKind::Dash => match number_or_money(&toks[..], span, true) {
            Ok(v) => v,
            Err(e) => placeholder(sink, e),
        },
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
            // A malformed reference still yields its text — better hover UX
            // than a vanished value, and the error carries the verdict.
            if let Err(e) = validate_secret(first.text(), span) {
                sink.push(e);
            }
            Value::Secret(first.text().to_string())
        }
        SyntaxKind::Ident => match first.text() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            other => Value::Reference(other.to_string()),
        },
        other => placeholder(
            sink,
            NmlError::syntax(
                crate::error::ParseErrorKind::Expected {
                    expected: vec![crate::error::ExpectedItem::Desc("a value")],
                    found: Some(crate::error::FoundToken {
                        kind: other,
                        text: String::new(),
                    }),
                    context: None,
                },
                span,
            ),
        ),
    };
    SpannedValue::new(value, span)
}

/// `Number (unit)?` or, when `negative`, `- Number (unit)?` — a bare
/// number, money, or a duration (RFC 0017). The parser consumed any
/// same-line trailing identifier as the unit *shape*; this is where it is
/// classified, totally and structurally: 3 uppercase letters → currency
/// (money's job to judge ISO validity), a duration suffix or anything
/// else → the duration decoder (which owns `NML3004` for unknown units).
/// Disjoint by construction — currency codes are uppercase, duration
/// units lowercase — so no lookahead is needed.
fn number_or_money(toks: &[SyntaxToken], span: Span, negative: bool) -> Result<Value, NmlError> {
    let num_idx = usize::from(negative);
    // Incomplete (`-` with no number) is a syntactic failure the parser reported;
    // yield a placeholder rather than a redundant decode error.
    let Some(number) = toks.get(num_idx).filter(|t| t.kind() == SyntaxKind::Number) else {
        return Ok(Value::Number(Number::ZERO));
    };
    let raw = if negative {
        format!("-{}", number.text())
    } else {
        number.text().to_string()
    };

    match toks.get(num_idx + 1) {
        Some(unit) if unit.kind() == SyntaxKind::Ident => {
            // RFC 0016 §1.8: the trailing-dot malformation is a
            // literal-layer rejection that fires BEFORE unit parsing —
            // anchored on the number token's REAL end. (`raw` is
            // reconstructed as `-` + digits, but the source may hold
            // trivia between the dash and the digits — `- 1299. JPY` —
            // so start+len arithmetic would mis-place the delete-dot fix
            // onto the last digit.)
            if raw.ends_with('.') {
                let num_end = text_offset(number.text_range().end());
                return Err(NmlError::syntax(
                    crate::error::ParseErrorKind::NumberTrailingDot {
                        raw: crate::error::echo_capture(&raw),
                    },
                    Span::new(span.start, num_end),
                ));
            }
            if money::is_currency_code(unit.text()) {
                return Ok(Value::Money(money::parse_money(&raw, unit.text(), span)?));
            }
            let unit_span = Span::new(
                text_offset(unit.text_range().start()),
                text_offset(unit.text_range().end()),
            );
            Ok(Value::Duration(crate::duration::parse_duration_literal(
                &raw,
                unit.text(),
                span,
                unit_span,
            )?))
        }
        _ => Ok(Value::Number(parse_number(&raw, span)?)),
    }
}

fn decode_array(node: &SyntaxNode, sink: &mut ValueErrors) -> SpannedValue {
    // Total per element: one bad item no longer hides its siblings' errors
    // (or their values).
    let items = value_children(node)
        .map(|c| decode_value_all(&c, sink))
        .collect::<Vec<_>>();
    SpannedValue::new(Value::Array(items), content_span(node))
}

/// `a | b | c` decodes right-associatively to `Fallback(a, Fallback(b, c))`
/// — the resolver tries left-to-right, so the chain nests rightward.
fn decode_fallback(node: &SyntaxNode, sink: &mut ValueErrors) -> SpannedValue {
    let mut values = value_children(node)
        .map(|c| decode_value_all(&c, sink))
        .collect::<Vec<_>>()
        .into_iter()
        .rev();
    let Some(mut acc) = values.next() else {
        sink.push(NmlError::syntax(
            crate::error::ParseErrorKind::Expected {
                expected: vec![crate::error::ExpectedItem::Desc("a fallback arm")],
                found: Some(crate::error::FoundToken {
                    kind: SyntaxKind::Fallback,
                    text: String::new(),
                }),
                context: None,
            },
            node_span(node),
        ));
        return SpannedValue::new(Value::String(String::new()), node_span(node));
    };
    for v in values {
        let span = v.span.merge(acc.span);
        acc = SpannedValue::new(Value::Fallback(Box::new(v), Box::new(acc)), span);
    }
    acc
}

// ── scalar decoders (spec: Strings; pinned by cst::tests + the fuzz batteries) ──

/// Decode a string token's raw text (`"…"` or `"""…"""`) into its value: strip
/// the delimiters, process escapes, and (triple-quoted) resolve the body in
/// the text-block order — see [`decode_multiline`]. `tok_start` is the
/// token's source offset, so escape errors get char-precise spans.
fn decode_string(raw: &str, tok_start: usize, sink: &mut ValueErrors) -> String {
    if let Some(body) = raw.strip_prefix("\"\"\"") {
        let body = body.strip_suffix("\"\"\"").unwrap_or(body);
        decode_multiline(body, tok_start + 3, sink)
    } else if let Some(body) = raw.strip_prefix('"') {
        let body = body.strip_suffix('"').unwrap_or(body);
        let mut out = String::with_capacity(body.len());
        decode_escapes_into(&mut out, body, tok_start + 1, sink);
        out
    } else {
        let mut out = String::with_capacity(raw.len());
        decode_escapes_into(&mut out, raw, tok_start, sink);
        out
    }
}

/// Decode `\" \\ \n \t \r \s \u{…}` into `out`, TOTALLY: every malformed
/// escape is collected (rustc-style — all at once, never whack-a-mole) and
/// recovery substitutes U+FFFD so decoding continues. `inner_start` is the
/// source offset of `inner`'s first byte, so errors point at the exact
/// escape. Content only, by construction: single-line tokens cannot span
/// lines and the multiline splitter owns line terminators, so none reach
/// this loop.
fn decode_escapes_into(out: &mut String, inner: &str, inner_start: usize, sink: &mut ValueErrors) {
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
            // Protected space (Java's `\s`): survives multiline dedent and
            // editor trim — the formatter emits it for edge spaces.
            Some((_, 's')) => out.push(' '),
            Some((_, 'u')) => match decode_unicode_escape(&mut chars, inner, inner_start, i) {
                Ok(ch) => out.push(ch),
                Err(e) => {
                    sink.push(e);
                    out.push('\u{FFFD}');
                }
            },
            Some((j, other)) => {
                // Span the `\` (at `i`) through the escape char (ending at `j + len`).
                let span = Span::new(inner_start + i, inner_start + j + other.len_utf8());
                sink.push(NmlError::syntax(
                    crate::error::ParseErrorKind::InvalidEscape {
                        escape: Some(other),
                    },
                    span,
                ));
                out.push('\u{FFFD}');
            }
            None => {
                let span = Span::new(inner_start + i, inner_start + inner.len());
                sink.push(NmlError::syntax(
                    crate::error::ParseErrorKind::InvalidEscape { escape: None },
                    span,
                ));
                out.push('\u{FFFD}');
            }
        }
    }
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
/// ("protected space", `\s`). Shape rules enforced here, all with
/// recovery: content on the opening line (NML0019 — the line joins the
/// value but is excluded from indent arithmetic), tabs in body indentation
/// (NML0005's principle, extended), a misaligned own-line closing
/// delimiter (NML0020, machine-fixable — moving it is provably
/// value-preserving), and `\` before a line break as Java/Swift line
/// continuation (odd trailing-backslash run; dangling on the last line is
/// the string-ends-mid-escape error).
fn decode_multiline(body: &str, body_start: usize, sink: &mut ValueErrors) -> String {
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
    let space_indent = |l: &str| l.chars().take_while(|c| *c == ' ').count();

    // NML0019: content begins on a NEW line. Recovery keeps the author's
    // text: the opening line joins the value as content (leading padding
    // dropped — it sits after the quotes, not at a column) but is excluded
    // from indent arithmetic, so it cannot steer dedent.
    let mut work: Vec<(usize, &str)> = Vec::new();
    if let Some((off, first)) = lines.first().copied() {
        if !is_blank(first) {
            let at = off + (first.len() - first.trim_start().len());
            sink.push(NmlError::syntax(
                crate::error::ParseErrorKind::MultilineOpeningContent,
                Span::new(body_start + at, body_start + off + first.len()),
            ));
            work.push((at, &first[at - off..]));
        }
    }

    // Edge trim on the remaining raw lines (line 0 is consumed above either
    // way); remember a trimmed own-line closing for the alignment check.
    let mut e = lines.len();
    let mut closing: Option<(usize, &str)> = None;
    if e > 1 && is_blank(lines[e - 1].1) {
        closing = Some(lines[e - 1]);
        e -= 1;
    }
    let retained = &lines[1..e.max(1)];

    let min_indent = retained
        .iter()
        .filter(|(_, l)| !is_blank(l))
        .map(|(_, l)| space_indent(l))
        .min()
        .unwrap_or(0);

    // NML0020: an own-line closing delimiter must align with the content's
    // indent — the only geometry where the delimiter-anchored reading and
    // the min-indent reading could disagree, closed by making them agree.
    // Machine-fixable: rewriting the closing line's indent is provably
    // value-preserving (the line is edge-trimmed either way).
    if let Some((c_off, c_line)) = closing {
        let found = space_indent(c_line);
        if !retained.is_empty() && found != min_indent {
            sink.push(NmlError::syntax(
                crate::error::ParseErrorKind::MultilineClosingMisaligned {
                    expected: min_indent,
                    found,
                },
                Span::new(body_start + c_off, body_start + c_off + c_line.len()),
            ));
        }
    }

    for (off, l) in retained {
        // Tabs in body indentation: NML0005's principle — column arithmetic
        // must not depend on editor settings — applies to dedent too.
        // Blank lines are exempt, as in layout.
        if !is_blank(l) {
            let ws = l.len() - l.trim_start().len();
            if l[..ws].contains('\t') {
                sink.push(NmlError::syntax(
                    crate::error::ParseErrorKind::TabInIndent,
                    Span::new(body_start + off, body_start + off + ws),
                ));
            }
        }
        let stripped = if l.len() >= min_indent && l.chars().take(min_indent).all(|c| c == ' ') {
            min_indent
        } else {
            0
        };
        work.push((off + stripped, &l[stripped..]));
    }

    // Emit with line-continuation joins: an odd trailing-backslash run is
    // an unpaired `\` before the line break — Java/Swift continuation. The
    // parity rule IS escape pairing, so `\\` stays a literal backslash.
    let mut out = String::new();
    let mut join_pending = false;
    let mut dangling: Option<Span> = None;
    for (i, (abs, slice)) in work.iter().enumerate() {
        if i > 0 && join_pending {
            out.push('\n');
        }
        let trailing = slice.bytes().rev().take_while(|b| *b == b'\\').count();
        let (slice, continued) = if trailing % 2 == 1 {
            (&slice[..slice.len() - 1], true)
        } else {
            (*slice, false)
        };
        join_pending = !continued;
        dangling = continued.then(|| {
            Span::new(
                body_start + abs + slice.len(),
                body_start + abs + slice.len() + 1,
            )
        });
        decode_escapes_into(&mut out, slice, body_start + abs, sink);
    }
    if let Some(span) = dangling {
        // A continuation with nothing to join: the string ends mid-escape.
        sink.push(NmlError::syntax(
            crate::error::ParseErrorKind::InvalidEscape { escape: None },
            span,
        ));
    }
    out
}

/// Parse a number literal: a thin adapter over the shared exact-decimal
/// core (RFC 0016 §1.6) — the only place [`crate::decimal::NumberError`]
/// gains a span and an NML code. Acceptance is value-based: any finite
/// decimal128 value parses exactly; inexact or out-of-range is an error,
/// never a silently rounded float.
pub(super) fn parse_number(raw: &str, span: Span) -> Result<Number, NmlError> {
    Number::parse_literal(raw).map_err(|e| {
        let kind = match e {
            crate::decimal::NumberError::Malformed => crate::error::ParseErrorKind::InvalidNumber {
                raw: crate::error::echo_capture(raw),
            },
            crate::decimal::NumberError::TrailingDot => {
                crate::error::ParseErrorKind::NumberTrailingDot {
                    raw: crate::error::echo_capture(raw),
                }
            }
            // Exhaustive in-crate on purpose: a new core variant must pick
            // its code here before it can ship. `issue` here is always one
            // of the three parse-reachable kinds (TooManyDigits/TooLarge/
            // TooSmall) — the error-index NML0014 entry enumerates exactly
            // those; the try_new-only kinds (CoefficientTooWide/
            // ScaleOutOfRange/NegativeScaleZero) never flow through a parse.
            crate::decimal::NumberError::Range(issue) => {
                crate::error::ParseErrorKind::NumberOutOfRange { issue }
            }
        };
        NmlError::syntax(kind, span)
    })
}

/// Validate a `$NS.key` reference: the namespace must be known and a key must
/// follow (relocated from the legacy lexer's `read_secret_ref`).
pub(super) fn validate_secret(text: &str, span: Span) -> Result<(), NmlError> {
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
