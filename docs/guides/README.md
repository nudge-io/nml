# NML Cookbook

Task-oriented recipes for embedding NML as a Rust library, plus one CLI page
(*Validate in CI*). Every Rust listing is **compiled and executed in CI**: the
listings are excerpts of the example programs in
[`examples/cookbook/`](examples/cookbook/) and of the test files the CLI page's
siblings name, which `just gate-docs` runs on every change — a recipe that
stops working fails the build by name. The CLI page's own blocks are executed
transcripts of the real binary, checked the same way.

New to the language itself? Start with the [tutorial](../tutorial/README.md).
Looking up a feature? The [language guide](../language-guide.md) and
[spec](../../spec/README.md) are the references.

## Reading and validating

1. [Parse a file and read values](parse-and-query.md) — the query API
2. [Deserialize into structs with serde](deserialize-with-serde.md)
3. [Collect *all* parse errors](collect-all-errors.md) — the editor-grade experience
4. [Validate in CI](validate-in-ci.md) — a workspace manifest, `--root`,
   strict mode, exit codes, `--schema`, `--json`
5. [Test your schemas and configs](test-your-schemas.md)

## Values and defaults

6. [Wire a custom secret resolver](custom-secret-resolver.md) — vault, fixtures, anything
7. [Apply schema defaults before deserializing](apply-schema-defaults.md)

## Operating on configs

8. [Diff two configs and classify changes](diff-and-classify.md) — live vs restart
9. [Edit a file without destroying formatting](edit-without-reformatting.md)
10. [Format user files idempotently](format-user-files.md)

## Shipping to your users

11. [Define a directive vocabulary for your tool](directive-vocabulary.md)
12. [Build and publish schema packages](schema-packages-and-store.md) — and
    [how a file finds its binding](schema-packages-and-store.md#how-a-file-finds-its-binding-workspace-resolution):
    manifests, `files` claims, grants and the closed workspace
13. [Embed the language server](embed-the-lsp.md) — `<your-tool> lsp` in one line

## Coming from elsewhere

14. [Migrate from TOML](migrate-from-toml.md) — side-by-side, with the
    equivalence executed in CI
