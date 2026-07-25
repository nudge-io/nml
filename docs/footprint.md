# Footprint & performance

Evaluators deserve numbers, not adjectives. Everything below is measured by
commands in this repo — rerun any of them yourself.

## Dependency footprint

Measured with `cargo tree -e normal --prefix none | sort -u` (2026-07):

| Crate | Direct deps | Total packages | Notes |
|---|---|---|---|
| `nml-core` | rowan, serde, thiserror | **19** | parsing, AST, query, serde bridge, defaults, diff, CST editing — **no tokio, no async, no I/O deps** |
| `nml-fmt` | nml-core | 20 | the formatter adds *zero* third-party deps |
| `nml-validate` | nml-core, blake3, dirs | **29** | schema validation + content-addressed packages/store |
| `nml-lsp` | tower-lsp/tokio stack | 124 | it's a language *server* — only your `<tool> lsp` subcommand pays for it |

The layering is the point: embedding NML parsing + typed deserialization in
your application costs 19 packages, all boring. The async stack exists only
in the crate whose job is serving editors.

## Binary size

A complete parse + schema-typed serde deserialization program
(the cookbook's [`deserialize`](guides/examples/cookbook/examples/deserialize.rs)
example), release profile, unstripped, aarch64-macOS:

```text
cargo build --release -p nml-cookbook --example deserialize
782,096 bytes   (~0.75 MiB)
```

## Parse throughput

`cargo run --release -p nml-cookbook --example measure_parse` walks this
repo's entire `.nml` corpus — including the deliberately *invalid* fixtures,
because resilient error recovery is part of the work — and reports:

```text
parsed 58 files (34052 bytes) in 2.5ms — 12.7 MiB/s (resilient parse, 3 diagnostics)
```

Measured on an Apple-silicon laptop; the corpus is small files (~600 bytes
median), so per-file overhead dominates — real-world single-file configs
parse in tens of microseconds. Config parsing will not be your hot path;
the number that matters is that it *rounds to zero* at startup, error
recovery included.

## What this buys architecturally

Lossless CST **and** typed AST in those 19 packages: the same core that
parses your config also powers format-preserving edits and semantic diff —
capabilities that usually cost a second dependency ecosystem. See the
[cookbook](guides/README.md) for each, compiled and run in CI.
