//! Validate an NML config file against schema models.
//!
//! Run with: cargo run --example validate_schema

use nml_core::{model_extract, parse};
use nml_validate::schema::SchemaValidator;

fn main() {
    let schema_source = r#"
model service:
    host: string
    port: number
    debug: bool?
"#;

    let config_source = r#"
service MyApp:
    host = "localhost"
    port = 8080
    debug = true
    unknown_field = "oops"
"#;

    let schema_file = parse(schema_source).expect("failed to parse schema");
    let schema = model_extract::extract(&schema_file);

    println!(
        "Loaded {} model(s), {} enum(s)",
        schema.models.len(),
        schema.enums.len()
    );

    let config_file = parse(config_source).expect("failed to parse config");

    let validator = SchemaValidator::new(schema.models, schema.enums);
    let diagnostics = validator.validate(&config_file);

    if diagnostics.is_empty() {
        println!("Validation passed!");
    } else {
        println!("Found {} issue(s):", diagnostics.len());
        for d in &diagnostics {
            println!("  [{}] {}", d.severity, d.message);
        }
    }
}
