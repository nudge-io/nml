# Migrate from TOML

Same `Deserialize` structs, different source — migration is incremental by
construction, and this page's central claim is **executed in CI**: the
equivalence test below parses the same config from TOML and from NML into
one struct and asserts equality.

## Side by side

| | TOML | NML |
|---|---|---|
| Scalars | `port = 8080` | `port = 8080` |
| Strings | `name = "api"` | `name = "api"` |
| Arrays | `features = ["gzip", "tls"]` | `features = ["gzip", "tls"]` |
| Nesting | `[limits]` table header | `limits:` indented block |
| Deep nesting | `[a.b.c]` repeated headers | nested blocks, read top-down |
| Many similar sections | `[[servers]]` array-of-tables | named blocks: `server Api:`, `server Worker:` |
| Comments | `#` | `//` |
| Secrets | strings (BYO discipline) | `$ENV.KEY` references, `secret` type, resolver-enforced |
| Schema | external (taplo, JSON Schema) | in the language: `model` + defaults + `nml check` |
| Editor support | generic | schema-aware LSP your tool can [embed](embed-the-lsp.md) |

## The equivalence, executed

```rust source=docs/guides/examples/cookbook/tests/toml_migration.rs
#[test]
fn the_same_config_means_the_same_thing_in_both_languages() {
    let from_toml: Config = toml::from_str(TOML_SOURCE).expect("toml parses");

    let file = parse(NML_SOURCE).expect("nml parses");
    let doc = Document::new(&file);
    let body = doc.block("service", "Api").body().expect("Api block");
    let from_nml: Config = from_body(body).expect("nml deserializes");

    assert_eq!(from_toml, from_nml);
}
```

Full test (both sources inline): [`toml_migration.rs`](examples/cookbook/tests/toml_migration.rs)
— `cargo test -p nml-cookbook`.

## The incremental path

1. **Keep TOML parsing behind a flag; add NML beside it.** Both feed the
   same structs (that's the whole point of the test above), so nothing
   downstream changes.
2. **Convert one config**, run both loaders, assert equality in your own
   tests the same way this page does.
3. **Add a schema** ([tutorial ch. 3](../tutorial/03-give-it-a-schema.md))
   — this is where NML starts paying: typed fields, defaults that
   [complete before deserializing](apply-schema-defaults.md), `nml check`
   [in CI](validate-in-ci.md), and schema-aware editing.
4. Retire the TOML path when the flag has been off everywhere for a
   release.

What you gain at each step is additive; nothing requires a flag-day.
