# Diff two configs and classify changes

The division of labor: nml owns the **semantic diff** — which fields
changed, compared by meaning, never by spans or formatting — and your
schema declares each field's reload class with `#live`/`#restart`
directives. Your tool reads the directives off each change and acts. No
string comparison, no re-parsing messages, no drift between "what changed"
and "what to do about it."

```nml check
model server:
    rateLimit number #live
    port number #restart
```

```rust source=docs/guides/examples/cookbook/examples/diff_and_classify.rs
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
```

`diff_config(&index, root_model, old, new)` returns `FieldChange`s —
`path` (with per-step schema directives and `is_secret`, which should drive
redaction in anything you log), `kind` (`Added`/`Removed`/`Modified` with
the values), and `origin` (did the value come from a file or a default —
so a default materializing isn't misread as an edit).

Full program: [`diff_and_classify.rs`](examples/cookbook/examples/diff_and_classify.rs)
— `cargo run -p nml-cookbook --example diff_and_classify`.

This is the pattern behind production zero-downtime reload: diff the old
and new config, apply `#live` changes in place, and report `#restart`
changes truthfully instead of pretending. Declare the vocabulary itself in
your package manifest (the [directive vocabulary
recipe](directive-vocabulary.md)) so editors validate it for your users.
