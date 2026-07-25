# Define a directive vocabulary for your tool

`#directives` are opaque to the language — **your package manifest declares
the vocabulary**: names, argument shapes, and docs. One declaration feeds
three consumers with zero drift: editors complete and check directives for
your users, hovers show your docs, and your tool reads the declarations
back to drive behavior (reload classes, ownership, anything you define).

```nml check
model server:
    rateLimit number #live
    port number #restart
```

The manifest declares what `#live` and `#restart` *are*:

```nml check
[]directive directives:
    - live:
        arg = "none"
        doc = "Change applies without a restart."
    - restart:
        arg = "none"
        doc = "Change requires a process restart."
```

```rust source=docs/guides/examples/cookbook/examples/directive_vocabulary.rs
    let names: Vec<&str> = package
        .manifest
        .directives
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(names, ["live", "restart"]);
```

Full program: [`directive_vocabulary.rs`](examples/cookbook/examples/directive_vocabulary.rs)
— `cargo run -p nml-cookbook --example directive_vocabulary`.

Enforcement is **editor-side**: in files your package covers, unknown
directives are flagged with a machine-applicable did-you-mean (a typo'd
`#lvie` gets "did you mean `#live`?" as a quick-fix), and completion after
`#` offers exactly your declared vocabulary with your docs. Your tool
reads the same declarations at runtime, so the editor's view and your
classify step can never disagree. See [diff and
classify](diff-and-classify.md) for the consumption side, and [schema
packages](schema-packages-and-store.md) for shipping the manifest to your
users.
