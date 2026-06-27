//! Validate an NML config file against schema models.
//!
//! Run with: cargo run --example validate_schema

use nml_validate::loader::load_schema;
use nml_validate::schema::SchemaValidator;

fn main() {
    let schema_source = r#"
model service:
    host string
    port number
    debug bool?
"#;

    let config_source = r#"
service MyApp:
    host = "localhost"
    port = 8080
    debug = true
    unknown_field = "oops"
"#;

    // `load_schema` runs the full pipeline over the CST (extraction, inheritance,
    // cycle/duplicate detection) and reports any schema problem as a diagnostic.
    let (schema, schema_diags) = load_schema(&[schema_source]);
    for d in &schema_diags {
        println!("schema: [{}] {}", d.severity, d.message);
    }
    println!(
        "Loaded {} model(s), {} enum(s)",
        schema.models.len(),
        schema.enums.len()
    );

    let config_file =
        nml_core::cst::parse_to_ast(config_source).expect("failed to parse config");
    let validator = SchemaValidator::from(schema);
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
