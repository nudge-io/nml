use std::collections::HashSet;
use std::sync::Arc;

use nml_core::ast::*;
use nml_core::model::{EnumDef, ModelDef, OneOfDef};
use nml_core::types::{TemplateSegment, Value};
use nml_validate::diagnostics::Severity;
use nml_validate::schema::{MembershipSemantics, SchemaValidator};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::position::LineIndex;

/// Embedder-supplied semantics for the LSP.  Every method has a default that
/// returns nothing, so a generic adopter gets a purely structural LSP.
pub trait LanguageExtension: Send + Sync {
    /// Resolve valid identifiers under `namespace` for a given declaration.
    /// Return `None` to skip reference checking for this namespace.
    fn resolve_identifiers(
        &self,
        _decl: &Declaration,
        _namespace: &str,
    ) -> Option<HashSet<String>> {
        None
    }

    /// Markdown hover documentation for `{{namespace.path}}`.
    fn template_hover(&self, _namespace: &str, _path: &str) -> Option<String> {
        None
    }

    /// Built-in `@kind/name`-style references for completion menus.
    /// Returns `(label, description)` pairs.
    fn builtin_reference_completions(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Configuration for diagnostics computation.
#[derive(Default, Clone)]
pub struct DiagnosticConfig {
    /// Valid template namespaces. If empty, all namespaces are accepted.
    pub template_namespaces: Vec<String>,
    /// Valid modifier names. If empty, all modifiers are accepted.
    pub modifiers: Vec<String>,
    /// Membership semantics passed through to `SchemaValidator`.
    pub membership: MembershipSemantics,
    /// Optional embedder-supplied language extension.
    pub language_extension: Option<Arc<dyn LanguageExtension>>,
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
pub fn compute(source: &str, mode: &SchemaMode<'_>, config: &DiagnosticConfig) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let line_index = LineIndex::new(source);

    // Resilient parse: always yields a best-effort AST plus the full set of
    // syntactic + semantic errors (position-sorted, bounded). Reporting every
    // error at once replaces the legacy first-error-only behaviour, and running
    // the validators on the best-effort AST keeps feedback alive mid-edit
    // instead of going dark on the first syntax error.
    let (file, parse_errors) = nml_core::cst::parse_to_ast_all(source);

    for err in &parse_errors {
        diagnostics.push(nml_error_to_diagnostic(err, &line_index));
    }

    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);

    for err in symbols.find_duplicates() {
        diagnostics.push(nml_error_to_diagnostic(&err, &line_index));
    }

    for err in symbols.find_unresolved_references(&file) {
        diagnostics.push(nml_error_to_diagnostic(&err, &line_index));
    }

    for err in symbols.find_const_cycles() {
        diagnostics.push(nml_error_to_diagnostic(&err, &line_index));
    }

    match mode {
        SchemaMode::Registry {
            models,
            enums,
            oneofs,
        } => {
            if !models.is_empty() || !enums.is_empty() || !oneofs.is_empty() {
                let validator =
                    SchemaValidator::new(models.to_vec(), enums.to_vec(), oneofs.to_vec())
                        .with_modifiers(config.modifiers.clone())
                        .with_membership_semantics(config.membership.clone());
                for diag in validator.validate(&file) {
                    push_validator_diagnostic(diag, None, &line_index, &mut diagnostics);
                }
            }
        }
        SchemaMode::Package {
            validator,
            identity,
        } => {
            for diag in validator.validate(&file) {
                push_validator_diagnostic(diag, Some(identity), &line_index, &mut diagnostics);
            }
        }
    }

    let ns: Vec<&str> = config
        .template_namespaces
        .iter()
        .map(|s| s.as_str())
        .collect();
    validate_templates(&file, &ns, config, &line_index, &mut diagnostics);

    diagnostics
}

/// Lower one validator diagnostic to LSP form: map severity, suffix the
/// binding identity onto package-mode errors, and carry any structured
/// suggestion into `Diagnostic.data` so the code-action handler can offer a
/// one-keystroke fix without re-deriving (or worse, message-parsing) it.
fn push_validator_diagnostic(
    diag: nml_validate::diagnostics::Diagnostic,
    identity: Option<&str>,
    line_index: &LineIndex,
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
    let message = match identity {
        Some(id) if is_error => format!("{} (schema: {id})", diag.message),
        _ => diag.message,
    };
    let data = diag.suggestion.as_ref().map(|s| {
        serde_json::json!({
            "suggestion": {
                "replacement": s.replacement,
                "start": s.span.start,
                "end": s.span.end,
            }
        })
    });
    out.push(Diagnostic {
        range: line_index.range(span),
        severity: Some(if is_error {
            DiagnosticSeverity::ERROR
        } else {
            DiagnosticSeverity::WARNING
        }),
        message,
        source: Some("nml".to_string()),
        data,
        ..Default::default()
    });
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
/// - directive validation against the covering package's vocabulary: unknown
///   name (with a machine-applicable did-you-mean) and arity per the declared
///   argument kind;
/// - the undeclared-sibling info — the forgot-the-manifest trap.
pub fn schema_source_pass(
    source: &str,
    vocab: &crate::packages::VocabularyMatch,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let line_index = LineIndex::new(source);
    let (schema, errors) = nml_core::cst::extract_schema(source);
    for err in &errors {
        out.push(nml_error_to_diagnostic(err, &line_index));
    }
    let vocab_names: Vec<String> = vocab.directives.iter().map(|d| d.name.clone()).collect();
    let vocab_has = |name: &str| vocab_names.iter().any(|n| n == name);
    for model in &schema.models {
        for field in &model.fields {
            for directive in &field.directives {
                check_directive(
                    directive,
                    source,
                    vocab,
                    &vocab_names,
                    &line_index,
                    &mut out,
                );
            }
            // Parity with nudge's boot gate (`verify_directive_vocabulary`):
            // `#live` and `#restart` on the SAME field is an error. Only when
            // the vocabulary declares both — in other vocabularies the names
            // carry no reload semantics (and undeclared ones already error as
            // unknown directives above).
            if vocab_has("live") && vocab_has("restart") {
                let live = field.directives.iter().find(|d| d.name == "live");
                let restart = field.directives.iter().find(|d| d.name == "restart");
                if let (Some(live), Some(restart)) = (live, restart) {
                    // Squiggle the later of the two — the addition that
                    // created the contradiction.
                    let span = if restart.span.start > live.span.start {
                        restart.span
                    } else {
                        live.span
                    };
                    out.push(Diagnostic {
                        range: line_index.range(span),
                        severity: Some(DiagnosticSeverity::ERROR),
                        message: "'#live' and '#restart' contradict — pick one".to_string(),
                        source: Some("nml".to_string()),
                        ..Default::default()
                    });
                }
            }
        }
    }
    if vocab.undeclared_sibling {
        out.push(Diagnostic {
            range: tower_lsp::lsp_types::Range::default(),
            severity: Some(DiagnosticSeverity::INFORMATION),
            message: format!(
                "not part of package '{}'; add a []schema entry to participate",
                vocab.package_name
            ),
            source: Some("nml".to_string()),
            ..Default::default()
        });
    }
    out
}

/// The byte span of a directive's *name* token. `Directive.span` covers the
/// whole construct (`#` through the close); the did-you-mean replacement must
/// cover the name only, so applying it yields a valid directive without
/// touching the `#` or any argument. Located by searching the directive's own
/// slice rather than assuming `start + 1` — the parser tolerates trivia
/// between `#` and the name, and a wrong span would make the quick-fix mangle
/// the source.
fn directive_name_span(
    directive: &nml_core::types::Directive,
    source: &str,
) -> nml_core::span::Span {
    let span = directive.span;
    let fallback = nml_core::span::Span::new(
        span.start + 1,
        (span.start + 1 + directive.name.len()).min(span.end),
    );
    let Some(slice) = source.get(span.start..span.end) else {
        return fallback;
    };
    // Skip the `#` itself, then take the first occurrence of the name — that
    // IS the name token (an argument can only follow it).
    match slice[1..].find(&directive.name) {
        Some(rel) => {
            let start = span.start + 1 + rel;
            nml_core::span::Span::new(start, start + directive.name.len())
        }
        None => fallback,
    }
}

/// Vocabulary checks for one parsed directive: unknown name (error, with a
/// structured suggestion when a near-miss exists) and arity per the declared
/// [`DirectiveArg`](nml_validate::package::DirectiveArg).
fn check_directive(
    directive: &nml_core::types::Directive,
    source: &str,
    vocab: &crate::packages::VocabularyMatch,
    vocab_names: &[String],
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    use nml_validate::package::DirectiveArg;
    // An empty name means the parser already reported "expected a directive
    // name" on this token — stacking an "unknown directive '#'" on top of
    // that error helps no one.
    if directive.name.is_empty() {
        return;
    }
    let decl = vocab.directives.iter().find(|d| d.name == directive.name);
    let Some(decl) = decl else {
        let mut message = format!(
            "unknown directive '#{}' (package '{}')",
            directive.name, vocab.package_name
        );
        let mut data = None;
        if let Some(suggested) =
            nml_validate::schema::suggest_directive(&directive.name, vocab_names)
        {
            message.push_str(&format!(" — did you mean '#{suggested}'?"));
            let name_span = directive_name_span(directive, source);
            // Same wire shape as the validator's enum suggestion — one
            // code-action consumer for both.
            data = Some(serde_json::json!({
                "suggestion": {
                    "replacement": suggested,
                    "start": name_span.start,
                    "end": name_span.end,
                }
            }));
        }
        out.push(Diagnostic {
            range: line_index.range(directive.span),
            severity: Some(DiagnosticSeverity::ERROR),
            message,
            source: Some("nml".to_string()),
            data,
            ..Default::default()
        });
        return;
    };
    // Wording matches nudge's boot gate (`verify_directive_vocabulary` in
    // reload_semantics.rs) byte-for-byte, so a schema author sees ONE message
    // per mistake regardless of which surface caught it first.
    let arity_error = match (decl.arg, directive.arg.is_some()) {
        (DirectiveArg::None, true) => Some(format!("'#{}' takes no argument", directive.name)),
        (DirectiveArg::None, false) => None,
        (_, false) => Some(format!("'#{}' requires an argument", directive.name)),
        (_, true) => None,
    };
    if let Some(message) = arity_error {
        out.push(Diagnostic {
            range: line_index.range(directive.span),
            severity: Some(DiagnosticSeverity::ERROR),
            message,
            source: Some("nml".to_string()),
            ..Default::default()
        });
    }
}

fn validate_shared_property_templates(
    sp: &SharedProperty,
    decl: &Declaration,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    match &sp.kind {
        SharedPropertyKind::Block(body) => {
            validate_body_templates(body, decl, valid_ns, config, line_index, diags);
        }
        SharedPropertyKind::Scalar(sv) => {
            validate_value_templates(&sv.value, decl, valid_ns, config, line_index, diags);
        }
    }
}

fn validate_templates(
    file: &File,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                validate_body_templates(&block.body, decl, valid_ns, config, line_index, diags);
            }
            DeclarationKind::Template(t) => {
                validate_value_templates(&t.value.value, decl, valid_ns, config, line_index, diags);
            }
            DeclarationKind::Const(c) => {
                validate_value_templates(&c.value.value, decl, valid_ns, config, line_index, diags);
            }
            DeclarationKind::Array(arr) => {
                for sp in &arr.body.shared_properties {
                    validate_shared_property_templates(
                        sp, decl, valid_ns, config, line_index, diags,
                    );
                }
                for prop in &arr.body.properties {
                    validate_value_templates(
                        &prop.value.value,
                        decl,
                        valid_ns,
                        config,
                        line_index,
                        diags,
                    );
                }
                for item in &arr.body.items {
                    validate_list_item_templates(item, decl, valid_ns, config, line_index, diags);
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
    decl: &Declaration,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                validate_value_templates(
                    &prop.value.value,
                    decl,
                    valid_ns,
                    config,
                    line_index,
                    diags,
                );
            }
            BodyEntryKind::NestedBlock(nested) => {
                validate_body_templates(&nested.body, decl, valid_ns, config, line_index, diags);
            }
            BodyEntryKind::ListItem(item) => {
                validate_list_item_templates(item, decl, valid_ns, config, line_index, diags);
            }
            BodyEntryKind::SharedProperty(shared) => {
                validate_shared_property_templates(
                    shared, decl, valid_ns, config, line_index, diags,
                );
            }
            _ => {}
        }
    }
}

fn validate_list_item_templates(
    item: &ListItem,
    decl: &Declaration,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    match &item.kind {
        ListItemKind::Named { body, .. } => {
            validate_body_templates(body, decl, valid_ns, config, line_index, diags);
        }
        ListItemKind::Shorthand { value, body } => {
            validate_value_templates(&value.value, decl, valid_ns, config, line_index, diags);
            if let Some(body) = body {
                validate_body_templates(body, decl, valid_ns, config, line_index, diags);
            }
        }
        _ => {}
    }
}

fn validate_value_templates(
    value: &Value,
    decl: &Declaration,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    line_index: &LineIndex,
    diags: &mut Vec<Diagnostic>,
) {
    if let Value::TemplateString(segments) = value {
        for seg in segments {
            if let TemplateSegment::Expression {
                namespace,
                path,
                span,
                ..
            } = seg
            {
                if !valid_ns.is_empty() && !valid_ns.contains(&namespace.as_str()) {
                    diags.push(Diagnostic {
                        range: line_index.range(*span),
                        severity: Some(DiagnosticSeverity::WARNING),
                        message: format!("unknown template namespace '{namespace}'"),
                        source: Some("nml".to_string()),
                        ..Default::default()
                    });
                }

                if let Some(ext) = &config.language_extension {
                    if let Some(known_ids) = ext.resolve_identifiers(decl, namespace) {
                        if let Some(id_name) = path.first() {
                            if !known_ids.contains(id_name.as_str()) {
                                diags.push(Diagnostic {
                                    range: line_index.range(*span),
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    message: format!(
                                        "unknown identifier '{id_name}' in '{namespace}' namespace"
                                    ),
                                    source: Some("nml".to_string()),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Convert a single NML error to an LSP diagnostic.
fn nml_error_to_diagnostic(err: &nml_core::error::NmlError, line_index: &LineIndex) -> Diagnostic {
    let severity = match err {
        nml_core::error::NmlError::Validation { .. } => DiagnosticSeverity::WARNING,
        _ => DiagnosticSeverity::ERROR,
    };

    Diagnostic {
        range: line_index.range(err.span()),
        severity: Some(severity),
        message: err.message().to_string(),
        source: Some("nml".to_string()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> DiagnosticConfig {
        DiagnosticConfig::default()
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
        );
        let dym = diags
            .iter()
            .find(|d| d.message.contains("did you mean \"Lax\""))
            .expect("did-you-mean");
        assert!(
            dym.message
                .contains("(schema: demo blake3:12345678, store current)"),
            "{}",
            dym.message
        );
        let suggestion = dym
            .data
            .as_ref()
            .and_then(|d| d.get("suggestion"))
            .expect("data");
        assert_eq!(suggestion.get("replacement").unwrap().as_str(), Some("Lax"));
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
            package_name: "demo".to_string(),
            directives: nml_validate::test_support::demo_package_with_directives()
                .manifest
                .directives,
            undeclared_sibling,
        }
    }

    /// Unknown directive: error + did-you-mean, and the structured suggestion
    /// APPLIES — splicing the replacement at its byte span yields a source the
    /// pass then accepts (guards the name-span math against the
    /// whole-directive span).
    #[test]
    fn unknown_directive_suggestion_applies() {
        let source = "model core:\n    name string #lvie\n";
        let diags = schema_source_pass(source, &demo_vocab(false));
        let diag = diags
            .iter()
            .find(|d| d.message.contains("unknown directive '#lvie'"))
            .expect("unknown directive flagged");
        assert!(
            diag.message.contains("did you mean '#live'"),
            "{}",
            diag.message
        );
        let s = diag
            .data
            .as_ref()
            .and_then(|d| d.get("suggestion"))
            .expect("structured suggestion");
        let (replacement, start, end) = (
            s.get("replacement").unwrap().as_str().unwrap(),
            s.get("start").unwrap().as_u64().unwrap() as usize,
            s.get("end").unwrap().as_u64().unwrap() as usize,
        );
        let fixed = format!("{}{}{}", &source[..start], replacement, &source[end..]);
        let rediags = schema_source_pass(&fixed, &demo_vocab(false));
        assert!(
            rediags.is_empty(),
            "applying the suggestion must yield a clean directive: {rediags:?}"
        );
    }

    /// Arity, both directions: a bare-declared directive with an argument,
    /// and an argful-declared directive without one.
    #[test]
    fn directive_arity_both_directions() {
        let source = "model core:\n    name string #live(3)\n    mode string? #key\n";
        let diags = schema_source_pass(source, &demo_vocab(false));
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
        assert!(schema_source_pass(ok, &demo_vocab(false)).is_empty());
    }

    /// Parity with nudge's boot gate (`verify_directive_vocabulary`):
    /// `#live` and `#restart` on the SAME field contradict — one error, on
    /// the later directive, wording matching the gate's.
    #[test]
    fn live_restart_conflict_on_same_field() {
        let source = "model core:\n    name string #live #restart\n    mode string? #live\n";
        let diags = schema_source_pass(source, &demo_vocab(false));
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
        let diags = schema_source_pass(source, &demo_vocab(false));
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
        let diags = schema_source_pass(source, &demo_vocab(false));
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
        let diags = schema_source_pass(source, &demo_vocab(true));
        let info = diags
            .iter()
            .find(|d| d.severity == Some(DiagnosticSeverity::INFORMATION))
            .expect("sibling info emitted");
        assert_eq!(
            info.message,
            "not part of package 'demo'; add a []schema entry to participate"
        );
        // Declared / non-sibling coverage carries no info note.
        assert!(schema_source_pass(source, &demo_vocab(false)).is_empty());
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

    #[test]
    fn parse_error_produces_diagnostic() {
        let source = "service\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(!diags.is_empty(), "parse error should produce diagnostics");
        assert!(diags
            .iter()
            .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)));
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

    #[test]
    fn duplicate_decl_produces_diagnostic() {
        let source =
            "service Svc:\n    localMount = \"/\"\n\nservice Svc:\n    localMount = \"/other\"\n";
        let diags = compute_registry(source, &[], &[], &[], &default_config());
        assert!(
            diags.iter().any(|d| d.message.contains("duplicate")),
            "duplicate declarations should be flagged: {:?}",
            diags
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

    struct TestExtension {
        known: HashSet<String>,
    }

    impl LanguageExtension for TestExtension {
        fn resolve_identifiers(
            &self,
            _decl: &nml_core::ast::Declaration,
            namespace: &str,
        ) -> Option<HashSet<String>> {
            if namespace == "steps" {
                Some(self.known.clone())
            } else {
                None
            }
        }

        fn template_hover(&self, _namespace: &str, _path: &str) -> Option<String> {
            None
        }

        fn builtin_reference_completions(&self) -> Vec<(String, String)> {
            Vec::new()
        }
    }

    #[test]
    fn no_extension_skips_identifier_checking() {
        let source = concat!(
            "workflow W:\n",
            "    steps:\n",
            "        - classify:\n",
            "            prompt:\n",
            "                system = \"{{steps.nonexistent.intent}} generate\"\n",
        );
        let config = config_with_namespaces(&["args", "steps"]);
        let diags = compute_registry(source, &[], &[], &[], &config);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("unknown identifier")),
            "without extension, identifier checking should be skipped: {:?}",
            diags
        );
    }

    #[test]
    fn extension_valid_identifier_no_diagnostic() {
        let source = "service Svc:\n    val = \"{{steps.classify.intent}} x\"\n";
        let ext = TestExtension {
            known: ["classify".to_string()].into(),
        };
        let config = DiagnosticConfig {
            template_namespaces: vec!["steps".into()],
            language_extension: Some(Arc::new(ext)),
            ..Default::default()
        };
        let diags = compute_registry(source, &[], &[], &[], &config);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("unknown identifier")),
            "valid identifier should not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn extension_invalid_identifier_produces_warning() {
        let source = "service Svc:\n    val = \"{{steps.clasify.intent}} x\"\n";
        let ext = TestExtension {
            known: ["classify".to_string()].into(),
        };
        let config = DiagnosticConfig {
            template_namespaces: vec!["steps".into()],
            language_extension: Some(Arc::new(ext)),
            ..Default::default()
        };
        let diags = compute_registry(source, &[], &[], &[], &config);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown identifier 'clasify'")),
            "invalid identifier should be flagged: {:?}",
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
}
