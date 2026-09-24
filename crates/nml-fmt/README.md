# nml-fmt

Canonical formatter for **NML**, a typed, indentation-based configuration
language.

```rust
use nml_fmt::formatter::format_source;

let formatted = format_source(source)?;
```

- **Comment-preserving** — `format_source` prints from the lossless CST, so
  own-line and trailing comments survive formatting in place.
- **Canonical style** — 4-space indentation, canonical string quoting,
  `set<T>` and `(K -> V)` type rendering, `?`/`+` suffix rendering
  ([`spec/style.md`](../../spec/style.md) is normative).
- **Your alignment, not ours** — the formatter never invents an aligned
  column and never destroys one: an aligned run of `oneof`/arm arrows stays
  aligned byte for byte, an unaligned one keeps its single space.
- **Idempotent** — formatting a formatted file is a no-op (tested).

The `nml fmt` command in [`nml-cli`](https://crates.io/crates/nml-cli) wraps
this crate with atomic write-back. Parsing lives in
[`nml-core`](https://crates.io/crates/nml-core).

## License

MIT OR Apache-2.0
