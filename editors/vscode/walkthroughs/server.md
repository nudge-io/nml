# The NML language server

Schema-aware editing (diagnostics, completions, hovers) is powered by the
`nml-lsp` language server.

- **Have the tool that owns your config?** (e.g. `nudge`) — you may not need
  anything else. Declare it in the next step and the editor uses the tool's own
  server.
- **Generic NML project?** Install the neutral server:
  - `cargo install nml-lsp`, or
  - copy the `nml-lsp` binary to `~/.cargo/bin/nml-lsp`, or
  - set **`nml.server.path`** to its location.

The neutral server is safe in untrusted workspaces — it only reads and validates
data, it never executes your project.
