# The NML Tutorial

You build one thing across nine short chapters: the configuration for
**Skylight**, a hosted status-page service. It starts as twelve lines of
plain config and ends as a schema-validated, access-controlled, live-reloadable
system whose schemas ship to your users' editors.

Each chapter takes 10–15 minutes and ends with a working file you can check
with the real tooling. Every code block on these pages runs against the real
`nml` CLI in CI, every chapter's finished config is a fixture the test suite
validates, and the Rust programs in chapters 7–9 compile and run in CI with
their printed output asserted — what you see here is tested, not transcribed.

## Chapters

| # | Chapter | You learn |
|---|---------|-----------|
| 1 | [Your first NML file](01-your-first-nml-file.md) | files, declarations, properties, nesting, comments, `nml check`/`parse`/`fmt` |
| 2 | [Types that mean something](02-types-that-mean-something.md) | the 9 primitives, money, durations, secrets and fallbacks, constants, templates |
| 3 | [Give it a schema](03-give-it-a-schema.md) | `model`, required-by-default, `?`, defaults, enums, unions, `nml check --schema`, reading diagnostics |
| 4 | [Compose and reuse](04-compose-and-reuse.md) | traits, `is` composition, shared properties `.key`, lists, `set<T>`, the positional marker `+` |
| 5 | [One of many](05-one-of-many.md) | `oneof`, discriminators, defaults, enum-typed exhaustiveness, body-shape dispatch |
| 6 | [Lock it down](06-lock-it-down.md) | `\|allow`/`\|deny`, built-in and user-defined roles, routing arms `(role -> V)` |
| 7 | [Embed it in Rust](07-embed-it-in-rust.md) | parse → resolve → defaults → serde structs; the full pipeline in ~40 lines |
| 8 | [React to change](08-react-to-change.md) | `#live`/`#restart` directives, `diff_config`, building a reload classifier |
| 9 | [Ship schemas to your users](09-ship-schemas-to-your-users.md) | `.package.nml`, content hashing, the schema store, `<your-tool> lsp` |

Chapters 1–6 need only the `nml` CLI. Chapters 7–9 assume you're comfortable
reading Rust; each one is a small standalone crate you can copy.

## Before you start

Install the CLI:

```bash
# until the crates.io release:
cargo install --git https://github.com/nicknudge/nml nml-cli
```

That gives you the `nml` binary (`nml help` to confirm). For the best
experience also install the VS Code extension — you get diagnostics with
quick fixes, completion, and hover docs as you type — but the tutorial only
assumes the CLI.

The finished file for every chapter lives in
[`examples/`](examples/) (one directory per chapter), so you can diff your
work against the expected state or skip ahead.
