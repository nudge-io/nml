# 9 · Ship schemas to your users

Everything Skylight's schema knows — types, defaults, enums, secrets
policy, reload directives — currently helps *you*. Your users write
`app.nml` for your tool, and their editors know none of it. This closing
chapter fixes that: you bundle the schema into a **package**, give it a
content-addressed identity, publish it to the per-user **store** editors
read, and see how one line of Rust turns your CLI into a language server
for your own config format.

This chapter's files: [`examples/09/`](examples/09/) — the manifest, the
schema it bundles, and the publishing program (`app/`).

## The package manifest

A schema package is declared in a `.package.nml` file — NML describing NML:

```nml check
package skylight:
    version = "0.1.0"
    formatVersion = 1
    rootMarkers:
        - "app.nml"
    modifiers:
        - "allow"
        - "deny"

[]schema schemas:
    - skylight:
        file = "skylight.model.nml"

[]validator validators:
    - service:
        files:
            - "**/app.nml"
        schemas:
            - skylight
        strict = true

[]directive directives:
    - live:
        arg = "none"
        doc = "Change applies to a running process without a restart."
    - restart:
        arg = "none"
        doc = "Change requires a process restart to take effect."
```

Four sections, four jobs:

- **`package`** — identity and anchoring. `rootMarkers` names files that
  mark a project root (here: wherever an `app.nml` sits), so binding globs
  anchor to the user's project, not the filesystem. `modifiers` declares
  the `|` names your tool accepts.
- **`[]schema`** — the sources to bundle, by logical name.
- **`[]validator`** — the bindings: *which files* (globs; `**` must be a
  whole path segment) get *which schemas*, and how hard. `strict = true`
  means your users' typos are errors in their editor, with the same
  did-you-mean fixes you've seen all tutorial.
- **`[]directive`** — the vocabulary from Chapter 8, declared. `#live` on
  a field is valid because the *package* says so, with hover documentation
  for each directive. Your reload semantics ship with your schema.

## Identity is a hash, not a version

Load the package and ask who it is:

```rust source=docs/tutorial/examples/09/app/src/main.rs
    let package = SchemaPackage::from_dir(Path::new(".")).map_err(|e| e.to_string())?;
    let manifest = &package.manifest;
```

```rust source=docs/tutorial/examples/09/app/src/main.rs
    // The content hash is the package's identity: length-prefixed,
    // newline-normalized frames over manifest + sources, blake3-hashed.
    // Same bytes, same hash — on every machine.
    let hash = package.content_hash();
    println!("content hash {} (short: {})", hash, hash8(&hash));
```

`version` is a human label. The **content hash** is the identity: blake3
over the manifest and every bundled source, newline-normalized so the same
package hashes the same on every OS. Two builds of the same bytes agree;
one edited field does not. That's what makes the store safe to share.

## The store

The store is a content-addressed directory of published packages — one
slot per `(version, hash)`, plus a `current` pointer flipped atomically:

```rust source=docs/tutorial/examples/09/app/src/main.rs
    // Publish to a store. Real tools use Store::user() — the per-user store
    // editors read; this demo uses a scratch directory.
    let store = Store::at(std::env::temp_dir().join("skylight-store-demo"));
    let outcome = store.publish(&package).map_err(|e| e.to_string())?;
    match outcome {
        PublishOutcome::Published { slot } => println!("published to slot {slot}"),
        PublishOutcome::Unchanged => println!("published already — store is current"),
    }
```

Run it (`cargo run` in `examples/09/`; real output, run in CI):

```text
package skylight v0.1.0 — 1 schema(s), 1 validator binding(s)
content hash blake3:27ff6038e8ab7bbaf17eb72d2cd526d57b1199e7272959b37d0706d5236fad4c (short: 27ff6038)
published to slot 0.1.0+27ff6038
store has: skylight v0.1.0 (27ff6038, 1 slot(s))
```

Publishing is idempotent — run it again and you get `Unchanged`. In a real
tool this is a `skylight schema sync` subcommand (or part of install), and
the store is `Store::user()` — the per-user location editors read
(overridable via `NML_SCHEMA_STORE_DIR`).

## What the user sees

Your user installs your tool, runs it once, and opens `app.nml` in an
editor running the NML language server. The server finds their project
root by your `rootMarkers`, matches the file against your validator globs,
and binds your `skylight` schema — strict, as the manifest said. They get:

- Validation with every diagnostic you've met in this tutorial, quick
  fixes included — against *your* schema, which they never installed.
- Completion and hover built from your models, field docs, and the
  directive vocabulary's `doc` strings.
- A status surface (the `nml/schemaInfo` request; the VS Code extension's
  status bar) answering "which package, which version, which hash is
  validating this file?" — an auditable chain from squiggle to store slot.

When you ship a schema change, you publish a new slot and the pointer
flips; their next edit validates against it. No plugin marketplace, no
copy-pasted JSON Schema, no drift.

## `<your-tool> lsp`

One more mile: don't make users find a language server at all — embed it.
`nml-lsp` is a library, and its whole embedding API is one call:

```rust
// skylight lsp   — serve *your* schema package over stdio
Some(Command::Lsp) => nml_lsp::serve(EMBEDDED_PACKAGE.clone()).await,
```

with the package compiled into your binary:

```rust
static EMBEDDED_PACKAGE: LazyLock<SchemaPackage> = LazyLock::new(|| {
    SchemaPackage::from_parts(include_str!("skylight.package.nml"), |file| {
        match file {
            "skylight.model.nml" => Ok(include_str!("skylight.model.nml").to_string()),
            other => Err(format!("unknown schema source {other}")),
        }
    })
    .expect("embedded schema package is valid")
});
```

The editor points at `skylight lsp` and gets a server whose in-binary
package can never drift from the binary's own validation — the same
`SchemaPackage` value drives boot-time config checks, `schema sync`, and
the editor. This is not hypothetical: the production workflow platform NML
was built for ships exactly this pattern — one embedded package behind its
boot validators, its schema sync, and its `lsp` subcommand.

## Exercises

1. Change `poolSize`'s default to `25` in `skylight.model.nml` and rerun
   the program. What changed in the output, and why did the slot name
   change?

   <details><summary>Solution</summary>

   The content hash is different, so publish writes a *new* slot
   (`0.1.0+<new hash8>`) and flips the pointer; the old slot remains on
   disk as history. Version strings didn't change — identity is content.
   (This is also your cue that a real release should bump `version`: the
   label is for humans.)

   </details>

2. In the manifest, misspell a bundled file (`file = "skylite.model.nml"`)
   and rerun. Where does the failure surface?

   <details><summary>Solution</summary>

   Immediately at `SchemaPackage::from_dir`, with a missing-source error
   naming the file the manifest asked for — a package cannot load with
   sources absent, so a broken bundle never reaches the store, the editor,
   or your users.

   </details>

## Common mistakes

- **Treating `version` as identity.** Two different schemas can both say
  `0.1.0`; only the content hash distinguishes them. Compare hashes when
  debugging "but it validates differently on my machine".
- **Overly-broad binding globs.** `**/*.nml` claims every NML file in the
  user's project — including other tools'. Bind what's yours (specific
  filenames or extensions like `*.workflow.nml`).
- **Skipping the directive declarations.** If your schemas use `#live`
  but the package doesn't declare `live`, your vocabulary is invisible to
  tooling — declare each directive with a `doc` so hovers teach it.

## Recap

- A `.package.nml` manifest bundles schema sources with binding globs
  (anchored by `rootMarkers`), strictness, modifier names, and the
  directive vocabulary — your language's rules as one shippable artifact.
- Identity is `content_hash()` (blake3, normalized); the store is
  content-addressed slots with an atomic `current` pointer, per-user, and
  publishing is idempotent.
- `nml_lsp::serve(package)` embeds a full language server behind
  `<your-tool> lsp`, with the same package your binary validates with —
  editor and runtime cannot disagree.

---

That's the tutorial. Skylight went from twelve untyped lines to a typed,
composed, access-controlled config with a Rust embedding, schema-declared
live reload, and schemas that ship to users' editors. Where to next:

- [Integration guide](../integration.md) — the embedding APIs, reference-style
- [Error index](../errors/README.md) — every diagnostic code, with examples
- [Language guide](../language-guide.md) — the full syntax reference
- [Stability policy](../stability.md) — what you can depend on
