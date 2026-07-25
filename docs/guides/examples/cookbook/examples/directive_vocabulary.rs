//! Recipe: define and enforce a directive vocabulary for your tool.
//!
//! `#directives` are opaque to the language — YOUR package manifest declares
//! the vocabulary (names, arg shapes, docs), editors complete and check it
//! for your users, and your tool reads the declarations back to drive
//! behavior (reload classes, ownership, anything). One declaration, three
//! consumers, zero drift.
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

    println!("recipe OK: directive_vocabulary");
    Ok(())
}
