# RFC 0012 — One namespace: self-validating files & closed authority

- **Status:** Implemented
- Date: 2026-07-23

## Summary

A file that declares a schema and instances together is fully validated by
`nml check` — `model cache:` above `cache Foo:` types `Foo`, in one file,
with no `--schema` flag. The check run composes **one schema universe**
(the `--schema` directory's sources plus the checked file), names are
unique across it (a collision is NML2009, attributed per file), and there
is **no precedence rule to learn**. Where a *package binding* validates a
file (RFC 0030 — an operator's schemas governing tenant-authored files),
the vocabulary is **closed**: in-file definitions never type anything, and
instead of today's silence they draw a diagnostic (NML2026) saying so.

## Motivation

Three forces, one design:

1. **Ergonomics.** "Put the schema above the config" is every newcomer's
   first intuition, and it silently did nothing — the single worst
   first-run astonishment in the language. CUE and Pkl treat
   self-contained files as the default mental model; NML was behind.
2. **Parity.** The editor already registers definitions from open
   `.model.nml` files; the CLI ignored in-file definitions entirely. Same
   file, different verdicts by surface.
3. **Authority (the security half).** With strict package bindings, a
   tenant file must be able to neither **shadow** an operator schema
   (redefine `model workflow` weaker) nor **extend the vocabulary**
   (declare `model x` + `x Foo:` to smuggle a block past strict's
   unknown-keyword wall). A naive "in-file definitions always count"
   design opens the second hole even with collision errors closing the
   first.

## Design

### Open contexts: one universe, one namespace

`nml check [--schema <dir>] <file>` performs a single `load_schema` over
the directory's sources **plus the checked file** (skipped if the file is
itself inside the directory — it is already a source). All loader passes —
reserved/duplicate names, composition (RFC 0011), oneof integrity,
positional arity, cycles — run over the whole universe with per-file
attribution. The validator is built from the composed schema (open,
`composition_checked_at_load`), so in-file models, traits, enums, and
oneofs type in-file instances exactly as directory schemas do.

- A name declared in both the file and the directory is **NML2009** — no
  shadowing, no precedence, fail-closed.
- Splitting one file into a `schemas/` directory changes file layout, not
  semantics — the gradually-adoptable promise, kept.
- `nml validate` remains definitions-focused (no instance typing); `check`
  remains its strict superset.

The editor's lenient (registry) mode gains the same semantics: a
document's own definitions join its validation set (workspace registry
first, document-local names filling in), so a self-contained file is typed
identically in CI and in the editor.

### Closed contexts: authority follows provenance

A validator built from a **schema package binding**
(`SchemaPackage::validator`, including the built-in meta-package) is
**closed**: the binding's composed schema is the entire vocabulary.
In-file definitions in a bound file:

- never type instances, never extend the keyword vocabulary (a
  tenant-minted keyword still fails strict's unknown-keyword wall), and
- draw **NML2026** — "in-file schema definitions have no effect under this
  binding" — a warning in lenient mode, an **error** under strict.
  Honest where the old behavior was silent, closed where it must be.

No configuration knob exists: closedness is set at the one constructor
that knows the provenance. Every nudge validator flows through it, so the
platform inherits the posture with zero changes.

## Compatibility

Pre-1.0. The only behavior change in open mode affects files that already
declare a model *and* instances of it — authors who unambiguously wanted
typing and weren't getting it. Closed mode replaces silence with a
diagnostic; it forbids nothing that worked before.

## Alternatives considered

- **File-wins / dir-wins precedence** — silently ignores someone's text or
  weakens operator schemas; collision-as-error is the only rule that never
  lies.
- **A CLI flag to opt into self-validation** — a knob for something that
  should simply be true; rejected.
- **Extending closed vocabularies with in-file definitions under a
  namespace** — speculative; no consumer wants tenant-defined types today.
