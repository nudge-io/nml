//! Recipe: apply schema defaults before deserializing.
//!
//! The schema declares defaults once; every consumer — CLI validation, the
//! editor, and your own deserialization — sees the same completed values.
use nml_core::cst::extract_schema;
use nml_core::schema::resolve_model_inheritance;
use nml_core::{Document, SchemaIndex, ValueResolver, from_document_defaulted, parse};
use serde::Deserialize;

#[derive(Deserialize)]
struct Server {
    host: String,
    port: u16,
    #[serde(rename = "logLevel")]
    log_level: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema_src = r#"
model server:
    host string
    port number = 8080
    logLevel string = "info"
"#;
    let config_src = r#"
server Main:
    host = "0.0.0.0"
"#;

    // Extract the schema and build the index the defaulting pass reads.
    let (mut schema, _diags) = extract_schema(schema_src);
    resolve_model_inheritance(&mut schema);
    let index = SchemaIndex::build(schema.models, schema.enums, schema.oneofs);

    let file = parse(config_src)?;
    let doc = Document::new(&file);

    // Defaults fill what the instance omitted, then references resolve,
    // then serde deserializes — one call.
    let server: Server =
        from_document_defaulted(&index, &doc, "server", "Main", &ValueResolver::env())?;
    assert_eq!(server.port, 8080); // from the schema default
    assert_eq!(server.log_level, "info"); // from the schema default
    assert_eq!(server.host, "0.0.0.0"); // from the instance

    println!("recipe OK: apply_defaults");
    Ok(())
}
