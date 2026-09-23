//! The content-span invariant, pinned: every span every AST and schema node
//! carries begins and ends on content — over the whole document corpus
//! (every fixture, every documented example, every spec example) and over
//! the shapes that used to break it. The `document` fuzz target holds the
//! same property for arbitrary input.

use nml_core::span::{Span, SpanShape, SpanSite};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every `.nml` under the repository's fixture, docs and spec trees.
fn corpus() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "target" || n == "node_modules")
                {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|x| x == "nml") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    // `fuzz/seeds` too (r103-cov): the tracked seeds are hand-written
    // LANDMARK documents — the dedented block in a list body, the
    // quote-merge shapes, the layer clause — the exact shapes earlier
    // rounds found by construction, and they were the one `.nml` tree
    // this corpus did not walk.
    for dir in ["tests/fixtures", "docs", "spec", "fuzz/seeds"] {
        walk(&root.join(dir), &mut out);
    }
    out.sort();
    out
}

/// Every span site of both pipelines (the lowered AST, the extracted
/// schema), with the ones that break the invariant. The verdict comes from
/// the shared shape-aware judge ([`nml_core::cst::TokenBoundaries::check`])
/// — a whole span token-aligned, a content window on character boundaries
/// AND strictly inside its token, a template expression inside its token —
/// read off the SHAPE its emitter declared, never off the site's name. Two
/// clauses a terminated corpus can carry that arbitrary input cannot are
/// added here: a whole span begins and ends on content (the byte-level
/// proxy, exact once every token is closed), and a template expression's
/// bytes are exactly its `{{…}}`.
fn violations(source: &str) -> (usize, Vec<SpanSite>) {
    let mut sites = Vec::new();
    let (file, _) = nml_core::cst::parse_to_ast_all(source);
    nml_core::ast::for_each_span(&file, &mut |s| sites.push(s));
    let (_, schema, _, _) = nml_core::cst::parse_and_extract_split(source);
    nml_core::schema::for_each_span(&schema, &mut |s| sites.push(s));
    let boundaries = nml_core::cst::parse(source).token_boundaries();
    let total = sites.len();
    sites.retain(|s| {
        if boundaries.check(*s, source).is_err() {
            return true;
        }
        let bytes = source.get(s.span.start..s.span.end);
        let corpus_extra = match s.shape {
            SpanShape::Aligned => s.span.is_content_in(source),
            SpanShape::ContentWindow { .. } => true,
            SpanShape::TemplateExpression { .. } => {
                bytes.is_some_and(|b| b.starts_with("{{") && b.ends_with("}}"))
            }
        };
        !corpus_extra
    });
    (total, sites)
}

#[test]
fn every_span_in_the_corpus_is_a_content_span() {
    let files = corpus();
    assert!(files.len() > 100, "corpus too small: {}", files.len());
    let mut checked = 0usize;
    let mut bad: Vec<String> = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("corpus file");
        let (total, wrong) = violations(&source);
        checked += total;
        for site in wrong {
            bad.push(format!("{}: {} {:?}", path.display(), site.kind, site.span));
        }
    }
    assert!(
        checked > 100_000,
        "the corpus carries {checked} spans — expected far more"
    );
    assert!(
        bad.is_empty(),
        "{} span(s) off content:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// Every site of both pipelines, in emission order.
fn sites(source: &str) -> Vec<SpanSite> {
    let mut out = Vec::new();
    let (file, _) = nml_core::cst::parse_to_ast_all(source);
    nml_core::ast::for_each_span(&file, &mut |s| out.push(s));
    let (_, schema, _, _) = nml_core::cst::parse_and_extract_split(source);
    nml_core::schema::for_each_span(&schema, &mut |s| out.push(s));
    out
}

fn spans_of(source: &str, kind: &str) -> Vec<Span> {
    sites(source)
        .into_iter()
        .filter(|s| s.kind == kind)
        .map(|s| s.span)
        .collect()
}

/// The one site of `kind` in `source`.
fn one_site(source: &str, kind: &str) -> SpanSite {
    let found: Vec<SpanSite> = sites(source)
        .into_iter()
        .filter(|s| s.kind == kind)
        .collect();
    assert_eq!(found.len(), 1, "{source:?}: {kind} x{}", found.len());
    found[0]
}

fn text(source: &str, span: Span) -> &str {
    &source[span.start..span.end]
}

/// An UNTERMINATED string literal: its token runs to the line break with no
/// closing delimiter, so its content window has nothing to strip at the end.
/// The producer used to strip one byte anyway, which cut the last character
/// in half when it was multi-byte — the shape the `validate` fuzz target
/// crashed on (RFC 0026 decision 2). Every carrier is here: a property
/// value, an array element, a fallback leg, an arm selector and an arm
/// literal target.
#[test]
fn an_unterminated_literal_carries_a_character_boundary_content_window() {
    for src in [
        "thing a:\n    v = \"GE\u{85}",
        "thing a:\n    v = \"caf\u{e9}",
        "thing a:\n    v = \"\u{1f600}",
        "thing a:\n    v = \"\"\"\n    x\u{85}",
        "thing a:\n    v = [\"ok\", \"GE\u{85}",
        "thing a:\n    v = \"GE\u{85} | \"fallback\"\n",
        "thing a:\n    dispatch:\n        else -> \"x\u{85}",
        "thing a:\n    dispatch:\n        \"k\u{85} -> target\n",
        "oneof o by kind:\n    \"a\u{85} -> m\n",
    ] {
        let (total, wrong) = violations(src);
        assert!(total > 0, "{src:?}: nothing walked");
        assert!(
            wrong.is_empty(),
            "{src:?}: {} span(s) off content: {wrong:?}",
            wrong.len()
        );
    }
}

/// The shapes that used to carry a node span: indentation before a field,
/// the blank line and zero-width `Dedent` after a block, the space before a
/// directive, the space after a facet comma, a comment between a name and
/// its colon, CRLF line ends, and an empty body.
#[test]
fn spans_begin_and_end_on_content_at_every_shape() {
    let src = "model m:\n    port number(min = 1, max = 5) #live // note\n\n\nthing t: // c\n    v = 1\r\n\r\n";
    let (_, wrong) = violations(src);
    assert!(wrong.is_empty(), "{wrong:?}");
    let decls = spans_of(src, "Declaration");
    assert_eq!(
        text(src, decls[0]),
        "model m:\n    port number(min = 1, max = 5) #live"
    );
    assert_eq!(text(src, decls[1]), "thing t: // c\n    v = 1");
    assert_eq!(
        text(src, spans_of(src, "ModelDef")[0]),
        "model m:\n    port number(min = 1, max = 5) #live"
    );
    assert_eq!(
        text(src, spans_of(src, "FieldDef")[0]),
        "port number(min = 1, max = 5) #live"
    );
    assert_eq!(text(src, spans_of(src, "Directive")[0]), "#live");
    let facets = spans_of(src, "FacetExpr");
    assert_eq!(text(src, facets[0]), "min = 1");
    assert_eq!(text(src, facets[1]), "max = 5");
    assert_eq!(text(src, spans_of(src, "FacetBound.max")[0]), "max = 5");
    assert_eq!(text(src, spans_of(src, "BodyEntry")[1]), "v = 1");
    // An empty body: the declaration is its header, and no span is off content.
    let empty = "model e:\n\nthing t:\n    v = 1\n";
    assert!(violations(empty).1.is_empty());
    assert_eq!(text(empty, spans_of(empty, "Declaration")[0]), "model e:");
    // An unterminated string runs to the end of the file, line breaks
    // included: the spans over it end on that token's last byte — aligned,
    // while the byte rule (a terminated document's proxy) does not hold.
    let open = "thing t:\n    v = \"\"\"\n        text\n\n";
    let boundaries = nml_core::cst::parse(open).token_boundaries();
    let decl = spans_of(open, "Declaration")[0];
    assert!(boundaries.aligns(decl), "{decl:?}");
    assert!(
        !decl.is_content_in(open),
        "the proxy is honest about the shape it cannot cover"
    );
}

/// Every carrier of a SUB-TOKEN span, with the shape its emitter declares
/// and the token it sits inside of. The crash this pins: a QUOTED routing
/// selector's `Arm::selector_content` is the window inside the quotes —
/// neither the selector's token nor token-aligned — and every reader used
/// to classify a site by matching its NAME against `\".content\"`, which
/// that carrier's name does not end in, so a content window was held to
/// the rule for whole spans. The corpus carries ten arms and not one
/// quoted selector, which is why 163,747 corpus spans never showed it.
#[test]
fn every_sub_token_carrier_declares_its_shape_and_the_token_it_sits_in() {
    // (source, site, the bytes the window spans, the bytes of its token)
    for (src, kind, window, token) in [
        (
            "router r:\n    routes:\n        \"GET\" -> h\n",
            "Arm.selector_content",
            "GET",
            "\"GET\"",
        ),
        (
            "router r:\n    routes:\n        \"\"\"GET\"\"\" -> h\n",
            "Arm.selector_content",
            "GET",
            "\"\"\"GET\"\"\"",
        ),
        (
            "router r:\n    routes:\n        else -> \"/api\"\n",
            "ArmTarget::Literal.content",
            "/api",
            "\"/api\"",
        ),
        (
            "thing t:\n    v = \"x\"\n",
            "SpannedValue.content",
            "x",
            "\"x\"",
        ),
    ] {
        let site = one_site(src, kind);
        let SpanShape::ContentWindow { token: tok } = site.shape else {
            panic!("{src:?}: {kind} is {:?}, not a content window", site.shape);
        };
        assert_eq!(text(src, site.span), window, "{src:?}: {kind}");
        assert_eq!(text(src, tok), token, "{src:?}: {kind} token");
        assert!(
            tok.start < site.span.start && site.span.end <= tok.end,
            "{src:?}: {kind} {:?} is not strictly inside {tok:?}",
            site.span
        );
        // The whole document still holds under the shared verdict.
        assert!(violations(src).1.is_empty(), "{src:?}");
    }
    // An UNQUOTED carrier has no delimiters to stay inside of: its content
    // IS its span, and holding it to the window rule would reject it.
    for (src, kind) in [
        (
            "router r:\n    routes:\n        else -> h\n",
            "Arm.selector_content",
        ),
        (
            "router r:\n    routes:\n        @admin -> h\n",
            "Arm.selector_content",
        ),
        ("thing t:\n    v = 1\n", "SpannedValue.content"),
    ] {
        assert_eq!(
            one_site(src, kind).shape,
            SpanShape::Aligned,
            "{src:?}: {kind}"
        );
        assert!(violations(src).1.is_empty(), "{src:?}");
    }
    // A template expression names the string token it sits inside.
    let src = "a b:\n    x = \"{{ env.HOME }}\"\n";
    let site = one_site(src, "TemplateSegment::Expression");
    let SpanShape::TemplateExpression { token } = site.shape else {
        panic!("{:?} is not a template expression", site.shape);
    };
    assert_eq!(text(src, site.span), "{{ env.HOME }}");
    assert_eq!(text(src, token), "\"{{ env.HOME }}\"");
}

/// The site census, ratcheted: which kinds an emitter ever gives a
/// SUB-TOKEN shape, over the whole corpus and over the quoted carriers the
/// corpus does not hold. A carrier added without declaring its shape lands
/// here as a whole span and is judged by the wrong rule in silence — which
/// is the defect the shape closed — so the set is pinned and every member
/// must be witnessed: adding a carrier is a deliberate edit to this list.
#[test]
fn only_the_declared_carriers_ever_carry_a_sub_token_shape() {
    let mut windows: BTreeSet<&'static str> = BTreeSet::new();
    let mut expressions: BTreeSet<&'static str> = BTreeSet::new();
    let mut sources: Vec<String> = corpus()
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    assert!(sources.len() > 100, "corpus too small: {}", sources.len());
    // The quoted shapes the corpus has none of.
    for extra in [
        "router r:\n    routes:\n        \"GET\" -> h\n",
        "router r:\n    routes:\n        else -> \"/api\"\n",
        "a b:\n    x = \"{{ env.HOME }}\"\n",
    ] {
        sources.push(extra.to_string());
    }
    for source in &sources {
        for site in sites(source) {
            match site.shape {
                SpanShape::Aligned => {}
                SpanShape::ContentWindow { .. } => {
                    windows.insert(site.kind);
                }
                SpanShape::TemplateExpression { .. } => {
                    expressions.insert(site.kind);
                }
            }
        }
    }
    assert_eq!(
        windows.iter().copied().collect::<Vec<_>>(),
        [
            "Arm.selector_content",
            "ArmTarget::Literal.content",
            "SpannedValue.content",
        ],
        "the content-window carriers"
    );
    assert_eq!(
        expressions.iter().copied().collect::<Vec<_>>(),
        ["TemplateSegment::Expression"],
        "the template-expression carriers"
    );
}

/// A template expression's span is its exact `{{…}}` bytes — beside an
/// escape before it (the decoded text is shorter than the source), and the
/// escaped `\u{7B}\u{7B}` beside a real expression stays literal.
#[test]
fn a_template_expression_span_is_its_exact_bytes() {
    let src = "a b:\n    x = \"q\\\"r {{ env.HOME }} and \\u{7B}\\u{7B}not}}\"\n";
    let (_, wrong) = violations(src);
    assert!(wrong.is_empty(), "{wrong:?}");
    let exprs = spans_of(src, "TemplateSegment::Expression");
    assert_eq!(exprs.len(), 1, "the escaped braces are not a template");
    assert_eq!(text(src, exprs[0]), "{{ env.HOME }}");
    let (file, _) = nml_core::cst::parse_to_ast_all(src);
    let nml_core::ast::DeclarationKind::Block(b) = &file.declarations[0].kind else {
        panic!("block");
    };
    let nml_core::ast::BodyEntryKind::Property(p) = &b.body.entries[0].kind else {
        panic!("property");
    };
    let nml_core::types::Value::TemplateString(segments) = &p.value.value else {
        panic!("template: {:?}", p.value.value);
    };
    let literals: Vec<&str> = segments
        .iter()
        .filter_map(|s| match s {
            nml_core::types::TemplateSegment::Literal(l) => Some(l.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        literals,
        ["q\"r ", " and {{not}}"],
        "literal segments decode their escapes"
    );
    let raw = match &segments[1] {
        nml_core::types::TemplateSegment::Expression { raw, .. } => raw.as_str(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        text(src, nml_core::template::namespace_span(raw, exprs[0])),
        "env"
    );
}

/// r103-cov: the MULTILINE template expression's span, characterized.
///
/// RFC 0026 B-23 records "multiline template spans remain approximate (an
/// offset-map follow-up)" and schedules the offset map as the second item
/// after landing. "Approximate" understates it: the decoded body is
/// DEDENTED and has lost its opening delimiter and first line break, so
/// the span — computed as the token's start plus the offset in the
/// DECODED text — can land entirely OUTSIDE the `{{…}}`, on unrelated
/// bytes, and the further into the block the expression sits the further
/// it drifts. The editor builds a machine-applicable did-you-mean for an
/// unknown namespace at `template::namespace_span(raw, span)`
/// (`nml-lsp/src/diagnostics.rs`, NML5004), so the replacement it offers
/// on a multiline template rewrites the wrong bytes.
///
/// No corpus file carried a template inside a `"""` block, so
/// `every_span_in_the_corpus_is_a_content_span` never saw this. This test
/// is the visible marker: it asserts the invariant HOLDS on the
/// single-line spelling and states exactly how the multiline one fails,
/// so the day the offset map lands this test fails and is rewritten into
/// the exactness assertion its single-line sibling already makes.
#[test]
fn a_multiline_template_expression_span_is_not_yet_exact() {
    let single = "a b:\n    x = \"lead {{ env.HOME }} tail\"\n";
    let exprs = spans_of(single, "TemplateSegment::Expression");
    assert_eq!(text(single, exprs[0]), "{{ env.HOME }}", "the exact bytes");
    assert!(violations(single).1.is_empty());

    let multi = "a b:\n    x = \"\"\"\n        lead {{ env.HOME }} tail\n        \"\"\"\n";
    let exprs = spans_of(multi, "TemplateSegment::Expression");
    assert_eq!(exprs.len(), 1, "one expression: {exprs:?}");
    let got = text(multi, exprs[0]);
    assert_ne!(
        got, "{{ env.HOME }}",
        "the offset map landed — rewrite this test as the exactness assertion"
    );
    // What it is today: the right LENGTH, at the token's start plus the
    // DECODED offset — the dedent and the dropped delimiter shift it
    // eleven bytes left of the braces it is supposed to name.
    assert_eq!(got, "       lead {{", "the drifted window");
    assert!(
        !multi[exprs[0].start..].starts_with("{{"),
        "the window does not begin at the expression"
    );
    // The shape-aware judge still passes it (it asks only that a template
    // expression lie inside its token) — which is why nothing caught it;
    // the corpus clause that asks for the exact bytes is the one that
    // bites, and only on a document that carries this shape.
    let boundaries = nml_core::cst::parse(multi).token_boundaries();
    let mut sites = Vec::new();
    let (file, _) = nml_core::cst::parse_to_ast_all(multi);
    nml_core::ast::for_each_span(&file, &mut |s| sites.push(s));
    for site in &sites {
        assert!(
            boundaries.check(*site, multi).is_ok(),
            "{} {:?}",
            site.kind,
            site.span
        );
    }
    // And the did-you-mean the editor would apply is inside that window.
    let (file, _) = nml_core::cst::parse_to_ast_all(multi);
    let nml_core::ast::DeclarationKind::Block(b) = &file.declarations[0].kind else {
        panic!("block");
    };
    let nml_core::ast::BodyEntryKind::Property(p) = &b.body.entries[0].kind else {
        panic!("property");
    };
    let nml_core::types::Value::TemplateString(segments) = &p.value.value else {
        panic!("template: {:?}", p.value.value);
    };
    let raw = segments
        .iter()
        .find_map(|s| match s {
            nml_core::types::TemplateSegment::Expression { raw, .. } => Some(raw.as_str()),
            _ => None,
        })
        .expect("expression");
    assert_ne!(
        text(multi, nml_core::template::namespace_span(raw, exprs[0])),
        "env",
        "the namespace replacement would rewrite the wrong bytes"
    );
}
