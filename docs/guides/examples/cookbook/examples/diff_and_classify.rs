//! Recipe: diff two configs and classify changes (live vs restart).
//!
//! The division of labor (RFC 0032): nml owns the *semantic* diff — which
//! fields changed, span-insensitively — and the schema declares each
//! field's reload class with `#live`/`#restart` directives. Your tool reads
//! the directives off each change's path and acts. Nothing here parses
//! message text or re-guesses semantics.
use std::path::PathBuf;

use nml_core::cst::extract_schema;
use nml_core::diff::diff_config;
use nml_core::schema::resolve_model_inheritance;
use nml_core::{Document, SchemaIndex, parse};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema_src = r#"
model server:
    rateLimit number #live
    port number #restart
"#;
    let old_src = "server Main:\n    rateLimit = 100\n    port = 8080\n";
    let new_src = "server Main:\n    rateLimit = 250\n    port = 9090\n";

    let (mut schema, _) = extract_schema(schema_src);
    resolve_model_inheritance(&mut schema);
    let index = SchemaIndex::build(schema.models, schema.enums, schema.oneofs);

    let old_file = parse(old_src)?;
    let new_file = parse(new_src)?;
    let old_body = Document::new(&old_file)
        .block("server", "Main")
        .body()
        .ok_or("old: no Main")?;
    let new_body = Document::new(&new_file)
        .block("server", "Main")
        .body()
        .ok_or("new: no Main")?;

    let path = PathBuf::from("server.nml");
    let changes = diff_config(
        &index,
        "server",
        &[(path.clone(), old_body)],
        &[(path, new_body)],
    );
    assert_eq!(changes.len(), 2);

    for change in &changes {
        // The leaf step carries the schema's directives for that field.
        let leaf = change.path.field_steps().last().ok_or("empty path")?;
        let class = leaf
            .directives
            .iter()
            .find_map(|d| match d.name.as_str() {
                "live" => Some("apply live"),
                "restart" => Some("restart required"),
                _ => None,
            })
            .unwrap_or("unclassified");
        println!("{}: {class}", change.path);
    }

    println!("recipe OK: diff_and_classify");
    Ok(())
}
