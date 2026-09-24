//! RFC 0004 — Lossless CST: the **production parser** for NML.
//!
//! A `rowan` red/green tree over the full grammar. It is the single parse entry
//! point; semantic consumers read the `ast` it lowers to (see `lower`), while
//! tooling reads the tree directly for spans/trivia/comments. Four guarantees,
//! upheld across the whole grammar and exercised by the fuzz harness:
//!
//! 1. **Losslessness** — the tree's text is byte-identical to the source on *any*
//!    input (every byte lands in a token, including trivia and `ErrorToken`).
//! 2. **Resilience / all-errors** — a syntax error never aborts the parse; the
//!    tree is always produced and every error is collected in one pass.
//! 3. **Offside correctness** — indentation drives structure via zero-width
//!    `Indent`/`Dedent` tokens, and layout is **suppressed inside `"""…"""`**.
//! 4. **Termination / bounded output** — recovery always makes forward progress
//!    and the error list is capped, so adversarial input is safe (RFC 0004 §9).
//!
//! Public surface: `parse` (→ lossless `Parse`), `parse_to_ast` /
//! `parse_to_ast_all` (→ semantic `ast`), `parse_checked` (→ the checked
//! lossless tree, the formatter's door),
//! `extract_schema`, the `ast` / `extract` layers, and `edit`
//! (structural green-tree splicing, RFC 0030 P2). The tree → AST lowering
//! (`lower`) is crate-private: every AST a consumer receives comes through
//! `parse_lowered`, so the rules emitted beside it (the name rules, the
//! source-character policy) are total by visibility, not by the absence
//! of callers (RFC 0026 B-15) — a public door would compile:
//!
//! ```compile_fail,E0603
//! use nml_core::cst::lower::to_ast_with_errors;
//! ```

pub mod ast;
pub mod duration_query;
pub mod edit;
pub mod extract;
mod lexer;
pub(crate) mod lower;
mod parser;
mod syntax;
mod value;

pub use duration_query::{DurationLiteralAt, duration_literal_at, duration_literals_in};
pub use syntax::{NmlLanguage, SyntaxKind, SyntaxNode, SyntaxToken};
pub(crate) use value::KNOWN_NAMESPACES;
pub use value::{ValueErrors, decode_value, decode_value_all, string_content_window};

/// The canonical indentation unit of NML source — `spec/syntax.md`: "The
/// canonical indentation unit is **4 spaces**". The ONE spelling: what the
/// formatter writes at every level, the level width a canonical-form
/// snippet nests by, and the unit a minted line falls back to when the
/// document it lands in offers none ([`edit::indentation_unit_at`]). Tabs
/// are not indentation (the lexer's `TabInIndent` finding), so the unit
/// is spaces.
pub const INDENT_UNIT: &str = "    ";

use crate::error::NmlError;
use rowan::GreenNode;

/// The result of parsing: always a (best-effort, lossless) tree, plus every
/// error (RFC 0004 §4.3).
pub struct Parse {
    green: GreenNode,
    errors: Vec<NmlError>,
    suppressed: usize,
}

/// Where a parse's significant tokens begin and end ([`Parse::token_boundaries`]).
#[derive(Debug, Clone, Default)]
pub struct TokenBoundaries {
    starts: std::collections::BTreeSet<usize>,
    ends: std::collections::BTreeSet<usize>,
}

impl TokenBoundaries {
    /// Whether `span` begins at a significant token's first byte and ends at
    /// one's last byte. An empty span is a position and aligns anywhere.
    pub fn aligns(&self, span: crate::span::Span) -> bool {
        span.start == span.end
            || (self.starts.contains(&span.start) && self.ends.contains(&span.end))
    }

    /// The span-site invariant, judged by the site's SHAPE
    /// ([`crate::span::SpanShape`]) and never by its name: `Ok(())`, or the
    /// reason it fails. The ONE implementation every reader shares — the
    /// corpus pin, the `document` fuzz target and the alignment pin in this
    /// file — so a carrier cannot be held to one rule here and another
    /// there. The reason is `&'static str` because a fuzz target runs this
    /// on every input and must allocate nothing when it holds.
    ///
    /// A content window gets the STRONGER rule, not the weaker one: on
    /// character boundaries (an applier splices it, and an unterminated
    /// literal ending in a multi-byte character used to cut one in half —
    /// RFC 0026 decision 2) AND strictly inside the token that carries it,
    /// because a window equal to the whole token satisfies the byte rule
    /// and still corrupts the fix.
    pub fn check(&self, site: crate::span::SpanSite, source: &str) -> Result<(), &'static str> {
        use crate::span::SpanShape;
        let span = site.span;
        if span.start > span.end || span.end > source.len() {
            return Err("is out of bounds");
        }
        match site.shape {
            SpanShape::Aligned => {
                if self.aligns(span) {
                    Ok(())
                } else {
                    Err("is not token-aligned")
                }
            }
            SpanShape::ContentWindow { token } => {
                if !self.aligns(token) {
                    Err("sits in a token that is not itself token-aligned")
                } else if !(token.start < span.start && span.end <= token.end) {
                    Err("is not strictly inside the token that carries it")
                } else if !source.is_char_boundary(span.start) || !source.is_char_boundary(span.end)
                {
                    Err("is not a character-boundary window")
                } else {
                    Ok(())
                }
            }
            SpanShape::TemplateExpression { token } => {
                if !self.aligns(token) {
                    Err("sits in a token that is not itself token-aligned")
                } else if !(token.start <= span.start && span.end <= token.end) {
                    Err("is not inside the token that carries it")
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl Parse {
    /// The root of the typed syntax tree.
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    /// The byte boundaries of this parse's significant tokens — where every
    /// span an AST or schema node carries begins and ends. The content-span
    /// invariant in its exact form, for EVERY input: a non-empty span is
    /// token-aligned (the byte-level [`crate::span::Span::is_content_in`]
    /// is its proxy for terminated documents — an unterminated string token
    /// runs to the end of the file, line breaks included, and a span ending
    /// on it ends on that token's last byte all the same).
    pub fn token_boundaries(&self) -> TokenBoundaries {
        let mut out = TokenBoundaries::default();
        for tok in self
            .syntax()
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind().is_significant())
        {
            let r = tok.text_range();
            out.starts.insert(syntax::text_offset(r.start()));
            out.ends.insert(syntax::text_offset(r.end()));
        }
        out
    }

    /// Every diagnostic collected during lexing and parsing.
    /// Errors dropped past the `MAX_ERRORS` bound (RFC 0009 exact-count
    /// honesty): `0` means [`Self::errors`] is the complete list.
    pub fn suppressed(&self) -> usize {
        self.suppressed
    }

    pub fn errors(&self) -> &[NmlError] {
        &self.errors
    }

    /// Strict callers: the full tree, or **every** error (RFC 0004 §4.3 — the
    /// all-errors contract reaches strict loads, not just the LSP).
    pub fn ok(self) -> Result<SyntaxNode, Vec<NmlError>> {
        if self.errors.is_empty() {
            Ok(SyntaxNode::new_root(self.green))
        } else {
            Err(self.errors)
        }
    }
}

use crate::diagnostic::MAX_ERRORS;
thread_local! {
    /// Parses begun on this thread — see [`parses_on_this_thread`].
    static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times [`parse`] has run on the calling thread. The
/// structural seam behind the one-parse-per-target pins: `nml check`,
/// `validate` and `fix` and the editor's diagnostics each derive every
/// view of a target (AST, extracted schema, findings) from ONE parse, and
/// their pins read this counter across the verb instead of timing it.
/// Thread-local so parallel test threads never see each other's parses;
/// a plain counter, never an environment knob.
pub fn parses_on_this_thread() -> usize {
    PARSES.with(|c| c.get())
}

/// Parse NML source into a lossless CST. Never fails, never panics: a
/// source past the token stream's 4 GiB bound yields an empty tree and
/// one typed [`ParseErrorKind::SourceTooLarge`](crate::error::ParseErrorKind::SourceTooLarge)
/// error, exactly as any other lexical finding.
pub fn parse(source: &str) -> Parse {
    parse_bounded(source, lexer::MAX_SOURCE_LEN)
}

/// [`parse`] under an explicit source ceiling — the seam the bound's pin
/// fakes; `parse` passes the real one.
fn parse_bounded(source: &str, max_len: usize) -> Parse {
    PARSES.with(|c| c.set(c.get() + 1));
    let lexed = lexer::lex_bounded(source, max_len);
    let mut p = parser::Parser::new(lexed.src, &lexed.tokens);
    p.parse_root();
    let (events, parse_errors, parser_suppressed) = p.finish_parse();
    let green = parser::build_tree(lexed.src, &lexed.tokens, &events);

    let mut errors = lexed.errors;
    errors.extend(parse_errors);
    // Exact suppression accounting (RFC 0009): layer drops + merge clip —
    // truncation is bounded output, never silent loss.
    let mut suppressed = lexed.suppressed + parser_suppressed;
    if errors.len() > MAX_ERRORS {
        suppressed += errors.len() - MAX_ERRORS;
        errors.truncate(MAX_ERRORS);
    }

    Parse {
        green,
        errors,
        suppressed,
    }
}

/// Parse to the **semantic AST**, collecting **every** diagnostic — syntactic
/// *and* semantic — in source-position order. The AST is always returned
/// (best-effort, with placeholders for un-decodable values); an **empty error
/// list means the input is fully valid**.
///
/// This is the all-errors form: value validation (escapes, money precision,
/// `$ENV` namespaces, number range) is deferred to decode, so a single
/// lowering pass (crate-private, reached only through this module's parse
/// funnel) collects those, merged with the syntactic errors. Use this to
/// report every problem at once; [`parse_to_ast`] is the
/// single-error drop-in derived from it.
pub fn parse_to_ast_all(source: &str) -> (crate::ast::File, Vec<crate::diagnostic::Diagnostic>) {
    let (_parsed, file, errors, suppressed) = parse_lowered(source);
    (file, finalize_diagnostics(errors, suppressed))
}

/// The truncation marker's row shape, shared by the composer
/// ([`finalize_diagnostics`]) and the recognizer ([`suppressed_count`])
/// so the rendered row and the read-back can never drift: the exact
/// suppressed count, then this tail, then the cap and a closing paren.
const SUPPRESSED_ROW_TAIL: &str = " additional error(s) suppressed (limit ";

/// The one truncation-marker row [`finalize_diagnostics`] appends.
fn suppressed_row(suppressed: usize) -> String {
    format!("{suppressed}{SUPPRESSED_ROW_TAIL}{MAX_ERRORS})")
}

/// The exact count a diagnostics list reports as suppressed (RFC 0009's
/// truncation honesty, read back): recognizes the marker row
/// `finalize_diagnostics` appends — `Severity::Info`, no code, the
/// shared row shape anchored at BOTH ends (the suffix stripped, the
/// whole remaining prefix parsed as the count) — and returns its N;
/// `0` with no marker present. Consumers that budget for findings
/// hidden past the cap (the `nml fix` convergence gate) read the count
/// through this, never by re-deriving the format.
pub fn suppressed_count(diags: &[crate::diagnostic::Diagnostic]) -> usize {
    let tail = format!("{SUPPRESSED_ROW_TAIL}{MAX_ERRORS})");
    diags
        .iter()
        .filter(|d| d.severity == crate::diagnostic::Severity::Info && d.code.is_none())
        .filter_map(|d| d.message.strip_suffix(&tail[..]))
        .filter_map(|prefix| prefix.parse::<usize>().ok())
        .sum()
}

/// The ONE findings boundary (RFC 0009): bounded output (RFC 0004 §9) with
/// exact-count honesty — when anything was clipped, the last entry says how
/// much. Shared by every `Vec<Diagnostic>`-returning surface, so the CLI
/// and the LSP inherit identical truncation behavior.
fn finalize_diagnostics(
    mut errors: Vec<NmlError>,
    mut suppressed: usize,
) -> Vec<crate::diagnostic::Diagnostic> {
    if errors.len() > MAX_ERRORS {
        suppressed += errors.len() - MAX_ERRORS;
        errors.truncate(MAX_ERRORS);
    }
    // The public reporting boundary speaks the unified findings model
    // (RFC 0008); NmlError stays the internal/abort currency.
    let mut out: Vec<crate::diagnostic::Diagnostic> =
        errors.iter().map(NmlError::to_diagnostic).collect();
    if suppressed > 0 {
        out.push(crate::diagnostic::Diagnostic::info(suppressed_row(
            suppressed,
        )));
    }
    out
}

/// Shared core: parse to the CST, lower to the semantic AST, judge the name
/// rules over it, and merge the syntactic + semantic errors into one
/// position-sorted list. Returns the [`Parse`] too (callers needing the
/// tree, e.g. for comments). The single home for the parse → AST +
/// diagnostics pipeline — every AST-producing entry point derives from it,
/// so a rule emitted here is total over every text a consumer parses.
fn parse_lowered(source: &str) -> (Parse, crate::ast::File, Vec<NmlError>, usize) {
    use ast::AstNode as _;
    let parsed = parse(source);
    let root = ast::Root::cast(parsed.syntax()).expect("parse always yields a Root node");
    let (file, mut errors, lower_suppressed) = lower::to_ast_with_errors(&root);
    errors.extend(parsed.errors().iter().cloned());
    // The name rules (NML1000 at the file scope, NML2093 in every body)
    // run beside every parse, so no consumer can skip them: a text that
    // declares a name twice does not parse — `parse_to_ast` refuses it as
    // it refuses a stray token, and the all-findings forms report it
    // located beside the syntax findings. The tree keeps both
    // occurrences (best-effort structure for tooling; the merge and the
    // validator judge each entry as authored).
    let (name_errors, names_suppressed) = crate::entry_names::check(&file);
    errors.extend(name_errors);
    // The source-character policy (spec: Source text) runs beside every
    // parse, so no consumer can forget it. Its teaching diagnostic
    // supersedes the lexer's generic `UnexpectedCharacter` at the same
    // span — and ONLY where one actually exists: the policy list is
    // bounded, so past its cap the generic error survives (per-site
    // coverage is never silently narrowed).
    let (policy_errors, policy_suppressed) = crate::source_policy::check(source);
    // A bare CR INSIDE a string literal is content, not transport: its
    // machine fix is the `\r` escape, never a deletion (which would
    // change the value). The policy scan is context-free; the lexed
    // string tokens supply the context — but only where the string
    // reading is CERTAIN: an UNTERMINATED string's token swallows to
    // EOF, and trusting it flipped every following transport CR to
    // "content", whose machine fix then rewrote line endings into
    // literal `\r` text — gluing an old-Mac file's line structure into
    // a string value (probed). Tokens named by an unterminated-string
    // error are excluded.
    // Cap note: this reads the CAPPED parse-error list, so a ≥128-error
    // flood before a stray quote can suppress the UnterminatedString
    // entry — the glue vector stays closed anyway because
    // finalize_diagnostics truncates the position-sorted merge at the
    // SAME constant, and lexer errors never occur inside a string
    // token's range, so the evictors always sort first and any
    // guard-blind in-string fix is truncated before it can render
    // (pinned by `a_flooded_stray_quote_still_grants_no_in_string_fix`).
    let unterminated: Vec<usize> = errors
        .iter()
        .filter_map(|e| match e {
            NmlError::Syntax {
                kind: crate::error::ParseErrorKind::UnterminatedString { open, .. },
                ..
            } => Some(open.start),
            _ => None,
        })
        .collect();
    let strings: Vec<(usize, usize, syntax::SyntaxToken)> = parsed
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == syntax::SyntaxKind::String)
        .filter(|t| !unterminated.contains(&usize::from(t.text_range().start())))
        .map(|t| {
            let r = t.text_range();
            (usize::from(r.start()), usize::from(r.end()), t)
        })
        .collect();
    let mut judges: std::collections::HashMap<usize, value::RepairJudge> =
        std::collections::HashMap::new();
    let policy_errors: Vec<NmlError> = policy_errors
        .into_iter()
        .map(|e| {
            use crate::error::ParseErrorKind::*;
            match e {
                NmlError::Syntax { kind, span } => {
                    // The in-string reading is granted per REPAIR
                    // SOUNDNESS, not per position: the character must sit
                    // inside a CERTAIN string token (the A2 guard above
                    // excluded unterminated ones) AND splicing the kind's
                    // own escape (`ParseErrorKind::in_string_escape` —
                    // the SAME string the repair inserts) must leave the
                    // decoded value byte-identical (`value::RepairJudge`
                    // — decode itself judges, so blank edge lines,
                    // min-indent-bearing blank lines, dropped opening
                    // padding, and backslash gluing all refuse without
                    // being named here). Refused characters keep the
                    // token-position reading: the diagnostic stands,
                    // with no repair.
                    // The containing-token find is hoisted so BOTH
                    // judgments — the escape keep-judgment and, for
                    // invisibles only, the remove judgment (D-C) —
                    // share one `judges` lookup.
                    let escape = kind.in_string_escape();
                    let judge = match &escape {
                        Some(_) => strings
                            .iter()
                            .find(|&&(s, e, _)| s < span.start && span.end <= e)
                            .map(|(s, _, tok)| {
                                &*judges
                                    .entry(*s)
                                    .or_insert_with(|| value::RepairJudge::new(tok.text(), *s))
                            }),
                        None => None,
                    };
                    let sound = match (&escape, &judge) {
                        (Some(escape), Some(judge)) => judge.escape_preserves_value(span, escape),
                        _ => false,
                    };
                    let kind = match kind {
                        BareCarriageReturn { .. } if sound => {
                            BareCarriageReturn { in_string: true }
                        }
                        ForbiddenControlCharacter { ch, .. } if sound => {
                            ForbiddenControlCharacter {
                                ch,
                                in_string: true,
                            }
                        }
                        InvisibleCharacter { ch, .. } if sound => InvisibleCharacter {
                            ch,
                            in_string: true,
                            // Judged ONLY where the keep-judgment
                            // passed: an unsound escape already keeps
                            // the whole token-position reading, and the
                            // remove grant is meaningless without a
                            // sound keep arm beside it.
                            remove_sound: judge
                                .expect("a sound judgment implies a judge")
                                .remove_preserves_value(span),
                        },
                        other => other,
                    };
                    NmlError::Syntax { kind, span }
                }
                other => other,
            }
        })
        .collect();
    let policy_spans: std::collections::HashSet<(usize, usize)> = policy_errors
        .iter()
        .map(|e| (e.span().start, e.span().end))
        .collect();
    errors.retain(|e| match e {
        NmlError::Syntax {
            kind: crate::error::ParseErrorKind::UnexpectedCharacter { .. },
            span,
        } => !policy_spans.contains(&(span.start, span.end)),
        _ => true,
    });
    errors.extend(policy_errors);
    errors.sort_by_key(|e| e.span().start);
    coalesce_expected(&mut errors);
    let suppressed = parsed.suppressed + lower_suppressed + policy_suppressed + names_suppressed;
    (parsed, file, errors, suppressed)
}

/// Merge consecutive same-offset `Expected` diagnostics by unioning their
/// alternatives — recovery cascades otherwise report "expected a name" AND
/// "expected a declaration" at one position, where rustc renders a single
/// "expected one of …". Information-lossless: only `Expected` kinds merge
/// (a co-located lexer error is never suppressed), the first entry's
/// found/context win, and the list is already position-sorted (stable), so
/// adjacency is guaranteed.
fn coalesce_expected(errors: &mut Vec<NmlError>) {
    use crate::error::ParseErrorKind::Expected;
    let mut i = 0;
    while i + 1 < errors.len() {
        let same_start = errors[i].span().start == errors[i + 1].span().start;
        let both_expected = matches!(
            &errors[i],
            NmlError::Syntax {
                kind: Expected { .. },
                ..
            }
        ) && matches!(
            &errors[i + 1],
            NmlError::Syntax {
                kind: Expected { .. },
                ..
            }
        );
        if same_start && both_expected {
            let NmlError::Syntax {
                kind: Expected { expected: add, .. },
                ..
            } = errors.remove(i + 1)
            else {
                unreachable!("matched Expected above");
            };
            let NmlError::Syntax {
                kind: Expected { expected, .. },
                ..
            } = &mut errors[i]
            else {
                unreachable!("matched Expected above");
            };
            for item in add {
                if !expected.contains(&item) {
                    expected.push(item);
                }
            }
        } else {
            i += 1;
        }
    }
}

/// Parse to the owned AST, returning the **first** error by source position.
/// This is what `nml_core::parse` names (`lib.rs`: the ergonomic alias).
/// Derived from [`parse_to_ast_all`]; callers wanting every diagnostic use
/// that directly.
pub fn parse_to_ast(source: &str) -> crate::error::NmlResult<crate::ast::File> {
    // Derived from `parse_lowered` (not `parse_to_ast_all`): the abort path
    // keeps `NmlError`; only the findings-report boundary speaks Diagnostic.
    let (_parsed, file, errors, _suppressed) = parse_lowered(source);
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(file),
    }
}

/// Parse to a **best-effort** owned AST, discarding diagnostics. Resilient
/// recovery means the AST is always populated with whatever parsed, so
/// structure-driven tooling (LSP completion/hover/goto/references) keeps working
/// mid-edit instead of going dark on the first syntax error. Diagnostics are the
/// diagnostics path's job ([`parse_to_ast_all`]); feature handlers want only the
/// structure, and this names that intent at the call site.
pub fn parse_best_effort(source: &str) -> crate::ast::File {
    parse_to_ast_all(source).0
}

/// The feature-handler seam: one lex+parse yields both the semantic AST (for
/// schema-governor walks) and the lossless CST (for span queries).
pub fn parse_best_effort_with_tree(source: &str) -> (crate::ast::File, Parse) {
    let (parsed, file, _errors, _suppressed) = parse_lowered(source);
    (file, parsed)
}

/// Extract schema definitions (models / enums / oneofs) from source over the CST,
/// reading the tree directly (extraction needs no owned AST). Returns the
/// [`ExtractedSchema`](crate::schema::ExtractedSchema) plus the **full**
/// error set — syntactic *and* semantic (e.g. an out-of-precision money default),
/// position-sorted and bounded, and a strict superset of [`parse_to_ast_all`]'s:
/// the RFC 0018 facet definition rules (NML2058) are appended here. Resilient:
/// definitions from the well-formed parts are always returned, so a mid-edit
/// schema file still contributes what it can. This is the single schema-loading
/// primitive shared by the validator's loader, the LSP registry, and embedders.
pub fn extract_schema(
    source: &str,
) -> (
    crate::schema::ExtractedSchema,
    Vec<crate::diagnostic::Diagnostic>,
) {
    let (_file, schema, diags) = parse_and_extract(source);
    (schema, diags)
}

/// One parse, every output: the semantic AST, the extracted schema, and
/// the full findings set (lower-pass diagnostics plus the RFC 0018
/// facet definition rules). The single-parse form exists for surfaces
/// that need both views of the same text — the LSP re-parsed a buffer
/// per keystroke deriving them separately (~1.3 ms each on a real
/// 26 KB schema file). [`extract_schema`] is a narrowing of this;
/// [`parse_to_ast_all`] is **not** — its findings are the lower pass
/// only (no facet definition rules), because the parse verb reports
/// syntax, not schema semantics.
///
/// RFC 0018: facet definition rules (NML2058) are emitted HERE — the
/// one API every schema-consuming surface constructs through (both
/// CLI verbs, the LSP, the nml-validate loader and thus packages) —
/// so a misdeclared facet cannot load anywhere by construction.
pub fn parse_and_extract(
    source: &str,
) -> (
    crate::ast::File,
    crate::schema::ExtractedSchema,
    Vec<crate::diagnostic::Diagnostic>,
) {
    let (file, schema, mut diags, facet_diags) = parse_and_extract_split(source);
    diags.extend(facet_diags);
    (file, schema, diags)
}

/// [`parse_and_extract`] with its two finding sets kept apart: the parse
/// findings ([`parse_to_ast_all`]'s list, exactly) and the RFC 0018 facet
/// definition rules. A surface that must report the parse findings on
/// their own AND feed the schema loader (`nml check`, `validate`, `fix`,
/// the editor's model-buffer passes) gets both from ONE parse and hands
/// the extraction to the loader in place of the text
/// (`nml_validate::loader::load_schema_parts`). Those surfaces used to
/// parse the same bytes twice, holding the first AST across the second
/// parse — the single largest term of `check`'s peak memory on dense
/// input.
pub fn parse_and_extract_split(
    source: &str,
) -> (
    crate::ast::File,
    crate::schema::ExtractedSchema,
    Vec<crate::diagnostic::Diagnostic>,
    Vec<crate::diagnostic::Diagnostic>,
) {
    use ast::AstNode as _;
    let (parsed, lowered_ast, errors, suppressed) = parse_lowered(source);
    let root = ast::Root::cast(parsed.syntax()).expect("parse always yields a Root node");
    let facet_diags = crate::schema::facet_definition_diagnostics(&lowered_ast);
    let diags = finalize_diagnostics(errors, suppressed);
    let schema = extract::extract(&root);
    (lowered_ast, schema, diags, facet_diags)
}

/// The leading documentation comment of the top-level declaration named `name`
/// — or, failing that, of the **named array item** (`- Name:`) so called (a
/// declaration outranks a same-named item) — per RFC 0004 §4.3 comment
/// attachment, or `None`. A convenience for tooling (LSP hover) that surfaces
/// comments as docs by name without holding the `!Send` tree; the item arm is
/// what documents an arm target's experience (RFC 0007 §4.1).
pub fn doc_comment_for(source: &str, name: &str) -> Option<String> {
    use ast::AstNode as _;
    let root = ast::Root::cast(parse(source).syntax())?;
    if let Some(decl) = root
        .decls()
        .find(|d| d.name().and_then(|n| n.text()).as_deref() == Some(name))
    {
        return decl.doc_comment();
    }
    let item = root.decls().find_map(|d| {
        let ast::Decl::Array(arr) = d else {
            return None;
        };
        arr.body()?.entries().find_map(|e| match e {
            ast::Entry::ListItem(item) if item.name().is_some_and(|n| n.text() == name) => {
                Some(item)
            }
            _ => None,
        })
    })?;
    item.doc_comment()
}

/// Parse to the **lossless tree**, refusing exactly what [`parse_to_ast`]
/// refuses — a syntax error, a repeated name, a source-character violation,
/// a value that does not decode. The formatter's door: it prints from the
/// tree, so it needs the tree, and it must refuse an invalid document for
/// the same reason `gofmt` and `rustfmt` do (a tree recovered from errors is
/// a guess at the author's intent, and writing a guess back over their file
/// is how a formatter loses work).
///
/// The tree it returns holds every byte of the source — comments,
/// whitespace, the author's line breaks — so it is what you want when the
/// answer has to be written back into the same file: [`edit`] applies a
/// change through it without reformatting anything else, and
/// `nml_fmt::formatter::format_source` prints a whole file from it. When
/// you only need the MEANING, [`parse_to_ast`] gives you the semantic tree
/// and is the cheaper door.
///
/// Which of the two refusals you get is the same in both: a
/// [`crate::error::NmlError`] carrying every finding, not the first.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use nml_core::cst::parse_checked;
///
/// let tree = parse_checked("// a note\nconst A = 1\n")?;
/// assert_eq!(tree.text().to_string(), "// a note\nconst A = 1\n");
/// assert!(parse_checked("const A = 1\nconst A = 2\n").is_err());
/// # Ok(())
/// # }
/// ```
pub fn parse_checked(source: &str) -> crate::error::NmlResult<SyntaxNode> {
    let (parsed, _file, errors, _suppressed) = parse_lowered(source);
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(parsed.syntax()),
    }
}

/// The file's line terminator — what a minted or re-emitted line ends with,
/// so neither an insertion nor a reformat leaves a CRLF file with one LF
/// line. The ONE speller: `cst::edit` mints with it and `nml-fmt` finishes
/// with it, so the two writers cannot disagree about a file's transport.
pub fn line_terminator(source: &str) -> &'static str {
    if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Interpret a [`SyntaxKind::String`] token into its semantic value —
/// escapes decoded, a multi-line body dedented. For the string tokens that
/// sit OUTSIDE a [`SyntaxKind::Value`] node (a `oneof` arm's selector, an
/// arm's selector or target), which have no [`ast::ValueNode`] to decode
/// through.
pub fn decode_string(tok: &SyntaxToken) -> Result<String, NmlError> {
    value::decode_string_token(tok)
}

#[cfg(test)]
mod tests {
    /// RFC 0018: the definition rules emit from THIS wrapper — the one
    /// API every schema surface constructs through.
    #[test]
    fn extract_schema_emits_facet_definition_rules() {
        let (_s, diags) = extract_schema("model m:\n    s string(min = 1)\n");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(crate::diagnostic::codes::FACET_DEFINITION)),
            "{diags:?}"
        );
    }

    use super::*;

    /// The embedder's path — `parse_to_ast`, the first error by position —
    /// refuses a repeated name as it refuses a stray token: NML2093 in a
    /// body, NML1000 at the file scope, each with the first occurrence as
    /// the note. No consumer that parses text can skip the rule.
    #[test]
    fn parse_to_ast_refuses_a_repeated_name_as_it_refuses_a_stray_token() {
        let err = parse_to_ast("thing t:\n    v = 1\n    v = 2\n").expect_err("refused");
        assert!(
            matches!(
                err,
                NmlError::Syntax {
                    kind: crate::error::ParseErrorKind::DuplicateEntry { .. },
                    ..
                }
            ),
            "{err:?}"
        );
        let d = err.to_diagnostic();
        assert_eq!(d.code, Some(crate::diagnostic::codes::DUPLICATE_ENTRY));
        assert_eq!(d.related.len(), 1, "{d:?}");
        let err =
            parse_to_ast("thing a:\n    v = 1\n\nthing a:\n    v = 2\n").expect_err("refused");
        let d = err.to_diagnostic();
        assert_eq!(
            d.code,
            Some(crate::diagnostic::codes::DUPLICATE_DECLARATION)
        );
        assert_eq!(d.related[0].message, "'a' first declared here");
        assert!(parse_checked("thing t:\n    v = 1\n    v = 2\n").is_err());
    }

    /// Every AST-producing entry point derives from ONE pipeline: the
    /// all-findings forms carry the name rules located beside the syntax
    /// findings — in the PARSE set, not the facet set — and the best-effort
    /// forms keep both occurrences in the tree for structure-driven tooling.
    #[test]
    fn every_parse_entry_point_carries_the_name_rules() {
        use crate::diagnostic::codes;
        let src = "model m:\n    a string\n    a number\n\nm x:\n    a = 1\n\nm x:\n    a = 2\n";
        let expect = |diags: &[crate::diagnostic::Diagnostic], who: &str| {
            let codes: Vec<_> = diags.iter().filter_map(|d| d.code).collect();
            assert_eq!(
                codes,
                [codes::DUPLICATE_ENTRY, codes::DUPLICATE_DECLARATION],
                "{who}: {diags:?}"
            );
        };
        let (file, diags) = parse_to_ast_all(src);
        expect(&diags, "parse_to_ast_all");
        assert_eq!(
            file.declarations.len(),
            3,
            "the tree keeps both occurrences"
        );
        let (_file, _schema, parse_diags, facet_diags) = parse_and_extract_split(src);
        expect(&parse_diags, "parse_and_extract_split");
        assert!(facet_diags.is_empty(), "{facet_diags:?}");
        let (_file, _schema, diags) = parse_and_extract(src);
        expect(&diags, "parse_and_extract");
        let (_schema, diags) = extract_schema(src);
        expect(&diags, "extract_schema");
        assert_eq!(parse_best_effort(src).declarations.len(), 3);
        assert_eq!(parse_best_effort_with_tree(src).0.declarations.len(), 3);
        // Position-sorted with the rest, and counted under the one cap.
        let (_file, diags) = parse_to_ast_all("thing t:\n    v = 1\n    v = 2\n    w = \"\n");
        let codes: Vec<_> = diags.iter().filter_map(|d| d.code).collect();
        assert_eq!(codes[0], codes::DUPLICATE_ENTRY, "{diags:?}");
        assert_eq!(codes[1], codes::UNTERMINATED_STRING, "{diags:?}");
    }

    /// The parser's own token copy is twelve bytes too.
    #[test]
    fn parser_tok_is_twelve_bytes() {
        assert_eq!(parser::TOK_SIZE, 12);
    }

    /// The 4 GiB bound as `parse` reports it: an EMPTY `Root` (nothing
    /// lexed, so nothing to be lossless over), exactly one typed
    /// `SourceTooLarge` finding carrying NML0022, zero suppressed — and
    /// the same text parses clean under the real bound. Faked bound: a
    /// 4 GiB allocation is not a unit test.
    #[test]
    fn a_source_past_the_bound_parses_to_an_empty_tree_and_one_typed_error() {
        let src = "service App:\n    port = 1\n";
        let over = parse_bounded(src, 8);
        assert_eq!(over.errors().len(), 1);
        assert!(
            matches!(
                over.errors()[0],
                NmlError::Syntax {
                    kind: crate::error::ParseErrorKind::SourceTooLarge { .. },
                    ..
                }
            ),
            "{:?}",
            over.errors()
        );
        assert_eq!(
            over.errors()[0].to_diagnostic().code,
            Some(crate::diagnostic::codes::SOURCE_TOO_LARGE)
        );
        assert_eq!(over.suppressed(), 0);
        let root = over.syntax();
        assert_eq!(root.kind(), SyntaxKind::Root);
        assert_eq!(root.children_with_tokens().count(), 0, "an empty tree");
        assert!(parse(src).errors().is_empty(), "clean under the real bound");
    }

    /// The parse counter counts every entry to `parse` on this thread —
    /// the seam the one-parse-per-target pins in the CLI and the editor
    /// read. Three public entries, three parses.
    #[test]
    fn parse_counter_counts_every_parse_on_this_thread() {
        let before = parses_on_this_thread();
        let _ = parse("a = 1\n");
        let _ = parse_to_ast_all("a = 1\n");
        let _ = parse_and_extract_split("a = 1\n");
        assert_eq!(parses_on_this_thread() - before, 3);
    }

    /// Minimal typed wrapper used by the structural tests. The full typed-wrapper
    /// layer (all node kinds) lands in P4 (`cst::ast`) with its consumers; until
    /// then the public API stays `Parse`/`parse`/`decode_value` only.
    struct BlockDecl(SyntaxNode);

    impl BlockDecl {
        fn cast(node: SyntaxNode) -> Option<Self> {
            (node.kind() == SyntaxKind::BlockDecl).then_some(Self(node))
        }
        fn keyword(&self) -> Option<SyntaxToken> {
            self.0
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| t.kind() == SyntaxKind::Ident)
        }
        fn name(&self) -> Option<String> {
            self.0
                .children()
                .find(|n| n.kind() == SyntaxKind::Name)
                .and_then(|n| first_ident(&n))
                .map(|t| t.text().to_string())
        }
        fn body(&self) -> Option<SyntaxNode> {
            self.0.children().find(|n| n.kind() == SyntaxKind::Body)
        }
    }

    fn block_decls(root: &SyntaxNode) -> impl Iterator<Item = BlockDecl> + use<> {
        root.children().filter_map(BlockDecl::cast)
    }

    /// First `Ident` token directly under `node`, skipping trivia.
    fn first_ident(node: &SyntaxNode) -> Option<SyntaxToken> {
        node.children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == SyntaxKind::Ident)
    }

    #[test]
    fn parse_to_ast_drop_in_smoke() {
        use crate::ast::DeclarationKind;
        // Valid input → semantic AST.
        let file = parse_to_ast("service App:\n    port = 8080\n").unwrap();
        assert_eq!(file.declarations.len(), 1);
        assert!(matches!(
            file.declarations[0].kind,
            DeclarationKind::Block(_)
        ));
        // Invalid input → Err (the first error).
        assert!(parse_to_ast("service App:\n    @@@\n").is_err());
    }

    #[test]
    fn parse_to_ast_surfaces_semantic_errors() {
        // The CST defers value validation to decode; the drop-in re-surfaces it,
        // so these all error.
        for src in [
            "service App:\n    p = 9.999 USD\n", // money precision (USD has 2 dp)
            "service App:\n    s = \"bad \\q\"\n", // unknown escape
            "service App:\n    k = $NOPE.X\n",   // unknown secret namespace
            "service App:\n    items = [9.999 USD]\n", // nested (inside an array)
        ] {
            assert!(parse_to_ast(src).is_err(), "should error: {src:?}");
        }
    }

    #[test]
    fn parse_to_ast_all_no_redundant_structural_errors() {
        // Incomplete values are *syntactic* failures the parser reports; decode
        // (semantic) must not double-count them. `x =` (empty) and `y = -` (dash,
        // no number) are each ONE problem → ONE error.
        for src in ["service App:\n    x =\n", "service App:\n    y = -\n"] {
            let (_ast, errors) = parse_to_ast_all(src);
            assert_eq!(
                errors.len(),
                1,
                "one problem → one error for {src:?}: {errors:?}"
            );
            // The single error is the parser's syntactic one, not a duplicate
            // "empty value" decode error (which would be earlier by position).
            assert!(!errors[0].message.contains("empty value"));
        }
    }

    /// A oneof arm value carrying several bad escapes reports EVERY one
    /// (rustc-style — parity with what property values get from
    /// `decode_value_all`) and lowers to the U+FFFD-recovered text rather
    /// than going empty, so lenient surfaces keep the arm's identity.
    #[test]
    fn oneof_arm_string_reports_every_escape_error() {
        let src = "oneof email by provider:\n    \"l\\qo\\pg\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let (file, errors) = parse_to_ast_all(src);
        let escape_spans: Vec<_> = errors
            .iter()
            .filter(|d| d.message.contains("escape"))
            .map(|d| d.span)
            .collect();
        assert_eq!(
            escape_spans.len(),
            2,
            "both bad escapes reported: {errors:?}"
        );
        assert_ne!(
            escape_spans[0], escape_spans[1],
            "each error points at its own escape"
        );
        let arm = file
            .declarations
            .iter()
            .find_map(|d| match &d.kind {
                crate::ast::DeclarationKind::OneOf(o) => o.arms.first(),
                _ => None,
            })
            .expect("oneof lowered");
        assert_eq!(
            arm.value, "l\u{FFFD}o\u{FFFD}g",
            "recovered text keeps the arm's identity"
        );
    }

    /// First Comment token whose text matches.
    fn comment_with(root: &SyntaxNode, needle: &str) -> SyntaxToken {
        root.descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == SyntaxKind::Comment && t.text().contains(needle))
            .unwrap_or_else(|| panic!("comment containing {needle:?} not found"))
    }

    #[test]
    fn comment_attachment_policy() {
        // RFC 0004 §4.3: own-line comment → leading of the FOLLOWING node;
        // same-line trailing comment → the PRECEDING node.
        let root = parse(
            "// header\nservice App: // hdr-trail\n    port = 8080 // trail\n    // own\n    host = 9090\n",
        )
        .syntax();

        // own-line file header → inside the following declaration, not Root.
        assert_eq!(
            comment_with(&root, "header").parent().unwrap().kind(),
            SyntaxKind::BlockDecl,
            "own-line header → following node"
        );
        // trailing comment on the header line → the BlockDecl (preceding node).
        assert_eq!(
            comment_with(&root, "hdr-trail").parent().unwrap().kind(),
            SyntaxKind::BlockDecl,
            "same-line trailing on header → preceding node"
        );
        // trailing comment after a value → the Value it trails.
        assert_eq!(
            comment_with(&root, " trail").parent().unwrap().kind(),
            SyntaxKind::Value,
            "same-line trailing → preceding (innermost) node"
        );
        // own-line comment before a property → leading of that property.
        assert_eq!(
            comment_with(&root, "own").parent().unwrap().kind(),
            SyntaxKind::Property,
            "own-line comment → following node"
        );
    }

    /// RFC 0004 §4.3 — an own-line comment separated from the following node by a
    /// body-closing **dedent** attaches to the following node, not the preceding
    /// body. `build_tree` defers such a comment past the (zero-width) dedent into
    /// the outer scope its column belongs to (column-aware deferred-trivia buffer).
    #[test]
    fn own_line_comment_before_dedent_attaches_to_following_node() {
        // `// between` (col 0) sits between two declarations; per §4.3 it should be
        // leading of the FOLLOWING declaration, not trapped in the preceding body.
        let src = "service A:\n    p = 1\n// between\nservice B:\n    q = 2\n";
        let p = parse(src);
        assert_eq!(
            p.syntax().text().to_string(),
            src,
            "deferral stays lossless"
        );
        let parent = comment_with(&p.syntax(), "between")
            .parent()
            .unwrap()
            .kind();
        assert_ne!(
            parent,
            SyntaxKind::Body,
            "must not attach to the closing body"
        );
        assert_eq!(
            parent,
            SyntaxKind::BlockDecl,
            "own-line → following declaration"
        );
    }

    #[test]
    fn trailing_and_deferred_comment_coexist_across_dedent() {
        // A same-line trailing comment on the body's last line AND an own-line
        // col-0 comment before the dedent must each attach correctly and stay
        // lossless: the trailer to its preceding value, the own-line to the
        // following declaration.
        let src = "service A:\n    p = 1 // trail\n// between\nservice B:\n    q = 2\n";
        let p = parse(src);
        assert_eq!(p.syntax().text().to_string(), src, "stays lossless");
        assert_eq!(
            comment_with(&p.syntax(), "trail").parent().unwrap().kind(),
            SyntaxKind::Value,
            "same-line trailing → preceding value"
        );
        assert_eq!(
            comment_with(&p.syntax(), "between")
                .parent()
                .unwrap()
                .kind(),
            SyntaxKind::BlockDecl,
            "own-line before dedent → following declaration"
        );
    }

    #[test]
    fn body_indented_comment_before_dedent_stays_in_body() {
        // The dual of the above: a comment indented at the body's level is the last
        // line *of that body*, so it must NOT be deferred out — column-awareness
        // distinguishes it from the outer-scope (col-0) case.
        let src = "service A:\n    p = 1\n    // tail\nservice B:\n    q = 2\n";
        let p = parse(src);
        assert_eq!(p.syntax().text().to_string(), src, "stays lossless");
        assert_eq!(
            comment_with(&p.syntax(), "tail").parent().unwrap().kind(),
            SyntaxKind::Body,
            "body-indented comment belongs to the closing body"
        );
    }

    #[test]
    fn comment_deferred_to_intermediate_scope_not_outermost() {
        // Three indent levels: a comment at the MIDDLE column must escape the inner
        // body yet stop at the middle scope (here, leading of the following
        // middle-level property) — not fall through to the outermost following decl.
        let src = "service A:\n    group:\n        x = 1\n    // mid\n    y = 2\n";
        let p = parse(src);
        assert_eq!(
            p.syntax().text().to_string(),
            src,
            "multi-level deferral is lossless"
        );
        let parent = comment_with(&p.syntax(), "mid").parent().unwrap();
        // It leads the following middle-scope property `y` — having escaped the
        // inner (col-8) body but stopped short of the outermost following decl.
        assert_eq!(
            parent.kind(),
            SyntaxKind::Property,
            "own-line → following property"
        );
        assert!(
            parent.text().to_string().contains("y = 2"),
            "must lead the middle-scope property y, got: {:?}",
            parent.text().to_string()
        );
    }

    #[test]
    fn deferred_comment_at_eof_stays_lossless() {
        // Hardest losslessness case for the deferral: own-line comments that close
        // out nested bodies at EOF, with and without a trailing newline. If a
        // deferred comment were stranded (its scope never reopening), tree text
        // would diverge from source — assert it never does.
        for src in [
            "a:\n    b:\n        x = 1\n// c", // col-0, deep nesting, no trailing nl
            "a:\n    b:\n        x = 1\n// c\n", // …with trailing nl
            "a:\n    b:\n        x = 1\n    // mid", // mid-column comment at EOF
            "a:\n    p = 1\n// c1\n// c2",     // multi-line block at EOF
            "a:\n    b:\n        x = 1\n  // c\n", // comment at an unaligned column
        ] {
            let p = parse(src);
            assert_eq!(
                p.syntax().text().to_string(),
                src,
                "deferral must stay byte-lossless for {src:?}"
            );
        }
    }

    #[test]
    fn extract_schema_is_cst_native_and_resilient() {
        // Well-formed schema: definitions extracted, no errors.
        let (schema, errors) = extract_schema("model svc:\n    port number\n");
        assert!(errors.is_empty(), "clean schema has no errors: {errors:?}");
        assert_eq!(schema.models.len(), 1);
        assert_eq!(schema.models[0].name, "svc");

        // Resilient: a malformed leading construct still yields the well-formed
        // model that follows, and the parse error is surfaced (bounded).
        let (schema, errors) = extract_schema("@@@\nmodel ok:\n    name string\n");
        assert!(
            schema.models.iter().any(|m| m.name == "ok"),
            "extraction continues past the error"
        );
        assert!(!errors.is_empty(), "the parse error is surfaced");
        assert!(errors.len() <= MAX_ERRORS, "error output stays bounded");
    }

    #[test]
    fn extract_schema_surfaces_semantic_errors_in_defaults() {
        // A schema default can carry a *semantic* (decode-layer) error — here a
        // money value with too much precision. `extract_schema` must report the
        // full diagnostic set, matching `parse_to_ast_all`, not just syntactic
        // errors (otherwise a malformed default would slip through schema loading).
        let src = "model m:\n    x number = 9.999 USD\n";
        let errors = extract_schema(src).1;
        assert!(
            errors.iter().any(|e| e.message.contains("decimal places")),
            "semantic default error must surface: {:?}",
            errors.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
        // Parity with the canonical all-errors entry point.
        assert_eq!(errors.len(), parse_to_ast_all(src).1.len());
    }

    #[test]
    fn doc_comment_for_reads_leading_comment_block() {
        // A multi-line comment block above a declaration becomes its doc, with
        // `//` markers stripped and lines joined. The dedent fix is what lets the
        // block attach to `B` rather than the preceding body.
        let src = "service A:\n    p = 1\n// line one\n// line two\nservice B:\n    q = 2\n";
        assert_eq!(
            doc_comment_for(src, "B").as_deref(),
            Some("line one\nline two"),
            "leading comment block is the declaration's documentation"
        );
        // A declaration without a leading comment has no doc.
        assert_eq!(doc_comment_for(src, "A"), None);
        // Unknown names resolve to nothing rather than panicking.
        assert_eq!(doc_comment_for(src, "Nope"), None);
    }

    /// The checked door hands back the LOSSLESS tree — comments and all —
    /// and refuses what `parse_to_ast` refuses. Comments are tree tokens;
    /// there is no side channel to keep in step with them.
    #[test]
    fn parse_checked_yields_the_lossless_tree() {
        let src = "// header\nservice App: // trailing\n    // indented\n    port = 8080 // why\n";
        let tree = parse_checked(src).expect("valid");
        assert_eq!(tree.text().to_string(), src, "the tree is byte-faithful");
        let comments = tree
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::Comment)
            .count();
        assert_eq!(comments, 4);
        assert!(parse_checked("service A:\n    x = = 1\n").is_err());
    }

    #[test]
    fn error_payload_captures_are_bounded() {
        // Memory-amplification hardening: a pathological multi-megabyte
        // token is captured one char past the render bound, never cloned
        // wholesale into the payload. The rendered message ellipsizes.
        let huge = "9".repeat(1_000_000);
        let src = format!("service Api:\n    x = {huge}.2.3\n");
        let (_ast, diags) = parse_to_ast_all(&src);
        let num = diags
            .iter()
            .find(|d| d.message.contains("invalid number"))
            .expect("invalid-number diagnostic");
        assert!(
            num.message.len() < 200,
            "payload capture must be bounded, message was {} bytes",
            num.message.len()
        );
        assert!(num.message.contains('…'), "{}", num.message);
    }

    #[test]
    fn parse_to_ast_all_bounds_error_output() {
        // Hundreds of semantic errors must not produce an unbounded list — the
        // output is capped at MAX_ERRORS like the parser's (RFC 0004 §9).
        let mut src = String::from("service App:\n");
        for i in 0..400 {
            src.push_str(&format!("    k{i} = 9.999 USD\n"));
        }
        let (_ast, diags) = parse_to_ast_all(&src);
        // Bounded output PLUS honesty (RFC 0009): at most MAX_ERRORS real
        // findings, and clipping appends ONE Info marker naming the exact
        // count — truncation is never silent.
        assert!(
            diags.len() <= MAX_ERRORS + 1,
            "unbounded error output: {}",
            diags.len()
        );
        let errors = diags
            .iter()
            .filter(|d| d.severity == crate::diagnostic::Severity::Error)
            .count();
        assert!(errors <= MAX_ERRORS, "{errors}");
        let marker = diags.last().expect("clipped run has a marker");
        assert_eq!(marker.severity, crate::diagnostic::Severity::Info);
        // 400 emitted − 128 kept = 272 suppressed, counted exactly.
        assert!(
            marker.message.contains("272") && marker.message.contains("suppressed"),
            "{:?}",
            marker.message
        );
    }

    /// The suppression row round-trips through its recognizer (D-A):
    /// what `finalize_diagnostics` composes, `suppressed_count` reads
    /// back exactly — the shared-const coupling, pinned. Near-miss
    /// shapes (wrong severity, a code, a prefixed message) read 0.
    #[test]
    fn suppressed_count_roundtrips_the_marker_row() {
        for n in [1usize, 2, 128, 272, 10_000] {
            let row = crate::diagnostic::Diagnostic::info(suppressed_row(n));
            assert_eq!(suppressed_count(&[row]), n, "{n}");
        }
        assert_eq!(suppressed_count(&[]), 0);
        // Anchoring: an ordinary Info row, an Error carrying the text,
        // and a marker with leading junk are all NOT the marker.
        let not_markers = [
            crate::diagnostic::Diagnostic::info("nothing suppressed here"),
            crate::diagnostic::Diagnostic::error(suppressed_row(7)),
            crate::diagnostic::Diagnostic::info(format!("note: {}", suppressed_row(7))),
        ];
        assert_eq!(suppressed_count(&not_markers), 0);
        // And the real pipeline's row is recognized end-to-end.
        let mut src = String::from("service App:\n");
        for i in 0..400 {
            src.push_str(&format!("    k{i} = 9.999 USD\n"));
        }
        let (_ast, diags) = parse_to_ast_all(&src);
        assert_eq!(suppressed_count(&diags), 400 - MAX_ERRORS);
    }

    /// The findings boundary is exact AT its edge, not merely far past it:
    /// at the cap every finding is reported and no marker rides along; ONE
    /// past it exactly one is clipped and the marker says `1`; far past,
    /// the marker's count is the exact excess. A count a layer clipped
    /// upstream adds to the merge's own clip on the SAME single row, and an
    /// upstream clip alone is still reported.
    #[test]
    fn the_findings_boundary_is_exact_at_the_cap_and_one_past_it() {
        let errs = |n: usize| -> Vec<NmlError> {
            (0..n)
                .map(|i| {
                    NmlError::syntax(
                        crate::error::ParseErrorKind::NestingLimit { what: "block" },
                        crate::span::Span::new(i, i + 1),
                    )
                })
                .collect()
        };
        let markers = |d: &[crate::diagnostic::Diagnostic]| {
            d.iter()
                .filter(|x| x.severity == crate::diagnostic::Severity::Info && x.code.is_none())
                .count()
        };
        let at = finalize_diagnostics(errs(MAX_ERRORS), 0);
        assert_eq!(at.len(), MAX_ERRORS);
        assert_eq!(markers(&at), 0, "nothing is clipped at the cap");

        let past = finalize_diagnostics(errs(MAX_ERRORS + 1), 0);
        assert_eq!(past.len(), MAX_ERRORS + 1, "the cap's rows plus one marker");
        assert_eq!(markers(&past), 1);
        assert_eq!(suppressed_count(&past), 1);

        let far = finalize_diagnostics(errs(MAX_ERRORS + 400), 0);
        assert_eq!(far.len(), MAX_ERRORS + 1);
        assert_eq!(suppressed_count(&far), 400);

        let both = finalize_diagnostics(errs(MAX_ERRORS + 5), 7);
        assert_eq!(markers(&both), 1, "one row carries both clips");
        assert_eq!(suppressed_count(&both), 12);

        let upstream_only = finalize_diagnostics(errs(1), 1);
        assert_eq!(upstream_only.len(), 2);
        assert_eq!(suppressed_count(&upstream_only), 1);
    }

    /// A parse's own findings are bounded and its clip is counted: a text
    /// whose LEXICAL and SYNTACTIC findings together pass the cap yields
    /// exactly `MAX_ERRORS` rows out of `parse`, and reported + suppressed
    /// still adds up to every finding the two halves raise alone.
    #[test]
    fn a_parses_findings_are_clipped_at_the_cap_and_the_clip_is_counted() {
        let lexical = "\u{7}\n".repeat(100);
        let syntactic = "= 1\n".repeat(100);
        let raised = |p: &Parse| p.errors().len() + p.suppressed();
        let left = parse(&lexical);
        let right = parse(&syntactic);
        let both = parse(&format!("{lexical}{syntactic}"));
        assert_eq!(both.errors().len(), MAX_ERRORS, "bounded output");
        assert_eq!(
            raised(&both),
            raised(&left) + raised(&right),
            "…and nothing is lost: {} + {}",
            raised(&left),
            raised(&right)
        );
        assert!(
            both.suppressed() > left.suppressed(),
            "the merge's own clip adds to each half's"
        );
    }

    /// One over-long VALUE clips its own decode findings, and that clip is
    /// carried into the document's count — never dropped on the floor.
    #[test]
    fn a_single_values_clipped_decode_findings_are_still_counted() {
        let bad = 200;
        let src = format!("thing t:\n    v = \"{}\"\n", "\\q".repeat(bad));
        let (_file, diags) = parse_to_ast_all(&src);
        assert_eq!(diags.len(), MAX_ERRORS + 1);
        assert_eq!(suppressed_count(&diags), bad - MAX_ERRORS);
    }

    #[test]
    fn parse_to_ast_all_reports_every_error_position_sorted() {
        // Two semantic (money, secret namespace) + one syntactic (`@@@`) — all
        // surfaced at once (exceeding legacy's first-error-only), position-sorted.
        let src = "service App:\n    p = 9.999 USD\n    q = $NOPE.X\n    @@@\n";
        let (_ast, errors) = parse_to_ast_all(src);
        assert!(
            errors.len() >= 3,
            "expected ≥3 errors, got {}: {errors:?}",
            errors.len()
        );
        assert!(
            errors
                .windows(2)
                .all(|w| w[0].span.unwrap().start <= w[1].span.unwrap().start),
            "errors must be position-sorted"
        );
        // And the single-error drop-in returns exactly the first of them.
        assert_eq!(
            parse_to_ast(src).unwrap_err().span().start,
            errors[0].span.unwrap().start
        );
    }

    #[test]
    fn parse_to_ast_reports_first_error_by_position() {
        // A semantic error (money precision, line 2) *before* a syntactic one
        // (`@@@`, line 3). The merge sorts syntactic + semantic errors by source
        // position, so the drop-in reports the EARLIER (money) error.
        let src = "service App:\n    p = 9.999 USD\n    @@@\n";
        let err = parse_to_ast(src).unwrap_err();
        assert!(
            err.message().contains("decimal places"),
            "should report the earlier money error, got: {}",
            err.message()
        );
    }

    /// RFC 0004 §11.1: the retention rule. `Parse` wraps the `Send + Sync` green
    /// tree, so consumers (nudge's async runtime, the LSP) may hold it across
    /// threads/awaits; the `!Send` red `SyntaxNode` is materialized locally and
    /// never retained. This compiles only if the invariant holds for real.
    #[test]
    fn parse_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Parse>();
    }

    fn tree_text(p: &Parse) -> String {
        p.syntax().text().to_string()
    }

    fn count_kind(root: &SyntaxNode, kind: SyntaxKind) -> usize {
        root.descendants_with_tokens()
            .filter(|e| e.kind() == kind)
            .count()
    }

    /// Losslessness is the foundational invariant: assert it on every example.
    fn assert_lossless(src: &str) -> Parse {
        let p = parse(src);
        assert_eq!(tree_text(&p), src, "tree text must equal source");
        p
    }

    /// Valid input: lossless, error-free, *and structurally sound* — no `Error`
    /// nodes, and no non-trivia token orphaned directly under `Root` (which would
    /// signal a grammar gap that losslessness alone cannot catch).
    fn parse_ok(src: &str) -> Parse {
        let p = assert_lossless(src);
        assert!(
            p.errors().is_empty(),
            "unexpected errors for {src:?}: {:?}",
            p.errors()
        );
        let root = p.syntax();
        assert_eq!(
            count_kind(&root, SyntaxKind::Error),
            0,
            "no Error nodes for {src:?}"
        );
        let orphan = root
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| !t.kind().is_trivia());
        assert!(!orphan, "non-trivia token orphaned under Root for {src:?}");
        p
    }

    fn has(root: &SyntaxNode, kind: SyntaxKind) -> bool {
        count_kind(root, kind) > 0
    }

    #[test]
    fn parses_clean_block_losslessly() {
        let src = "service App:\n    port = 8080\n    host = \"localhost\"\n";
        let p = assert_lossless(src);
        assert!(p.errors().is_empty(), "errors: {:?}", p.errors());

        let root = p.syntax();
        let block = block_decls(&root).next().expect("one block");
        assert_eq!(block.keyword().unwrap().text(), "service");
        assert_eq!(block.name().as_deref(), Some("App"));
        let body = block.body().expect("body");
        assert_eq!(count_kind(&body, SyntaxKind::Property), 2);
    }

    #[test]
    fn parses_nested_block() {
        let src = "service App:\n    db:\n        port = 5432\n";
        let p = assert_lossless(src);
        assert!(p.errors().is_empty(), "errors: {:?}", p.errors());
        let root = p.syntax();
        assert_eq!(count_kind(&root, SyntaxKind::NestedBlock), 1);
        assert_eq!(count_kind(&root, SyntaxKind::Property), 1);
    }

    #[test]
    fn multiline_string_suppresses_layout() {
        // The triple-quoted value contains deeper indentation and a `key:` line;
        // none of it must become tree structure — it is one String token.
        let src = "service App:\n    note = \"\"\"\n        not: indentation\n        still string\n\"\"\"\n    port = 1\n";
        let p = assert_lossless(src);
        assert!(p.errors().is_empty(), "errors: {:?}", p.errors());

        let root = p.syntax();
        // Exactly one String token, covering the whole multi-line literal.
        assert_eq!(count_kind(&root, SyntaxKind::String), 1);
        // Two properties (note, port) — the string's inner lines added none.
        assert_eq!(count_kind(&root, SyntaxKind::Property), 2);
        // The deeper indentation inside the string produced no extra Indent.
        // (Body opens once for the block + the string contributes no layout.)
        assert_eq!(count_kind(&root, SyntaxKind::Indent), 1);
    }

    #[test]
    fn recovers_and_collects_all_errors() {
        // Two broken lines, then a valid property: resilience must keep going and
        // still parse `port`, collecting every error (RFC 0004 §2 all-errors).
        let src = "service App:\n    = bad\n    @@@\n    port = 8080\n";
        let p = assert_lossless(src);
        assert!(
            p.errors().len() >= 2,
            "expected multiple errors, got {:?}",
            p.errors()
        );
        let root = p.syntax();
        // The valid property after the errors is still recovered.
        let has_port = root
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::Property)
            .any(|prop| first_ident(&prop).is_some_and(|t| t.text() == "port"));
        assert!(has_port, "valid `port` property should survive recovery");
    }

    #[test]
    fn inconsistent_dedent_recovers() {
        // 8-space body, then a 4-space line matching no open level.
        let src = "service App:\n        a = 1\n    b = 2\n";
        let p = assert_lossless(src);
        assert!(
            p.errors().iter().any(|e| e.message().contains("dedent")),
            "expected an inconsistent-dedent diagnostic, got {:?}",
            p.errors()
        );
    }

    #[test]
    fn empty_and_trivia_only_inputs_are_lossless() {
        for src in ["", "\n", "   \n\n", "// just a comment\n", "   // x"] {
            let p = assert_lossless(src);
            // No declarations, but always a tree.
            assert_eq!(block_decls(&p.syntax()).count(), 0);
        }
    }

    #[test]
    fn ok_surfaces_all_errors_for_strict_callers() {
        let clean = parse("service App:\n    port = 1\n");
        assert!(clean.ok().is_ok());

        let broken = parse("service App:\n    @@@\n    %%%\n");
        match broken.ok() {
            Ok(_) => panic!("strict caller must reject invalid input"),
            Err(errors) => assert!(errors.len() >= 2, "all errors, not just the first"),
        }
    }

    /// RFC 0006: `=>` is rejected with the one-character fix named, the
    /// parse recovers (every stale arrow surfaces in a single pass), and
    /// the arms still lower — so `nml fmt` on strict=false pipelines can
    /// even auto-heal a legacy file into `->`.
    #[test]
    fn legacy_fat_arrow_gets_guidance_and_recovers() {
        let src = "oneof email by provider:\n    \"log\" => emailLog\n    \"postmark\" => emailPostmark\n";
        let parsed = parse(src);
        // Recovery is lossless and kept the structure: both arms present.
        assert_eq!(tree_text(&parsed), src, "rejected-arrow file round-trips");
        assert_eq!(count_kind(&parsed.syntax(), SyntaxKind::OneOfArm), 2);
        // The rejected token lives under an Error node, like every recovery.
        assert!(has(&parsed.syntax(), SyntaxKind::Error));
        match parsed.ok() {
            Ok(_) => panic!("'=>' must be rejected"),
            Err(errors) => {
                assert_eq!(errors.len(), 2, "one guidance error per stale arrow");
                for e in &errors {
                    assert!(
                        e.to_string().contains("'=>' was replaced by '->'"),
                        "guidance must name the fix: {e}"
                    );
                }
            }
        }

        // Truncated arm: a stale arrow as the final token (no model name)
        // still recovers without panic — guidance plus the cascading
        // missing-name error, and the tree stays lossless.
        let truncated = "oneof email by provider:\n    \"log\" =>";
        let parsed = parse(truncated);
        assert_eq!(tree_text(&parsed), truncated, "truncated arm round-trips");
        let errors = parsed.ok().expect_err("truncated stale arm must error");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("'=>' was replaced by '->'")),
            "guidance survives truncation"
        );
    }

    #[test]
    fn declarations_const_template_oneof_array() {
        let const_d = parse_ok("const MaxRetries = 3\n");
        assert!(has(&const_d.syntax(), SyntaxKind::ConstDecl));

        let tmpl = parse_ok("template Greeting:\n    \"Hello, world\"\n");
        assert!(has(&tmpl.syntax(), SyntaxKind::TemplateDecl));

        let oneof = parse_ok(
            "oneof email by provider as providerKind = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
        );
        let root = oneof.syntax();
        assert!(has(&root, SyntaxKind::OneOfDecl));
        assert_eq!(count_kind(&root, SyntaxKind::OneOfArm), 2);

        let array = parse_ok("[]route myRoutes:\n    - Home\n    - About\n");
        assert!(has(&array.syntax(), SyntaxKind::ArrayDecl));
    }

    #[test]
    fn field_definitions_all_forms() {
        let p = parse_ok(
            "model Plan:\n    name string\n    tier string?\n    region string = \"us\"\n    tags []string\n    mode (active | inactive)\n",
        );
        let root = p.syntax();
        assert_eq!(count_kind(&root, SyntaxKind::FieldDef), 5);
        // `[]string` and `(active | inactive)` are nested type expressions.
        assert!(count_kind(&root, SyntaxKind::TypeExpr) >= 7);
    }

    #[test]
    fn value_forms_money_negative_fallback_array_secret_role() {
        let p = parse_ok(
            "service App:\n    port = 8080\n    price = 100 USD\n    temp = -5\n    host = primary | \"localhost\"\n    key = $ENV.SECRET\n    owner = @role/admin\n    enabled = true\n    tags = [\"x\", \"y\"]\n",
        );
        let root = p.syntax();
        assert!(has(&root, SyntaxKind::Fallback), "fallback chain");
        assert!(has(&root, SyntaxKind::ArrayValue), "array literal");
        // Money is Number + currency Ident inside one Value (syntactic only).
        assert!(has(&root, SyntaxKind::Value));
    }

    #[test]
    fn entries_modifiers_shared_nested_list() {
        let p = parse_ok(
            "service App is Base, Mixin:\n    db:\n        timeout = 30\n    |visibility = \"public\"\n    |allow:\n        - @role/admin\n        - Guest\n    .defaults:\n        retries = 3\n    .region = \"us\"\n",
        );
        let root = p.syntax();
        assert!(has(&root, SyntaxKind::Extends));
        assert!(has(&root, SyntaxKind::NestedBlock));
        assert_eq!(count_kind(&root, SyntaxKind::Modifier), 2);
        assert_eq!(count_kind(&root, SyntaxKind::SharedProperty), 2);
        assert_eq!(count_kind(&root, SyntaxKind::ListItem), 2);
    }

    #[test]
    fn list_item_forms_named_shorthand_reference() {
        let p = parse_ok(
            "[]route r:\n    - Home:\n        path = \"/\"\n    - \"shorthand\"\n    - SomeRef\n",
        );
        assert_eq!(count_kind(&p.syntax(), SyntaxKind::ListItem), 3);
    }

    #[test]
    fn core_model_fixture_parses_and_extracts_clean() {
        // The repurposed RFC 0005 fixture parses + extracts with no errors, and its
        // `+` / `?+` markers land. Guards the fixture against rot.
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/models/core.model.nml"
        ))
        .unwrap();
        parse_ok(&src); // no parse errors, lossless
        let (schema, errors) = crate::cst::extract_schema(&src);
        assert!(
            errors.is_empty(),
            "fixture should extract clean: {errors:?}"
        );

        let field = |model: &str, field: &str| {
            schema
                .models
                .iter()
                .find(|m| m.name == model)
                .and_then(|m| m.fields.iter().find(|f| f.name == field))
                .unwrap_or_else(|| panic!("{model}.{field} missing"))
        };
        assert!(field("resource", "path").shorthand);
        assert!(field("role", "name").shorthand);
        let run = field("command", "run");
        assert!(
            run.shorthand && run.optional,
            "`run string?!` is shorthand + optional"
        );

        // The fixture's mixin is a real trait (RFC 0011): extracted, marked,
        // and composable — its modifier fields reach `resource` via `is`.
        let ac = schema
            .models
            .iter()
            .find(|m| m.name == "accessControlled")
            .expect("trait extracted");
        assert!(ac.is_trait());
        let mut schema = schema;
        crate::schema::resolve_model_inheritance(&mut schema);
        assert!(
            schema
                .models
                .iter()
                .find(|m| m.name == "resource")
                .unwrap()
                .fields
                .iter()
                .any(|f| f.name == "allow"),
            "trait modifier fields merge into composing models"
        );
    }

    #[test]
    fn scalar_list_item_with_body_parses_clean_and_lossless() {
        // `- "/admin":` + indented body (scalar-key-with-body, RFC 0005 §9). parse_ok
        // asserts no errors + losslessness; the item carries a scalar value and a body.
        let p = parse_ok("[]resource resources:\n    - \"/admin\":\n        method = \"POST\"\n");
        assert_eq!(count_kind(&p.syntax(), SyntaxKind::ListItem), 1);
    }

    #[test]
    fn kitchen_sink_parses_clean_and_lossless() {
        // Every construct in one document: the strongest completeness check.
        let src = "\
const MaxRetries = 3

template Greeting:
    \"Hello\"

oneof email by provider:
    \"log\" -> emailLog

model Plan:
    name string
    tier string?
    tags []string
    mode (active | inactive)

service App is Base:
    port = 8080
    price = 100 USD
    host = $ENV.HOST | \"localhost\"
    tags = [\"a\", \"b\"]
    db:
        timeout = 30
    |allow:
        - @role/admin
    .region = \"us\"
";
        let p = parse_ok(src);
        let root = p.syntax();
        for kind in [
            SyntaxKind::ConstDecl,
            SyntaxKind::TemplateDecl,
            SyntaxKind::OneOfDecl,
            SyntaxKind::BlockDecl,
            SyntaxKind::FieldDef,
            SyntaxKind::Property,
            SyntaxKind::Modifier,
            SyntaxKind::SharedProperty,
            SyntaxKind::Fallback,
            SyntaxKind::ArrayValue,
        ] {
            assert!(has(&root, kind), "kitchen sink should contain {kind:?}");
        }
    }

    /// Grammar completeness: a corpus of representative *valid* inputs across every
    /// construct must parse clean and lossless. The strongest "is the grammar
    /// complete" check.
    /// The value node under the first `Property` in a parsed document.
    fn first_value_node(root: &SyntaxNode) -> SyntaxNode {
        root.descendants()
            .find(|n| n.kind() == SyntaxKind::Property)
            .expect("a property")
            .children()
            .find(|c| {
                matches!(
                    c.kind(),
                    SyntaxKind::Value | SyntaxKind::ArrayValue | SyntaxKind::Fallback
                )
            })
            .expect("a value node")
    }

    /// Wrap a value expression in a block property (properties live in blocks).
    fn wrap(value_expr: &str) -> String {
        format!("service App:\n    k = {value_expr}\n")
    }

    fn decode_first(value_expr: &str) -> crate::types::Value {
        let src = wrap(value_expr);
        decode_value(&first_value_node(&parse(&src).syntax()))
            .expect("decode")
            .value
    }

    /// RFC 0016 §1.8 regression: the trailing-dot machine fix must target
    /// the DOT even when trivia separates the dash from the digits
    /// (`- 1299.` — the reconstructed `-1299.` string is one byte shorter
    /// than the source region, so start+len span arithmetic would delete
    /// the final digit instead; found by implementation review).
    #[test]
    fn trailing_dot_fix_targets_dot_under_dash_gap_spellings() {
        for src in [
            "plan P:\n    price = - 1299. JPY\n",
            "plan P:\n    price = -1299. JPY\n",
            "plan P:\n    price = - 1299.\n",
            "plan P:\n    price = 1299.\n",
        ] {
            let err = parse_to_ast(src).expect_err("trailing dot must reject");
            let dot = src.find('.').unwrap();
            match err {
                crate::error::NmlError::Syntax { ref kind, span } => {
                    assert!(
                        matches!(kind, crate::error::ParseErrorKind::NumberTrailingDot { .. }),
                        "{src:?}: expected NumberTrailingDot, got {kind:?}"
                    );
                    let crate::error::Repairs::Fix(replacement, fix) = kind.repairs(span) else {
                        panic!("{src:?}: fix exists");
                    };
                    assert_eq!(replacement, "", "{src:?}");
                    assert_eq!(
                        (fix.start, fix.end),
                        (dot, dot + 1),
                        "{src:?}: fix must delete exactly the dot"
                    );
                }
                other => panic!("{src:?}: expected syntax error, got {other:?}"),
            }
        }
    }

    #[test]
    fn value_decode_scalars_correct() {
        use crate::types::{Number, Value};
        assert_eq!(
            decode_first("\"a\\nb\\t!\""),
            Value::String("a\nb\t!".into())
        );
        assert_eq!(decode_first("42"), Value::Number(Number::from(42)));
        assert_eq!(decode_first("-5"), Value::Number(Number::from(-5)));
        assert_eq!(decode_first("2.5"), Value::Number(crate::num!(2.5)));
        assert_eq!(decode_first("true"), Value::Bool(true));
        assert_eq!(decode_first("false"), Value::Bool(false));
        assert_eq!(
            decode_first("GroqFast"),
            Value::Reference("GroqFast".into())
        );
        assert_eq!(
            decode_first("@role/admin"),
            Value::Role("@role/admin".into())
        );
        assert_eq!(decode_first("$ENV.X"), Value::Secret("$ENV.X".into()));
        assert!(matches!(decode_first("100 USD"), Value::Money(_)));
        assert!(matches!(
            decode_first("\"hi {{n}}\""),
            Value::TemplateString(_)
        ));
    }

    #[test]
    fn value_decode_multiline_dedent() {
        use crate::types::Value;
        // Common leading indent stripped; blank first/last lines trimmed.
        let v = decode_first("\"\"\"\n    Hello\n    World\n    \"\"\"");
        assert_eq!(v, Value::String("Hello\nWorld".into()));

        // A CRLF source yields the SAME value — line endings are transport,
        // not content; a checkout's eol convention must never change what a
        // document means.
        let v = decode_first("\"\"\"\r\n    Hello\r\n    World\r\n    \"\"\"");
        assert_eq!(v, Value::String("Hello\nWorld".into()));

        // Escaped CRs are CONTENT and survive dedent even at end-of-line in
        // a CRLF body — only the raw CRLF pair is transport (`\r` decodes
        // after the transport CR was already dropped at true offsets).
        let v = decode_first("\"\"\"\r\n    a\\r\r\n    b\r\n    \"\"\"");
        assert_eq!(v, Value::String("a\r\nb".into()));
        let v = decode_first("\"\"\"\n    a\\u{D}\n    b\n    \"\"\"");
        assert_eq!(v, Value::String("a\r\nb".into()));
    }

    /// The JEP 378 (Java text-block) order: dedent is computed on SOURCE
    /// lines BEFORE escapes are interpreted, so content can never steer
    /// transport interpretation. Each case here was validated by the
    /// differential battery (scratchpad crlf-probe) before the port.
    #[test]
    fn multiline_dedent_is_transport_only() {
        use crate::types::Value;
        // An escaped newline can no longer zero min-indent — indentation
        // stays transport and never leaks into the value.
        let v = decode_first("\"\"\"\n        a\\u{A}b\n        c\n        \"\"\"");
        assert_eq!(v, Value::String("a\nb\nc".into()));
        let v = decode_first("\"\"\"\n        a\\nb\n        c\n        \"\"\"");
        assert_eq!(v, Value::String("a\nb\nc".into()));

        // Escaped leading whitespace is CONTENT: it survives dedent
        // (protected space — Java needed `\s` for this; ours is emergent).
        let v = decode_first("\"\"\"\n    \\u{20}x\n    \\u{20}y\n    \"\"\"");
        assert_eq!(v, Value::String(" x\n y".into()));

        // A decode-blank line abutting the closing quotes is raw content,
        // never edge-trimmed (blankness is a transport property).
        let v = decode_first("\"\"\"\n    x\n    \\u{20}\"\"\"");
        assert_eq!(v, Value::String("x\n ".into()));

        // The empty multiline degenerate stays legal.
        let v = decode_first("\"\"\"\"\"\"");
        assert_eq!(v, Value::String(String::new()));
    }

    /// NML0019: multi-line string content must begin on a new line (the
    /// Swift/Java rule) — content on the opening line is the one remaining
    /// way dedent could be steered, so it is closed. Recovery is NML0012
    /// parity: error recorded, value degrades to the placeholder, lowering
    /// stays total.
    #[test]
    fn multiline_content_on_opening_line_is_an_error() {
        let (_, diags) =
            parse_to_ast_all("service App:\n    x = \"\"\"abc\n        def\n        \"\"\"\n");
        assert!(
            diags.iter().any(|d| d.to_string().contains("NML0019")),
            "{diags:?}"
        );
        // Whitespace-only after the opening quotes is harmless (it cannot
        // participate in min-indent) and stays legal.
        let (_, diags) =
            parse_to_ast_all("service App:\n    x = \"\"\"   \n        ok\n        \"\"\"\n");
        assert!(diags.is_empty(), "{diags:?}");
    }

    /// Fuzz-found (libFuzzer, formatter target): a `- $ENV.KEY` list item
    /// PARSED but lowered to nothing — the parser bumped the token bare
    /// instead of routing it through `value()`, so no `Value` node existed,
    /// lowering's fallthrough emptied the item, and the formatter re-emitted
    /// an invalid bare `- `. A secret is a scalar like any other in list
    /// position: it now lowers as `Shorthand`, so decode, `$NS` validation,
    /// shorthand placement and formatting all apply by construction.
    #[test]
    fn secret_list_item_is_a_scalar_item() {
        use crate::ast::{BodyEntryKind, DeclarationKind, ListItemKind};
        use crate::types::Value;
        let (file, diags) = parse_to_ast_all("w s:\n    - $ENV.KEY\n");
        assert!(diags.is_empty(), "{diags:?}");
        let DeclarationKind::Block(b) = &file.declarations[0].kind else {
            panic!("expected a block");
        };
        let BodyEntryKind::ListItem(item) = &b.body.entries[0].kind else {
            panic!("expected a list item, got {:?}", b.body.entries[0].kind);
        };
        assert!(
            matches!(&item.kind, ListItemKind::Shorthand { value, body: None }
                if matches!(&value.value, Value::Secret(s) if s == "$ENV.KEY")),
            "{:?}",
            item.kind
        );
        // A malformed reference is diagnosed, never silently dropped —
        // both shapes: unknown namespace, and the bare `$` (the original
        // fuzz byte, previously a silent empty item).
        for bad in ["w s:\n    - $NOPE.KEY\n", "w s:\n    - $\n"] {
            let (_, diags) = parse_to_ast_all(bad);
            assert!(
                diags.iter().any(|d| d.to_string().contains("NML0015")),
                "{bad:?}: {diags:?}"
            );
        }
        // The shapeless item (`- ` alone) is an error, not a silent empty.
        let (_, diags) = parse_to_ast_all("w s:\n    - \n");
        assert!(!diags.is_empty(), "bare '-' must diagnose");
    }

    /// Fuzz-found (libFuzzer, first corpus run): a comment-only line whose
    /// column is SHALLOWER than where a partial dedent lands (here: col 0
    /// between an indent-2 line and an indent-1 line) was deferred past the
    /// next line's REAL tokens — scrambling byte order and breaking the
    /// lossless-CST invariant. The `bump()` losslessness floor flushes held
    /// comments before any non-zero-width token.
    #[test]
    fn partial_dedent_comment_stays_byte_ordered() {
        for src in [
            " d\n  r\n//\n t",
            " d\n  r\n// c\n t\nx = 1\n",
            "a:\n    b:\n        x = 1\n// out\n    y = 2\n",
        ] {
            let p = parse(src);
            assert_eq!(tree_text(&p), src, "lossless on {src:?}");
        }

        // ATTACHMENT (not just order): the flush lands the comment in the
        // scope open once the dedent run has closed — i.e. as leading
        // trivia of the FOLLOWING line, which is where a reader puts it.
        // Declining to defer instead would emit it before the dedent,
        // trapping it in the body being closed — the exact failure §4.3's
        // deferral exists to prevent. This pins the good behavior so a
        // future "precision" refactor cannot silently regress it.
        let p = parse(" d\n   r\n// c\n t\n");
        let owner = p
            .syntax()
            .descendants()
            .find(|n| {
                n.kind() == SyntaxKind::BlockDecl
                    && n.children_with_tokens().any(|e| {
                        e.as_token()
                            .is_some_and(|t| t.kind() == SyntaxKind::Ident && t.text() == "t")
                    })
            })
            .expect("the block owning `t`");
        assert!(
            owner.children_with_tokens().any(|e| {
                e.as_token()
                    .is_some_and(|t| t.kind() == SyntaxKind::Comment)
            }),
            "the comment must attach to the following line's scope: {owner:?}"
        );
    }

    /// The total decoder (rustc-style): EVERY malformed escape in a string
    /// is reported at once — never whack-a-mole — with U+FFFD recovery, so
    /// lenient surfaces get a best-effort value AND the full findings list.
    /// The strict path still returns the first error.
    #[test]
    fn total_decode_reports_all_escape_errors_with_recovery() {
        let mut sink = ValueErrors::default();
        let p = parse("service App:\n    x = \"a\\q b\\z c\"\n");
        let node = p
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::Value)
            .expect("value node");
        let v = decode_value_all(&node, &mut sink);
        assert_eq!(sink.errors.len(), 2, "{:?}", sink.errors);
        assert_eq!(sink.suppressed, 0);
        assert!(
            matches!(&v.value, crate::types::Value::String(s) if s == "a\u{FFFD} b\u{FFFD} c"),
            "{v:?}"
        );
        // Strict view: first error only, exactly as before.
        assert!(decode_value(&node).is_err());
    }

    /// `\s` is Java's protected space: survives dedent and editor trim.
    #[test]
    fn protected_space_escape_decodes() {
        use crate::types::Value;
        assert_eq!(decode_first("\"a\\sb\""), Value::String("a b".into()));
        let v = decode_first("\"\"\"\n    \\sx\n    \\sy\n    \"\"\"");
        assert_eq!(v, Value::String(" x\n y".into()));
    }

    /// `\` before a line break joins lines without a newline (Java/Swift),
    /// with dedent applied first; `\\` stays a literal backslash; a
    /// dangling continuation on the last content line is an error.
    #[test]
    fn line_continuation_joins_without_newline() {
        use crate::types::Value;
        let v = decode_first("\"\"\"\n    https://example.com/\\\n    very/long/path\n    \"\"\"");
        assert_eq!(
            v,
            Value::String("https://example.com/very/long/path".into())
        );
        // Even run: literal backslash, no continuation.
        let v = decode_first("\"\"\"\n    a\\\\\n    b\n    \"\"\"");
        assert_eq!(v, Value::String("a\\\nb".into()));
        // CRLF transcription joins identically.
        let v = decode_first("\"\"\"\r\n    a\\\r\n    b\r\n    \"\"\"");
        assert_eq!(v, Value::String("ab".into()));
        // Dangling continuation: string ends mid-escape.
        let e = parse_to_ast("service App:\n    x = \"\"\"\n    a\\\n    \"\"\"\n")
            .expect_err("dangling continuation");
        assert!(
            e.message().contains("end of string inside an escape"),
            "{}",
            e.message()
        );
    }

    /// NML0020: an own-line closing `"""` must align with the content —
    /// the geometry where delimiter-anchored and min-indent readings could
    /// diverge, closed by making them agree. Machine-fixable (whitespace
    /// replacement renders as a fix, not a did-you-mean).
    #[test]
    fn misaligned_closing_delimiter_is_an_error_with_fix() {
        let (_, diags) =
            parse_to_ast_all("service App:\n    x = \"\"\"\n        content\n    \"\"\"\n");
        let m: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
        assert!(m.iter().any(|s| s.contains("NML0020")), "{m:?}");
        assert!(m.iter().any(|s| s.contains("(fix: `        `)")), "{m:?}");
        // Closing on the content line: no alignment exists to check.
        let (_, diags) = parse_to_ast_all("service App:\n    x = \"\"\"\n        content\"\"\"\n");
        assert!(diags.is_empty(), "{diags:?}");

        // UNTERMINATED (P2 gate): with no closing delimiter the
        // trailing blank line is a recovery artifact, not the closing
        // quotes — NML0003 tells the real story and NML0020 must not
        // fire (its phantom fix used to APPLY, rewriting whitespace at
        // EOF on a string with nothing to align).
        let (_, diags) = parse_to_ast_all("service App:\n    doc = \"\"\"\n        body\n      \n");
        let m: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
        assert!(m.iter().any(|s| s.contains("NML0003")), "{m:?}");
        assert!(
            m.iter().all(|s| !s.contains("NML0020")),
            "no alignment exists on an unterminated string: {m:?}"
        );

        // The escaped-close corner: `\"""` at EOF is lexer-UNTERMINATED
        // (the backslash pairs one closing quote) even though the text
        // ends in three quotes — the parity flag agrees with the lexer
        // and no NML0020 collateral appears.
        let (_, diags) = parse_to_ast_all("service App:\n    doc = \"\"\"x\\\"\"\"");
        let codes: Vec<String> = diags
            .iter()
            .filter_map(|d| d.code.map(|c| c.to_string()))
            .collect();
        assert!(codes.contains(&"NML0003".to_string()), "{codes:?}");
        assert!(!codes.contains(&"NML0020".to_string()), "{codes:?}");

        // Even-run control: an interior line ending `a\\` (escaped
        // backslash, even run) with a TERMINATED misaligned close —
        // the gate must keep this real NML0020 (guards the parity
        // direction).
        let (_, diags) =
            parse_to_ast_all("service App:\n    x = \"\"\"\n        a\\\\\n    \"\"\"\n");
        let m: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
        assert!(m.iter().any(|s| s.contains("NML0020")), "{m:?}");
    }

    /// NML0005's principle extends into multiline bodies: a tab in a body
    /// line's leading whitespace is the same editor-dependent column
    /// arithmetic the layout rule bans. Content tabs stay legal.
    #[test]
    fn tab_in_multiline_body_indentation_is_an_error() {
        let (_, diags) = parse_to_ast_all("service App:\n    x = \"\"\"\n    \ta\n    \"\"\"\n");
        assert!(
            diags.iter().any(|d| d.to_string().contains("NML0005")),
            "{diags:?}"
        );
        // Mid-line tab is content, not indentation.
        let (_, diags) = parse_to_ast_all("service App:\n    x = \"\"\"\n    a\tb\n    \"\"\"\n");
        assert!(diags.is_empty(), "{diags:?}");
    }

    /// NML0019 recovery keeps the author's text: the opening-line content
    /// joins the value (excluded from indent arithmetic) instead of
    /// vanishing into a placeholder.
    #[test]
    fn opening_content_recovery_keeps_the_text() {
        let mut sink = ValueErrors::default();
        let p = parse("service App:\n    x = \"\"\"abc\n        def\n        \"\"\"\n");
        let node = p
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::Value)
            .expect("value node");
        let v = decode_value_all(&node, &mut sink);
        assert_eq!(sink.errors.len(), 1, "{:?}", sink.errors);
        assert!(
            matches!(&v.value, crate::types::Value::String(s) if s == "abc\ndef"),
            "{v:?}"
        );
    }

    /// NML0021: a fallback chain in a list position gets a TEACHING error
    /// naming the actual mistake — never the old mis-parse into pipe-
    /// modifier syntax ("expected a modifier name"). One diagnostic per
    /// chain, both list spellings, all item shapes; recovery keeps later
    /// lines parsing and the tree lossless.
    #[test]
    fn fallback_chain_in_list_position_teaches() {
        // Every item shape that can precede a chain, dash spelling.
        for src in [
            "w s:\n    keys:\n        - $ENV.A | $ENV.B\n",
            "w s:\n    keys:\n        - \"a\" | \"b\"\n",
            "w s:\n    keys:\n        - 1 | 2\n",
            "w s:\n    keys:\n        - foo | bar\n",
            "w s:\n    keys:\n        - @role/a | @role/b\n",
        ] {
            let (_, diags) = parse_to_ast_all(src);
            let rendered: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
            assert!(
                rendered.iter().any(|m| m.contains("NML0021")),
                "{src:?}: {rendered:?}"
            );
            assert!(
                !rendered.iter().any(|m| m.contains("modifier name")),
                "the old mis-parse must be gone: {src:?}: {rendered:?}"
            );
            let p = parse(src);
            assert_eq!(tree_text(&p), src, "lossless under recovery: {src:?}");
        }

        // Inline-array spelling: same code, same message prose.
        let (_, dash) = parse_to_ast_all("w s:\n    keys:\n        - $ENV.A | $ENV.B\n");
        let (_, inline) = parse_to_ast_all("w s:\n    keys = [$ENV.A | $ENV.B]\n");
        let msg = |ds: &[crate::diagnostic::Diagnostic]| {
            ds.iter()
                .map(|d| d.rendered_message())
                .find(|m| m.contains("fallback chain"))
                .unwrap_or_default()
        };
        assert_eq!(msg(&dash), msg(&inline), "prose parity across spellings");
        assert!(!msg(&inline).is_empty());

        // Trailing `: body` after the chain: ONE diagnostic, body consumed.
        let (_, diags) =
            parse_to_ast_all("w s:\n    keys:\n        - \"a\" | \"b\":\n            k = 1\n");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].to_string().contains("NML0021"));

        // Recovery: content AFTER the chain still parses.
        let (file, diags) =
            parse_to_ast_all("w s:\n    keys:\n        - $ENV.A | $ENV.B\n    port = 1\n");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(!file.declarations.is_empty());

        // A newline inside brackets already breaks layout today (multi-
        // line arrays are not offside-legal), so a cross-line chain sits
        // among pre-existing layout errors — but the chain must STILL be
        // named (any-pipe rule in brackets) with a healthy span (the
        // entry/loop predicate mismatch once consumed zero legs here and
        // computed an INVERTED span).
        let (_, diags) = parse_to_ast_all("w s:\n    keys = [$ENV.A\n        | $ENV.B]\n");
        let chain: Vec<_> = diags
            .iter()
            .filter(|d| d.to_string().contains("NML0021"))
            .collect();
        assert_eq!(chain.len(), 1, "{diags:?}");
        let sp = chain[0].span.expect("spanned");
        assert!(sp.end > sp.start, "inverted span: {sp:?}");

        // NON-regression: a next-line `|modifier` entry is NOT a chain (the
        // same-line rule) — nudge's `|grant` syntax depends on this.
        let (_, diags) = parse_to_ast_all(
            "model t:\n    hosts:\n        - \"api.example.com\"\n    |grant []string?\n",
        );
        assert!(
            !diags.iter().any(|d| d.to_string().contains("NML0021")),
            "{diags:?}"
        );
    }

    /// Template detection reads RAW text: `\u{7B}\u{7B}` is a literal `{{`
    /// (the escape hatch — previously inexpressible), never a template, so
    /// an escape can't smuggle a template expression past review; written
    /// plainly, `{{…}}` still templates.
    #[test]
    fn escaped_braces_are_literal_never_template() {
        use crate::types::Value;
        assert_eq!(
            decode_first("\"\\u{7B}\\u{7B}x\\u{7D}\\u{7D}\""),
            Value::String("{{x}}".into())
        );
        assert!(matches!(
            decode_first("\"{{args.x}}\""),
            Value::TemplateString(_)
        ));
    }

    #[test]
    fn value_decode_array_and_fallback_structure() {
        use crate::types::Value;
        match decode_first("[\"a\", \"b\", \"c\"]") {
            Value::Array(items) => assert_eq!(items.len(), 3),
            other => panic!("expected array, got {other:?}"),
        }
        // Nested arrays decode element-wise (each element is itself an array).
        match decode_first("[[1, 2], [3, 4]]") {
            Value::Array(rows) => {
                assert_eq!(rows.len(), 2);
                assert!(rows.iter().all(|r| matches!(r.value, Value::Array(_))));
            }
            other => panic!("expected nested array, got {other:?}"),
        }
        // `a | b | c` is right-associative: Fallback(a, Fallback(b, c)).
        match decode_first("a | b | c") {
            Value::Fallback(first, rest) => {
                assert_eq!(first.value, Value::Reference("a".into()));
                assert!(matches!(rest.value, Value::Fallback(_, _)));
            }
            other => panic!("expected fallback, got {other:?}"),
        }
    }

    #[test]
    fn escape_error_span_is_char_precise() {
        // value: "ok \q bad" — `\q` is an invalid escape.
        let src = wrap("\"ok \\q bad\"");
        let node = first_value_node(&parse(&src).syntax());
        let cst_span = decode_value(&node).unwrap_err().span();
        // The diagnostic underlines exactly the offending `\q`, not the whole value.
        assert_eq!(&src[cst_span.start..cst_span.end], "\\q");
    }

    #[test]
    fn value_decode_rejects_unknown_secret_namespace() {
        let src = wrap("$NOPE.X");
        let node = first_value_node(&parse(&src).syntax());
        let err = decode_value(&node).unwrap_err();
        assert!(
            err.message().contains("unknown variable source"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn valid_corpus_parses_clean() {
        let corpus = [
            // enums (list items), traits/models (field defs)
            "enum Status:\n    - active\n    - inactive\n    - pending\n",
            "trait Auditable:\n    createdAt string\n    updatedAt string\n",
            "model User:\n    name string\n    age number\n",
            // value forms
            "service App:\n    greeting = \"Hello {{args.name}}\"\n",
            "service App:\n    dir = \"./static\"\n",
            "service App:\n    provider = GroqFast\n",
            "service App:\n    role = @role/admin\n",
            "service S:\n    price = 100.00 USD\n",
            "service S:\n    a = -5\n    b = 3.14\n",
            "service S:\n    key = $ENV.KEY | \"default\"\n",
            // multiline string with internal indentation
            "service App:\n    bio = \"\"\"\n    Hello\n    World\n    \"\"\"\n",
            // arrays, modifiers, shared properties, named list items
            "[]mount mounts:\n    |allow = [@authenticated]\n    - Main:\n        path = \"/\"\n",
            "workflow W:\n    .defaults:\n        retries = 3\n    - step1:\n        x = 1\n",
            "workflow W:\n    .interval = 7200\n    - step1:\n        x = 1\n",
            // role list items with rich role syntax
            "role admin:\n    members:\n        - @role/editor\n        - @user/test@example.com\n",
            // discriminated unions
            "oneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
            // const + template
            "const MaxRetries = 3\n",
            "template Greeting:\n    \"Hello\"\n",
        ];
        for src in corpus {
            parse_ok(src);
        }
    }

    #[test]
    fn currency_does_not_cross_newline() {
        // A 3-uppercase identifier starting the next line is its own entry, not
        // the previous number's currency code.
        let p = parse_ok("service App:\n    count = 5\n    USD = 1\n");
        let root = p.syntax();
        assert_eq!(count_kind(&root, SyntaxKind::Property), 2);
        // `count` decodes to a plain number, not money.
        assert!(matches!(decode_first("5"), crate::types::Value::Number(_)));
        // Same-line currency still parses as money.
        assert!(matches!(
            decode_first("5 USD"),
            crate::types::Value::Money(_)
        ));
    }

    #[test]
    fn duration_does_not_cross_newline() {
        // A field named `s`/`m`/`h` starting the next line is its own entry,
        // not the previous number's duration unit. This matters MORE than
        // the currency pin above: one-letter field names are plausible in
        // real configuration, so a cross-line join would be live corruption
        // (RFC 0017 §1).
        let p = parse_ok("service App:\n    count = 5\n    s = 1\n    m = 2\n    h = 3\n");
        let root = p.syntax();
        assert_eq!(count_kind(&root, SyntaxKind::Property), 4);
        assert!(matches!(decode_first("5"), crate::types::Value::Number(_)));
        // Same-line unit still parses as a duration — attached or spaced
        // (whitespace between number and unit is insignificant, as for
        // money; fmt canonicalizes to attached).
        for expr in ["5s", "5 s"] {
            assert!(
                matches!(decode_first(expr), crate::types::Value::Duration(_)),
                "{expr}"
            );
        }
    }

    /// RFC 0017 §1: suffix classification is total and structural — three
    /// uppercase letters route to money, lowercase units to duration, and
    /// everything else is the coded unknown-unit rejection (never a
    /// generic parse error).
    #[test]
    fn number_suffix_classification_is_total() {
        use crate::diagnostic::codes;
        let d = |expr: &str| match decode_first(expr) {
            crate::types::Value::Duration(d) => d,
            other => panic!("{expr} should be a duration, got {other:?}"),
        };
        assert_eq!(d("30s").to_string(), "30s");
        assert_eq!(d("500ms").to_string(), "500ms");
        assert_eq!(d("72h").to_string(), "72h");
        assert_eq!(d("5m").to_string(), "5m");
        assert_eq!(d("1h30m").to_string(), "1h30m");
        assert_eq!(d("5m2s").to_string(), "5m2s");
        assert_eq!(d("1h 30m").to_string(), "1h30m");
        // Magnitude spelling normalizes through the decoded value.
        assert_eq!(d("030s").to_string(), "30s");
        assert!(matches!(
            decode_first("19.99 USD"),
            crate::types::Value::Money(_)
        ));

        let err_code = |expr: &str| {
            let src = wrap(expr);
            decode_value(&first_value_node(&parse(&src).syntax()))
                .expect_err(expr)
                .to_diagnostic()
                .code
                .map(|c| c.to_string())
        };
        // Unknown, wrong-case, and spelled-out units are NML3004 — `30S`
        // is a rejection with a fix, never a case-fold (RFC 0017 §1).
        for expr in ["30x", "30S", "30sec", "30 units"] {
            assert_eq!(
                err_code(expr),
                Some(codes::UNKNOWN_UNIT.to_string()),
                "{expr}"
            );
        }
        assert_eq!(
            err_code("30.5s"),
            Some(codes::FRACTIONAL_DURATION.to_string())
        );
        for expr in ["-30s", "12345678901234567890123s"] {
            assert_eq!(
                err_code(expr),
                Some(codes::DURATION_OUT_OF_RANGE.to_string()),
                "{expr}"
            );
        }
        // Precedence: the trailing-dot malformation fires BEFORE duration
        // decoding, exactly as it does for money (`19. USD`).
        assert_eq!(err_code("30. s"), Some(codes::INVALID_NUMBER.to_string()));
    }

    /// Compound-literal rejections end to end (RFC 0017 §10): the codes,
    /// the machine fixes, and the fix-suppression rule that keeps every
    /// suggestion value-preserving.
    #[test]
    fn compound_duration_rejections_teach_and_fix() {
        use crate::diagnostic::codes;
        let diag_of = |expr: &str| {
            let src = wrap(expr);
            decode_value(&first_value_node(&parse(&src).syntax()))
                .expect_err(expr)
                .to_diagnostic()
        };

        // NML3007: a repeated unit, with the whole literal's merged form
        // as the fix — value-preserving even with other units present.
        let d = diag_of("1h2h");
        assert_eq!(d.code.map(|c| c.to_string()).as_deref(), Some("NML3007"));
        assert_eq!(
            d.suggestions.first().map(|s| s.replacement.as_str()),
            Some("3h")
        );
        let d = diag_of("1h2h30m");
        assert_eq!(
            d.suggestions.first().map(|s| s.replacement.as_str()),
            Some("3h30m")
        );

        // NML3008: a dangling magnitude — no machine fix (completing or
        // deleting it would change the value), the related span points
        // at the break.
        let d = diag_of("1h30");
        assert_eq!(d.code.map(|c| c.to_string()).as_deref(), Some("NML3008"));
        assert!(d.suggestions.is_empty(), "no value-changing fix: {d:?}");
        assert!(
            !d.related.is_empty(),
            "the break location must be pointed at: {d:?}"
        );

        // NML3005 in a compound: the respelling fix is WITHHELD — replacing
        // `1.5h30m` with the component's respelling (`1h30m`) would
        // silently drop the `30m` while looking plausible.
        let d = diag_of("1.5h30m");
        assert_eq!(
            d.code.map(|c| c.to_string()),
            Some(codes::FRACTIONAL_DURATION.to_string())
        );
        assert!(
            d.suggestions.is_empty(),
            "a component fix would corrupt: {d:?}"
        );

        // Sole component gets the granularity-preserving compound fix.
        let d = diag_of("1.5h");
        assert_eq!(
            d.suggestions.first().map(|s| s.replacement.as_str()),
            Some("1h30m")
        );
    }

    /// `_` digit separators end to end (RFC 0017 follow-up): legal
    /// spellings decode to the identical value across every literal kind
    /// (number, duration, money); misplaced separators are NML0013 with
    /// the whole-anchor strip fix.
    #[test]
    fn digit_separators_decode_and_teach() {
        use crate::diagnostic::codes;
        assert!(matches!(
            decode_first("1_000_000"),
            crate::types::Value::Number(n) if n == crate::types::Number::from(1_000_000)
        ));
        match decode_first("1_000_000ns") {
            crate::types::Value::Duration(d) => assert_eq!(d.to_string(), "1000000ns"),
            other => panic!("expected duration, got {other:?}"),
        }
        assert!(matches!(
            decode_first("1_299 JPY"),
            crate::types::Value::Money(m) if m.amount == 1299
        ));
        // Misplaced separators: NML0013 with the strip fix, per kind.
        for (expr, fixed) in [("1__000", "1000"), ("1_", "1"), ("1_.5", "1.5")] {
            let src = wrap(expr);
            let diag = decode_value(&first_value_node(&parse(&src).syntax()))
                .expect_err(expr)
                .to_diagnostic();
            assert_eq!(
                diag.code.map(|c| c.to_string()).as_deref(),
                Some(codes::INVALID_NUMBER.to_string().as_str()),
                "{expr}"
            );
            let sug = diag.suggestions.first().expect("strip fix");
            assert_eq!(sug.replacement, fixed, "{expr}");
            let start = src.find(expr).unwrap();
            assert_eq!(
                (sug.span.start, sug.span.end),
                (start, start + expr.len()),
                "whole-literal anchor: {expr}"
            );
        }
        // Duration magnitude: the strip fix carries the unit back in.
        let src = wrap("1__0s");
        let diag = decode_value(&first_value_node(&parse(&src).syntax()))
            .expect_err("1__0s")
            .to_diagnostic();
        assert_eq!(
            diag.suggestions.first().map(|s| s.replacement.as_str()),
            Some("10s")
        );
        // Money amount: the fix rewrites the amount sub-span only.
        let src = wrap("1__299 JPY");
        let diag = decode_value(&first_value_node(&parse(&src).syntax()))
            .expect_err("1__299 JPY")
            .to_diagnostic();
        let sug = diag.suggestions.first().expect("amount strip fix");
        assert_eq!(sug.replacement, "1299");
        let start = src.find("1__299").unwrap();
        assert_eq!(
            (sug.span.start, sug.span.end),
            (start, start + "1__299".len())
        );
    }

    /// The `30S` fix is machine-applicable on the suffix's own sub-span.
    #[test]
    fn unknown_unit_fix_targets_the_suffix() {
        let src = wrap("30S");
        let diag = decode_value(&first_value_node(&parse(&src).syntax()))
            .expect_err("30S is not a unit")
            .to_diagnostic();
        let sug = diag.suggestions.first().expect("nearest-unit fix");
        assert_eq!(sug.replacement, "s");
        // `wrap` puts the value at "service App:\n    k = " — the suffix
        // `S` is the literal's last byte.
        let start = src.find("30S").unwrap() + 2;
        assert_eq!((sug.span.start, sug.span.end), (start, start + 1));
    }

    #[test]
    fn body_entry_sequence_is_structurally_correct() {
        // Order *and* kind of every entry in the outer block body must be right
        // — losslessness can't catch a mis-typed or mis-nested entry.
        let src = "service App is Base, Mixin:\n    db:\n        timeout = 30\n    |visibility = \"public\"\n    |allow:\n        - @role/admin\n    .defaults:\n        retries = 3\n    .region = \"us\"\n    port = 8080\n";
        let root = parse_ok(src).syntax();
        let body = root
            .descendants()
            .find(|n| n.kind() == SyntaxKind::Body)
            .expect("a body");
        let kinds: Vec<SyntaxKind> = body.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::NestedBlock,    // db:
                SyntaxKind::Modifier,       // |visibility = ...
                SyntaxKind::Modifier,       // |allow:
                SyntaxKind::SharedProperty, // .defaults:
                SyntaxKind::SharedProperty, // .region = ...
                SyntaxKind::Property,       // port = 8080
            ]
        );
        // `is Base, Mixin` is an Extends on the *declaration*, not in the body.
        assert!(
            root.descendants()
                .find(|n| n.kind() == SyntaxKind::BlockDecl)
                .unwrap()
                .children()
                .any(|n| n.kind() == SyntaxKind::Extends)
        );
    }

    fn block_of(src: &str) -> crate::ast::BlockDecl {
        let (file, diags) = parse_to_ast_all(src);
        assert!(
            diags.is_empty(),
            "unexpected diagnostics for {src:?}: {diags:?}"
        );
        match &file.declarations[0].kind {
            crate::ast::DeclarationKind::Block(b) => b.clone(),
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn uses_clause_lowers_in_authored_order() {
        let b = block_of("flow F uses alpha, beta:\n    a = 1\n");
        let refs: Vec<&str> = b.uses.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(refs, ["alpha", "beta"]);
        assert!(b.extends.is_empty());
    }

    #[test]
    fn uses_clause_with_is_keeps_both() {
        let b = block_of("flow F is T uses base:\n    a = 1\n");
        assert_eq!(b.extends.len(), 1);
        assert_eq!(b.uses.len(), 1);
        assert_eq!(b.uses[0].name, "base");
    }

    #[test]
    fn uses_ref_named_as_is_rejected_loudly() {
        // `flow F uses as base:` — `as` is never a layer ref; the annotation
        // rejection claims it and the parse errors instead of misbinding.
        let p = parse("flow F uses as base:\n    a = 1\n");
        assert!(
            !p.errors().is_empty(),
            "expected a parse error for `uses as`"
        );
    }

    #[test]
    fn bodyless_uses_declaration_parses_clean() {
        let b = block_of("flow F uses base\n");
        assert_eq!(b.uses[0].name, "base");
        assert!(b.body.entries.is_empty());
    }

    #[test]
    fn repeated_or_misordered_header_clauses_error_loudly() {
        // Regression class: any stray header clause used to fall through
        // the missing-colon leniency into a SILENT declaration split (the
        // body landing on a bogus block) — which `nml fmt` then wrote to
        // disk. The clause loop makes every shape loud.
        for src in [
            "flow F uses base is T:\n    x = 1\n", // is after uses
            "flow F uses a uses b:\n    x = 1\n",  // repeated uses
            "flow F is A is B:\n    x = 1\n",      // repeated is
            "[]x ys uses base:\n    - one\n",      // clause on array decl
        ] {
            let p = parse(src);
            assert!(!p.errors().is_empty(), "must error loudly: {src:?}");
        }
    }

    #[test]
    fn header_clauses_never_continue_across_newlines() {
        // Regression: the clause-ref path lacked the same-line rule every
        // other header decision has, so a trailing comma (or a dangling
        // `uses`/`is`) consumed the NEXT declaration's tokens as refs —
        // silently, in the `uses X,\nY:` shape (zero diagnostics, Y
        // swallowed as a ref, its body attached to the wrong block).
        for src in [
            "service A uses X,\nY:\n    port = 1\n",
            "service A uses\nservice B:\n    port = 1\n",
            "service A uses X,\r\nY:\r\n    port = 1\r\n",
            "service A is\nservice B:\n    port = 1\n",
        ] {
            let (file, diags) = parse_to_ast_all(src);
            assert!(!diags.is_empty(), "must error loudly: {src:?}");
            // The next-line tokens survive as their OWN declaration —
            // never as refs of the dangling clause.
            let refs: Vec<String> = file
                .declarations
                .iter()
                .filter_map(|d| match &d.kind {
                    crate::ast::DeclarationKind::Block(b) => Some(b),
                    _ => None,
                })
                .flat_map(|b| b.uses.iter().chain(b.extends.iter()))
                .map(|r| r.name.clone())
                .collect();
            assert!(
                !refs.iter().any(|r| r == "Y" || r == "service" || r == "B"),
                "next-line tokens are never clause refs: {refs:?} in {src:?}"
            );
        }
    }

    #[test]
    fn valid_edge_cases_parse_clean() {
        let corpus = [
            "service App:\n",                                        // empty body
            "service App:\n    tags = []\n",                         // empty array literal
            "service App:\n    matrix = [[1, 2], [3, 4]]\n",         // nested arrays
            "service App:\n    greeting =\n        \"hi\"\n",        // indented (block) value
            "model M:\n    f string?\n    g (a | b)?\n", // optional field + optional union
            "service App:\n    |field string?\n",        // modifier type annotation
            "service App:\n    items = [1, 2, 3,]\n",    // trailing comma in array
            "const C = a | b | c\n",                     // const with fallback chain
            "[]x ys:\n    .shared = 1\n    - One:\n        a = 1\n", // array decl: shared + named item
            "flow F uses base:\n    a = 1\n",                        // RFC 0019 uses clause
            "flow F uses a, b, c:\n    x = 1\n",                     // multi-ref uses
            "flow F uses base\n", // pure stack assembly (no colon)
            "flow F is T uses base:\n    a = 1\n", // is + uses, canonical order
            "model M:\n    uses string\n", // `uses` stays a legal field name
        ];
        for src in corpus {
            parse_ok(src);
        }
    }

    #[test]
    fn indented_value_decodes() {
        // The block-form value (`= <newline> <indent> value`) must decode like
        // an inline value.
        let src = "service App:\n    greeting =\n        \"hi\"\n";
        let v = decode_value(&first_value_node(&parse(src).syntax()))
            .unwrap()
            .value;
        assert_eq!(v, crate::types::Value::String("hi".into()));
    }

    #[test]
    fn field_type_does_not_cross_newline() {
        // `flag` has no same-line type, so it is incomplete (an error); `other`
        // is a *separate* property — the field-def detector must not consume the
        // next line's identifier as `flag`'s type.
        let p = assert_lossless("model M:\n    flag\n    other = 1\n");
        assert!(!p.errors().is_empty(), "bare `flag` should be an error");
        let root = p.syntax();
        // `other = 1` survives as its own property (not swallowed as a type).
        let has_other = root
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::Property)
            .any(|prop| first_ident(&prop).is_some_and(|t| t.text() == "other"));
        assert!(has_other, "`other = 1` must be a separate property");
        // And it was *not* mis-parsed as a single field definition.
        assert_eq!(count_kind(&root, SyntaxKind::FieldDef), 0);
    }

    #[test]
    fn deeply_nested_values_are_bounded() {
        // Nested arrays and union types must be depth-bounded too (not just
        // blocks) — `depth_guarded_type` covers both.
        let arr = format!("x:\n    v = {}{}\n", "[".repeat(400), "]".repeat(400));
        let p = assert_lossless(&arr);
        assert!(p.errors().iter().any(|e| e.message().contains("depth")));

        let ty = format!("model M:\n    f {}int\n", "[]".repeat(400));
        let p2 = assert_lossless(&ty);
        assert!(p2.errors().iter().any(|e| e.message().contains("depth")));
    }

    #[test]
    fn deep_nesting_is_bounded_no_stack_overflow() {
        // Adversarial: hundreds of ever-deeper nested blocks. Without a depth
        // guard this overflows the stack (RFC 0004 §9). With it, recursion caps
        // and the over-deep tail is consumed iteratively.
        let mut src = String::new();
        for level in 0..600 {
            for _ in 0..level {
                src.push(' ');
            }
            src.push_str("a:\n");
        }
        let p = parse(&src); // must not overflow or panic
        assert_eq!(tree_text(&p), src, "lossless on deeply-nested input");
        assert!(
            p.errors().iter().any(|e| e.message().contains("depth")),
            "depth guard should fire past the limit"
        );
    }

    /// Fuzz/property harness (RFC 0004 §9): on *any* input the parser must
    /// terminate, never panic, stay byte-lossless, and keep the error list
    /// bounded. A small deterministic LCG drives it (no external deps).
    #[test]
    fn fuzz_termination_losslessness_bounded() {
        // Covers every token-triggering character class introduced in P1 plus
        // adversarial bytes (null, a control char, a 4-byte emoji, a non-`\n`
        // Unicode line separator) that exercise the lexer's catch-all/`utf8_len`
        // path — so losslessness/termination/no-panic hold across the *whole* byte
        // space, not just the syntactic subset (RFC 0004 §9).
        let alphabet = [
            "service", "App", ":", "=", "=>", "->", " ", "    ", "\n", "\r\n", "\"", "\"\"\"",
            "\\", "//c", "x", "1", "12.5", "@role/x", "$ENV.K", "-", "|", ".", "[", "]", "(", ")",
            ",", "?", "\t", "é", "\0", "\u{1}", "🎉", "\u{2028}", "\u{FEFF}", "\\s",
        ];
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..4000 {
            let len = (next() % 40) as usize;
            let mut src = String::new();
            for _ in 0..len {
                src.push_str(alphabet[(next() as usize) % alphabet.len()]);
            }
            let p = parse(&src); // must not panic
            assert_eq!(tree_text(&p), src, "lossless on adversarial input: {src:?}");
            assert!(p.errors().len() <= MAX_ERRORS, "error list stays bounded");

            // Decode is reachable from untrusted input (nudge decodes tenant
            // config values), so it must never panic on *any* tree — only return
            // Ok/Err. Decode every value node the fuzzed parse produced.
            for node in p.syntax().descendants() {
                if matches!(
                    node.kind(),
                    SyntaxKind::Value | SyntaxKind::ArrayValue | SyntaxKind::Fallback
                ) {
                    let _ = decode_value(&node);
                }
            }

            // Schema extraction and the semantic-AST lowering are also input-reachable
            // and recurse (resolve_field_type / nested bodies); neither may panic.
            use ast::AstNode as _;
            if let Some(root) = ast::Root::cast(p.syntax()) {
                let _ = extract::extract(&root);
                let _ = lower::to_ast_with_errors(&root);
            }
            // The public drop-ins compose parse + lower (+ the checked-tree
            // door + schema extraction); prove they are panic-free on any
            // source too.
            let _ = parse_to_ast(&src);
            let _ = parse_checked(&src);
            let _ = extract_schema(&src);
        }
    }

    /// Serialize a lowered [`crate::ast::File`] with every `span` key
    /// stripped — semantic shape only. (The serde_json dev-dependency
    /// exists for exactly this span-stripped comparison.)
    fn spanless(file: &crate::ast::File) -> serde_json::Value {
        fn strip(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Object(map) => {
                    map.remove("span");
                    for v in map.values_mut() {
                        strip(v);
                    }
                }
                serde_json::Value::Array(items) => {
                    for v in items.iter_mut() {
                        strip(v);
                    }
                }
                _ => {}
            }
        }
        let mut v = serde_json::to_value(file).expect("File serializes");
        strip(&mut v);
        v
    }

    /// Spec (Source text): line endings are transport, not content — a
    /// document and its CRLF transcription lower to the SAME `File`, valid
    /// or invalid (error recovery included). Fuzzed so every value path
    /// present *and future* is covered, not just the literal forms someone
    /// remembered to hand-test. The alphabet is the termination fuzzer's
    /// minus its explicit CRLF entry (transcribing an existing CRLF would
    /// manufacture a bare CR, which is its own contract — NML0016).
    #[test]
    fn fuzz_eol_insensitivity_lf_and_crlf_mean_the_same_file() {
        let alphabet = [
            "service", "App", ":", "=", "=>", "->", " ", "    ", "\n", "\"", "\"\"\"", "\\", "//c",
            "x", "1", "12.5", "@role/x", "$ENV.K", "-", "|", ".", "[", "]", "(", ")", ",", "?",
            "\t", "é", "🎉", "\u{FEFF}",
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = (next() % 40) as usize;
            let mut src = String::new();
            for _ in 0..len {
                src.push_str(alphabet[(next() as usize) % alphabet.len()]);
            }
            let crlf = src.replace('\n', "\r\n");
            let (lf_file, _) = parse_to_ast_all(&src);
            let (crlf_file, _) = parse_to_ast_all(&crlf);
            assert_eq!(
                spanless(&lf_file),
                spanless(&crlf_file),
                "LF and CRLF transcriptions must mean the same File: {src:?}"
            );
        }
    }

    // ── Source-character policy (spec: Source text) ──

    /// Policy diagnostics ride the one findings channel every consumer
    /// shares, and the policy's teaching error supersedes the lexer's
    /// generic `UnexpectedCharacter` for the same character — one
    /// diagnostic per hostile character, the specific one.
    #[test]
    fn source_policy_flows_through_parse_and_supersedes_generic_errors() {
        // Inside a string: only the policy can see it.
        let (_, diags) = parse_to_ast_all("service App:\n    x = \"a\u{202E}b\"\n");
        let rendered: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
        assert!(
            rendered.iter().any(|m| m.contains("NML0018")),
            "{rendered:?}"
        );

        // In token position: the policy error replaces the generic
        // unexpected-character error — exactly one char diagnostic, the
        // one that teaches the fix. (The parser's recovery NML0002s are
        // a separate accompaniment, hence the filter idiom.)
        let (_, diags) = parse_to_ast_all("service App:\n    x = \u{1}1\n");
        let about_char: Vec<String> = diags
            .iter()
            .map(|d| d.to_string())
            .filter(|m| m.contains("NML0017") || m.contains("NML0004"))
            .collect();
        assert_eq!(about_char.len(), 1, "{about_char:?}");
        assert!(about_char[0].contains("NML0017"), "{about_char:?}");

        // The closed classes' new members ride the same supersede: NEL
        // (C1 → NML0017) and LS (steering invisible → NML0018), at
        // token position and inside strings.
        for (src, policy) in [
            ("service App:\n    x\u{85} = 1\n", "NML0017"),
            ("service App:\n    x\u{2028} = 1\n", "NML0018"),
            ("service App:\n    x\u{E0067} = 1\n", "NML0018"),
        ] {
            let (_, diags) = parse_to_ast_all(src);
            let about_char: Vec<String> = diags
                .iter()
                .map(|d| d.to_string())
                .filter(|m| m.contains(policy) || m.contains("NML0004"))
                .collect();
            assert_eq!(about_char.len(), 1, "{src:?}: {about_char:?}");
            assert!(about_char[0].contains(policy), "{about_char:?}");
            // The mojibake clause is NEL's alone (0x85 is a CP-1252
            // byte; LS/PS and tags have no such reading) — guards the
            // hint-arm split against re-merging. The NEL row itself
            // keeps it, in token position too.
            assert_eq!(
                about_char[0].contains("Windows-1252"),
                policy == "NML0017",
                "{src:?}: {about_char:?}"
            );
        }
        let (_, diags) = parse_to_ast_all("service App:\n    x = \"a\u{85}b\"\n");
        let rendered: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
        assert_eq!(
            rendered.len(),
            1,
            "in-string NEL is the policy's alone: {rendered:?}"
        );

        assert!(
            rendered[0].contains("NML0017") && rendered[0].contains("Unicode line break"),
            "the line-break hint teaches the line-break intent: {rendered:?}"
        );
        assert!(
            rendered[0].contains("Windows-1252"),
            "NEL's hint teaches its third reading — the mojibake \
             ellipsis — mirroring its three repair arms: {rendered:?}"
        );

        // The tag block (U+E0000–E007F) is an invisible ASCII mirror: in
        // a string or a comment a raw tag character was ZERO diagnostics
        // — a hidden payload channel through review. The policy scan is
        // context-free, so membership alone closes every position.
        for src in [
            "service App:\n    x = \"a\u{E0067}b\"\n",
            "service App:\n    // note\u{E0067}\n    x = 1\n",
        ] {
            let (_, diags) = parse_to_ast_all(src);
            let tags: Vec<String> = diags
                .iter()
                .map(|d| d.to_string())
                .filter(|m| m.contains("NML0018"))
                .collect();
            assert_eq!(tags.len(), 1, "{src:?}: {diags:?}");
            assert_eq!(
                diags.len(),
                1,
                "the surrounding syntax parses clean — the policy diagnostic \
                 is the ONLY finding: {src:?}: {diags:?}"
            );
            assert!(
                tags[0].contains("U+E0067") && tags[0].contains("tag characters"),
                "the tag hint names the channel and the emoji use: {tags:?}"
            );
        }

        // An UNTERMINATED string never grants in-string context: on an
        // old-Mac file with a stray quote, the swallowed-to-EOF token
        // must not flip the following TRANSPORT CRs to content (whose
        // `\r` fix would glue the file's line structure into a string).
        // Both open shapes pin the guard: the single- and triple-quote
        // openers record different open spans, but each starts exactly
        // at its String token's start.
        for src in [
            "service App:\r    a = \"stray x\r    b = 2\r",
            "service App:\r    a = \"\"\"stray\r    b = 2\r",
        ] {
            let (_, diags) = parse_to_ast_all(src);
            let cr: Vec<String> = diags
                .iter()
                .map(|d| d.to_string())
                .filter(|m| m.contains("NML0016"))
                .collect();
            assert!(!cr.is_empty(), "{src:?}: {diags:?}");
            assert!(
                cr.iter().all(|m| !m.contains("(fix:")),
                "no CR behind a stray quote may carry the in-string fix: {cr:?}"
            );
        }

        // The bare-CR diagnostic carries the `\r` escape INSIDE a
        // string, where the CR is content — and NO machine fix in token
        // position, where a deletion glues lines on a CR-terminated file.
        let (_, diags) = parse_to_ast_all("service App:\n    tag = \"a\rb\"\n");
        let cr: Vec<String> = diags
            .iter()
            .map(|d| d.to_string())
            .filter(|m| m.contains("NML0016"))
            .collect();
        assert_eq!(cr.len(), 1, "{cr:?}");
        assert!(cr[0].contains("(fix: `\\r`)"), "{cr:?}");
        let (_, diags) = parse_to_ast_all("service App:\r    port = 1\n");
        let cr: Vec<String> = diags
            .iter()
            .map(|d| d.to_string())
            .filter(|m| m.contains("NML0016"))
            .collect();
        assert_eq!(cr.len(), 1, "{cr:?}");
        assert!(
            !cr[0].contains("(fix:"),
            "no machine fix in token position: {cr:?}"
        );
    }

    /// A leading U+FEFF is a byte-order mark: accepted, filed as trivia
    /// (lossless), and invisible to the policy. Everywhere else it is
    /// NML0018.
    /// The cap coincidence made structural (r30 F4): with ≥128 lexer
    /// errors ahead of a stray quote, the UnterminatedString entry is
    /// suppressed past MAX_ERRORS and the A2 guard goes blind — but the
    /// rendered window truncates at the SAME constant on the
    /// position-sorted merge, and lexer errors never occur inside a
    /// string token's range, so the evictors always sort first and no
    /// guard-blind in-string fix can ever render. If the two caps
    /// diverge, this fails.
    #[test]
    fn a_flooded_stray_quote_still_grants_no_in_string_fix() {
        // 130 unexpected-character errors, then a stray quote, then
        // transport CRs the blind flip would have granted `\r` fixes.
        let mut src = String::from("service App:\n");
        for _ in 0..130 {
            src.push_str("    ~\n");
        }
        src.push_str("    a = \"stray x\r    b = 2\r");
        let (_, diags) = parse_to_ast_all(&src);
        assert!(
            diags.iter().all(|d| !d.to_string().contains("(fix")),
            "no fix may render behind a suppressed unterminated-string error"
        );
        assert!(
            diags.iter().any(|d| d.to_string().contains("suppressed")),
            "the flood must report suppression honestly: {} diags",
            diags.len()
        );
    }

    /// The judge's size cap (D-B): a string token over
    /// `MAX_JUDGED_TOKEN_BYTES` is never decode-judged — the policy
    /// diagnostic stands with NO machine fix (fail-closed, the token
    /// text never decoded); the same content under the cap keeps its
    /// singular escape fix. Boundary-exact: the cap itself is judged,
    /// one byte past it is not.
    #[test]
    fn an_oversized_string_token_refuses_machine_repair_and_keeps_the_diagnostic() {
        let cap = 64 * 1024;
        let rendered_0017 = |src: &str| -> Vec<String> {
            let (_, diags) = parse_to_ast_all(src);
            diags
                .iter()
                .map(|d| d.to_string())
                .filter(|m| m.contains("NML0017"))
                .collect()
        };
        // Token text = `"` + filler + C0 + `"`; filler sized so the
        // token is exactly one byte OVER the cap…
        let over = format!("service App:\n    x = \"{}\u{1}\"\n", "a".repeat(cap - 2));
        let hits = rendered_0017(&over);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(
            !hits[0].contains("(fix"),
            "an over-cap token must refuse machine repair: {}…",
            &hits[0][..120.min(hits[0].len())]
        );
        // …and exactly AT the cap, where the fix is granted.
        let at = format!("service App:\n    x = \"{}\u{1}\"\n", "a".repeat(cap - 3));
        let hits = rendered_0017(&at);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(
            hits[0].contains("(fix: `\\u{1}`)"),
            "at-cap content keeps its singular escape fix: {}…",
            &hits[0][..120.min(hits[0].len())]
        );
    }

    /// The in-string repair taxonomy (RFC 0023 D1): each ambiguity
    /// class of NML0017/NML0018 content gets exactly its enumerated
    /// resolution space — singular where one reading is provable,
    /// alternatives where intent is genuinely open, and none at all in
    /// token position, where any rewrite is a structural guess.
    #[test]
    fn in_string_policy_characters_carry_their_repair_taxonomy() {
        let rendered_for = |src: &str, code: &str| -> String {
            let (_, diags) = parse_to_ast_all(src);
            let hits: Vec<String> = diags
                .iter()
                .map(|d| d.to_string())
                .filter(|m| m.contains(code))
                .collect();
            assert_eq!(hits.len(), 1, "{src:?}: {diags:?}");
            hits[0].clone()
        };

        // C0 in a string: the escape is the ONE value-preserving
        // reading — singular, auto-appliable (NML0016's `\r` shape).
        let m = rendered_for("service App:\n    x = \"a\u{1}b\"\n", "NML0017");
        assert!(m.contains("(fix: `\\u{1}`)"), "{m}");

        // The five unmapped C1 bytes (CP-1252 holes) are singular too.
        let m = rendered_for("service App:\n    x = \"a\u{90}b\"\n", "NML0017");
        assert!(m.contains("(fix: `\\u{90}`)"), "{m}");

        // NEL: the one character in BOTH ambiguity classes — line
        // break | kept byte | mojibake ellipsis. Three alternatives,
        // rendered in full (no truncation tail), never auto-applied.
        let m = rendered_for("service App:\n    x = \"a\u{85}b\"\n", "NML0017");
        assert!(m.contains("(fixes: `\\n`, `\\u{85}`, `…`)"), "{m}");
        assert!(!m.contains("more"), "three renders whole: {m}");

        // A mapped C1 byte: keep it escaped, or repair the
        // double-decode to what the CP-1252 author typed.
        let m = rendered_for("service App:\n    x = \"a\u{93}b\"\n", "NML0017");
        assert!(m.contains("(fixes: `\\u{93}`, `\u{201C}`)"), "{m}");

        // LS/PS: a line break was meant, or the separator itself.
        let m = rendered_for("service App:\n    x = \"a\u{2028}b\"\n", "NML0018");
        assert!(m.contains("(fixes: `\\n`, `\\u{2028}`)"), "{m}");

        // Bidi controls, interior FEFF, tags: remove | keep escaped —
        // and the deletion alternative reads `remove`, in the singular
        // arm's vocabulary, never an empty backtick pair.
        let m = rendered_for("service App:\n    x = \"a\u{202E}b\"\n", "NML0018");
        assert!(m.contains("(fixes: remove, `\\u{202E}`)"), "{m}");
        let m = rendered_for("service App:\n    x = \"a\u{E0067}b\"\n", "NML0018");
        assert!(m.contains("(fixes: remove, `\\u{E0067}`)"), "{m}");
        let m = rendered_for("service App:\n    x = \"a\u{FEFF}b\"\n", "NML0018");
        assert!(m.contains("(fixes: remove, `\\u{FEFF}`)"), "{m}");

        // The in-string reading is granted by DECODE-JUDGED soundness
        // (`escape_preserves_value`): the escape must leave the decoded
        // value byte-identical. Every corrupting geometry refuses for
        // exactly the reason it corrupts — blank edge lines are dropped
        // from the value (r28: an applied `\r` fix turned `"body"`
        // into `"body\n\r"` with a clean post-state), a blank middle
        // line's blankness can hold min-indent up (r29: escaping it
        // re-indented every line), the opening line's padding is
        // dropped, and a preceding backslash glues the spliced escape
        // into different text. Content lines keep their repairs.
        let no_fix = |src: &str, code: &str| {
            let (_, diags) = parse_to_ast_all(src);
            let hits: Vec<String> = diags
                .iter()
                .map(|d| d.to_string())
                .filter(|m| m.contains(code))
                .collect();
            assert!(!hits.is_empty(), "{src:?}: {diags:?}");
            assert!(
                hits.iter().all(|m| !m.contains("(fix")),
                "no repair in a dropped edge line: {src:?}: {hits:?}"
            );
        };
        // Aligned blank closing line (no NML0020 in play): bare CR, NEL.
        no_fix(
            "service App:\n    doc = \"\"\"\n        body\n        \u{D}\"\"\"\n",
            "NML0016",
        );
        no_fix(
            "service App:\n    doc = \"\"\"\n        body\n        \u{85}\"\"\"\n",
            "NML0017",
        );
        // Blank opening line (CR not followed by LF).
        no_fix(
            "service App:\n    doc = \"\"\"\u{D}  \n        body\n        \"\"\"\n",
            "NML0016",
        );
        // r29 geometries. A blank middle line whose indent is BELOW the
        // content's min-indent: escaping makes it participate and drops
        // min for every line (the whole value re-indents) — refused.
        no_fix(
            "service App:\n    doc = \"\"\"\n        body\n \u{D} \n        more\n        \"\"\"\n",
            "NML0016",
        );
        // The empty-min corner: the ONLY body line is blank; escaping
        // it changes min from 0 to its own indent — refused.
        no_fix(
            "service App:\n    doc = \"\"\"\n \u{D} \n\"\"\"\n",
            "NML0016",
        );
        // A non-blank opening line's leading PADDING is dropped by the
        // NML0019 recovery; an escape there would join the value with
        // the padding after it — refused.
        no_fix(
            "service App:\n    doc = \"\"\" \u{C}hey\n        world\n        \"\"\"\n",
            "NML0017",
        );
        // A raw backslash before the character glues the spliced escape
        // into different text (`a\\` + `\\u{1}` reads as an escaped
        // backslash then literal `u{1}`) — refused.
        no_fix("service App:\n    x = \"a\\\u{1}b\"\n", "NML0017");
        // A blank middle line ALIGNED with the content's min-indent is
        // geometry-inert: its bytes are value bytes and the escape is
        // exact — the repair stays (over-narrowing guard).
        let m = rendered_for(
            "service App:\n    doc = \"\"\"\n    body\n    \u{D} \n    more\n    \"\"\"\n",
            "NML0016",
        );
        assert!(m.contains("(fix: `\\r`)"), "{m}");
        // Opening-line CONTENT (after the dropped padding) is value
        // bytes — the repair stays.
        let m = rendered_for(
            "service App:\n    doc = \"\"\"x\u{1}y\n        world\n        \"\"\"\n",
            "NML0017",
        );
        assert!(m.contains("(fix: `\\u{1}`)"), "{m}");

        // CONTENT lines keep their repairs — the middle of the body and
        // a non-blank closing line are value bytes (over-narrowing guard).
        let m = rendered_for(
            "service App:\n    doc = \"\"\"\n        a\u{1}b\n        \"\"\"\n",
            "NML0017",
        );
        assert!(m.contains("(fix: `\\u{1}`)"), "{m}");
        let m = rendered_for(
            "service App:\n    doc = \"\"\"\n        body\n        x\u{1}\"\"\"\n",
            "NML0017",
        );
        assert!(m.contains("(fix: `\\u{1}`)"), "{m}");

        // Behind an UNTERMINATED string (the A2 guard) the string
        // reading is uncertain, so the in-string taxonomy must stay
        // silent for EVERY policy kind — a bidi control swallowed by a
        // stray quote gets the diagnostic, never the remove|escape
        // alternatives.
        let (_, diags) = parse_to_ast_all("service App:\n    a = \"stray\n    b = \"x\u{202E}y\n");
        let bidi: Vec<String> = diags
            .iter()
            .map(|d| d.to_string())
            .filter(|m| m.contains("NML0018"))
            .collect();
        assert!(!bidi.is_empty(), "{diags:?}");
        assert!(
            bidi.iter().all(|m| !m.contains("(fix")),
            "no repair behind an uncertain string reading: {bidi:?}"
        );

        // TOKEN position: no repair, either class — the character is
        // structure there, and a rewrite would be a guess.
        for src in [
            "service App:\n    x\u{85} = 1\n",
            "service App:\n    x\u{2028} = 1\n",
            "service App:\n    x\u{E0067} = 1\n",
            "service App:\n    x\u{1} = 1\n",
        ] {
            let (_, diags) = parse_to_ast_all(src);
            for d in &diags {
                let m = d.to_string();
                assert!(
                    !m.contains("(fix"),
                    "token position carries no repair: {src:?}: {m}"
                );
            }
        }
    }

    /// The sentinel-judged remove grant (D-C): the *remove* arm of a
    /// bidi/FEFF/tag character's enumerated alternatives is offered
    /// ONLY where deletion provably removes just that character — the
    /// sentinel judgment (decode with a marker escape vs. decode with
    /// the deletion) plus the delete-splice lex-integrity clause (the
    /// spliced text must stay ONE clean String token). Where either
    /// refuses, the set COLLAPSES to the singular escape fix — one
    /// suggestion, auto-appliable, never a phantom remove.
    #[test]
    fn the_remove_arm_is_sentinel_judged_and_collapses_where_unsound() {
        use crate::diagnostic::codes;
        let d18 = |src: &str| -> Vec<crate::diagnostic::Diagnostic> {
            let (_, diags) = parse_to_ast_all(src);
            diags
                .into_iter()
                .filter(|d| d.code == Some(codes::INVISIBLE_CHARACTER))
                .collect()
        };
        let escape_only = |src: &str, escape: &str| {
            let hits = d18(src);
            assert_eq!(hits.len(), 1, "{src:?}: {hits:?}");
            let suggestions = &hits[0].suggestions;
            assert_eq!(
                suggestions.len(),
                1,
                "an unsound removal must collapse to the escape alone: {src:?}: {suggestions:?}"
            );
            assert_eq!(suggestions[0].replacement, escape, "{src:?}");
        };
        let remove_and_escape = |src: &str, escape: &str| {
            let hits = d18(src);
            assert_eq!(hits.len(), 1, "{src:?}: {hits:?}");
            let suggestions = &hits[0].suggestions;
            assert_eq!(suggestions.len(), 2, "{src:?}: {suggestions:?}");
            assert!(
                suggestions.iter().any(|s| s.replacement.is_empty()),
                "sound removal keeps the remove arm: {src:?}: {suggestions:?}"
            );
            assert!(
                suggestions.iter().any(|s| s.replacement == escape),
                "{src:?}: {suggestions:?}"
            );
        };

        // Sentinel EXHAUSTION: a value already holding all sixteen
        // candidates U+E000–E00F leaves the picker nothing to mark
        // with — the judgment refuses fail-closed and the set
        // collapses, even in an otherwise-safe position. (The escape
        // still auto-applies: exhaustion costs the remove arm, never
        // the repair — the error-index NML0018 sentence states this
        // split.)
        let all16: String = ('\u{E000}'..='\u{E00F}').collect();
        escape_only(
            &format!("service App:\n    x = \"{all16}a\u{FEFF}b\"\n"),
            "\\u{FEFF}",
        );
        // Picker SKIP: only U+E000 taken — the picker moves to U+E001
        // and the sound remove arm survives.
        remove_and_escape(
            "service App:\n    x = \"\u{E000}a\u{FEFF}b\"\n",
            "\\u{FEFF}",
        );

        // P1 — last-line blank flip: deleting the FEFF turns the final
        // body line blank, which the edge trim then DROPS from the
        // value ("body\n⟨char⟩" would silently become "body"). The
        // sentinel judgment refuses; the escape stands alone.
        escape_only(
            "service App:\n    doc = \"\"\"\n        body\n        \u{FEFF}\"\"\"\n",
            "\\u{FEFF}",
        );
        // P7 — middle-line min-indent flip: the FEFF's line holds the
        // body's min-indent DOWN (indent 1 under content at 8);
        // deleting it turns the line blank, min-indent jumps, and the
        // whole value re-indents. Refused; escape alone.
        escape_only(
            "service App:\n    doc = \"\"\"\n        body\n \u{FEFF}\n        more\n        \"\"\"\n",
            "\\u{FEFF}",
        );
        // P2 — quote merge: the FEFF separates `""` from `"`; deleting
        // it glues them into a CLOSING `\"\"\"` and the rest of the file
        // is re-tokenized (decode of the old token text cannot see
        // this — the delete-splice still decodes to the "right" value,
        // which is exactly why the lex-integrity clause exists).
        // Refused; escape alone.
        let p2 = "service App:\n    doc = \"\"\"\n        a\n        \"\"\u{FEFF}\"\n        \"\"\"\n    port = 1\n";
        escape_only(p2, "\\u{FEFF}");
        // …and applying that ONE offered fix leaves a file that parses
        // clean with the value preserved (the escape spells the same
        // character the raw byte carried).
        let hits = d18(p2);
        let sug = &hits[0].suggestions[0];
        let mut fixed = String::new();
        fixed.push_str(&p2[..sug.span.start]);
        fixed.push_str(&sug.replacement);
        fixed.push_str(&p2[sug.span.end..]);
        let (_, diags) = parse_to_ast_all(&fixed);
        assert!(
            diags.is_empty(),
            "the applied escape must land clean: {diags:?}"
        );
        let before = decode_value(&first_value_node(&parse(p2).syntax())).expect("value");
        let after = decode_value(&first_value_node(&parse(&fixed).syntax())).expect("value");
        assert_eq!(before.value, after.value, "the fix preserves the value");
        // The four-quote corner: `\"\"` ⟨char⟩ `\"\"` — deletion makes
        // `\"\"\"\"`, an early close plus a stray quote. Same refusal.
        escape_only(
            "service App:\n    doc = \"\"\"\n        a\n        \"\"\u{FEFF}\"\"\n        \"\"\"\n    port = 1\n",
            "\\u{FEFF}",
        );
        // P12 — CR glue: the FEFF sits between a bare CR and an LF;
        // deleting it fuses them into a CRLF line ending, and the CR
        // silently leaves the VALUE (transport now, content before).
        // The sentinel judgment refuses; escape alone.
        escape_only(
            "service App:\n    doc = \"\"\"\n        x\r\u{FEFF}\n        y\n        \"\"\"\n",
            "\\u{FEFF}",
        );

        // Sound geometries keep the full enumeration — a mid-paragraph
        // FEFF in a multiline body, and a single-line bidi control.
        remove_and_escape(
            "service App:\n    doc = \"\"\"\n        a\u{FEFF}b\n        \"\"\"\n",
            "\\u{FEFF}",
        );
        remove_and_escape("service App:\n    x = \"a\u{202E}b\"\n", "\\u{202E}");
    }

    #[test]
    fn leading_bom_accepted_interior_feff_rejected() {
        let src = "\u{FEFF}service App:\n    port = 1\n";
        let p = parse(src);
        assert!(p.errors().is_empty(), "{:?}", p.errors());
        assert_eq!(tree_text(&p), src, "BOM stays in the lossless tree");
        let (_, diags) = parse_to_ast_all(src);
        assert!(diags.is_empty(), "{diags:?}");

        let (_, diags) = parse_to_ast_all("service App:\n    x = \"\u{FEFF}\"\n");
        assert!(diags.iter().any(|d| d.to_string().contains("NML0018")));
    }

    /// `\u{…}` and `\r` decode end-to-end; every malformed shape gets its
    /// precise teaching message (NML0012), spanned from the backslash.
    #[test]
    fn value_decode_unicode_and_cr_escapes() {
        use crate::types::Value;
        assert_eq!(
            decode_first("\"H\\u{E9}\\u{1F389}\\r\\u{0}\""),
            Value::String("Hé🎉\r\u{0}".into())
        );
        for (body, needle) in [
            ("\\uX", "must be followed by `{`"),
            ("\\u{12", "unterminated"),
            ("\\u{}", "empty"),
            ("\\u{1234567}", "overlong"),
            ("\\u{12G}", "expected a hex digit"),
            ("\\u{D800}", "not a Unicode scalar"),
            ("\\u{110000}", "not a Unicode scalar"),
        ] {
            let e = parse_to_ast(&format!("service App:\n    x = \"{body}\"\n")).expect_err(body);
            assert!(e.message().contains(needle), "{body}: {}", e.message());
        }
    }

    // ── RFC 0032: field directives ──

    /// `#name` / `#name(value)` parse, stay lossless, and extract onto
    /// `FieldDef.directives` in source order with decoded args and spans.
    #[test]
    fn directives_parse_losslessly_and_extract() {
        let src = "model server:\n    ceiling capabilitySet #live\n    hosts set<string> #live #key(\"host\")\n";
        let p = parse(src);
        assert!(p.errors().is_empty(), "parse errors: {:?}", p.errors());

        let (schema, errs) = extract_schema(src);
        assert!(errs.is_empty(), "extract errors: {errs:?}");
        let fields = &schema.models[0].fields;
        assert_eq!(fields[0].directives.len(), 1);
        assert_eq!(fields[0].directives[0].name, "live");
        assert!(
            fields[0].directives[0].arg.is_none(),
            "bare directive has no arg"
        );

        assert_eq!(fields[1].directives.len(), 2, "source order, both kept");
        assert_eq!(fields[1].directives[0].name, "live");
        assert_eq!(fields[1].directives[1].name, "key");
        match &fields[1].directives[1].arg {
            Some(sv) => assert_eq!(
                sv.value,
                crate::types::Value::String("host".to_string()),
                "argument decodes"
            ),
            None => panic!("#key(\"host\") must carry its argument"),
        }
        // A field with no directives extracts empty (control).
        let (plain, _) = extract_schema("model m:\n    x string\n");
        assert!(plain.models[0].fields[0].directives.is_empty());
    }

    /// Directive syntax rules: duplicates are one-per-key errors, a bare `#`
    /// needs a name, and a NEXT-LINE `#` never attaches to the previous field
    /// (line significance) — it is an error where it stands.
    #[test]
    fn directive_syntax_rules_are_enforced() {
        let msgs = |src: &str| {
            parse(src)
                .errors()
                .iter()
                .map(|e| e.message().to_string())
                .collect::<Vec<_>>()
        };
        assert!(
            msgs("model m:\n    x string #live #live\n")
                .iter()
                .any(|m| m.contains("duplicate directive")),
            "same key twice on one field is an error"
        );
        assert!(
            msgs("model m:\n    x string #\n")
                .iter()
                .any(|m| m.contains("expected a directive name")),
            "bare '#' needs a name"
        );
        // Next-line '#' does not silently attach: the field parses clean and
        // the stray line errors on its own.
        let next_line = "model m:\n    x string\n    #live\n";
        assert!(
            !msgs(next_line).is_empty(),
            "a next-line directive must not attach silently"
        );
        let (schema, _) = extract_schema(next_line);
        assert!(
            schema.models[0].fields[0].directives.is_empty(),
            "the previous field must NOT have picked the directive up"
        );
    }

    /// Modifier TYPE declarations are fields too (nudge's reloadable `|block`
    /// is one), so they take directives — parsed, extracted onto the modifier
    /// FieldDef, dup-checked like any field.
    #[test]
    fn modifier_declarations_take_directives() {
        let src = "model sandboxCeiling:\n    |block []string? #live\n";
        let p = parse(src);
        assert!(p.errors().is_empty(), "parse errors: {:?}", p.errors());
        let (schema, errs) = extract_schema(src);
        assert!(errs.is_empty(), "extract errors: {errs:?}");
        let field = &schema.models[0].fields[0];
        assert_eq!(field.name, "block");
        assert_eq!(field.directives.len(), 1);
        assert_eq!(field.directives[0].name, "live");
        // Dup rule applies to modifiers too.
        assert!(
            parse("model m:\n    |b []string #live #live\n")
                .errors()
                .iter()
                .any(|e| e.message().contains("duplicate directive")),
            "modifier dup-directive is an error"
        );
    }

    // ── RFC 0032: the `set<T>` type constructor ──

    /// Adversarial edges: malformed constructor syntax must error *sanely*
    /// (diagnostics, never a panic), pathological nesting must hit the depth
    /// guard (never the stack — untrusted config reaches this parser), and
    /// every form must stay lossless + extractable.
    #[test]
    fn set_adversarial_edges_error_sanely_never_panic() {
        let cases = [
            "model m:\n    xs set<>\n",               // empty angles
            "model m:\n    xs set<string\n",          // unclosed
            "model m:\n    xs set<string, number>\n", // comma is not a separator
            "model m:\n    xs set<string>>\n",        // stray closer
            "model m:\n    xs set<set<string>\n",     // half-closed nesting
            "model m:\n    xs set\n",                 // bare `set` = model ref, no error here
        ];
        for src in cases {
            let p = parse(src);
            let _ = extract_schema(src); // must not panic either
            if src.contains("xs set\n") {
                assert!(p.errors().is_empty(), "bare `set` is a legal model ref");
            } else {
                assert!(!p.errors().is_empty(), "{src:?} must produce a diagnostic");
            }
        }
        // Depth bomb: deeply nested constructors trip the recursion guard with
        // a diagnostic — the guard, not the stack, is the bound.
        let deep = format!(
            "model m:\n    xs {}string{}\n",
            "set<".repeat(300),
            ">".repeat(300)
        );
        let p = parse(&deep);
        assert!(
            p.errors()
                .iter()
                .any(|e| e.message().contains("nesting depth")),
            "deep nesting must be depth-guarded"
        );
    }

    /// `set<T>` parses without errors, stays byte-faithful in the lossless
    /// tree, and extracts to `FieldType::Set`.
    #[test]
    fn set_type_parses_losslessly_and_extracts() {
        use crate::model::FieldType;
        use crate::types::PrimitiveType;
        let src = "model m:\n    tags set<string>\n";
        let p = parse(src);
        assert!(p.errors().is_empty(), "parse errors: {:?}", p.errors());

        let (schema, errs) = extract_schema(src);
        assert!(errs.is_empty(), "extract errors: {errs:?}");
        let ft = &schema.models[0].fields[0].field_type;
        assert!(
            matches!(ft, FieldType::Set(inner)
                if matches!(**inner, FieldType::Primitive { ty: PrimitiveType::String, .. })),
            "expected set<string>, got {ft}"
        );
        assert_eq!(ft.to_string(), "set<string>", "canonical Display");
    }

    /// Bare unions are canonical inside the angles (`set<a | b>`), and the
    /// redundantly-parenthesized form extracts to the SAME type (fmt strips
    /// the parens; the type system never sees a difference).
    #[test]
    fn set_union_bare_and_parenthesized_extract_identically() {
        let shape = |src: &str| {
            let (schema, errs) = extract_schema(src);
            assert!(errs.is_empty(), "{src}: {errs:?}");
            schema.models[0].fields[0].field_type.to_string()
        };
        let bare = shape("model m:\n    xs set<string | number>\n");
        let parens = shape("model m:\n    xs set<(string | number)>\n");
        assert_eq!(bare, "set<string | number>", "bare union is canonical");
        assert_eq!(bare, parens, "grouping parens are semantically inert");
    }

    /// Nesting composes (`set<set<string>>` — no `>>` munch hazard), and a
    /// spaced `set <string>` parses identically (fmt canonicalizes).
    #[test]
    fn set_nesting_and_spacing_parse() {
        for src in [
            "model m:\n    xs set<set<string>>\n",
            "model m:\n    xs set <string>\n",
            "model m:\n    xs []set<string>\n",
            "model m:\n    xs set<[]string>\n",
            "model m:\n    xs set<string>?\n",
        ] {
            let p = parse(src);
            assert!(p.errors().is_empty(), "{src}: {:?}", p.errors());
        }
    }

    /// Constructor-name errors are targeted: unknown names and the reserved
    /// `map` get their own guidance, and the arms-parens rule inside angles
    /// points at the fix.
    #[test]
    fn set_constructor_errors_are_targeted() {
        let msgs = |src: &str| {
            parse(src)
                .errors()
                .iter()
                .map(|e| e.message().to_string())
                .collect::<Vec<_>>()
        };
        assert!(
            msgs("model m:\n    xs foo<string>\n")
                .iter()
                .any(|m| m.contains("unknown type constructor")),
            "unknown constructor gets a targeted error"
        );
        assert!(
            msgs("model m:\n    xs map<string>\n")
                .iter()
                .any(|m| m.contains("reserved for a future map type")),
            "map is reserved with its own guidance"
        );
        assert!(
            msgs("model m:\n    xs set<string, number>\n")
                .iter()
                .any(|m| m.contains("separated by '|'")),
            "comma (the map-habit typo) points at the pipe fix"
        );
        assert!(
            msgs("model m:\n    xs set<role -> denial>\n")
                .iter()
                .any(|m| m.contains("set<(K -> V)>")),
            "bare arms inside angles point at the parenthesized fix"
        );
    }

    // ───────────────────────────────────────────────────────────────
    // Tree-builder timing gates (RFC 0004 §4.3 comment attachment).
    // Generated in-process (a 200k-line file has no business in the
    // repo); bounds are catastrophic-regression tripwires for the two
    // named complexity traps in `build_tree` — a per-comment rescan of
    // the trivia run (`dedent_ends_run` is probed once per run) and a
    // front-removal release of held comments (`deferred` is a deque).
    // Each shape costs ~10–20 ms fixed in release; the trap cost ~20 s.
    // Deliberately loose (absolutes drift across machines). `--ignored`
    // only: `cargo test -p nml-core --release --lib -- --ignored perf_`.
    // ───────────────────────────────────────────────────────────────

    /// Number of generated comment lines per timing gate.
    const PERF_COMMENT_LINES: usize = 200_000;

    /// Parse `src` under `bound`, and pin the two properties the gate must not
    /// buy speed with: losslessness and a clean parse.
    fn perf_parse(name: &str, src: &str, bound: std::time::Duration) {
        let started = std::time::Instant::now();
        let parsed = parse(src);
        let elapsed = started.elapsed();
        assert!(
            parsed.errors().is_empty(),
            "{name}: parse must be clean: {:?}",
            parsed.errors()
        );
        assert_eq!(
            parsed.syntax().text().to_string(),
            src,
            "{name}: stays lossless"
        );
        assert!(
            elapsed < bound,
            "{name}: parsed in {elapsed:?}, over the {bound:?} tripwire \
             — a tree-builder complexity trap regressed (RFC 0004 §4.3)"
        );
    }

    /// A body-indented own-line comment run ending at the body's last entry:
    /// every comment sits before the same closing dedent (the rescan trap).
    #[test]
    #[ignore = "timing gate — run with --release -- --ignored"]
    fn perf_body_comment_run_parses_within_bounds() {
        let src = format!(
            "service App:\n{}    port = 1\n",
            "    // p\n".repeat(PERF_COMMENT_LINES)
        );
        perf_parse("body-comment-run", &src, std::time::Duration::from_secs(2));
    }

    /// A column-0 own-line comment run between two declarations: every comment
    /// is deferred past the closing dedent and released together (the
    /// front-removal trap).
    #[test]
    #[ignore = "timing gate — run with --release -- --ignored"]
    fn perf_deferred_comment_run_parses_within_bounds() {
        let src = format!(
            "service App:\n    port = 1\n{}\nservice B:\n    port = 2\n",
            "// p\n".repeat(PERF_COMMENT_LINES)
        );
        perf_parse(
            "deferred-comment-run",
            &src,
            std::time::Duration::from_secs(2),
        );
    }

    /// A column-0 comment run before the first declaration — the license-header
    /// / manifest shape RFC 0019 item 0 parses on every invocation.
    #[test]
    #[ignore = "timing gate — run with --release -- --ignored"]
    fn perf_header_comment_run_parses_within_bounds() {
        let src = format!(
            "{}service S:\n    port = 1\n",
            "// p\n".repeat(PERF_COMMENT_LINES)
        );
        perf_parse(
            "header-comment-run",
            &src,
            std::time::Duration::from_secs(2),
        );
    }

    /// The shape-aware verdict at each shape's edges. The point is the
    /// STRONGER rule a content window gets: the byte rule alone (in bounds,
    /// on character boundaries) admits a window equal to the WHOLE token,
    /// and splicing that over a literal's delimiters rewrites `"GE" -> h`
    /// into `GET -> h` — so the window must also be strictly inside the
    /// token it carries.
    #[test]
    fn the_site_verdict_holds_each_shape_to_its_own_rule() {
        use crate::span::{Span, SpanShape, SpanSite};
        let src = "thing t:\n    v = \"caf\u{e9}\"\n";
        let boundaries = parse(src).token_boundaries();
        let token = Span::new(
            src.find('"').expect("the literal"),
            src.rfind('"').expect("the literal") + 1,
        );
        let window = Span::new(token.start + 1, token.end - 1);
        let ok = |site: SpanSite| boundaries.check(site, src);
        // A whole span: the token, not its inside.
        assert_eq!(ok(SpanSite::aligned("w", token)), Ok(()));
        assert!(ok(SpanSite::aligned("w", window)).is_err());
        // A content window: inside the delimiters, on character boundaries.
        assert_eq!(ok(SpanSite::window("c", window, token)), Ok(()));
        // The structurally ill-shaped sites are built as the STRUCT: the
        // constructor refuses them at the mint, and the check must refuse
        // them too, for a site built any other way.
        assert!(
            ok(SpanSite {
                kind: "c",
                span: token,
                shape: SpanShape::ContentWindow { token }
            })
            .is_err(),
            "a window equal to its token passes the byte rule and corrupts a fix"
        );
        assert!(
            ok(SpanSite {
                kind: "c",
                span: Span::new(token.start, window.end),
                shape: SpanShape::ContentWindow { token }
            })
            .is_err(),
            "a window that starts on the opening delimiter"
        );
        assert!(
            ok(SpanSite::window(
                "c",
                Span::new(window.start, window.end - 1),
                token
            ))
            .is_err(),
            "the literal's last character is two bytes: its middle is no boundary"
        );
        assert!(
            ok(SpanSite {
                kind: "c",
                span: window,
                shape: SpanShape::ContentWindow { token: window }
            })
            .is_err(),
            "a window whose token is not itself token-aligned"
        );
        // A template expression: inside its token, delimiters included.
        assert_eq!(ok(SpanSite::expression("e", window, token)), Ok(()));
        assert_eq!(ok(SpanSite::expression("e", token, token)), Ok(()));
        assert!(
            ok(SpanSite::expression("e", Span::new(0, token.end), token)).is_err(),
            "an expression reaching outside its token"
        );
        // Out of bounds, whatever the shape.
        let past = Span::new(src.len(), src.len() + 1);
        assert!(ok(SpanSite::aligned("w", past)).is_err());
        assert!(
            ok(SpanSite {
                kind: "c",
                span: past,
                shape: SpanShape::ContentWindow { token }
            })
            .is_err()
        );
        assert!(ok(SpanSite::expression("e", past, token)).is_err());
    }

    /// A missing fallback arm (`"a" |`) recovers to a token-less node: its
    /// span is a POSITION (the empty span at its start), never its raw
    /// extent over the trivia after the bar, and the fallback's merged span
    /// stays the `"a"` token — so every span of the best-effort tree is
    /// token-aligned, as the `document` fuzz target holds for every input.
    #[test]
    fn a_missing_fallback_arm_keeps_every_span_token_aligned() {
        use super::ast::AstNode as _;
        for src in [
            "service App:\n    k = \"a\" |\n    m = 1\n",
            "service App:\n    k = \"a\" | \n",
            "service App:\n    k = \"a\" | // gone\n    m = 1\n",
            "service App:\n    k = \"a\" |\n",
        ] {
            let parsed = parse(src);
            let root = super::ast::Root::cast(parsed.syntax()).expect("a Root");
            let (file, lowered_errors, _) = super::lower::to_ast_with_errors(&root);
            assert!(
                !parsed.errors().is_empty() || !lowered_errors.is_empty(),
                "the missing arm is an error: {src:?}"
            );
            let boundaries = parsed.token_boundaries();
            let mut sites = 0;
            crate::ast::for_each_span(&file, &mut |site| {
                sites += 1;
                // Each site by the shape its emitter declared — the same
                // verdict `content_spans` and the `document` fuzz target read.
                if let Err(why) = boundaries.check(site, src) {
                    panic!(
                        "{} span {:?} ({:?}) {why} in {src:?}",
                        site.kind,
                        site.span,
                        src.get(site.span.start..site.span.end)
                    );
                }
            });
            assert!(sites > 0, "{src:?}");
            // The chain ends at its line: the fallback's content span is the
            // `"a"` token alone in EVERY shape, a following line included
            // (the chain never takes that line's name as its arm —
            // `a_fallback_chain_ends_at_its_line`).
            let crate::ast::DeclarationKind::Block(block) = &file.declarations[0].kind else {
                panic!("a block: {src:?}");
            };
            let crate::ast::BodyEntryKind::Property(property) = &block.body.entries[0].kind else {
                panic!("a property: {src:?}");
            };
            let span = property.value.span;
            assert_eq!(
                &src[span.start..span.end],
                "\"a\"",
                "the value's span is the primary alone, the missing arm a position: {src:?}"
            );
        }
    }

    /// A fallback chain never crosses a line break (RFC 0026 decision 2): a
    /// `|` that ends a line has no arm — ONE NML0002 at the pipe, naming the
    /// line break (or the end of the file) — and the next line parses as its
    /// own entry, never as the arm. The chain used to continue past the break
    /// (`m` swallowed as the arm, `= 1` reported twice on line 3). In list
    /// position the chain is NML0021 once and the next item is its own.
    #[test]
    fn a_fallback_chain_ends_at_its_line() {
        use super::ast::AstNode as _;
        let entries = |src: &str| -> (Vec<NmlError>, crate::ast::File) {
            let parsed = parse(src);
            let root = super::ast::Root::cast(parsed.syntax()).expect("a Root");
            let (file, lowered, _) = super::lower::to_ast_with_errors(&root);
            let mut errors: Vec<NmlError> = parsed.errors().to_vec();
            errors.extend(lowered);
            (errors, file)
        };
        for (src, found) in [
            ("service App:\n    k = \"a\" |\n    m = 1\n", "a line break"),
            (
                "service App:\n    k = \"a\" | // gone\n    m = 1\n",
                "a line break",
            ),
            ("service App:\n    k = \"a\" |\n", "a line break"),
            ("service App:\n    k = \"a\" |", "end of file"),
        ] {
            let (errors, file) = entries(src);
            assert_eq!(errors.len(), 1, "one row: {src:?} {errors:?}");
            let pipe = src.find('|').expect("the pipe");
            assert_eq!(
                errors[0].span(),
                crate::span::Span::new(pipe, pipe + 1),
                "at the pipe: {src:?}"
            );
            assert_eq!(
                errors[0].message(),
                format!("expected a value after `|`, found {found}"),
                "{src:?}"
            );
            let crate::ast::DeclarationKind::Block(block) = &file.declarations[0].kind else {
                panic!("a block: {src:?}");
            };
            let names: Vec<&str> = block
                .body
                .entries
                .iter()
                .filter_map(|e| match &e.kind {
                    crate::ast::BodyEntryKind::Property(p) => Some(p.name.name.as_str()),
                    _ => None,
                })
                .collect();
            let want: &[&str] = if src.contains("m = 1") {
                &["k", "m"]
            } else {
                &["k"]
            };
            assert_eq!(names, want, "the next line is its own entry: {src:?}");
        }
        // List position: one NML0021 for the chain, the next item its own.
        let (errors, file) = entries("[]a b:\n    - \"a\" |\n    - \"b\"\n");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(
            errors[0].to_diagnostic().code,
            Some(crate::diagnostic::codes::FALLBACK_IN_LIST_ITEM),
            "{errors:?}"
        );
        let crate::ast::DeclarationKind::Array(array) = &file.declarations[0].kind else {
            panic!("an array");
        };
        assert_eq!(array.body.items.len(), 2, "{array:?}");
    }

    /// A multiline (`"""`) template is segmented on its DECODED text — the
    /// dedented body, quotes gone — so the literal segments are the string's
    /// content and the expression is found where it is; segmenting the raw
    /// token would keep the indentation and the quote remnants as literal
    /// text.
    #[test]
    fn a_multiline_template_is_segmented_on_its_decoded_text() {
        use crate::types::{TemplateSegment, Value};
        let v = decode_first("\"\"\"\n    Hello {{ name }}\n    \"\"\"");
        let Value::TemplateString(segs) = v else {
            panic!("a template: {v:?}");
        };
        assert_eq!(
            crate::template::segments_to_string(&segs),
            "Hello {{ name }}"
        );
        assert_eq!(segs.len(), 2, "{segs:?}");
        assert!(
            matches!(&segs[0], TemplateSegment::Literal(l) if l == "Hello "),
            "{segs:?}"
        );
        assert!(
            matches!(
                &segs[1],
                TemplateSegment::Expression { namespace, path, .. }
                    if namespace == "name" && path.is_empty()
            ),
            "{segs:?}"
        );
    }
}
