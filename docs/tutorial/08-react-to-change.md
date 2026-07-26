# 8 · React to change

Skylight is running. Ops edits the config: bumps the database pool, adds a
monitoring region, rotates the API key, and drops the debug log level.
Which of those needs a restart? Today that answer usually lives in tribal
knowledge. In NML it lives in the schema — and in this chapter you build
the classifier that enforces it, in about 60 lines:

```text
Reload plan (app.v1.nml -> app.nml):
  live     logLevel: "debug" -> "info" (schema default)
  live     apiKey: changed (secret — values not shown)
  restart  database.poolSize: 10 -> 20
  live     endpoints.Api.regions: +"eu-west"
3 change(s) apply live; 1 need(s) a restart — restart required.
```

Real output, run in CI. Notice four things already: the differ saw a change
whose *new* side is a schema default (nobody wrote `"info"`), the rotated
secret was reported without leaking a byte, the set-typed `regions` diffed
as an element delta, and each change carries a verdict that came from the
schema, not from `if` statements.

This chapter's files: [`examples/08/`](examples/08/) — the schema with
directives, yesterday's config (`app.v1.nml`), today's (`app.nml`), and the
program (`app/`).

## Directives: schema-declared reload semantics

A **directive** is `#name` trailing a field declaration — the `#` you were
promised in Chapter 1. NML parses and carries directives but assigns them
no meaning; *your tool* declares the vocabulary and the semantics. Skylight
uses two: `#live` (a running process absorbs the change) and `#restart`:

```nml fragment
model service is accessControlled:
    host string #restart
    port number #restart
    logLevel logLevel = "info" #live
    requestTimeout duration = 30s #live
    apiKey secret #live
    database database #restart
    endpoints []endpoint #live
```

Read `apiKey secret #live` out loud: *key rotation must not require a
restart*. That's an operational policy, reviewable in the schema diff like
any other line of code. A directive on a branch field (`database`,
`endpoints`) covers everything beneath it unless something deeper says
otherwise — the **nearest directive wins**.

## Diff two versions

`nml_core::diff::diff_config` compares two versions of a model instance and
returns semantic changes — not text lines:

```rust source=docs/tutorial/examples/08/app/src/main.rs
    let changes = diff_config(
        &index,
        "service",
        &[(PathBuf::from("app.v1.nml"), &old_body)],
        &[(PathBuf::from("app.nml"), &new_body)],
    );
```

Each `FieldChange` is **where** (`path: FieldPath`), **what**
(`kind: ChangeKind`), and the source `origin`. The kinds you'll meet:
`Added`, `Removed`, `Modified { old, new }`, and — Chapter 4's promise —
`SetDelta { added, removed }` for set-typed fields, where "the same list in
a different order" is *no change at all* and one new region is exactly
`+"eu-west"`.

Because the differ applies schema defaults, it also sees through them:
deleting `logLevel = "debug"` from the file is `Modified` to `"info"`, with
`Origin::Default` telling you the new value came from the schema.

## The classifier

The heart of the chapter — and it's a fold, not a rules engine. A
`FieldPath` is structured (field hops and element hops, like
`endpoints.Api.regions`), and every *field* hop carries the schema facts a
consumer needs: its directives and whether it's a secret. Nearest directive
wins, so walk the field steps leaf-to-root and take the first verdict:

```rust source=docs/tutorial/examples/08/app/src/main.rs
/// What one change needs, per the schema's directives: the *nearest*
/// directive wins, reading the field path leaf-to-root; a path with no
/// directive at all is conservatively a restart.
fn classify(change: &FieldChange) -> &'static str {
    let steps: Vec<_> = change.path.field_steps().collect();
    for step in steps.into_iter().rev() {
        for directive in &step.directives {
            match directive.name.as_str() {
                "live" => return "live",
                "restart" => return "restart",
                _ => {}
            }
        }
    }
    "restart"
}
```

The default matters: a path with no directive is a **restart**. When you
add a field and forget to classify it, the failure mode is "unnecessary
restart", never "silently skipped a change the process can't absorb".

Secrets get the same by-construction treatment. `change.is_secret()` folds
the terminal field hop's `secret` flag, and the report checks it *before*
touching values:

```rust source=docs/tutorial/examples/08/app/src/main.rs
fn describe(change: &FieldChange) -> String {
    if change.is_secret() {
        return "changed (secret — values not shown)".to_string();
    }
```

You never wrote a redaction list, and there's no list to forget to update —
the schema already knows which fields are secrets.

The rest of the program loads the schema and both files exactly like
Chapter 7 and prints the plan; run `cargo run` in `examples/08/` to see the
output from the top of the page. Wire the same loop to SIGHUP or a file
watcher and you have live reload with a truthful report — the architecture
behind NML-based platforms' `reload --check`, at tutorial scale.

## Exercises

1. Ops decides pool-size changes are safe after all. Move the verdict:
   mark `poolSize` itself `#live` inside `model database` (leaving
   `database #restart` for everything else) and rerun. Why does the
   *nearest* rule make this work?

   <details><summary>Solution</summary>

   In `model database`: `poolSize number = 10 #live`. The path
   `database.poolSize` now hits `#live` at the leaf before `#restart` at
   `database` — nearest wins, so the pool change reports live and the run
   ends `all 4 change(s) apply live — no restart needed.` Field-level
   overrides beneath branch-level policy is exactly what leaf-to-root
   folding buys.

   </details>

2. Change `host` in `app.nml` (say, to `"127.0.0.1"`) and rerun. Where
   does the verdict come from?

   <details><summary>Solution</summary>

   `restart  host: "0.0.0.0" -> "127.0.0.1"` — from `host string #restart`
   in the schema. Delete the directive and it's *still* restart: the
   undirected default is conservative by design.

   </details>

## Common mistakes

- **Classifying from rendered path strings.** `FieldPath` implements
  `Display` for reports, but fold `field_steps()` for decisions — the
  structure carries the directives and secret flags; a string doesn't.
- **A permissive default for undirected fields.** Default to restart.
  "Live unless stated" turns every forgotten annotation into a process
  that's running stale config while claiming it reloaded.
- **Hand-rolled redaction lists.** Use `is_secret()` — the schema is the
  single source of truth for what must never appear in a report.

## Recap

- Directives (`#live`, `#restart`) are schema-carried, tool-defined
  metadata — reload policy becomes a reviewable line in the schema, with
  branch-level directives covering subtrees and leaf overrides beating
  them.
- `diff_config` returns semantic changes — `Modified`, `SetDelta`,
  default-aware, origin-tagged — with a structured `FieldPath` whose field
  hops carry directives and secret flags.
- The classifier is a leaf-to-root fold with a conservative default, and
  redaction is `is_secret()` — both by construction, neither maintainable
  by memory.

Next: [Chapter 9 — Ship schemas to your users](09-ship-schemas-to-your-users.md):
everything Skylight's schema knows — validation, defaults, directives —
delivered to the people who write the config, in their editors.
