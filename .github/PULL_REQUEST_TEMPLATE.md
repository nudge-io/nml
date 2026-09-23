## What

<!-- One or two sentences: what does this PR change and why. -->

## Documentation

<!-- User-facing changes (syntax, public API, CLI, LSP, diagnostics) ship
     with their docs in the same PR. Check one: -->

- [ ] Docs updated (guide / spec / CHANGELOG as applicable)
- [ ] No docs needed, because: <!-- e.g. internal refactor, test-only -->

## Checklist

- [ ] `just gate fast core` passes (and `just gate` before review)
- [ ] Any new check is a `just gate-*` recipe — CI may not run a gate any other way
- [ ] A changed `pub` item in a library crate moved the API stamp
      (`API_STAMP` in `nml-cli/src/out.rs`) and has its CHANGELOG ledger
      entry, with `docs/api/*.api.txt` regenerated after both
- [ ] Any regenerated record (goldens, the limits table, the wire shape, the
      API record) was read as a diff, and the PR says why each line moved
- [ ] New examples are runnable and self-contained
- [ ] RFC status headers / index updated (language changes only)
