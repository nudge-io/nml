//! Recipe: test your schemas and configs.
//!
//! Schemas are code — test them like code. The pattern: load the schema
//! exactly as production does, then assert that good configs pass and that
//! bad configs fail WITH THE CODE you expect (message text is not a stable
//! interface; codes are).
use nml_core::parse;
use nml_validate::loader::load_schema;
use nml_validate::schema::SchemaValidator;

fn validator() -> SchemaValidator {
    let schema_src = "model server:\n    host string\n    port number\n    logLevel string?\n";
    let (schema, diags) = load_schema(&[("server.model.nml", schema_src)]);
    assert!(diags.is_empty(), "schema itself must be clean: {diags:?}");
    SchemaValidator::from(schema)
}

#[test]
fn a_valid_config_passes() {
    let file = parse("server Main:\n    host = \"0.0.0.0\"\n    port = 8080\n").unwrap();
    let diagnostics = validator().validate(&file);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn a_missing_required_field_fails_with_the_documented_code() {
    let file = parse("server Main:\n    host = \"0.0.0.0\"\n").unwrap();
    let diagnostics = validator().validate(&file);
    // Assert on the STABLE code (NML2007: missing required field), never on
    // message prose.
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.is_some_and(|c| c.to_string() == "NML2007")),
        "{diagnostics:?}"
    );
}

#[test]
fn a_type_mismatch_fails_with_the_documented_code() {
    let file = parse("server Main:\n    host = \"0.0.0.0\"\n    port = \"eighty\"\n").unwrap();
    let diagnostics = validator().validate(&file);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.is_some_and(|c| c.to_string() == "NML2008")),
        "{diagnostics:?}"
    );
}
