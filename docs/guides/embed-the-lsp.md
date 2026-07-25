# Embed the language server

Give your CLI a `<your-tool> lsp` subcommand and your users get
schema-aware editing **against the exact binary they run** — diagnostics
with stable codes and quick-fixes, completion, hover docs, in-editor error
explanations — with zero schema sync, because the schema ships inside your
tool. This is the entire implementation:

```rust source=docs/guides/examples/cookbook/examples/embed_lsp.rs
    nml_lsp::serve(package).await;
```

Full program: [`embed_lsp.rs`](examples/cookbook/examples/embed_lsp.rs) —
`cargo run -p nml-cookbook --example embed_lsp` (it serves stdio and exits
on EOF, which is exactly how the docs harness runs it in CI).

`serve(package)` is the neutral server **plus** your embedded package at
in-binary precedence: files your package's bindings claim validate against
your schemas; everything else behaves exactly like the standalone
`nml-lsp`. Your package's directive vocabulary, modifiers, and strictness
all apply — declared once ([directive vocabulary](directive-vocabulary.md)),
enforced in the editor.

**How editors find it:** users declare `provider: tool = "<your-tool>"` in
`nml-project.nml`; the VS Code extension launches `<your-tool> lsp` — only
in trusted workspaces, only from PATH, only after a per-workspace prompt
(the trust model is deliberate: a repository can never redirect the editor
at an arbitrary binary). Untrusted workspaces still get the bundled neutral
server against committed schema files.
