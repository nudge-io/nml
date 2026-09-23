# The NML language server

Schema-aware editing (diagnostics, quick fixes, completions, hovers,
formatting) comes from the `nml-lsp` language server. The extension ships
it compiled to WebAssembly and runs it sandboxed, so there is nothing to
install: open an `.nml` file and it starts. It only reads and validates
data — it never executes your project — so it runs in untrusted workspaces
too.

Two optional alternatives:

- **A native build** — for a very large tree, or a machine where the WASI
  host extension cannot run: install one (until the crates.io release,
  `cargo install --locked --git https://github.com/nudge-io/nml nml-lsp`
  puts it at `~/.cargo/bin/nml-lsp`) and set **`nml.server.path`** to it.
  A path inside your workspace is refused.
- **Your tool's own server** — if the tool that owns your config (for
  example `nudge`) ships one, declare it in `nml-project.nml` (previous
  step) and the editor validates against the exact tool binary you have
  installed.

If the server does not start, the status bar says so; **NML: Show Language
Server Log** has the cause.
