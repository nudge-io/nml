# RFC 0013 — Document-scope deserialization

- **Status:** Implemented
- Date: 2026-07-23

## Summary

`from_document_defaulted(index, &doc, keyword, name, resolver)` is the
serde pipeline at **document** scope: a property whose value references a
top-level array declaration (`endpoints = monitoredEndpoints`) is
materialized — shared properties, properties, modifiers, and items inlined
exactly as if authored in place — before the body pipeline
(positional → shared → defaults → resolve → serde) runs. The modular
layout the language encourages and the structs an embedder wants stop
being a trade-off.

## Motivation

`from_body_defaulted` deserializes a *body*, so declaration references were
invisible to it: the tutorial had to inline its endpoint list to teach
chapter 7, and every embedder faced the same choice between modular files
and typed access. `ValueResolver::with_symbols` cannot close the gap —
array declarations are not `Value`s — so the fix is a new scope, not a
resolver hook: the unit of meaning is the document, references included.

## Design

- New query primitive: `Document::array_body(name)` — a top-level
  `[]keyword Name:` declaration's body, by name.
- `defaults::from_document_defaulted` walks the target block's body and
  rewrites `field = ArrayName` properties into the inline nested-list form
  (`array_declaration_as_body`), recursively (a materialized array's own
  properties may reference further arrays), bounded by
  `MAX_REFERENCE_DEPTH` — a pathological chain degrades to "reference left
  in place" for symbols validation to report, never a hang.
- Everything else is the existing pipeline: the materialized form is
  byte-equivalent in meaning to the hand-inlined form, so shared-property
  scoping, positional materialization, defaults, and resolution apply
  identically (pinned by tests).
- `const` references keep resolving through `with_symbols`; unknown
  references pass through untouched (symbols validation owns that report).

## Deferred

- Materializing *block*-instance references (`webServer = DefaultWeb`) —
  same shape, no consumer yet.
- Reference list items (`- SomeRef`) — item-position references remain
  host-resolved.
