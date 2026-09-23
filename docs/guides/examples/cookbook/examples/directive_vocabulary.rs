//! Recipe: define and enforce a directive vocabulary for your tool.
//!
//! The language interprets four directives — the merge policies of RFC 0019
//! (`#sealed`, `#identity`, `#append`, `#overlay`) — and is otherwise opaque
//! to a directive's meaning: YOUR package manifest declares the rest of the
//! vocabulary (names, arg shapes, docs), the kernel checks it for your users
//! on every front end, editors complete and hover it, and your tool reads
//! the declarations back to drive behavior (reload classes, ownership,
//! anything). One declaration, three consumers, zero drift.
use nml_validate::directives::Vocabulary;
use nml_validate::package::{DirectiveArg, SchemaPackage};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = r#"package skylight:
    version = "0.1.0"
    formatVersion = 1
    rootMarkers:
        - "skylight.nml"

[]schema schemas:
    - server:
        file = "server.model.nml"

[]directive directives:
    - live:
        arg = "none"
        doc = "Change applies without a restart."
    - restart:
        arg = "none"
        doc = "Change requires a process restart."
"#;
    let server_schema = "model server:\n    rateLimit number #live\n    port number #restart\n";

    let package = SchemaPackage::from_parts(manifest, |file| match file {
        "server.model.nml" => Ok(server_schema.to_string()),
        other => Err(format!("unknown source {other}")),
    })?;

    // The vocabulary, programmatically — this is what your tool's classify
    // step and the editor's completion both read.
    let names: Vec<&str> = package
        .manifest
        .directives
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(names, ["live", "restart"]);
    assert!(
        package
            .manifest
            .directives
            .iter()
            .all(|d| matches!(d.arg, DirectiveArg::None) && !d.doc.is_empty())
    );

    // The kernel's judge — the verdicts `nml check`, `nml validate`, `nml fix`
    // and the editor report for a source this package covers: the four
    // language directives are known without a declaration, and a near-miss
    // of a declared name gets its did-you-mean.
    let vocabulary = Vocabulary::new(&package.manifest.name, package.manifest.directives.clone());
    let source = "model server:\n    rateLimit number #live\n    apiKey string #sealed\n    port number #restrat\n";
    let (schema, _) = nml_core::cst::extract_schema(source);
    let verdicts = vocabulary.judge(&schema.models, source);
    assert_eq!(
        verdicts.len(),
        1,
        "the language's `#sealed` is known; `#restrat` is not"
    );
    assert_eq!(verdicts[0].suggestions[0].replacement, "#restart");

    println!("recipe OK: directive_vocabulary");
    Ok(())
}
