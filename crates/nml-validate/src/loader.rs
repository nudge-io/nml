//! Orchestrated schema-loading pipeline.
//!
//! [`load_schema`] is the single entry point for turning parsed schema files
//! into definitions ready for a
//! [`SchemaValidator`](crate::schema::SchemaValidator).  It runs the full
//! `nml-core` extraction pipeline -- extract, duplicate detection,
//! inheritance-cycle detection, inheritance resolution, and reference-cycle
//! detection -- and surfaces every problem as a [`Diagnostic`].

use std::collections::HashSet;

// Import the passes by name (not the module) so the bare `schema` identifier stays
// free for the local `ExtractedSchema` value and our own `crate::schema` module.
use nml_core::schema::{
    find_composition_errors, find_enum_errors, find_extends_cycles, find_model_cycles,
    find_oneof_errors, find_shorthand_errors, resolve_model_inheritance, ExtractedSchema,
};

use nml_core::diagnostic::Diagnostic;

/// Load a schema from one or more NML source documents.
///
/// Pipeline:
/// 1. extract model/enum definitions from every source over the CST
///    ([`extract_schema`](nml_core::cst::extract_schema)) — surfacing any parse
///    error as a diagnostic while still extracting from the well-formed parts,
/// 2. report duplicate model/enum names (first definition wins),
/// 3. report inheritance (`is`) cycles as **errors** -- such hierarchies
///    cannot be resolved,
/// 4. resolve model inheritance so child models carry ancestor fields,
/// 5. report model-reference cycles as **warnings** -- recursive models are
///    structurally valid (instance validation is depth-limited) but worth
///    flagging.
///
/// Always returns the loaded definitions, even when diagnostics are present,
/// so callers can keep validating with a best-effort schema.
/// Sources are `(name, text)` pairs; the name identifies the source document
/// in diagnostics (RFC 0030: multi-source loads — package schemas — need file
/// attribution, since spans from different sources are numerically
/// ambiguous). Every definition is stamped with its declaring source before
/// the merge, so per-source findings (parse errors, reserved/duplicate
/// names) AND definition-anchored structural findings (composition, oneof
/// integrity, shorthand arity, cycle members) all carry the file that owns
/// their anchor. Only a finding with no single defining anchor would stay
/// unattributed.
pub fn load_schema(sources: &[(&str, &str)]) -> (ExtractedSchema, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();

    let mut schema = ExtractedSchema::default();
    // Name checks run during the merge so the offending *source* is in hand:
    // a reserved name is flagged where it is defined, and a duplicate is
    // flagged on the second definition (first wins downstream).
    let mut seen_models = HashSet::new();
    let mut seen_enums = HashSet::new();
    let mut seen_oneofs = HashSet::new();
    for (name, text) in sources {
        let (mut extracted, errors) = nml_core::cst::extract_schema(text);
        diagnostics.extend(errors.into_iter().map(|d| d.with_source(*name)));
        // Stamp each definition with its declaring source before the merge:
        // definition-anchored findings (composition, oneof integrity, arity,
        // cycles) copy it so they render `file:line:col` against the right
        // file instead of a directory-prefixed byte span.
        for m in &mut extracted.models {
            m.source = Some((*name).to_string());
        }
        for o in &mut extracted.oneofs {
            o.source = Some((*name).to_string());
        }
        for e in &mut extracted.enums {
            e.source = Some((*name).to_string());
        }
        for m in &extracted.models {
            // Traits share the model namespace (RFC 0011): a trait and a
            // model with one name would be unresolvable at `is` sites.
            check_definition_name(
                m.kind.label(),
                &m.name,
                m.span,
                name,
                &mut seen_models,
                &mut diagnostics,
            );
        }
        for e in &extracted.enums {
            check_definition_name(
                "enum",
                &e.name,
                e.span,
                name,
                &mut seen_enums,
                &mut diagnostics,
            );
        }
        for o in &extracted.oneofs {
            check_definition_name(
                "oneof",
                &o.name,
                o.span,
                name,
                &mut seen_oneofs,
                &mut diagnostics,
            );
        }
        schema.models.extend(extracted.models);
        schema.enums.extend(extracted.enums);
        schema.oneofs.extend(extracted.oneofs);
    }

    diagnostics.extend(find_extends_cycles(&schema));

    // Composition integrity (RFC 0011): every `is` target resolves, traits
    // never appear in value-type or `oneof`-arm position. Run *before*
    // inheritance resolution so each violation reports once, at the
    // definition that wrote it.
    diagnostics.extend(find_composition_errors(&schema));

    resolve_model_inheritance(&mut schema);

    // Positional-shorthand (`+`, RFC 0005) arity — axis-aware, checked post-inheritance
    // so an inherited `!` and a child `!` are caught together (RFC 0005 §8).
    diagnostics.extend(find_shorthand_errors(&schema));

    // `oneof` integrity (arm models exist, unique values, name collisions) is
    // an error: a malformed union cannot be validated against.
    diagnostics.extend(find_oneof_errors(&schema));

    // Enum tidiness (duplicate variants, empty enums) — warnings.
    diagnostics.extend(find_enum_errors(&schema));

    // Severity travels with the diagnostic (warning at the source).
    diagnostics.extend(find_model_cycles(&schema));

    (schema, diagnostics)
}

/// Type-constructor names (RFC 0032): `set` is live, `map` reserved for the
/// future map type. A definition so named could never be referenced with
/// arguments (`set<…>` always parses as the constructor), so it is rejected at
/// load rather than left as a shadow-confusion trap.
const RESERVED_TYPE_CONSTRUCTORS: [&str; 2] = ["set", "map"];

/// One rule set per definition kind: a reserved constructor name is an error
/// wherever it appears (any definition kind is referenceable as a type, so
/// all three carry the shadow trap), and duplicates within a kind are errors
/// (first definition wins downstream). Both attribute the source in hand.
fn check_definition_name(
    kind: &str,
    name: &str,
    span: nml_core::span::Span,
    source: &str,
    seen: &mut HashSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if RESERVED_TYPE_CONSTRUCTORS.contains(&name) {
        diagnostics.push(
            Diagnostic::error(format!(
                "'{name}' is a reserved type-constructor name (RFC 0032) — rename the {kind}"
            ))
            .with_code(nml_core::diagnostic::codes::RESERVED_TYPE_NAME)
            .with_span(span)
            .with_source(source),
        );
    }
    if !seen.insert(name.to_string()) {
        diagnostics.push(
            Diagnostic::error(format!("duplicate {kind} definition '{name}'"))
                .with_code(nml_core::diagnostic::codes::DUPLICATE_DEFINITION)
                .with_span(span)
                .with_source(source),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nml_core::diagnostic::Severity;

    /// Wrap anonymous test sources as named ones (`src0`, `src1`, …) — tests
    /// exercising attribution pass named tuples to `load_schema` directly.
    fn load(sources: &[&str]) -> (ExtractedSchema, Vec<Diagnostic>) {
        let named: Vec<(String, &str)> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("src{i}"), *s))
            .collect();
        let named_refs: Vec<(&str, &str)> = named.iter().map(|(n, s)| (n.as_str(), *s)).collect();
        load_schema(&named_refs)
    }
    use crate::schema::SchemaValidator;

    /// RFC 0030: per-source findings carry their source name — a parse error
    /// names the broken file, and a cross-source duplicate names the *second*
    /// definition's file (first wins downstream).
    #[test]
    fn diagnostics_attribute_their_source() {
        let (_, diags) = load_schema(&[
            ("good.model.nml", "model a:\n    x string\n"),
            ("broken.model.nml", "model b:\n    @@@\n"),
            ("dup.model.nml", "model a:\n    y string\n"),
        ]);
        let parse_err = diags
            .iter()
            .find(|d| d.source.as_deref() == Some("broken.model.nml"))
            .expect("parse error attributed to broken.model.nml");
        assert!(matches!(parse_err.severity, Severity::Error));
        let dup = diags
            .iter()
            .find(|d| d.message.contains("duplicate model definition 'a'"))
            .expect("duplicate reported");
        assert_eq!(dup.source.as_deref(), Some("dup.model.nml"));
    }

    fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .collect()
    }

    /// RFC 0032: `set`/`map` are reserved type-constructor names — a model,
    /// enum, or oneof so named is rejected at schema load (shadow-confusion trap).
    #[test]
    fn reserved_constructor_names_are_rejected_at_load() {
        for src in [
            "model set:\n    x string\n",
            "model map:\n    x string\n",
            "enum set:\n    - \"a\"\n",
        ] {
            let (_, diags) = load(&[src]);
            assert!(
                errors(&diags)
                    .iter()
                    .any(|d| d.message.contains("reserved type-constructor name")),
                "{src:?} must be rejected: {diags:?}"
            );
        }
        // Control: ordinary names stay legal.
        let (_, diags) = load(&["model settings:\n    x string\n"]);
        assert!(
            errors(&diags).is_empty(),
            "'settings' is not reserved: {diags:?}"
        );
    }

    #[test]
    fn test_inheritance_resolved_through_pipeline() {
        let (schema, diags) = load(&[
            "model base:\n    name string\n",
            "model child is base:\n    extra string\n",
        ]);

        assert!(errors(&diags).is_empty(), "unexpected errors: {diags:?}");
        let child = schema.models.iter().find(|m| m.name == "child").unwrap();
        let names: Vec<&str> = child.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["name", "extra"],
            "child should inherit parent fields"
        );
    }

    #[test]
    fn test_inherited_required_field_enforced_by_validator() {
        // `id` (not `name`) is the inherited required field, so block-name injection
        // (which fills `name` from the block identifier) doesn't mask the enforcement.
        let (schema, diags) =
            load(&["model base:\n    id string\n\nmodel child is base:\n    extra string\n"]);
        assert!(errors(&diags).is_empty(), "unexpected errors: {diags:?}");

        let validator = SchemaValidator::from(schema);
        let doc = nml_core::cst::parse_to_ast("child C:\n    extra = \"x\"\n").unwrap();
        let result = validator.validate(&doc);
        assert!(
            result
                .iter()
                .any(|d| d.message.contains("missing required field 'id'")),
            "inherited required field should be enforced; diags: {result:?}"
        );
    }

    #[test]
    fn test_extends_cycle_reported_as_error() {
        let (_, diags) = load(&["model a is b:\n    x string\n\nmodel b is a:\n    y string\n"]);
        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.contains("circular inheritance")),
            "extends cycle should be an error diagnostic; diags: {diags:?}"
        );
    }

    #[test]
    fn test_model_ref_cycle_reported_as_warning() {
        let (_, diags) = load(&["model a:\n    child b?\n\nmodel b:\n    parent a?\n"]);
        let cycle = diags
            .iter()
            .find(|d| d.message.contains("circular dependency"))
            .expect("model-reference cycle should be reported");
        assert!(
            matches!(cycle.severity, Severity::Warning),
            "recursive models are valid, so reference cycles are warnings"
        );
    }

    #[test]
    fn test_duplicate_model_names_across_files() {
        let (schema, diags) = load(&[
            "model server:\n    port number\n",
            "model server:\n    host string\n",
        ]);

        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.contains("duplicate model definition 'server'")),
            "duplicate model names should be errors; diags: {diags:?}"
        );
        // First definition wins for validation purposes.
        let server = schema.models.iter().find(|m| m.name == "server").unwrap();
        assert_eq!(server.fields[0].name, "port");
    }

    #[test]
    fn test_duplicate_enum_names_across_files() {
        let (_, diags) = load(&[
            "enum status:\n    - \"on\"\n",
            "enum status:\n    - \"off\"\n",
        ]);
        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.contains("duplicate enum definition 'status'")),
            "duplicate enum names should be errors; diags: {diags:?}"
        );
    }

    #[test]
    fn malformed_source_still_loads_wellformed_definitions() {
        // Resilience (exceeds the old pre-parsed-File API, which could not carry a
        // parse error): a syntax error in one source surfaces as a diagnostic, yet
        // the well-formed definitions across all sources still load.
        let (schema, diags) = load(&["model good:\n    name string\n", "model M:\n    @@@\n"]);
        assert!(
            schema.models.iter().any(|m| m.name == "good"),
            "well-formed model still loads despite a sibling parse error"
        );
        assert!(
            !errors(&diags).is_empty(),
            "the parse error is surfaced as a diagnostic: {diags:?}"
        );
    }

    #[test]
    fn test_clean_schema_no_diagnostics() {
        let (schema, diags) = load(&[
            "enum status:\n    - \"on\"\n    - \"off\"\n\nmodel base:\n    name string\n",
            "model server is base:\n    status status\n",
        ]);
        assert!(
            diags.is_empty(),
            "clean schema should load silently: {diags:?}"
        );
        assert!(!schema.is_empty());
        assert_eq!(schema.models.len(), 2);
        assert_eq!(schema.enums.len(), 1);
    }
}

#[cfg(test)]
mod trait_tests {
    //! RFC 0011 end-to-end through `load_schema`: traits merge like models,
    //! and every misuse carries its code and source attribution.

    use super::*;
    use nml_core::schema_index::SchemaIndex;

    fn code_strings(diags: &[nml_core::diagnostic::Diagnostic]) -> Vec<String> {
        diags
            .iter()
            .filter_map(|d| d.code.map(|c| c.to_string()))
            .collect()
    }

    #[test]
    fn traits_load_merge_and_default_through_the_index() {
        let src = "trait monitored:\n    timeout duration = \"5s\"\n\n\
                   model endpoint is monitored:\n    url string+\n";
        let (schema, diags) = load_schema(&[("t.model.nml", src)]);
        assert!(diags.is_empty(), "{diags:?}");
        let index = SchemaIndex::build(schema.models, schema.enums, schema.oneofs);
        let endpoint = index.model("endpoint").unwrap();
        assert!(
            endpoint.fields.iter().any(|f| f.name == "timeout"),
            "trait field must be merged into the composing model"
        );
        // The trait itself stays visible in the index (hover, `is` resolution)
        // and keeps its kind.
        assert!(index.model("monitored").unwrap().is_trait());
    }

    #[test]
    fn composition_errors_carry_codes_and_sources() {
        let src = "trait cap:\n    x string?\n\n\
                   model a is missing:\n    y string?\n\n\
                   model b:\n    c cap?\n\n\
                   oneof entry by kind:\n    \"a\" -> cap\n";
        let (_, diags) = load_schema(&[("bad.model.nml", src)]);
        let codes = code_strings(&diags);
        for expected in ["NML2020", "NML2022", "NML2023"] {
            assert!(
                codes.iter().any(|c| c == expected),
                "missing {expected} in {codes:?}"
            );
        }
        // Definition-anchored findings carry their declaring source (stamped
        // at load), so the CLI renders them `file:line:col` — and every one
        // still points at a real span in that file.
        assert!(
            diags
                .iter()
                .all(|d| d.span.is_some() && d.source.as_deref() == Some("bad.model.nml")),
            "{diags:?}"
        );
    }

    #[test]
    fn positional_arity_counts_trait_contributed_fields() {
        // NML2011 runs post-inheritance: a trait's `+` field and the
        // composing model's own `+` field collide on the merged set.
        let src = "trait keyed:\n    id string+\n\n\
                   model item is keyed:\n    slug string+\n";
        let (_, diags) = load_schema(&[("t.model.nml", src)]);
        assert!(
            code_strings(&diags).contains(&"NML2011".to_string()),
            "{diags:?}"
        );
    }

    #[test]
    fn directives_travel_with_the_trait_merge() {
        let src = "trait tuned:\n    rate number = 5 #live\n\n\
                   model server is tuned:\n    host string?\n";
        let (schema, diags) = load_schema(&[("t.model.nml", src)]);
        assert!(diags.is_empty(), "{diags:?}");
        let server = schema.models.iter().find(|m| m.name == "server").unwrap();
        let rate = server.fields.iter().find(|f| f.name == "rate").unwrap();
        assert_eq!(rate.directives.len(), 1, "inherited field keeps #live");
        assert_eq!(rate.directives[0].name, "live");
    }

    #[test]
    fn duplicate_trait_and_model_name_is_one_namespace() {
        let src = "trait cap:\n    x string?\n\nmodel cap:\n    y string?\n";
        let (_, diags) = load_schema(&[("dup.model.nml", src)]);
        assert!(
            code_strings(&diags).contains(&"NML2009".to_string()),
            "trait/model share one namespace: {diags:?}"
        );
    }
}

#[cfg(test)]
mod enum_tests {
    //! Enum tidiness (NML2027/NML2028): duplicate variants normalize across
    //! both authored forms; empty enums warn.

    use super::*;
    use nml_core::diagnostic::Severity;

    #[test]
    fn duplicate_variant_across_both_authored_forms_warns() {
        let (_, diags) =
            load_schema(&[("e.model.nml", "enum level:\n    - \"info\"\n    - info\n")]);
        let hits: Vec<_> = diags
            .iter()
            .filter(|d| d.code.map(|c| c.to_string()).as_deref() == Some("NML2027"))
            .collect();
        assert_eq!(hits.len(), 1, "{diags:?}");
        assert_eq!(hits[0].severity, Severity::Warning);
        assert_eq!(hits[0].source.as_deref(), Some("e.model.nml"));
    }

    #[test]
    fn empty_enum_warns() {
        let (_, diags) = load_schema(&[("e.model.nml", "enum pending:\n")]);
        assert!(
            diags.iter().any(
                |d| d.code.map(|c| c.to_string()).as_deref() == Some("NML2028")
                    && d.severity == Severity::Warning
            ),
            "{diags:?}"
        );
    }
}
