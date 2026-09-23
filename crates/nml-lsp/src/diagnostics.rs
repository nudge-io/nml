use nml_core::ast::*;
use nml_core::diagnostic::{Severity, Suggestion, codes};
use nml_core::model::{EnumDef, ModelDef, OneOfDef};
use nml_core::types::{TemplateSegment, Value};
use nml_validate::schema::{MembershipSemantics, SchemaValidator};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::position::LineIndex;

/// Configuration for diagnostics computation.
#[derive(Default, Clone)]
pub struct DiagnosticConfig {
    /// Valid template namespaces. If empty, all namespaces are accepted.
    pub template_namespaces: Vec<String>,
    /// Valid modifier names. If empty, all modifiers are accepted.
    pub modifiers: Vec<String>,
    /// Membership semantics passed through to `SchemaValidator`.
    pub membership: MembershipSemantics,
    /// Whether the document itself feeds the scope registry (`.model.nml`):
    /// its definitions arrive through the registry, so the open-mode
    /// doc-local merge (and its collision reporting) must not re-add them.
    pub uri_is_registry_source: bool,
    /// Whether the schema LOAD pass owns composition verdicts for this
    /// document: true only when the universe it will load is authoritative
    /// (snapshot, declared, or an untruncated registry set). Only then may
    /// the validator's `UNKNOWN_MIXIN`/`INVALID_MIXIN_KIND` findings be
    /// suppressed in its favor — a truncated or single-source universe
    /// judging composition would report false "unknown `is` target" errors
    /// for parents the registry validator resolves fine.
    pub load_pass_owns_composition: bool,
    /// The universe's composition grant for this document (step 0e: the
    /// kernel's `Grant`, the CLI's own verdict), `None` for the open
    /// developer context — a file outside every root, a buffer the
    /// server resolves through no universe.
    pub grant: Option<nml_validate::workspace::Grant>,
}

impl DiagnosticConfig {
    /// The provider `compose_file` asks: the universe's grant, else the
    /// open context.
    fn grant_provider(&self) -> &dyn nml_core::layers::LayerGrantProvider {
        match &self.grant {
            Some(grant) => grant,
            None => &nml_core::layers::OpenContext,
        }
    }
}

/// Where schema validation for a document comes from (RFC 0030).
pub enum SchemaMode<'a> {
    /// Today's zero-config path: definitions from the workspace scope
    /// registry, lenient, merged.
    Registry {
        models: &'a [ModelDef],
        enums: &'a [EnumDef],
        oneofs: &'a [OneOfDef],
    },
    /// A package-bound document: the package's exclusive, profile-applied
    /// validator, with the binding identity suffixed onto its errors so a
    /// stale-schema state presents as "your schema copy is old", not "your
    /// config is wrong".
    Package {
        validator: &'a nml_validate::schema::SchemaValidator,
        /// e.g. `nudge blake3:9f3a, store current`
        identity: String,
    },
}

/// Compute diagnostics for an NML source document.
/// Compose the buffer's same-file `uses` stacks and validate the
/// RESOLVED view — the editor and `nml check` must agree on every
/// uses-bearing file: validating a raw overlay body reports phantom
/// missing-required errors on files `check` accepts, and misses every
/// compose finding (NML2059–NML2084). Mirrors `cmd_check` exactly:
/// compose diagnostics first, then the validator STREAMED over the
/// substituted validation view through the kernel's one deduplicating
/// sink (`layers::Deduped`, seeded by `ComposedFile::dedup_seed` — a
/// base defect cloned into every overlay's resolved body is one
/// finding, not one per overlay) into a `Bounded` tranche of
/// [`MAX_DIAGNOSTICS`]: the editor holds no more validator
/// findings than it publishes, and returns how many it counted past the
/// cap — exact, for the summary row. `keep` is the front end's own
/// suppression (the load pass's composition verdicts), applied AHEAD of
/// the tranche through `Filtered`, so a suppressed finding neither fills
/// the tranche nor rides the count. Grants: the universe's verdict for
/// this file (`provider`, the same lookup `nml check` composes under —
/// NML2064 for an unbound file in a closed universe, NML2065 for a
/// reference a binding's grant refuses), the open context only where no
/// universe applies.
fn composed_validate(
    validator: &SchemaValidator,
    file: &nml_core::ast::File,
    source_name: &str,
    provider: &dyn nml_core::layers::LayerGrantProvider,
    keep: &dyn Fn(&nml_core::diagnostic::Diagnostic) -> bool,
) -> Validated {
    never_dark(
        || {
            let composed =
                nml_core::layers::compose_file(validator.index(), source_name, file, provider);
            let seed = composed.dedup_seed(source_name);
            let mut out: Vec<_> = composed
                .diagnostics
                .into_iter()
                .filter(|diag| keep(diag))
                .collect();
            let mut bounded = nml_core::diagnostic::Bounded::new(&mut out, MAX_DIAGNOSTICS);
            let mut deduped = nml_core::layers::Deduped::new(&mut bounded, source_name, seed);
            let mut sink = nml_core::diagnostic::Filtered::new(&mut deduped, keep);
            validator.validate_into(composed.validation_file.as_ref().unwrap_or(file), &mut sink);
            let elided = bounded.elided;
            Validated {
                diagnostics: out,
                elided,
            }
        },
        || {
            let mut out = Vec::new();
            let mut bounded = nml_core::diagnostic::Bounded::new(&mut out, MAX_DIAGNOSTICS);
            let mut sink = nml_core::diagnostic::Filtered::new(&mut bounded, keep);
            validator.validate_into(file, &mut sink);
            let elided = bounded.elided;
            Validated {
                diagnostics: out,
                elided,
            }
        },
    )
}

/// A composition verdict — an `is` target the validator could not resolve
/// (`UNKNOWN_MIXIN`) or resolved to the wrong kind (`INVALID_MIXIN_KIND`).
/// The one predicate the two passes share: the schema load pass drops
/// these when its universe is partial, the validator pass when the load
/// pass owns composition — never both, so every verdict has one home.
fn is_composition_verdict(diag: &nml_core::diagnostic::Diagnostic) -> bool {
    matches!(
        diag.code,
        Some(codes::UNKNOWN_MIXIN | codes::INVALID_MIXIN_KIND)
    )
}

/// The validator pass's result: the bounded tranche it holds, and how
/// many findings it counted past the tranche (exact; the summary row's).
#[derive(Default)]
struct Validated {
    diagnostics: Vec<nml_core::diagnostic::Diagnostic>,
    elided: usize,
}

impl nml_core::diagnostic::DiagnosticSink for Validated {
    fn push(&mut self, diag: nml_core::diagnostic::Diagnostic) {
        self.diagnostics.push(diag);
    }
}

/// The editor must never go dark: a panic inside compose+validate (an
/// engine invariant, an `expect`) would otherwise unwind through
/// tower-lsp's inline handler future and take the whole server down into
/// a crash-restart loop. Degrade instead: `raw` (guarded too — a second
/// panic would be the same loop) plus one NML2086 anchored at the buffer
/// start, so the degradation is visible, never silent. The debug
/// assertion at the compose boundary stays loud in nml-core's tests. (On
/// `wasm32-wasip1` panics abort — the guard is inert there by
/// construction; the CLI's own process posture applies.)
fn never_dark<T: Default + nml_core::diagnostic::DiagnosticSink>(
    attempt: impl FnOnce() -> T,
    raw: impl FnOnce() -> T,
) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(attempt)) {
        Ok(out) => out,
        Err(payload) => {
            let what = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string());
            let mut out =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(raw)).unwrap_or_default();
            out.push(compose_guard_diag(&what));
            out
        }
    }
}

/// The guard's own finding — anchored at the buffer start: a span-less
/// diagnostic never reaches the editor (`push_diagnostic` drops it), and
/// a silent degradation would defeat the guard's purpose.
fn compose_guard_diag(what: &str) -> nml_core::diagnostic::Diagnostic {
    nml_core::diagnostic::Diagnostic::error(format!(
        "internal error while composing this file — showing findings for the raw \
         text only; please report the input ({what})"
    ))
    .with_code(nml_core::diagnostic::codes::INTERNAL_COMPOSE_INVARIANT)
    .with_span(nml_core::span::Span::empty(0))
}

/// One buffer's parse, every view: the semantic AST, the
/// buffer's own extracted definitions and the full parse findings
/// (lower pass + RFC 0018 facet rules) — from ONE `parse_and_extract`.
/// The server parses once per publish and hands this to
/// [`compute_parsed`]; a `.model.nml` buffer's schema passes reuse the
/// same extraction (cloned definitions, never a re-parse of the text).
pub struct ParsedBuffer {
    pub file: nml_core::ast::File,
    pub own_defs: nml_core::schema::ExtractedSchema,
    pub parse_errors: Vec<nml_core::diagnostic::Diagnostic>,
}

impl ParsedBuffer {
    pub fn parse(source: &str) -> Self {
        let (file, own_defs, parse_errors) = nml_core::cst::parse_and_extract(source);
        Self {
            file,
            own_defs,
            parse_errors,
        }
    }
}

/// Parse `source` and run [`compute_parsed`] over it.
pub fn compute(
    source: &str,
    mode: &SchemaMode<'_>,
    config: &DiagnosticConfig,
    uri: Option<&tower_lsp::lsp_types::Url>,
    source_name: &str,
    locate: &dyn Fn(&str) -> Option<(tower_lsp::lsp_types::Url, String)>,
) -> Vec<Diagnostic> {
    compute_parsed(
        source,
        ParsedBuffer::parse(source),
        mode,
        config,
        uri,
        source_name,
        locate,
    )
}

/// The instance-side diagnostics of an already-parsed buffer. Parses
/// nothing itself — the one-parse-per-publish pin reads
/// `nml_core::cst::parses_on_this_thread` across this call. `source_name`
/// is the buffer's name on every finding it composes and every dedup key
/// (its workspace KEY under a root — the CLI's vocabulary, step 0f — its
/// path outside every root); a same-file `Related.source` equals it, a
/// foreign one goes through `locate`.
pub fn compute_parsed(
    source: &str,
    parsed: ParsedBuffer,
    mode: &SchemaMode<'_>,
    config: &DiagnosticConfig,
    uri: Option<&tower_lsp::lsp_types::Url>,
    source_name: &str,
    locate: &dyn Fn(&str) -> Option<(tower_lsp::lsp_types::Url, String)>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    // Validator findings counted past the bounded tranche: the
    // cap row's count is exact across every pass.
    let mut elided = 0usize;
    let line_index = LineIndex::new(source);
    let push_diagnostic = |diag: nml_core::diagnostic::Diagnostic,
                           identity: Option<&str>,
                           uri: Option<&tower_lsp::lsp_types::Url>,
                           line_index: &LineIndex,
                           out: &mut Vec<Diagnostic>| {
        push_diagnostic_located(diag, identity, uri, line_index, source_name, locate, out);
    };

    // Resilient parse: always yields a best-effort AST plus the full set of
    // syntactic + semantic errors (position-sorted, bounded). Reporting every
    // error at once replaces the legacy first-error-only behaviour, and running
    // the validators on the best-effort AST keeps feedback alive mid-edit
    // instead of going dark on the first syntax error.
    // ONE parse yields all three views: the semantic AST for the
    // validators, this buffer's own extracted definitions for the
    // definition-side checks, and the findings set — which already
    // includes the RFC 0018 facet definition rules (NML2058), so the
    // parse band carries them for every document with no separate walk.
    // (Previously this fn parsed the same text up to three times per
    // keystroke: parse, facet re-extraction, merge re-extraction.)
    let ParsedBuffer {
        file,
        own_defs,
        parse_errors,
    } = parsed;

    for diag in parse_errors {
        push_diagnostic(diag, None, uri, &line_index, &mut diagnostics);
    }

    // Declared defaults, composition, cycles, and every other
    // schema-level rule for a `.model.nml` buffer arrive through
    // [`schema_load_pass`] — the editor loads the buffer's package
    // through the SAME `load_schema` entry the CLI and embedders use,
    // then keeps only this buffer's findings. Nothing schema-level is
    // checked here: one engine, one findings set, no drift.

    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);

    for diag in symbols.find_unresolved_references(&file) {
        push_diagnostic(diag, None, uri, &line_index, &mut diagnostics);
    }

    for diag in symbols.find_const_cycles() {
        push_diagnostic(diag, None, uri, &line_index, &mut diagnostics);
    }

    match mode {
        SchemaMode::Registry {
            models,
            enums,
            oneofs,
        } => {
            // Open mode is one namespace (RFC 0012): the document's own
            // definitions join its validation set, so a self-contained file
            // (`model cache` above `cache Foo:`) is typed in the editor
            // exactly as `nml check` types it. Workspace-registry
            // definitions stay authoritative on a name collision (the same
            // first-wins rule the loader applies; the CLI reports the
            // collision as NML2009).
            let mut models = models.to_vec();
            let mut enums = enums.to_vec();
            let mut oneofs = oneofs.to_vec();
            // A `.model.nml` document IS a registry source — its definitions
            // arrived through the registry already, so merging (or
            // collision-checking) them against themselves would be noise.
            let doc_is_registry_source = config.uri_is_registry_source;
            if !doc_is_registry_source {
                let own = own_defs;
                let mut collisions = join_own_definitions(
                    own.models,
                    &mut models,
                    |m| &m.name,
                    |m| m.span,
                    |m| m.kind.label(),
                );
                collisions.extend(join_own_definitions(
                    own.enums,
                    &mut enums,
                    |e| &e.name,
                    |e| e.span,
                    |_| "enum",
                ));
                collisions.extend(join_own_definitions(
                    own.oneofs,
                    &mut oneofs,
                    |o| &o.name,
                    |o| o.span,
                    |_| "oneof",
                ));
                for diag in collisions {
                    push_diagnostic(diag, None, uri, &line_index, &mut diagnostics);
                }
            }
            if models.is_empty() && enums.is_empty() && oneofs.is_empty() {
                // No schema anywhere — but `check` still composes
                // STRUCTURALLY (NML2059/2061/2062/2077 need no schema),
                // so the editor must too or a schema-less buffer goes
                // dark on a typo'd `uses` ref the CLI catches.
                let empty = nml_core::schema_index::SchemaIndex::build(vec![], vec![], vec![]);
                // Same never-go-dark guard as the schema-bearing path.
                let composed = never_dark(
                    || {
                        nml_core::layers::compose_file(
                            &empty,
                            source_name,
                            &file,
                            config.grant_provider(),
                        )
                        .diagnostics
                    },
                    Vec::new,
                );
                for diag in composed {
                    push_diagnostic(diag, None, uri, &line_index, &mut diagnostics);
                }
            } else {
                let mut schema = nml_core::schema::ExtractedSchema {
                    models,
                    enums,
                    oneofs,
                };
                nml_core::schema::resolve_model_inheritance(&mut schema);
                // Mixed `.nml` buffers get their schema-load lints
                // (NML2068 is an ERROR) here — the load pass only covers
                // `.model.nml` URIs, so without this the documented
                // error-index repros squiggle nowhere in the editor.
                if !config.uri_is_registry_source {
                    for diag in nml_core::layers::validate_merge_policies_over(
                        &schema.models,
                        &schema.oneofs,
                    ) {
                        push_diagnostic(diag, None, uri, &line_index, &mut diagnostics);
                    }
                }
                let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs)
                    .with_modifiers(config.modifiers.clone())
                    .with_membership_semantics(config.membership.clone());
                // A `.model.nml` buffer's mixin (`is`) targets are judged
                // by the SCHEMA LOAD PASS against the buffer's TRUE
                // universe — its covering package's sources (workspace,
                // store, or in-binary snapshot). The validator resolves
                // them against the WORKSPACE REGISTRY, which cannot see
                // snapshot sources: for a store-covered buffer this arm
                // reported `unknown \`is\` target` for parents the
                // package genuinely defines (caught by the provenance
                // matrix e2e). Same-code findings from the load pass
                // carry the same did-you-mean, so nothing is lost.
                // Suppression is gated on the load pass actually OWNING
                // composition: when its universe is truncated (registry
                // cap) or single-source (non-file buffer), this arm's
                // uncapped registry is the only correct judge and its
                // verdicts must stand. The rule is applied AT THE SOURCE,
                // ahead of the bounded tranche: a suppressed verdict never
                // fills the tranche or rides the cap row's count.
                let keep = |diag: &nml_core::diagnostic::Diagnostic| {
                    !(config.load_pass_owns_composition && is_composition_verdict(diag))
                };
                let validated = composed_validate(
                    &validator,
                    &file,
                    source_name,
                    config.grant_provider(),
                    &keep,
                );
                elided += validated.elided;
                for diag in validated.diagnostics {
                    push_diagnostic(diag, None, uri, &line_index, &mut diagnostics);
                }
            }
        }
        SchemaMode::Package {
            validator,
            identity,
        } => {
            let validated = composed_validate(
                validator,
                &file,
                source_name,
                config.grant_provider(),
                &|_| true,
            );
            elided += validated.elided;
            for diag in validated.diagnostics {
                push_diagnostic(diag, Some(identity), uri, &line_index, &mut diagnostics);
            }
        }
    }

    let ns: Vec<&str> = config
        .template_namespaces
        .iter()
        .map(|s| s.as_str())
        .collect();
    validate_templates(&file, &ns, uri, &line_index, &mut diagnostics);

    cap_diagnostics(&mut diagnostics, elided);

    diagnostics
}

/// Open mode is one namespace (RFC 0012): a buffer's own definitions join
/// its validation set, except where the workspace registry already defines
/// the name — the registry's stays authoritative (the loader's first-wins
/// rule) and the buffer's is NML2009, the collision `nml check` reports:
/// never a silent shadow. ONE rule for models, enums and oneofs; the
/// collisions come back in declaration order for the caller to place.
fn join_own_definitions<T>(
    own: Vec<T>,
    registry: &mut Vec<T>,
    name: impl Fn(&T) -> &str,
    span: impl Fn(&T) -> nml_core::span::Span,
    what: impl Fn(&T) -> &'static str,
) -> Vec<nml_core::diagnostic::Diagnostic> {
    let mut collisions = Vec::new();
    for def in own {
        if registry.iter().any(|k| name(k) == name(&def)) {
            collisions.push(
                nml_core::diagnostic::Diagnostic::error(format!(
                    "duplicate {} definition '{}' — the workspace schema registry already \
                     defines it",
                    what(&def),
                    name(&def)
                ))
                .with_code(codes::DUPLICATE_DEFINITION)
                .with_span(span(&def)),
            );
        } else {
            registry.push(def);
        }
    }
    collisions
}

/// Editor flood cap: a hostile or badly broken buffer can yield tens of
/// thousands of findings (one per unmatched list item), and serializing
/// them all on EVERY keystroke is a client-side DoS. The first
/// [`MAX_DIAGNOSTICS`] tell the story; the tail is summarized in one row.
/// The CLI prints 512 per run by default and `--max-findings 0` lifts
/// it — a one-shot terminal run is still the place for the
/// full list. The validator pass is bounded AT THE SOURCE (a
/// `Bounded` sink keeps its first tranche and counts the rest, so a
/// flood is never held whole — after the load pass's suppression, so
/// the count holds only findings that would have published); the
/// other passes and the union are
/// truncated here, and `prior` — what the validator counted past its
/// tranche — rides the summary row, so the count is exact. Bounds
/// memory and SERIALIZATION, not compute: the analysis still runs over
/// everything (fine while it stays linear).
///
/// LIMIT: reach=content guards=output surface=editor shown="500" — diagnostics published per document; the tail is summarized in one row
const MAX_DIAGNOSTICS: usize = 500;

fn cap_diagnostics(diagnostics: &mut Vec<Diagnostic>, prior: usize) {
    let over = diagnostics.len().saturating_sub(MAX_DIAGNOSTICS);
    if over + prior > 0 {
        let elided = over + prior;
        diagnostics.truncate(MAX_DIAGNOSTICS);
        diagnostics.push(Diagnostic {
            severity: Some(DiagnosticSeverity::INFORMATION),
            message: format!(
                "{elided} further finding(s) not shown — resolve the above, \
                 or run `nml check` for the full list"
            ),
            ..Default::default()
        });
    }
}

/// The `data.suggestions[]` a row carries for the code-action handler —
/// each edit's kind, payload and anchor, and the file it lands in when
/// that is not `own_source` (`file_of`: for a finding the inheritance
/// `Diagnostic::suggestion_source` settles; a degraded note's carried
/// edit names its file itself) — so the action is minted on that
/// document, never resolved against this one's text. `None` with
/// nothing to offer: the key is then absent. ONE builder for a located
/// finding's row and a universe note's row alike.
pub(crate) fn suggestion_data<'s>(
    suggestions: &'s [nml_core::diagnostic::Suggestion],
    file_of: impl Fn(&'s nml_core::diagnostic::Suggestion) -> Option<&'s str>,
    own_source: &str,
) -> Option<serde_json::Value> {
    (!suggestions.is_empty()).then(|| {
        serde_json::json!({
            "suggestions": suggestions
                .iter()
                .map(|s| {
                    let mut entry = serde_json::json!({
                        "replacement": s.replacement,
                        "start": s.span.start,
                        "end": s.span.end,
                        // The names live with the kind (`wire_name`),
                        // where a new variant cannot compile unnamed.
                        "kind": s.kind.wire_name(),
                    });
                    if let Some(src) = file_of(s).filter(|src| *src != own_source) {
                        entry["source"] = serde_json::Value::String(src.to_string());
                    }
                    entry
                })
                .collect::<Vec<_>>(),
        })
    })
}

/// Lower one core diagnostic to LSP form — **the** converter (RFC 0008):
/// every producer (parse, symbols, validator, templates, directives,
/// advisory notices) flows through here. Maps severity three ways, carries
/// the stable code as the LSP string `code`, suffixes the binding identity
/// onto package-mode errors, and rides the structured suggestion in
/// `Diagnostic.data` so the code-action handler offers a one-keystroke fix
/// without re-deriving (or worse, message-parsing) it.
/// [`push_diagnostic_located`] for the passes with no compose notes to
/// locate (templates, model-source checks): same-file notes behave
/// identically, and a foreign-source note falls back to the
/// diagnostic's own location with the file named — never a foreign span
/// through this document's index.
fn push_diagnostic(
    diag: nml_core::diagnostic::Diagnostic,
    identity: Option<&str>,
    uri: Option<&tower_lsp::lsp_types::Url>,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let own = uri.map(|u| u.path().to_string()).unwrap_or_default();
    push_diagnostic_located(diag, identity, uri, line_index, &own, &|_| None, out);
}

fn push_diagnostic_located(
    diag: nml_core::diagnostic::Diagnostic,
    identity: Option<&str>,
    uri: Option<&tower_lsp::lsp_types::Url>,
    line_index: &LineIndex,
    own_source: &str,
    locate: &dyn Fn(&str) -> Option<(tower_lsp::lsp_types::Url, String)>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(span) = diag.span else {
        // A span-less validator diagnostic is a validator defect: the parity
        // test suite is the enforcement point (every diagnostic must carry a
        // span there). The request path must not assert on attacker-supplied
        // input — skip, and let the tests keep the invariant.
        return;
    };
    let is_error = matches!(diag.severity, Severity::Error);
    // `rendered_message` appends the standard did-you-mean hint derived from
    // the structured suggestion — the one renderer every surface shares.
    let message = match identity {
        Some(id) if is_error => format!("{} (schema: {id})", diag.rendered_message()),
        _ => diag.rendered_message(),
    };
    let data = suggestion_data(&diag.suggestions, |s| diag.suggestion_source(s), own_source);
    out.push(Diagnostic {
        range: line_index.range(span),
        severity: Some(match diag.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            // Info and any future advisory levels surface as INFORMATION.
            _ => DiagnosticSeverity::INFORMATION,
        }),
        // Stable code (RFC 0008) — the LSP spec types `code` as
        // number-or-string; the formatted form is the public identity.
        code: diag
            .code
            .map(|c| tower_lsp::lsp_types::NumberOrString::String(c.to_string())),
        message,
        source: Some("nml".to_string()),
        data,
        related_information: uri.map(|uri| {
            related_information(
                diag.source.as_deref(),
                &diag.related,
                diag.span,
                uri,
                line_index,
                own_source,
                locate,
            )
        }),
        ..Default::default()
    });
}

/// Secondary locations (RFC 0009), spec-native — each located in ITS
/// OWN file (`Related.source`, RFC 0019 plan item 2): the current
/// document's index for a same-file note, a located foreign file's own
/// index otherwise, and the diagnostic's own location (`diag_span`, or
/// the document start) with the file named in the message when the file
/// cannot be located — never the right file with a wrong range. ONE
/// mapping for a located finding's notes and a kernel row's (a degraded
/// note at the top of the file: NML2091's first failing source line).
pub fn related_information(
    diag_source: Option<&str>,
    related: &[nml_core::diagnostic::Related],
    diag_span: Option<nml_core::span::Span>,
    uri: &tower_lsp::lsp_types::Url,
    line_index: &LineIndex,
    own_source: &str,
    locate: &dyn Fn(&str) -> Option<(tower_lsp::lsp_types::Url, String)>,
) -> Vec<tower_lsp::lsp_types::DiagnosticRelatedInformation> {
    related
        .iter()
        .map(|rel| {
            let foreign = rel
                .source
                .as_deref()
                .or(diag_source)
                .filter(|s| *s != own_source)
                .map(|s| (s, locate(s)));
            match foreign {
                None => tower_lsp::lsp_types::DiagnosticRelatedInformation {
                    location: tower_lsp::lsp_types::Location {
                        uri: uri.clone(),
                        range: line_index.range(rel.span),
                    },
                    message: rel.message.clone(),
                },
                Some((_, Some((furl, text)))) => {
                    tower_lsp::lsp_types::DiagnosticRelatedInformation {
                        location: tower_lsp::lsp_types::Location {
                            uri: furl,
                            range: LineIndex::new(&text).range(rel.span),
                        },
                        message: rel.message.clone(),
                    }
                }
                Some((s, None)) => tower_lsp::lsp_types::DiagnosticRelatedInformation {
                    location: tower_lsp::lsp_types::Location {
                        uri: uri.clone(),
                        range: diag_span.map(|sp| line_index.range(sp)).unwrap_or_default(),
                    },
                    message: format!("{} (in {s})", rel.message),
                },
            }
        })
        .collect()
}

/// Cross-definition schema validation for a `.model.nml` buffer — the
/// editor calls the loader. `sources` is the buffer's whole validation
/// universe (its covering package's `[]schema` files in manifest order,
/// or the workspace registry set when uncovered), read buffer-first;
/// `own_name` is the entry naming this buffer. The pass runs
/// [`nml_validate::loader::load_schema`] — the byte-for-byte entry point
/// the CLI and embedders use, so composition, cycles, shorthand arity,
/// oneof/enum integrity, reserved/duplicate names, and declared defaults
/// reach the editor with CLI parity by identity, not by test — and keeps
/// only findings stamped with `own_name`. Foreign files' findings carry
/// their own source stamps and are dropped: their spans index THEIR text,
/// and they get their own findings when their buffers are validated (the
/// wrong-buffer attribution rule). Extraction errors for the own file are
/// byte-identical to the parse band's; the caller's exact-duplicate
/// suppression collapses them.
///
/// `owns_composition` mirrors [`DiagnosticConfig::load_pass_owns_composition`]:
/// when false, the universe is known-partial (truncated registry set or a
/// single-source non-file buffer), so this pass's `UNKNOWN_MIXIN`/
/// `INVALID_MIXIN_KIND` findings are truncation artifacts and are dropped —
/// the registry validator's unsuppressed verdicts own composition instead.
///
/// `own` is the buffer's OWN extraction (definitions + parse findings),
/// already derived by the publish's one parse: the buffer's slot in the
/// universe takes it in place of its text, so the pass never re-parses
/// the buffer; every other source is extracted one at a time,
/// exactly as `load_schema` would.
pub fn schema_load_pass(
    own_name: &str,
    sources: &[(String, String)],
    uri: Option<&tower_lsp::lsp_types::Url>,
    owns_composition: bool,
    own: (
        nml_core::schema::ExtractedSchema,
        Vec<nml_core::diagnostic::Diagnostic>,
    ),
) -> Vec<Diagnostic> {
    let own_text = match sources.iter().find(|(n, _)| n == own_name) {
        Some((_, text)) => text,
        // No own entry means the caller assembled a universe that cannot
        // attribute anything to this buffer — nothing to report.
        None => return Vec::new(),
    };
    let line_index = LineIndex::new(own_text);
    let mut own = Some(own);
    let parts = sources.iter().map(|(n, t)| {
        if n == own_name {
            if let Some((schema, diags)) = own.take() {
                return (n.as_str(), schema, diags);
            }
        }
        let (extracted, errors) = nml_core::cst::extract_schema(t);
        (n.as_str(), extracted, errors)
    });
    let (_schema, findings) = nml_validate::loader::load_schema_parts(parts);
    // The note locator speaks the UNIVERSE's name vocabulary — the same
    // names the loader stamps — and serves from the in-hand sources
    // (read buffer-first), so a same-file note never degrades on a
    // canonicalized-vs-`uri.path()` spelling mismatch and a sibling's
    // note locates in the sibling's own text with zero I/O. A
    // non-path universe name (an untitled buffer) yields no Url and
    // falls back loudly, named in the message.
    let locate = |src: &str| -> Option<(tower_lsp::lsp_types::Url, String)> {
        let (name, text) = sources.iter().find(|(n, _)| n == src)?;
        let url = tower_lsp::lsp_types::Url::from_file_path(std::path::Path::new(name)).ok()?;
        Some((url, text.clone()))
    };
    let mut out = Vec::new();
    for diag in findings
        .into_iter()
        .filter(|d| d.source.as_deref() == Some(own_name))
    {
        if !owns_composition && is_composition_verdict(&diag) {
            continue;
        }
        push_diagnostic_located(diag, None, uri, &line_index, own_name, &locate, &mut out);
    }
    out
}

/// Schema-source pass (RFC 0030): diagnostics for a **covered** `.model.nml`
/// document — a file `vocabulary_for` resolved to a covering package. Opaque
/// files never reach here (zero vocabulary diagnostics by construction).
///
/// Three concerns, all schema-side (the instance-side `compute` pass knows
/// nothing about them):
/// - extraction errors: `rebuild_schema_registry` silently discards them, so
///   without this pass a broken schema source only ever manifests as missing
///   completions elsewhere;
/// - the covering vocabulary's verdicts (`VocabularyMatch::judge` — the
///   kernel's one judge, the rows `nml check` and `nml validate` report
///   for the same file): unknown name with a did-you-mean, arity, the
///   `#live`/`#restart` contradiction, and the undeclared-sibling note.
///
/// `schema`/`errors` are the buffer's own extraction — the publish's one
/// parse, shared with the load pass; this pass parses nothing.
/// The kernel's note on a schema source judged under no vocabulary for a
/// reason the author can act on ([`crate::packages::VocabularyOutcome::note`]:
/// the truncated
/// universe, an ambiguous coverage) — one info row at the top of the file,
/// through the one converter; nothing for a covered or opaque file.
pub fn coverage_note(
    source: &str,
    outcome: &crate::packages::VocabularyOutcome,
    uri: Option<&tower_lsp::lsp_types::Url>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if let Some(note) = outcome.note() {
        push_diagnostic(note, None, uri, &LineIndex::new(source), &mut out);
    }
    out
}

pub fn schema_source_pass(
    source: &str,
    schema: &nml_core::schema::ExtractedSchema,
    errors: &[nml_core::diagnostic::Diagnostic],
    vocab: &crate::packages::VocabularyMatch,
    uri: Option<&tower_lsp::lsp_types::Url>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let line_index = LineIndex::new(source);
    for diag in errors.iter().cloned() {
        push_diagnostic(diag, None, uri, &line_index, &mut out);
    }

    for diag in vocab.judge(&schema.models, source) {
        push_diagnostic(diag, None, uri, &line_index, &mut out);
    }
    out
}

fn validate_shared_property_templates(
    sp: &SharedProperty,
    valid_ns: &[&str],
    uri: Option<&tower_lsp::lsp_types::Url>,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    match &sp.kind {
        SharedPropertyKind::Block(body) => {
            validate_body_templates(body, valid_ns, uri, line_index, diags);
        }
        SharedPropertyKind::Scalar(sv) => {
            validate_value_templates(&sv.value, valid_ns, uri, line_index, diags);
        }
    }
}

fn validate_templates(
    file: &File,
    valid_ns: &[&str],
    uri: Option<&tower_lsp::lsp_types::Url>,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                validate_body_templates(&block.body, valid_ns, uri, line_index, diags);
            }
            DeclarationKind::Template(t) => {
                validate_value_templates(&t.value.value, valid_ns, uri, line_index, diags);
            }
            DeclarationKind::Const(c) => {
                validate_value_templates(&c.value.value, valid_ns, uri, line_index, diags);
            }
            DeclarationKind::Array(arr) => {
                for sp in &arr.body.shared_properties {
                    validate_shared_property_templates(sp, valid_ns, uri, line_index, diags);
                }
                for prop in &arr.body.properties {
                    validate_value_templates(&prop.value.value, valid_ns, uri, line_index, diags);
                }
                for item in &arr.body.items {
                    validate_list_item_templates(item, valid_ns, uri, line_index, diags);
                }
            }
            // `oneof` arms hold only discriminator literals and model names;
            // there are no template-bearing values to validate.
            DeclarationKind::OneOf(_) => {}
        }
    }
}

fn validate_body_templates(
    body: &Body,
    valid_ns: &[&str],
    uri: Option<&tower_lsp::lsp_types::Url>,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                validate_value_templates(&prop.value.value, valid_ns, uri, line_index, diags);
            }
            BodyEntryKind::NestedBlock(nested) => {
                validate_body_templates(&nested.body, valid_ns, uri, line_index, diags);
            }
            BodyEntryKind::ListItem(item) => {
                validate_list_item_templates(item, valid_ns, uri, line_index, diags);
            }
            BodyEntryKind::SharedProperty(shared) => {
                validate_shared_property_templates(shared, valid_ns, uri, line_index, diags);
            }
            _ => {}
        }
    }
}

fn validate_list_item_templates(
    item: &ListItem,
    valid_ns: &[&str],
    uri: Option<&tower_lsp::lsp_types::Url>,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    match &item.kind {
        ListItemKind::Named { body, .. } => {
            validate_body_templates(body, valid_ns, uri, line_index, diags);
        }
        ListItemKind::Shorthand { value, body } => {
            validate_value_templates(&value.value, valid_ns, uri, line_index, diags);
            if let Some(body) = body {
                validate_body_templates(body, valid_ns, uri, line_index, diags);
            }
        }
        _ => {}
    }
}

fn validate_value_templates(
    value: &Value,
    valid_ns: &[&str],
    uri: Option<&tower_lsp::lsp_types::Url>,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    if let Value::TemplateString(segments) = value {
        for seg in segments {
            if let TemplateSegment::Expression {
                namespace,
                raw,
                span,
                ..
            } = seg
            {
                if !valid_ns.is_empty() && !valid_ns.contains(&namespace.as_str()) {
                    let mut diag = nml_core::diagnostic::Diagnostic::warning(format!(
                        "unknown template namespace '{namespace}'"
                    ))
                    .with_code(codes::UNKNOWN_TEMPLATE_NAMESPACE)
                    .with_span(*span);
                    if let Some(s) = nml_core::suggest::suggest(namespace, valid_ns.iter().copied())
                    {
                        // The fix replaces the NAMESPACE, not the whole
                        // `{{…}}` the row squiggles.
                        diag = diag.with_suggestion(
                            Suggestion::did_you_mean(s)
                                .at(nml_core::template::namespace_span(raw, *span)),
                        );
                    }
                    push_diagnostic(diag, None, uri, line_index, diags);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// [`schema_source_pass`] over a freshly extracted source.
    fn source_pass(source: &str, vocab: &crate::packages::VocabularyMatch) -> Vec<Diagnostic> {
        let (schema, errors) = nml_core::cst::extract_schema(source);
        schema_source_pass(source, &schema, &errors, vocab, None)
    }

    /// The editor's double parse, closed: a publish parses its
    /// buffer ONCE. `compute_parsed` parses nothing; the load pass takes
    /// the buffer's own extraction and parses only the OTHER sources of
    /// the universe; the source pass parses nothing. Read at the parse
    /// counter — structural, never timed.
    #[test]
    fn the_editor_passes_never_reparse_the_buffer() {
        use nml_core::cst::parses_on_this_thread;
        let text = "model m:\n    name string\n";
        let parsed = ParsedBuffer::parse(text);
        let before = parses_on_this_thread();
        let _ = compute_parsed(
            text,
            parsed,
            &SchemaMode::Registry {
                models: &[],
                enums: &[],
                oneofs: &[],
            },
            &DiagnosticConfig::default(),
            None,
            "",
            &|_| None,
        );
        assert_eq!(
            parses_on_this_thread() - before,
            0,
            "compute_parsed parses nothing"
        );

        let own = "/ws/m.model.nml";
        let sources = vec![
            (own.to_string(), text.to_string()),
            (
                "/ws/a.model.nml".to_string(),
                "model a:\n    x string\n".to_string(),
            ),
            (
                "/ws/b.model.nml".to_string(),
                "model b:\n    y string\n".to_string(),
            ),
        ];
        let own_part = nml_core::cst::extract_schema(text);
        let before = parses_on_this_thread();
        let _ = schema_load_pass(own, &sources, None, true, own_part);
        assert_eq!(
            parses_on_this_thread() - before,
            2,
            "the load pass parses the two OTHER sources, never the buffer"
        );

        let (schema, errors) = nml_core::cst::extract_schema(text);
        // The demo vocabulary is built from a package text (its own
        // parse) — settled before the counter is read.
        let vocab = demo_vocab(false);
        let before = parses_on_this_thread();
        let _ = schema_source_pass(text, &schema, &errors, &vocab, None);
        assert_eq!(
            parses_on_this_thread() - before,
            0,
            "the source pass parses nothing"
        );
    }

    fn default_config() -> DiagnosticConfig {
        DiagnosticConfig::default()
    }

    /// Step 0e: the editor composes under the UNIVERSE's grant. An
    /// unbound file's `uses` in a closed universe is NML2064 in the CLI's
    /// own sentence (the root and the claim count named); the open
    /// developer context permits it. `OpenContext` used to be the
    /// editor's only context, so the CI gate and the editor disagreed on
    /// every uses-bearing file a closed universe left unbound.
    #[test]
    fn composition_is_judged_under_the_universes_grant_not_the_open_context() {
        let text = "thing base:\n    v = \"b\"\n\nthing t uses base:\n    v = \"t\"\n";
        let url = tower_lsp::lsp_types::Url::parse("file:///ws/docs/unclaimed.nml").unwrap();
        let registry = SchemaMode::Registry {
            models: &[],
            enums: &[],
            oneofs: &[],
        };
        let code = |d: &Diagnostic| match &d.code {
            Some(tower_lsp::lsp_types::NumberOrString::String(c)) => c.clone(),
            _ => String::new(),
        };
        let mut closed = default_config();
        closed.grant = Some(nml_validate::workspace::Grant::Unbound { closed: Some(2) });
        let out = compute(
            text,
            &registry,
            &closed,
            Some(&url),
            "docs/unclaimed.nml",
            &|_| None,
        );
        let denied: Vec<&Diagnostic> = out.iter().filter(|d| code(d) == "NML2064").collect();
        assert_eq!(denied.len(), 1, "{out:?}");
        assert!(
            denied[0].message.contains(
                "no binding governs this file in the closed universe (2 manifest(s) discovered)"
            ),
            "{}",
            denied[0].message
        );
        assert_eq!(
            denied[0].severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
        );
        let open = compute(
            text,
            &registry,
            &default_config(),
            Some(&url),
            "docs/unclaimed.nml",
            &|_| None,
        );
        assert!(open.iter().all(|d| code(d) != "NML2064"), "{open:?}");
        // A binding without a grant denies too (NML2064's no-grant form
        // names the binding and its manifest); under the store's copy of
        // the package the sentence says so and names the change it needs.
        let mut no_grant = default_config();
        no_grant.grant = Some(nml_validate::workspace::Grant::NoGrant {
            binding: "tenantFlows".to_string(),
            manifest: "<store current>".to_string(),
            package: "demo".to_string(),
            home: nml_core::layers::ManifestHome::External(nml_core::layers::ExternalClass::Store),
        });
        let out = compute(
            text,
            &registry,
            &no_grant,
            Some(&url),
            "tenants/cu/x.flow.nml",
            &|_| None,
        );
        let denied: Vec<&Diagnostic> = out.iter().filter(|d| code(d) == "NML2064").collect();
        assert_eq!(denied.len(), 1, "{out:?}");
        assert!(
            denied[0].message.contains("tenantFlows")
                && denied[0].message.contains("(<store current>)")
                && denied[0].message.contains(
                    "the store's current copy of package `demo`, never edited in place: add the \
                     grant in the package's source and republish it"
                ),
            "{}",
            denied[0].message
        );
        assert!(denied[0].data.is_none(), "no editable manifest: no edit");
        assert!(
            denied[0]
                .related_information
                .as_ref()
                .is_none_or(|notes| notes.is_empty()),
            "no editable manifest: no located note: {:?}",
            denied[0].related_information
        );
        // With the binding's span in an editable manifest, the wire
        // carries the remedy as a structured insertion IN THAT
        // DOCUMENT: the entry names the manifest (`source`), the
        // binding's name span (the anchor the resolver relocates) and
        // the zero-indent block — the manifest-document quick-fix's
        // whole input: the code action routes to that document.
        let mut editable = default_config();
        editable.grant = Some(nml_validate::workspace::Grant::NoGrant {
            binding: "tenantFlows".to_string(),
            manifest: "demo.package.nml".to_string(),
            package: "demo".to_string(),
            home: nml_core::layers::ManifestHome::Workspace {
                at: nml_core::span::Span::new(152, 163),
            },
        });
        let out = compute(
            text,
            &registry,
            &editable,
            Some(&url),
            "tenants/cu/x.flow.nml",
            &|_| None,
        );
        let denied = out
            .iter()
            .find(|d| code(d) == "NML2064")
            .unwrap_or_else(|| panic!("{out:?}"));
        let entry = denied
            .data
            .as_ref()
            .and_then(|d| d.get("suggestions"))
            .and_then(|s| s.as_array())
            .and_then(|s| s.first())
            .unwrap_or_else(|| panic!("{denied:?}"));
        assert_eq!(
            *entry,
            serde_json::json!({
                "kind": "insert",
                "source": "demo.package.nml",
                "start": 152,
                "end": 163,
                "replacement": "layers:\n    allowRefs:\n        - \"tenants/cu/x.flow.nml\"",
            })
        );
    }

    /// `Related.source` at the LSP wire (RFC 0019 plan item 2), pinned
    /// with a synthetic diagnostic because both consumers compose
    /// single-file today: a located foreign note gets ITS OWN file's
    /// uri and line index; an un-locatable one falls back to the
    /// diagnostic's own location with the file named in the message —
    /// never a foreign span through this document's index.
    #[test]
    fn related_notes_locate_in_their_own_files() {
        use nml_core::span::Span;
        let own_url = tower_lsp::lsp_types::Url::parse("file:///ws/main.nml").unwrap();
        let foreign_url = tower_lsp::lsp_types::Url::parse("file:///ws/b.nml").unwrap();
        let foreign_text = "x = 1\ny = 2\n";
        let locate = |src: &str| -> Option<(tower_lsp::lsp_types::Url, String)> {
            (src == "/ws/b.nml").then(|| (foreign_url.clone(), foreign_text.to_string()))
        };
        let diag = nml_core::diagnostic::Diagnostic::error("sealed")
            .with_code(nml_core::diagnostic::codes::SEALED_FIELD_VIOLATION)
            .with_span(Span::new(0, 1))
            .with_related_in(Span::new(0, 1), "sealed here", None)
            .with_related_in(Span::new(6, 7), "sealed here", Some("/ws/b.nml".into()))
            .with_related_in(Span::new(3, 4), "sealed here", Some("/ws/gone.nml".into()));
        let mut out = Vec::new();
        let line_index = LineIndex::new("a = 1\n");
        push_diagnostic_located(
            diag,
            None,
            Some(&own_url),
            &line_index,
            "/ws/main.nml",
            &locate,
            &mut out,
        );
        let related = out[0].related_information.as_ref().expect("notes");
        assert_eq!(related.len(), 3);
        assert_eq!(related[0].location.uri, own_url, "same-file note");
        assert_eq!(related[1].location.uri, foreign_url, "its own file");
        assert_eq!(
            related[1].location.range.start.line, 1,
            "byte 6 is LINE 2 of b.nml, through b.nml's OWN index"
        );
        assert_eq!(
            related[2].location.uri, own_url,
            "un-locatable: the diagnostic's own location"
        );
        assert!(
            related[2].message.contains("(in /ws/gone.nml)"),
            "{related:?}"
        );
    }

    /// A tag character (U+E0067, 4 UTF-8 bytes / 2 UTF-16 units) maps
    /// to editor coordinates through the astral math: the NML0018 range
    /// must cover exactly the two surrogate units, and text AFTER the
    /// character must not be shifted — a byte-counted range here would
    /// select the wrong columns in every editor.
    #[test]
    fn a_tag_character_diagnostic_spans_two_utf16_units() {
        let text = "x = \"a\u{E0067}b\"\n";
        let url = tower_lsp::lsp_types::Url::parse("file:///ws/tag.nml").unwrap();
        let out = compute(
            text,
            &SchemaMode::Registry {
                models: &[],
                enums: &[],
                oneofs: &[],
            },
            &default_config(),
            Some(&url),
            "",
            &|_| None,
        );
        let tag = out
            .iter()
            .find(|d| {
                d.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "NML0018".into(),
                    ))
            })
            .expect("the policy fires on the raw tag character");
        assert_eq!(tag.range.start.line, 0);
        assert_eq!(tag.range.start.character, 6, "after `x = \"a`");
        assert_eq!(
            tag.range.end.character, 8,
            "one astral scalar = two UTF-16 units"
        );
    }

    /// RFC 0019: the editor composes same-file `uses` stacks exactly as
    /// `nml check` does — an overlay whose base supplies a required field
    /// gets no phantom missing-required error, and compose findings
    /// (here a sealed-field violation) surface in the buffer.
    #[test]
    fn editor_composes_uses_stacks_like_check() {
        let schema = nml_core::cst::extract_schema(
            "model flow:\n    entrypoint string #sealed\n    label string\n",
        )
        .0;
        let clean = compute(
            "flow base:\n    entrypoint = \"search\"\n    label = \"x\"\n\n\
             flow t uses base:\n    label = \"y\"\n",
            &SchemaMode::Registry {
                models: &schema.models,
                enums: &schema.enums,
                oneofs: &schema.oneofs,
            },
            &default_config(),
            None,
            "",
            &|_| None,
        );
        assert!(
            clean.is_empty(),
            "the overlay validates through its RESOLVED body: {clean:?}"
        );
        let sealed = compute(
            "flow base:\n    entrypoint = \"search\"\n    label = \"x\"\n\n\
             flow t uses base:\n    entrypoint = \"admin\"\n",
            &SchemaMode::Registry {
                models: &schema.models,
                enums: &schema.enums,
                oneofs: &schema.oneofs,
            },
            &default_config(),
            None,
            "",
            &|_| None,
        );
        assert!(
            sealed.iter().any(|d| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "NML2060".to_string()
                ))),
            "compose findings reach the buffer: {sealed:?}"
        );
    }

    /// Editor parity for the union compose family: a discarded
    /// contribution (NML2085) and its "established here" related note
    /// reach the buffer through the composed path, in-buffer.
    #[test]
    fn editor_surfaces_union_discards_like_check() {
        let schema = nml_core::cst::extract_schema(
            "model ua:\n    x string\n\nmodel h:\n    slot (ua | string)\n",
        )
        .0;
        let uri = tower_lsp::lsp_types::Url::parse("file:///h.nml").unwrap();
        let diags = compute(
            "h base:\n    slot as ua:\n        x = \"1\"\n\nh t uses base:\n    slot = \"v\"\n",
            &SchemaMode::Registry {
                models: &schema.models,
                enums: &schema.enums,
                oneofs: &schema.oneofs,
            },
            &default_config(),
            Some(&uri),
            "",
            &|_| None,
        );
        let discard = diags
            .iter()
            .find(|d| {
                d.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "NML2085".to_string(),
                    ))
            })
            .unwrap_or_else(|| panic!("the discard reaches the buffer: {diags:?}"));
        assert_eq!(discard.range.start.line, 5, "on the discarded entry");
        let related = discard
            .related_information
            .as_ref()
            .expect("related note present when a uri is known");
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].message, "established here");
        assert_eq!(
            related[0].location.range.start.line, 1,
            "the establishing entry"
        );
    }

    /// Why the guard's finding carries a span: `push_diagnostic` drops a
    /// span-less finding (a validator defect the parity suite polices),
    /// so an un-anchored guard diagnostic would degrade SILENTLY.
    #[test]
    fn push_diagnostic_drops_a_spanless_finding_and_keeps_the_anchored_guard() {
        let line_index = LineIndex::new("flow t uses base:\n");
        let mut out = Vec::new();
        push_diagnostic(
            nml_core::diagnostic::Diagnostic::error("no span"),
            None,
            None,
            &line_index,
            &mut out,
        );
        assert!(out.is_empty(), "{out:?}");
        push_diagnostic(
            compose_guard_diag("boom"),
            None,
            None,
            &line_index,
            &mut out,
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            (out[0].range.start.line, out[0].range.start.character),
            (0, 0)
        );
        assert!(out[0].message.contains("boom"));
    }

    /// A non-item line in a modifier block reaches the editor at its own
    /// range (the entry, not the indent).
    #[test]
    fn the_editor_surfaces_a_modifier_block_non_item_at_its_range() {
        let schema = nml_core::cst::extract_schema("model policy:\n    |deny []string\n").0;
        let diags = compute(
            "policy p:\n    |deny:\n        - \"a\"\n        .note = \"x\"\n",
            &SchemaMode::Registry {
                models: &schema.models,
                enums: &schema.enums,
                oneofs: &schema.oneofs,
            },
            &default_config(),
            None,
            "",
            &|_| None,
        );
        let d = diags
            .iter()
            .find(|d| {
                d.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "NML0002".to_string(),
                    ))
            })
            .unwrap_or_else(|| panic!("{diags:?}"));
        assert!(
            d.message.contains("found a shared property"),
            "{}",
            d.message
        );
        assert_eq!((d.range.start.line, d.range.start.character), (3, 8));
        assert_eq!((d.range.end.line, d.range.end.character), (3, 19));
    }

    /// The never-go-dark guard: a panicking compose pass degrades to the
    /// raw findings plus one NML2086 that names the panic and carries a
    /// span (a span-less finding is dropped before the editor sees it);
    /// a panicking fallback is guarded too.
    #[test]
    fn a_compose_panic_degrades_to_raw_findings_plus_nml2086() {
        let raw = || {
            vec![
                nml_core::diagnostic::Diagnostic::warning("raw")
                    .with_span(nml_core::span::Span::empty(3)),
            ]
        };
        let out = never_dark(|| panic!("boom"), raw);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].message, "raw");
        assert_eq!(
            out[1].code,
            Some(nml_core::diagnostic::codes::INTERNAL_COMPOSE_INVARIANT)
        );
        assert!(out[1].message.contains("boom"), "{}", out[1].message);
        assert!(out[1].span.is_some(), "anchored, so it reaches the editor");
        let out: Vec<nml_core::diagnostic::Diagnostic> =
            never_dark(|| panic!("boom"), || panic!("again"));
        assert_eq!(out.len(), 1, "{out:?}");
    }

    /// The zero-item warning at a union position reaches the editor in
    /// its union wording (a scalar/list variant is not "the list").
    #[test]
    fn editor_surfaces_union_zero_item_warnings() {
        let schema = nml_core::cst::extract_schema(
            "model ua:\n    x string\n\nmodel ub:\n    kind string\n\nmodel h:\n    slot (ua | []ub)\n",
        )
        .0;
        let diags = compute(
            "h base:\n    slot:\n        - w:\n            kind = \"k\"\n\nh t uses base:\n    slot = []\n",
            &SchemaMode::Registry {
                models: &schema.models,
                enums: &schema.enums,
                oneofs: &schema.oneofs,
            },
            &default_config(),
            None,
            "",
            &|_| None,
        );
        let warn = diags
            .iter()
            .find(|d| {
                d.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "NML2079".to_string(),
                    ))
            })
            .unwrap_or_else(|| panic!("the zero-item warning reaches the buffer: {diags:?}"));
        assert_eq!(warn.range.start.line, 6);
        assert!(
            warn.message.contains("never establishes a variant"),
            "{}",
            warn.message
        );
    }

    /// Editor parity with `check`'s structural compose: a buffer with NO
    /// schema anywhere still composes structurally, so a typo'd `uses`
    /// ref squiggles instead of going dark.
    #[test]
    fn schemaless_buffers_compose_structurally() {
        let diags = compute(
            "flow memberLookup:\n    entrypoint = \"x\"\n\nflow t uses memberLookop:\n    entrypoint = \"y\"\n",
            &SchemaMode::Registry {
                models: &[],
                enums: &[],
                oneofs: &[],
            },
            &default_config(),
            None,
            "",
            &|_| None,
        );
        assert!(
            diags.iter().any(|d| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "NML2059".to_string()
                ))
                && d.message.contains("did you mean 'memberLookup'")),
            "structural NML2059 with hint reaches the buffer: {diags:?}"
        );
    }

    /// Mixed `.nml` buffers get schema-load merge-policy lints — NML2068
    /// is an ERROR the CLI reports; the load pass covers only
    /// `.model.nml` URIs, so this arm must carry it.
    #[test]
    fn mixed_buffers_get_merge_policy_lints() {
        let schema = nml_core::cst::extract_schema("model m:\n    xs []string\n").0;
        let diags = compute(
            "model bad:\n    xs []string #sealed #append\n\nm i:\n    xs = [\"a\"]\n",
            &SchemaMode::Registry {
                models: &schema.models,
                enums: &schema.enums,
                oneofs: &schema.oneofs,
            },
            &default_config(),
            None,
            "",
            &|_| None,
        );
        assert!(
            diags.iter().any(|d| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "NML2068".to_string()
                ))),
            "the incoherent-policy ERROR squiggles in the editor: {diags:?}"
        );
    }

    /// The cap boundary is exact: AT the cap nothing changes (no phantom
    /// summary row); one past it truncates and appends the summary with
    /// an accurate count.
    #[test]
    fn diagnostic_cap_boundary_is_exact() {
        let row = |i: usize| Diagnostic {
            message: format!("finding {i}"),
            ..Default::default()
        };
        let mut at_cap: Vec<Diagnostic> = (0..MAX_DIAGNOSTICS).map(row).collect();
        cap_diagnostics(&mut at_cap, 0);
        assert_eq!(at_cap.len(), MAX_DIAGNOSTICS, "at the cap: untouched");
        assert!(
            !at_cap.last().unwrap().message.contains("not shown"),
            "no phantom summary at the boundary"
        );
        let mut over: Vec<Diagnostic> = (0..MAX_DIAGNOSTICS + 1).map(row).collect();
        cap_diagnostics(&mut over, 0);
        assert_eq!(over.len(), MAX_DIAGNOSTICS + 1, "tranche plus summary");
        assert!(
            over.last()
                .unwrap()
                .message
                .starts_with("1 further finding"),
            "accurate elided count: {}",
            over.last().unwrap().message
        );
        // What the validator counted past its own tranche rides the row
        // even when the union sits at the cap.
        let mut at_cap: Vec<Diagnostic> = (0..MAX_DIAGNOSTICS).map(row).collect();
        cap_diagnostics(&mut at_cap, 7);
        assert_eq!(at_cap.len(), MAX_DIAGNOSTICS + 1);
        assert!(
            at_cap
                .last()
                .unwrap()
                .message
                .starts_with("7 further finding"),
            "{}",
            at_cap.last().unwrap().message
        );
    }

    /// r89 (P7): the validator pass HOLDS only its tranche — `Validated`
    /// carries [`MAX_DIAGNOSTICS`] findings and the exact count past
    /// them — so a flood is bounded at the source, not truncated after
    /// being held whole (the cap row alone would read the same either
    /// way; this pins the memory shape).
    #[test]
    fn the_validator_pass_holds_only_its_tranche() {
        let (schema, diags) =
            nml_validate::loader::load_schema(&[("m.model.nml", "model thing:\n    v string\n")]);
        assert!(diags.is_empty(), "{diags:?}");
        let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs).strict();
        let mut text = String::from("thing t:\n    v = \"x\"\n");
        for i in 0..1200 {
            text.push_str(&format!("    f{i} = 1\n"));
        }
        let file = nml_core::cst::parse_to_ast(&text).expect("parses");
        let validated = composed_validate(
            &validator,
            &file,
            "flood.nml",
            &nml_core::layers::OpenContext,
            &|_| true,
        );
        assert_eq!(validated.diagnostics.len(), MAX_DIAGNOSTICS);
        assert_eq!(validated.elided, 700);
    }

    /// The front end's suppression is applied AT THE SOURCE, ahead of
    /// the bounded tranche: 600 composition verdicts the load pass owns
    /// and three unknown-field errors publish exactly the three, with
    /// nothing counted past the tranche. Filtering after the tranche
    /// kept 500 of the 603, dropped the suppressed ones among them, and
    /// counted the 103 it never saw — a phantom `103 further`.
    #[test]
    fn suppressed_findings_never_fill_the_tranche_or_the_count() {
        let (schema, diags) =
            nml_validate::loader::load_schema(&[("m.model.nml", "model thing:\n    v string\n")]);
        assert!(diags.is_empty(), "{diags:?}");
        let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs).strict();
        let mut text = String::new();
        for i in 0..600 {
            text.push_str(&format!("model m{i} is nonexistent:\n    v string\n"));
        }
        for i in 0..3 {
            text.push_str(&format!("thing t{i}:\n    v = \"x\"\n    bogus = 1\n"));
        }
        let file = nml_core::cst::parse_to_ast(&text).expect("parses");
        // The ground truth, unbounded: the verdicts and the rest.
        let truth = validator.validate(&file);
        let verdicts = truth.iter().filter(|d| is_composition_verdict(d)).count();
        let rest = truth.len() - verdicts;
        assert!(
            verdicts >= 600 && rest < MAX_DIAGNOSTICS,
            "{verdicts} verdicts, {rest} others"
        );
        let all = composed_validate(
            &validator,
            &file,
            "mixed.nml",
            &nml_core::layers::OpenContext,
            &|_| true,
        );
        assert_eq!(all.diagnostics.len(), MAX_DIAGNOSTICS);
        assert_eq!(all.elided, truth.len() - MAX_DIAGNOSTICS);
        let kept = composed_validate(
            &validator,
            &file,
            "mixed.nml",
            &nml_core::layers::OpenContext,
            &|d| !is_composition_verdict(d),
        );
        assert_eq!(kept.elided, 0, "{:?}", kept.diagnostics);
        assert_eq!(kept.diagnostics.len(), rest, "{:?}", kept.diagnostics);
        assert!(kept.diagnostics.iter().all(|d| !is_composition_verdict(d)));
    }

    /// The same, as the editor publishes it: a registry-mode buffer
    /// whose load pass owns composition publishes its three type errors
    /// and NO summary row — the 600 suppressed verdicts are neither
    /// shown nor counted as `further`.
    #[test]
    fn a_suppressed_flood_publishes_no_phantom_summary_row() {
        let (schema, diags) =
            nml_validate::loader::load_schema(&[("m.model.nml", "model thing:\n    v string\n")]);
        assert!(diags.is_empty(), "{diags:?}");
        let mut text = String::new();
        for i in 0..600 {
            text.push_str(&format!("model m{i} is nonexistent:\n    v string\n"));
        }
        for i in 0..3 {
            text.push_str(&format!("thing t{i}:\n    v = 1\n"));
        }
        let url = tower_lsp::lsp_types::Url::parse("file:///ws/mixed.nml").unwrap();
        let registry = SchemaMode::Registry {
            models: &schema.models,
            enums: &schema.enums,
            oneofs: &schema.oneofs,
        };
        let mut owned = default_config();
        owned.load_pass_owns_composition = true;
        let out = compute(&text, &registry, &owned, Some(&url), "mixed.nml", &|_| None);
        assert!(
            !out.iter().any(|d| d.message.contains("further finding")),
            "phantom summary row: {out:?}"
        );
        assert!(
            !out.iter()
                .any(|d| d.message.contains("unknown `is` target")),
            "a suppressed verdict published: {out:?}"
        );
        assert_eq!(
            out.iter()
                .filter(|d| d.message.contains("expected"))
                .count(),
            3,
            "{out:?}"
        );
        // Not owning composition, the same buffer floods: the tranche and
        // an exact row.
        let out = compute(
            &text,
            &registry,
            &default_config(),
            Some(&url),
            "mixed.nml",
            &|_| None,
        );
        assert!(
            out.iter().any(|d| d.message.contains("further finding")),
            "{}",
            out.len()
        );
    }

    /// r89 (P7): the validator's flood is bounded AT THE SOURCE — a
    /// buffer yielding 1,200 unknown-field errors under a strict binding
    /// publishes the first [`MAX_DIAGNOSTICS`] and one summary row whose
    /// count is exact — through the kernel's `Bounded` sink, never a
    /// 1,200-entry list truncated afterwards.
    #[test]
    fn the_validator_flood_is_bounded_at_the_source_with_an_exact_count() {
        let (schema, diags) =
            nml_validate::loader::load_schema(&[("m.model.nml", "model thing:\n    v string\n")]);
        assert!(diags.is_empty(), "{diags:?}");
        let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs).strict();
        let mut text = String::from("thing t:\n    v = \"x\"\n");
        for i in 0..1200 {
            text.push_str(&format!("    f{i} = 1\n"));
        }
        let url = tower_lsp::lsp_types::Url::parse("file:///ws/flood.nml").unwrap();
        let mode = SchemaMode::Package {
            validator: &validator,
            identity: "t".to_string(),
        };
        let out = compute(
            &text,
            &mode,
            &default_config(),
            Some(&url),
            "flood.nml",
            &|_| None,
        );
        assert_eq!(
            out.len(),
            MAX_DIAGNOSTICS + 1,
            "the tranche plus the summary row"
        );
        assert!(
            out.last()
                .unwrap()
                .message
                .starts_with("700 further finding(s) not shown"),
            "{}",
            out.last().unwrap().message
        );
    }

    /// A hostile or badly broken buffer must not stream tens of
    /// thousands of diagnostics to the client per keystroke: the cap
    /// keeps the first tranche and summarizes the tail.
    #[test]
    fn diagnostic_flood_is_capped_with_a_summary_row() {
        let schema = nml_core::cst::extract_schema(
            "model item:\n    name string+\n    v string\n\nmodel flow:\n    items []item #identity\n",
        )
        .0;
        let mut src = String::from(
            "flow base:\n    items:\n        - b0:\n            v = \"x\"\n\nflow t uses base:\n    items:\n",
        );
        for i in 0..1200 {
            src.push_str(&format!("        - g{i}:\n            v = \"y\"\n"));
        }
        let diags = compute(
            &src,
            &SchemaMode::Registry {
                models: &schema.models,
                enums: &schema.enums,
                oneofs: &schema.oneofs,
            },
            &default_config(),
            None,
            "",
            &|_| None,
        );
        assert!(
            diags.len() <= 501,
            "capped at the tranche plus one summary: {}",
            diags.len()
        );
        let last = diags.last().expect("non-empty");
        assert!(
            last.message.contains("not shown"),
            "the tail is summarized: {}",
            last.message
        );
    }

    /// The wire's `data.suggestions[]` names a file only for an edit that
    /// lands in ANOTHER document: an own-file edit carries no `source`
    /// (the code action is offered on this document, its title unqualified,
    /// and two twins of one edit collapse), a foreign one always carries it
    /// (the action is minted on that document and titled with it). Nothing
    /// to offer is no key at all.
    #[test]
    fn suggestion_data_names_a_file_only_when_the_edit_is_foreign() {
        use nml_core::diagnostic::Suggestion;
        use nml_core::span::Span;
        let span = Span::new(4, 10);
        let own = Suggestion::did_you_mean("version").at(span);
        let foreign = Suggestion::delete().at(span).in_file("core.model.nml");
        let data = suggestion_data(
            std::slice::from_ref(&own),
            |s: &Suggestion| s.source.as_deref().or(Some("demo.nml")),
            "demo.nml",
        )
        .expect("an own-file edit is still offered");
        let rows = data["suggestions"].as_array().expect("suggestions");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].get("source").is_none(),
            "own file, unnamed: {rows:?}"
        );
        assert_eq!(rows[0]["kind"], "didYouMean", "{rows:?}");
        let data = suggestion_data(
            &[own, foreign],
            |s: &Suggestion| s.source.as_deref().or(Some("demo.nml")),
            "demo.nml",
        )
        .expect("data");
        let rows = data["suggestions"].as_array().expect("suggestions");
        assert!(rows[0].get("source").is_none(), "{rows:?}");
        assert_eq!(rows[1]["source"], "core.model.nml", "{rows:?}");
        assert!(
            suggestion_data(&[], |s: &Suggestion| s.source.as_deref(), "demo.nml").is_none(),
            "nothing to offer is no key"
        );
    }

    /// RFC 0030: package-bound documents validate through the package's
    /// exclusive validator; strict errors carry the binding identity suffix,
    /// and derived suggestions ride `Diagnostic.data` for the code-action
    /// handler.
    #[test]
    fn package_mode_suffixes_identity_and_carries_suggestion_data() {
        let manifest = "\
package demo:
    version = \"0.1.0\"
    formatVersion = 1

[]schema schemas:
    - core:
        file = \"core.model.nml\"

[]validator validators:
    - core:
        files:
            - \"demo.nml\"
        schemas:
            - core
        strict = true
";
        let core = "enum sameSite:\n    - \"Lax\"\n    - \"Strict\"\n\nmodel core:\n    name string+\n    mode sameSite?\n";
        let package =
            nml_validate::package::SchemaPackage::from_parts(manifest, |_| Ok(core.to_string()))
                .unwrap();
        let binding = package.binding_for("demo.nml").unwrap();
        let validator = package.validator(binding).unwrap();
        let diags = compute(
            "core main:\n    mode = \"lax\"\n    unknownKey = 1\n",
            &SchemaMode::Package {
                validator: &validator,
                identity: "demo blake3:12345678, store current".to_string(),
            },
            &default_config(),
            None,
            "",
            &|_| None,
        );
        let dym = diags
            .iter()
            .find(|d| d.message.contains("did you mean \"Lax\""))
            .expect("did-you-mean");
        // The stable code rides the LSP wire as a string (RFC 0008).
        assert_eq!(
            dym.code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "NML2000".to_string()
            ))
        );
        assert!(
            dym.message
                .contains("(schema: demo blake3:12345678, store current)"),
            "{}",
            dym.message
        );
        let suggestion = dym
            .data
            .as_ref()
            .and_then(|d| d.get("suggestions"))
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .expect("data");
        assert_eq!(suggestion.get("replacement").unwrap().as_str(), Some("Lax"));
        assert_eq!(suggestion.get("kind").unwrap().as_str(), Some("didYouMean"));
        assert!(
            diags.iter().any(|d| d.message.contains("unknownKey")
                && d.severity == Some(DiagnosticSeverity::ERROR)),
            "strict mode active through the package profile: {diags:?}"
        );
    }

    /// Vocabulary fixture for the schema-source pass: the demo package's
    /// `live`/`restart`/`key(ident)` directives.
    fn demo_vocab(undeclared_sibling: bool) -> crate::packages::VocabularyMatch {
        crate::packages::VocabularyMatch {
            vocabulary: nml_validate::directives::Vocabulary::new(
                "demo",
                nml_validate::test_support::demo_package_with_directives()
                    .manifest
                    .directives,
            ),
            undeclared_sibling,
            universe: crate::packages::SchemaUniverse::None,
        }
    }

    /// Unknown directive: error + did-you-mean, and the structured suggestion
    /// APPLIES — splicing the replacement at its byte span yields a source the
    /// pass then accepts (guards the name-span math against the
    /// whole-directive span).
    #[test]
    fn unknown_directive_suggestion_applies() {
        let source = "model core:\n    name string #lvie\n";
        let diags = source_pass(source, &demo_vocab(false));
        let diag = diags
            .iter()
            .find(|d| d.message.contains("unknown directive '#lvie'"))
            .expect("unknown directive flagged");
        assert!(
            diag.message.contains("did you mean \"#live\""),
            "{}",
            diag.message
        );
        let s = diag
            .data
            .as_ref()
            .and_then(|d| d.get("suggestions"))
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .expect("structured suggestion");
        let (replacement, start, end) = (
            s.get("replacement").unwrap().as_str().unwrap(),
            s.get("start").unwrap().as_u64().unwrap() as usize,
            s.get("end").unwrap().as_u64().unwrap() as usize,
        );
        let fixed = format!("{}{}{}", &source[..start], replacement, &source[end..]);
        let rediags = source_pass(&fixed, &demo_vocab(false));
        assert!(
            rediags.is_empty(),
            "applying the suggestion must yield a clean directive: {rediags:?}"
        );
    }

    /// The language's merge-policy directives are known under a declared
    /// vocabulary (RFC 0019: merged into every vocabulary outcome), and a
    /// near-miss of one is suggested like any declared name.
    #[test]
    fn builtin_directives_are_known_under_a_declared_vocabulary() {
        let clean = "model core:\n    name string #sealed\n    steps []core #identity #append\n";
        assert!(source_pass(clean, &demo_vocab(false)).is_empty());
        let diags = source_pass("model core:\n    name string #seled\n", &demo_vocab(false));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0].message.contains("did you mean \"#sealed\""),
            "{}",
            diags[0].message
        );
    }

    /// Arity, both directions: a bare-declared directive with an argument,
    /// and an argful-declared directive without one.
    #[test]
    fn directive_arity_both_directions() {
        let source = "model core:\n    name string #live(3)\n    mode string? #key\n";
        let diags = source_pass(source, &demo_vocab(false));
        assert!(
            diags
                .iter()
                .any(|d| d.message == "'#live' takes no argument"),
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message == "'#key' requires an argument"),
            "{diags:?}"
        );
        // The satisfied shapes are clean.
        let ok = "model core:\n    name string #live\n    mode string? #key(host)\n";
        assert!(source_pass(ok, &demo_vocab(false)).is_empty());
    }

    /// Parity with nudge's boot gate (`verify_directive_vocabulary`):
    /// `#live` and `#restart` on the SAME field contradict — one error, on
    /// the later directive, wording matching the gate's.
    #[test]
    fn live_restart_conflict_on_same_field() {
        let source = "model core:\n    name string #live #restart\n    mode string? #live\n";
        let diags = source_pass(source, &demo_vocab(false));
        let conflicts: Vec<_> = diags
            .iter()
            .filter(|d| d.message == "'#live' and '#restart' contradict — pick one")
            .collect();
        assert_eq!(conflicts.len(), 1, "{diags:?}");
        assert_eq!(
            conflicts[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "{diags:?}"
        );
        // Either directive alone stays clean (asserted above via `mode`, and
        // the whole pass emits nothing else here).
        assert_eq!(diags.len(), 1, "{diags:?}");
    }

    /// A bare `#` (empty directive name) already carries the parser's
    /// "expected a directive name" error — the vocabulary pass must not
    /// stack an "unknown directive '#'" on top.
    #[test]
    fn bare_hash_is_not_double_reported() {
        let source = "model core:\n    name string #\n";
        let diags = source_pass(source, &demo_vocab(false));
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("unknown directive")),
            "{diags:?}"
        );
    }

    /// Extraction errors surface through the pass — the registry path
    /// (`rebuild_schema_registry`) still discards them; this is the surface
    /// that reports them.
    #[test]
    fn extraction_error_surfaces() {
        let source = "model core:\n    name strin g+ @@@\n";
        let diags = source_pass(source, &demo_vocab(false));
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "schema-source extraction errors must surface: {diags:?}"
        );
    }

    /// The forgot-the-manifest trap: covered by the root rule, sitting next
    /// to the manifest, not declared — one info diagnostic names the fix.
    #[test]
    fn undeclared_sibling_info() {
        let source = "model extra:\n    name string+\n";
        let diags = source_pass(source, &demo_vocab(true));
        let info = diags
            .iter()
            .find(|d| d.severity == Some(DiagnosticSeverity::INFORMATION))
            .expect("sibling info emitted");
        assert_eq!(
            info.message,
            "not part of package 'demo'; add a []schema entry to participate"
        );
        // Declared / non-sibling coverage carries no info note.
        assert!(source_pass(source, &demo_vocab(false)).is_empty());
    }

    /// Registry-mode shim keeping the existing test bodies terse.
    fn compute_registry(
        source: &str,
        models: &[ModelDef],
        enums: &[EnumDef],
        oneofs: &[OneOfDef],
        config: &DiagnosticConfig,
    ) -> Vec<Diagnostic> {
        compute(
            source,
            &SchemaMode::Registry {
                models,
                enums,
                oneofs,
            },
            config,
            None,
            "",
            &|_| None,
        )
    }

    #[test]
    fn valid_source_no_diagnostics() {
        let source = "service Svc:\n    localMount = \"/\"\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            diags.is_empty(),
            "valid source should produce no diagnostics: {:?}",
            diags
        );
    }

    /// Round 22: a COVERED `.model.nml` file is validated by BOTH
    /// `compute` (parse band, this RFC) and `schema_source_pass`
    /// (RFC 0030, which re-derives extraction errors) — and the
    /// server's duplicate suppression collapses them to one squiggle.
    /// This pins the invariant that suppression depends on: the two
    /// paths must emit byte-identical range/message/severity, or a
    /// covered schema author sees the same error twice.
    #[test]
    fn covered_model_file_facet_error_is_not_double_squiggled() {
        let source = "model m:\n    s string(min = 1)\n";
        let mut cfg = default_config();
        cfg.uri_is_registry_source = true;
        let from_compute = compute_registry(source, &[], &[], &[], &cfg);
        let from_schema_pass = source_pass(source, &demo_vocab(false));

        let facet_of = |v: &[Diagnostic]| -> Vec<Diagnostic> {
            v.iter()
                .filter(|d| d.message.contains("facets attach only to `number`"))
                .cloned()
                .collect()
        };
        let a = facet_of(&from_compute);
        let b = facet_of(&from_schema_pass);
        assert_eq!(a.len(), 1, "compute must emit it once: {from_compute:?}");
        assert_eq!(
            b.len(),
            1,
            "schema_source_pass re-derives it once: {from_schema_pass:?}"
        );
        // The server's suppression predicate, verbatim.
        assert!(
            a[0].range == b[0].range
                && a[0].message == b[0].message
                && a[0].severity == b[0].severity,
            "the two paths must be exact duplicates or the server \
             double-squiggles:\n  compute: {:?}\n  schema:  {:?}",
            a[0],
            b[0]
        );
    }

    // ── Schema load pass (the editor calls the loader) ────────

    fn load_pass(own: &str, sources: &[(&str, &str)]) -> Vec<Diagnostic> {
        let owned: Vec<(String, String)> = sources
            .iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect();
        let own_text = sources
            .iter()
            .find(|(n, _)| *n == own)
            .map_or("", |(_, t)| *t);
        schema_load_pass(
            own,
            &owned,
            None,
            true,
            nml_core::cst::extract_schema(own_text),
        )
    }

    /// The certification probe, pinned: the load pass locates notes in
    /// the UNIVERSE'S name vocabulary — the names the loader stamps —
    /// never `uri.path()`, so a canonicalized `own_name` (macOS
    /// `/private/tmp`) beside a `/tmp` uri still renders a same-file
    /// note at its OWN span with a clean message, in the buffer's uri.
    #[test]
    fn load_pass_notes_survive_a_uri_spelling_mismatch() {
        let own_name = "/private/tmp/probe-ws/m.model.nml";
        let uri = tower_lsp::lsp_types::Url::parse("file:///tmp/probe-ws/m.model.nml").unwrap();
        // An unterminated string: the finding carries a same-file
        // "string opened here" note at the opening quote.
        let text = "model m:\n    name string = \"oops\n";
        let sources = vec![(own_name.to_string(), text.to_string())];
        let out = schema_load_pass(
            own_name,
            &sources,
            Some(&uri),
            true,
            nml_core::cst::extract_schema(text),
        );
        let with_note = out
            .iter()
            .find(|d| {
                d.related_information
                    .as_ref()
                    .is_some_and(|r| !r.is_empty())
            })
            .unwrap_or_else(|| panic!("no noted finding: {out:?}"));
        let note = &with_note.related_information.as_ref().unwrap()[0];
        assert_eq!(
            note.message, "string opened here",
            "clean message — no `(in …)` fallback: {note:?}"
        );
        assert_eq!(note.location.uri, uri, "the buffer's own uri");
        let quote_line = 1u32; // the opening quote sits on line 2 (0-based 1)
        assert_eq!(
            note.location.range.start.line, quote_line,
            "the note's OWN span, not the diagnostic's: {note:?}"
        );
    }

    /// Another file's findings must NOT paint onto this buffer. The
    /// merged registry once put five other files' default errors on a
    /// two-line config buffer, spans mapped through the wrong line
    /// index and clamped (round 29). The load pass enforces the rule
    /// structurally: every finding carries its declaring source, and
    /// only this buffer's survive the filter.
    #[test]
    fn other_files_findings_never_land_on_this_buffer() {
        let bad = "model cfg:\n    count number = \"high\"\n";
        let clean = "// just a comment\n";
        let diags = load_pass(
            "own.model.nml",
            &[
                ("own.model.nml", clean),
                ("f1.model.nml", bad),
                ("f2.model.nml", "model broken:\n    @@@\n"),
            ],
        );
        assert!(
            diags.is_empty(),
            "foreign defaults and parse errors must not appear here: {diags:?}"
        );
        // And the buffer's OWN bad default still reports, exactly once.
        let diags = load_pass("own.model.nml", &[("own.model.nml", bad)]);
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.message.contains("as the default for"))
                .count(),
            1,
            "own default reports once: {diags:?}"
        );
    }

    /// A schema author sees a bad DEFAULT in the editor, once — via the
    /// loader itself, so editor and CLI cannot disagree about a schema
    /// they both read. An unresolvable enum reference must NOT
    /// false-positive (value-shape fallback: false negatives, never
    /// false positives).
    #[test]
    fn default_type_errors_reach_the_editor_once() {
        let source = "model cfg:\n    count number = \"high\"\n";
        let diags = load_pass("cfg.model.nml", &[("cfg.model.nml", source)]);
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.message.contains("as the default for"))
                .count(),
            1,
            "want exactly one default diagnostic: {diags:?}"
        );
        let partial = "model m:\n    e someClosedEnum? = \"active\"\n";
        let d2 = load_pass("m.model.nml", &[("m.model.nml", partial)]);
        assert!(
            !d2.iter().any(|d| d.message.contains("as the default for")),
            "partial view must not invent default errors: {d2:?}"
        );
    }

    /// Cross-file composition: an unknown `is` target in THIS buffer is
    /// reported here; one resolved by a SIBLING source is not. The rule
    /// class that motivated the pass — load-only until now.
    #[test]
    fn unknown_is_target_reaches_editor_and_siblings_resolve() {
        let diags = load_pass(
            "child.model.nml",
            &[("child.model.nml", "model child is missing:\n    y string\n")],
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown `is` target")),
            "unresolved mixin must reach the editor: {diags:?}"
        );
        let diags = load_pass(
            "child.model.nml",
            &[
                ("base.model.nml", "trait base:\n    x string\n"),
                ("child.model.nml", "model child is base:\n    y string\n"),
            ],
        );
        assert!(
            diags.is_empty(),
            "a sibling-resolved mixin must not error: {diags:?}"
        );
    }

    /// Shorthand arity is judged POST-inheritance (RFC 0005 §8): an
    /// inherited `!` plus an own `!` collide, and the finding lands on
    /// THIS buffer (the child wrote the second one).
    #[test]
    fn inherited_shorthand_collision_reaches_editor() {
        let diags = load_pass(
            "child.model.nml",
            &[
                ("base.model.nml", "trait base:\n    a string!\n"),
                ("child.model.nml", "model child is base:\n    b string!\n"),
            ],
        );
        assert!(
            diags.iter().any(|d| d.message.contains("!")),
            "post-inheritance shorthand collision must reach the editor: {diags:?}"
        );
    }

    /// A reference cycle spanning two files reports on THIS buffer's
    /// member (each member gets its own copy, stamped with its source).
    #[test]
    fn cross_file_cycle_lands_on_own_member() {
        let diags = load_pass(
            "b.model.nml",
            &[
                ("a.model.nml", "model a:\n    fb b\n"),
                ("b.model.nml", "model b:\n    fa a\n"),
            ],
        );
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.message.contains("circular dependency"))
                .count(),
            1,
            "own cycle member reports once: {diags:?}"
        );
    }

    /// Reserved type-constructor names reach the editor (previously
    /// load-only), and a cross-source duplicate is attributed to the
    /// SECOND source — shown when editing it, silent on the first.
    #[test]
    fn reserved_and_duplicate_names_reach_editor_with_loader_attribution() {
        let diags = load_pass(
            "s.model.nml",
            &[("s.model.nml", "model set:\n    x string\n")],
        );
        assert!(
            diags.iter().any(|d| d.message.contains("set")),
            "reserved constructor name must reach the editor: {diags:?}"
        );
        let first = ("first.model.nml", "model x:\n    a string\n");
        let second = ("second.model.nml", "model x:\n    b string\n");
        let on_second = load_pass("second.model.nml", &[first, second]);
        assert!(
            on_second
                .iter()
                .any(|d| d.message.contains("duplicate model definition")),
            "duplicate attributes to the second source: {on_second:?}"
        );
        let on_first = load_pass("first.model.nml", &[first, second]);
        assert!(
            !on_first
                .iter()
                .any(|d| d.message.contains("duplicate model definition")),
            "first definition wins and stays clean: {on_first:?}"
        );
    }

    /// Snapshot universes deliver LOGICAL source names ("base"), not
    /// filenames — the load pass and its filter must be name-shape
    /// agnostic (the buffer's own key is a path; package entries are
    /// bare identifiers).
    #[test]
    fn logical_source_names_resolve_like_filenames() {
        let diags = load_pass(
            "/ws/child.model.nml",
            &[
                ("base", "trait base:\n    x string\n"),
                (
                    "/ws/child.model.nml",
                    "model child is base, nope:\n    y string\n",
                ),
            ],
        );
        let msgs: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        assert!(
            !msgs.iter().any(|m| m.contains("'base'")),
            "base must resolve: {msgs:?}"
        );
    }

    /// A universe with no entry for the buffer reports nothing — the
    /// pass can attribute nothing to it.
    #[test]
    fn load_pass_without_own_entry_is_empty() {
        let diags = load_pass(
            "missing.model.nml",
            &[("other.model.nml", "model broken:\n    @@@\n")],
        );
        assert!(diags.is_empty(), "no own entry ⇒ no findings: {diags:?}");
    }

    /// RFC 0018 (round 22): facet DEFINITION findings reach the editor
    /// for BOTH document shapes — exactly once each. The pure
    /// `.model.nml` case is the one that matters most (facets are
    /// authored there) and is precisely the one a mode-gated
    /// publication missed: registry sources skip the merge branch, and
    /// the covered-schema pass has no production caller.
    #[test]
    fn facet_definition_errors_publish_once_for_every_document_shape() {
        let mut model_cfg = default_config();
        model_cfg.uri_is_registry_source = true;
        for (label, source, cfg) in [
            (
                "pure .model.nml",
                "model m:\n    s string(min = 1)\n",
                &model_cfg,
            ),
            (
                "mixed self-validating",
                "model m:\n    s string(min = 1)\n\nm A:\n    s = \"x\"\n",
                &default_config(),
            ),
        ] {
            let diags = compute_registry(source, &[], &[], &[], cfg);
            let hits = diags
                .iter()
                .filter(|d| d.message.contains("facets attach only to `number`"))
                .count();
            assert_eq!(hits, 1, "{label}: want exactly one NML2058, got {diags:?}");
        }
    }

    #[test]
    fn parse_error_produces_diagnostic() {
        let source = "service\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(!diags.is_empty(), "parse error should produce diagnostics");
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        );
    }

    #[test]
    fn validation_survives_syntax_error_in_another_decl() {
        // The legacy parser went dark on the first syntax error, suppressing all
        // semantic feedback. With resilient parsing the malformed declaration is
        // recovered and the duplicate among the well-formed ones is still flagged.
        let source = concat!(
            "service @@@\n", // syntactic garbage — recovered, not fatal
            "service Svc:\n    localMount = \"/\"\n\n",
            "service Svc:\n    localMount = \"/other\"\n",
        );
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            diags.iter().any(|d| d.message.contains("duplicate")),
            "duplicate must be reported despite an earlier syntax error: {:?}",
            diags
        );
    }

    #[test]
    fn multiple_syntax_errors_all_reported() {
        // All-errors: every syntax error surfaces at once rather than the user
        // fixing one to reveal the next (legacy first-error whack-a-mole).
        let source = "const = 1\nconst = 2\nconst = 3\n";
        let errors: Vec<_> = compute_registry(source, &[], &[], &[], &default_config())
            .into_iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        assert!(
            errors.len() >= 3,
            "expected an error per malformed const, got {}: {:?}",
            errors.len(),
            errors
        );
    }

    #[test]
    fn schema_validation_survives_syntax_error_elsewhere() {
        // Resilience with a configured schema: a required-field violation in a
        // well-formed declaration is still reported even though another
        // declaration has a hard syntax error. Legacy went dark on the first error.
        let extracted = nml_core::cst::extract_schema("model svc:\n    port number\n").0;
        let source = "svc A:\n    @@@\n\nsvc B:\n    other = \"x\"\n";
        let diags = compute_registry(
            source,
            &extracted.models,
            &extracted.enums,
            &extracted.oneofs,
            &default_config(),
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("missing required field 'port'")),
            "schema validation must run on the best-effort AST despite a syntax error: {diags:?}"
        );
    }

    /// A repeated declaration name is the parse band's row (NML1000) at
    /// the later NAME (the note rides `relatedInformation` where a uri
    /// gives it a Location — the harness pins that on the document).
    #[test]
    fn duplicate_decl_produces_diagnostic() {
        let source =
            "service Svc:\n    localMount = \"/\"\n\nservice Svc:\n    localMount = \"/other\"\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        let rows: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(&d.code, Some(tower_lsp::lsp_types::NumberOrString::String(c)) if c == "NML1000")
            })
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "duplicate declarations should be flagged: {diags:?}"
        );
        assert_eq!(rows[0].range.start.line, 3, "{:?}", rows[0]);
        assert_eq!(rows[0].range.start.character, 8, "{:?}", rows[0]);
        assert!(
            rows[0].message.starts_with("duplicate declaration 'Svc'"),
            "{:?}",
            rows[0]
        );
    }

    #[test]
    fn const_cycle_produces_diagnostic() {
        let source = "const A = B\nconst B = A\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("circular reference")),
            "const cycles should be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn const_chain_without_cycle_not_flagged() {
        let source = "const A = 1\nconst B = A\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("circular reference")),
            "acyclic const chains should not be flagged: {:?}",
            diags
        );
    }

    /// The NML5004 did-you-mean replaces exactly the namespace of an
    /// expression whose row spans the whole `{{…}}` — applied at its byte
    /// span it yields the corrected expression, nothing else touched.
    #[test]
    fn unknown_template_namespace_fix_replaces_only_the_namespace() {
        let source = "service Svc:\n    val = \"ab {{ arg.name }} x\"\n";
        let config = config_with_namespaces(&["args"]);
        let diag = compute_registry(source, &[], &[], &[], &config)
            .into_iter()
            .find(|d| d.message.contains("unknown template namespace 'arg'"))
            .expect("namespace diagnostic expected");
        let data = diag.data.expect("suggestion data");
        let s = &data["suggestions"][0];
        let (start, end) = (
            s["start"].as_u64().expect("start") as usize,
            s["end"].as_u64().expect("end") as usize,
        );
        assert_eq!(&source[start..end], "arg", "{data}");
        let mut fixed = source.to_string();
        fixed.replace_range(start..end, s["replacement"].as_str().expect("replacement"));
        assert_eq!(fixed, "service Svc:\n    val = \"ab {{ args.name }} x\"\n");
    }

    #[test]
    fn diagnostic_ranges_use_utf16_columns() {
        // Identical sources except "ab" (2 bytes / 2 chars / 2 UTF-16 units)
        // is replaced by an emoji (4 bytes / 1 char / 2 UTF-16 units).
        // Equal ranges prove the conversion is UTF-16, not bytes or chars.
        let ascii = "service Svc:\n    val = \"ab {{foo.bar}} x\"\n";
        let emoji = "service Svc:\n    val = \"😀 {{foo.bar}} x\"\n";
        let config = config_with_namespaces(&["args"]);

        let find_range = |source: &str| {
            compute_registry(source, &[], &[], &[], &config)
                .into_iter()
                .find(|d| d.message.contains("unknown template namespace 'foo'"))
                .expect("namespace diagnostic expected")
                .range
        };

        let ascii_range = find_range(ascii);
        let emoji_range = find_range(emoji);
        assert_eq!(
            ascii_range, emoji_range,
            "multibyte prefix with equal UTF-16 width must not shift the range"
        );
        assert!(ascii_range.start.character > 0);
    }

    #[test]
    fn unresolved_ref_produces_diagnostic() {
        let source = "workflow W:\n    provider = NonExistent\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            diags.iter().any(|d| d.message.contains("unresolved")),
            "unresolved references should be flagged: {:?}",
            diags
        );
    }

    fn config_with_namespaces(ns: &[&str]) -> DiagnosticConfig {
        DiagnosticConfig {
            template_namespaces: ns.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn valid_template_namespace_no_diagnostic() {
        let source = "service Svc:\n    instructions = \"{{args.instructions}} base\"\n";
        let config = config_with_namespaces(&["args", "steps"]);
        let diags = compute_registry(source, &[], &[], &[], &config);
        assert!(
            !diags.iter().any(|d| d.message.contains("namespace")),
            "valid namespace should not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn empty_namespaces_accepts_all() {
        let source = "service Svc:\n    val = \"{{anything.goes}} ok\"\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            !diags.iter().any(|d| d.message.contains("namespace")),
            "empty namespace config should accept all namespaces: {:?}",
            diags
        );
    }

    #[test]
    fn unknown_namespace_flagged_when_configured() {
        let source = "service Svc:\n    val = \"{{foo.bar}} baz\"\n";
        let config = config_with_namespaces(&["args", "steps"]);
        let diags = compute_registry(source, &[], &[], &[], &config);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown template namespace 'foo'")),
            "unknown namespace should be flagged when namespaces are configured: {:?}",
            diags
        );
    }

    #[test]
    fn valid_bare_step_ref_no_diagnostic() {
        let source = concat!(
            "workflow W:\n",
            "    entrypoint = start\n",
            "    steps:\n",
            "        - start:\n",
            "            next = respond\n",
            "        - respond:\n",
            "            provider = \"groq\"\n",
        );
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            !diags.iter().any(|d| d.message.contains("unresolved")),
            "valid bare step refs should not produce diagnostics: {:?}",
            diags
        );
    }

    #[test]
    fn invalid_bare_step_ref_produces_diagnostic() {
        let source = concat!(
            "workflow W:\n",
            "    entrypoint = start\n",
            "    steps:\n",
            "        - start:\n",
            "            next = nonexistent\n",
        );
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unresolved reference 'nonexistent'")),
            "invalid bare step ref should be flagged: {:?}",
            diags
        );
    }

    /// RFC 0009: related information reaches the editor spec-natively —
    /// an unterminated string's diagnostic carries a `relatedInformation`
    /// entry pointing at the opening delimiter, located in the document's
    /// own uri.
    #[test]
    fn related_information_is_spec_native_with_uri() {
        let uri = tower_lsp::lsp_types::Url::parse("file:///test.nml").unwrap();
        let diags = compute(
            "service Api:\n    name = \"abc\n",
            &SchemaMode::Registry {
                models: &[],
                enums: &[],
                oneofs: &[],
            },
            &DiagnosticConfig::default(),
            Some(&uri),
            "",
            &|_| None,
        );
        let unterminated = diags
            .iter()
            .find(|d| d.message.contains("unterminated"))
            .expect("unterminated-string diagnostic");
        let related = unterminated
            .related_information
            .as_ref()
            .expect("relatedInformation present when a uri is known");
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].location.uri, uri);
        assert!(related[0].message.contains("opened here"));
    }

    /// RFC 0012: in registry (open) mode, the document's own definitions
    /// type its own instances — editor parity with `nml check`.
    #[test]
    fn registry_mode_types_self_contained_documents() {
        let source = "model cache:\n    maxEntries number\n\ncache Hot:\n    enabled = false\n";
        let diags = compute(
            source,
            &SchemaMode::Registry {
                models: &[],
                enums: &[],
                oneofs: &[],
            },
            &DiagnosticConfig::default(),
            None,
            "",
            &|_| None,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("missing required field 'maxEntries'")),
            "doc-local model must type the doc's own instance: {diags:?}"
        );
    }

    /// RFC 0012 editor parity: an open-mode document redefining a registry
    /// name gets the same NML2009 the CLI reports — never a silent shadow.
    #[test]
    fn registry_collision_reports_nml2009() {
        let registry = nml_core::cst::extract_schema("model cache:\n    maxEntries number\n").0;
        let source = "model cache:\n    other string?\n\ncache Hot:\n    other = \"x\"\n";
        let diags = compute(
            source,
            &SchemaMode::Registry {
                models: &registry.models,
                enums: &registry.enums,
                oneofs: &registry.oneofs,
            },
            &DiagnosticConfig::default(),
            None,
            "",
            &|_| None,
        );
        assert!(
            diags.iter().any(|d| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "NML2009".into()
                ))
                && d.message.contains("duplicate model definition 'cache'")),
            "{diags:?}"
        );
    }

    /// ONE registry-first rule for every definition kind: a buffer
    /// redefining a registry model, enum AND oneof gets three NML2009 rows,
    /// each naming its kind, and the REGISTRY's definition keeps typing the
    /// buffer's instances (the buffer's `cache` has no `maxEntries`; the
    /// registry's requires it) — a shadow would type them by the buffer's.
    /// The buffer's definition does not reach the DEFINITION-level passes
    /// either: its `#append` on a scalar would mint NML2068 against
    /// `cache.other` — a field the REGISTRY's `cache` has not got, a
    /// buffer minting a schema lint against a model it does not own.
    #[test]
    fn registry_first_join_is_one_rule_for_models_enums_and_oneofs() {
        let registry = nml_core::cst::extract_schema(concat!(
            "model cache:\n    maxEntries number\n\nenum level:\n    - \"a\"\n\n",
            "oneof shape by kind:\n    \"c\" -> cache\n"
        ))
        .0;
        let source = concat!(
            "model cache:\n    other string? #append\n\nenum level:\n    - \"b\"\n\n",
            "oneof shape by kind:\n    \"d\" -> cache\n\ncache Hot:\n    other = \"x\"\n"
        );
        let diags = compute(
            source,
            &SchemaMode::Registry {
                models: &registry.models,
                enums: &registry.enums,
                oneofs: &registry.oneofs,
            },
            &DiagnosticConfig::default(),
            None,
            "",
            &|_| None,
        );
        let nml2009: Vec<&str> = diags
            .iter()
            .filter(|d| {
                d.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "NML2009".into(),
                    ))
            })
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(
            nml2009,
            [
                "duplicate model definition 'cache' — the workspace schema registry already \
                 defines it",
                "duplicate enum definition 'level' — the workspace schema registry already \
                 defines it",
                "duplicate oneof definition 'shape' — the workspace schema registry already \
                 defines it",
            ],
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("missing required field 'maxEntries'")),
            "the registry's `cache` types the buffer's instance: {diags:?}"
        );
        assert!(
            !diags.iter().any(|d| {
                d.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "NML2068".into(),
                    ))
            }),
            "the shadowed definition reached the policy pass: {diags:?}"
        );
    }

    /// A `.model.nml` document IS a registry source: its own definitions
    /// must not self-collide (the registry already carries them).
    #[test]
    fn registry_source_documents_do_not_self_collide() {
        let src = "model cache:\n    maxEntries number\n";
        let registry = nml_core::cst::extract_schema(src).0;
        let config = DiagnosticConfig {
            uri_is_registry_source: true,
            ..DiagnosticConfig::default()
        };
        let diags = compute(
            src,
            &SchemaMode::Registry {
                models: &registry.models,
                enums: &registry.enums,
                oneofs: &registry.oneofs,
            },
            &config,
            None,
            "",
            &|_| None,
        );
        assert!(
            diags.iter().all(|d| d.code
                != Some(tower_lsp::lsp_types::NumberOrString::String(
                    "NML2009".into()
                ))),
            "{diags:?}"
        );
    }

    /// The ONE predicate the two passes share names both composition
    /// verdicts — an `is` target the validator could not resolve
    /// (`UNKNOWN_MIXIN`) and one of the wrong kind (`INVALID_MIXIN_KIND`)
    /// — and nothing else:
    /// a predicate that knew one of the two would let the other be
    /// reported twice (the load pass's and the validator's), or dropped
    /// by neither.
    #[test]
    fn the_composition_verdict_predicate_covers_both_codes_and_no_other() {
        let with = |code| nml_core::diagnostic::Diagnostic::error("x").with_code(code);
        assert!(is_composition_verdict(&with(codes::UNKNOWN_MIXIN)));
        assert!(is_composition_verdict(&with(codes::INVALID_MIXIN_KIND)));
        assert!(!is_composition_verdict(&with(codes::UNKNOWN_PROPERTY)));
        assert!(!is_composition_verdict(
            &nml_core::diagnostic::Diagnostic::error("uncoded")
        ));
    }
}
