# Define a directive vocabulary for your tool

`#directives` trail a schema field. The language interprets four of them —
the merge-policy directives `#sealed`, `#identity`, `#append` and `#overlay`
(RFC 0019) — and is otherwise opaque to a directive's meaning: **your package
manifest declares the rest of the vocabulary** — names, argument shapes, and
docs — and the four builtins are part of every vocabulary without a
declaration. One declaration feeds three consumers with zero drift: the
kernel checks directives for your users on every front end, editors complete
and hover them, and your tool reads the declarations back to drive behavior
(reload classes, ownership, anything you define).

```nml check
model server:
    rateLimit number #live
    port number #restart
    apiKey string #sealed
```

The manifest declares what `#live` and `#restart` *are* (`#sealed` needs no
entry — it is the language's; declaring one of the four under your own
meaning is refused at load, NML2082):

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

Enforcement is the **kernel's**, on both front ends: a schema source your
package covers — declared in its `[]schema`, or a `*.model.nml` beside the
manifest — is judged under one vocabulary, the four builtins plus your
entries, by `nml check`, `nml validate`, `nml fix` and the editor alike. An
unknown directive is NML5000 with a machine-applicable did-you-mean over
every known name (a typo'd `#lvie` gets `#live`, a `#seled` gets `#sealed`);
an argument the entry does not take, or lacks, is NML5001; `#live` beside
`#restart` on one field is NML5002. The same row and the same sentence, in
`nml check`, its `--json` stream and the editor — and `nml fix` applies the
suggestion:

```text transcript=tests/fixtures/directive-vocabulary
$ nml check --root . core.model.nml
core.model.nml:3:22: error[NML5000]: unknown directive '#lvie' (package 'demo') (did you mean "#live"?)
for more information, run: nml explain NML5000
error: 1 error(s)
```

```text transcript=tests/fixtures/directive-vocabulary
$ nml check --root . --json core.model.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"code":"NML5000","col":22,"line":3,"message":"unknown directive '#lvie' (package 'demo') (did you mean \"#live\"?)","related":[],"severity":"error","source":"core.model.nml","suggestions":[{"edits":[{"col":22,"endCol":27,"endLine":3,"line":3,"lines":["#live"]}],"kind":"didYouMean","source":"core.model.nml"}],"type":"diagnostic"}
{"declarations":1,"errors":1,"key":"core.model.nml","ok":false,"target":"core.model.nml","type":"result","verb":"check","warnings":0}
{"closure":"complete","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":1,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

```text transcript=tests/fixtures/directive-vocabulary
$ nml fix --root . --dry-run core.model.nml
--- a/core.model.nml
+++ b/core.model.nml
@@ -1,3 +1,3 @@
 model core:
     action string #sealed
-    rateLimit number #lvie
+    rateLimit number #live
would fix core.model.nml (1 edit(s))
1 edit(s) would apply across 1 of 1 file(s); 0 diagnostic(s) not auto-fixable
```

The judge is yours to call too, on any extracted schema — the recipe runs it:

```rust source=docs/guides/examples/cookbook/examples/directive_vocabulary.rs
    let vocabulary = Vocabulary::new(&package.manifest.name, package.manifest.directives.clone());
    let source = "model server:\n    rateLimit number #live\n    apiKey string #sealed\n    port number #restrat\n";
    let (schema, _) = nml_core::cst::extract_schema(source);
    let verdicts = vocabulary.judge(&schema.models, source);
    assert_eq!(
        verdicts.len(),
        1,
        "the language's `#sealed` is known; `#restrat` is not"
    );
    assert_eq!(verdicts[0].suggestions[0].replacement, "#restart");
```

The other verdicts ride the same judge: a declared directive given an
argument it does not take (NML5001), `#live` beside `#restart` on one field
when the package declares both (NML5002), and a schema source that sits beside
the manifest without a `[]schema` entry — an advisory note (NML5003), which
`-q` drops:

```text transcript=tests/fixtures/directive-vocabulary-sibling
$ nml check --root . stray.model.nml
stray.model.nml:2:17: error[NML5001]: '#live' takes no argument
stray.model.nml:3:23: error[NML5002]: '#live' and '#restart' contradict — pick one
stray.model.nml:1:1: info[NML5003]: not part of package 'demo'; add a []schema entry to participate
for more information, run: nml explain NML5001
error: 2 error(s)
```

In the editor, completion after `#` offers the four builtins and then your
declared vocabulary with your docs, and hovering a directive shows its entry.
Your tool reads the same declarations at runtime, so the editor's view and
your classify step can never disagree. A schema source no package covers is
judged under no vocabulary — every directive is accepted there; where that
has a reason you can act on, both front ends say so in one info line at the
top of the file: two or more packages in the root could cover the file and
none declares it (`package coverage ambiguous: 2 packages could cover this
schema source (demo, other) …` — declare it in one package's `[]schema`),
or the root exceeded the scan bound. See [diff and
classify](diff-and-classify.md) for the consumption side, and [schema
packages](schema-packages-and-store.md) for shipping the manifest to your
users.
