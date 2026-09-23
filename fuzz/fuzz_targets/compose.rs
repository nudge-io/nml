//! Fuzz the layer-composition engine (RFC 0019/0020/0025): `compose_file`,
//! the one orchestration `nml check`, `nml fix` and the editor share.
//!
//! It is the stage no other target reached. `validate` stops at the schema
//! validator, `document` and `format` stop at the parse — so linearization,
//! the merge, the seal scan, the item pool and the grant gate were driven
//! only by hand-written fixtures, though five of the perf tier's pins bound
//! that engine's complexity and `nml fix` applies the fixes it mints.
//! One input serves as schema and instance together, the way the
//! single-file workflow does (RFC 0012), so a `model` above a `uses`
//! stack composes against its own definitions.
//!
//! Three invariants, none of them "does not panic":
//!
//! 1. **The applier is the judge of every composed span.** `nml fix`
//!    splices these suggestions into real files with no human in the loop;
//!    `cst::edit::splice` refuses a reversed, out-of-bounds or mid-UTF-8
//!    span as the last line of defense. No producer should ever mint one,
//!    so each suggestion goes to the real applier — and only those an
//!    applier would route at this file, since a suggestion carries the
//!    file its span indexes into.
//! 2. **Composition is a function of the file.** The composition golden
//!    records one hash per stack, so an engine whose output followed
//!    hash-map iteration order would be a golden that drifts between runs.
//!    The engine keys its one-home dedup through a `HashSet` and sorts its
//!    sink by a total key; this holds the pair to the promise over
//!    arbitrary input instead of over the fixtures on disk.
//! 3. **One home per finding, in the key the FIXER dedups on.** The engine
//!    dedups on `finding_key` — the source exactly as stamped. Every front
//!    end then keys through `finding_key_in`, where an unstamped finding is
//!    a finding about the checked file. Two composed findings that differ
//!    only by a `None` against the own path are two findings to the engine
//!    and one to the fixer, which adds compose's findings to its round
//!    unfiltered — so the gap between the two keys is an edit applied
//!    twice.

#![no_main]

use std::collections::HashSet;

use libfuzzer_sys::fuzz_target;
use nml_core::cst::edit::{SpliceEdit, splice};
use nml_core::diagnostic::Diagnostic;
use nml_core::layers::{FindingKey, OpenContext, compose_file, finding_key_in};
use nml_core::schema_index::SchemaIndex;
use nml_validate::schema::SchemaValidator;

/// The path the single fuzzed source is checked as.
const OWN: &str = "fuzz.nml";

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };

    // The front ends' own assembly, in their order: the schema universe
    // the text declares, then the file, then one index shared by the
    // validator and the engine (`nml-cli/src/main.rs`, `fix.rs`).
    let (schema, _) = nml_validate::loader::load_schema(&[(OWN, source)]);
    let (file, _) = nml_core::cst::parse_to_ast_all(source);
    let empty = SchemaIndex::build(vec![], vec![], vec![]);
    let validator =
        (!schema.is_empty()).then(|| SchemaValidator::from(schema).composition_checked_at_load());
    let index = validator.as_ref().map_or(&empty, |v| v.index());

    let composed = compose_file(index, OWN, &file, &OpenContext);

    // ── 1. every composed edit is one the applier accepts ───────────────
    let mut homes: HashSet<FindingKey> = HashSet::new();
    for d in &composed.diagnostics {
        // Rendering walks the payload and bounds every echo; a control
        // character must never reach output raw (a hostile file would
        // otherwise smuggle terminal escapes through a finding).
        assert!(
            !d.rendered_message().contains('\u{1b}'),
            "a raw escape character reached a composed finding's output"
        );
        for s in &d.suggestions {
            // An applier edits only the file it was asked to; a
            // suggestion may carry the edit an operator applies
            // elsewhere, and that one is resolved against another text.
            if d.suggestion_source(s).is_some_and(|src| src != OWN) {
                continue;
            }
            let edit = SpliceEdit {
                span: s.span,
                replacement: s.replacement.clone(),
            };
            splice(source, std::slice::from_ref(&edit)).unwrap_or_else(|e| {
                panic!(
                    "the applier refused a composed fix — `nml fix` would corrupt the file: {e} \
                     (replacement {:?})",
                    s.replacement
                )
            });
        }

        // ── 3. one home per finding, keyed as the fixer keys it ─────────
        assert!(
            homes.insert(finding_key_in(d, OWN)),
            "two composed findings share one home in the fixer's key — the same edit would \
             be applied twice: {:?} at {:?} in {:?}",
            d.code,
            d.span,
            d.source
        );
    }

    // ── 2. composing the same file twice is composing it once ───────────
    let again = compose_file(index, OWN, &file, &OpenContext);
    assert_eq!(
        fingerprint(&composed.diagnostics),
        fingerprint(&again.diagnostics),
        "composition's findings are not a function of the file"
    );
    let view = |f: &Option<nml_core::ast::File>| {
        f.as_ref()
            .map(|f| serde_json::to_value(f).expect("the AST serializes"))
    };
    assert_eq!(
        view(&composed.validation_file),
        view(&again.validation_file),
        "the composed view is not a function of the file"
    );
});

/// Everything a consumer reads off a finding — the code, where it sits,
/// what it says, whose file it is about, and each edit it offers.
type Print = (
    Option<String>,
    Option<(usize, usize)>,
    String,
    Option<String>,
    Vec<(usize, usize, String, Option<String>)>,
);

fn fingerprint(diags: &[Diagnostic]) -> Vec<Print> {
    diags
        .iter()
        .map(|d| {
            (
                d.code.map(|c| c.to_string()),
                d.span.map(|sp| (sp.start, sp.end)),
                d.rendered_message(),
                d.source.clone(),
                d.suggestions
                    .iter()
                    .map(|s| {
                        (
                            s.span.start,
                            s.span.end,
                            s.replacement.clone(),
                            s.source.clone(),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}
