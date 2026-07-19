# Open an NML file

Open any `.nml` file. Look at the **status bar** (bottom-right):

- `nml: <package> <version>` — the schema governing this file, and its delivery
  channel (workspace file, in-binary, or store).
- `nml: no schema` — nothing governs this file yet. Commit a
  `<name>.package.nml`, or run your tool's `schema sync`.

Diagnostics, completions, and hovers all come from that schema. Click the status
item to restart the server.
