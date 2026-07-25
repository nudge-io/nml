# Test your schemas and configs

Schemas are code — test them like code. The pattern: load the schema
exactly as production does, assert that good configs pass, and assert that
bad configs fail **with the code you expect**. Codes are stable forever;
message text is explicitly not an interface — a wording improvement must
never break your suite.

```rust source=docs/guides/examples/cookbook/tests/schema_tests.rs
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
```

Full suite: [`schema_tests.rs`](examples/cookbook/tests/schema_tests.rs) —
`cargo test -p nml-cookbook`.

**Also assert the schema itself is clean** (the `validator()` helper does):
a broken schema otherwise fails every config test with misleading noise.
Every code you can pin is documented in the [error index](../errors/README.md)
(`nml explain NML2007`); `nml-validate`'s `test-support` feature ships the
shared demo-package fixtures used across this repo's own suites — useful
when your tests need a full store/package roundtrip rather than a bare
validator.
