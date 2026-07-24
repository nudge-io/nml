# Editor Integration

NML ships a language server (`nml-lsp`, native and `wasm32-wasip1`) and a
VS Code extension that bundles it. Any LSP-capable editor gets the same
surfaces from the server; the extension adds the packaging.

## Diagnostics

Full-fidelity validation as you type: every parse, symbol, schema, and
value diagnostic the CLI reports, at the same spans, with the same stable
`NML0000` codes ([error index](../errors/README.md)). Machine-applicable
fixes (did-you-means, syntax migrations like `=>`→`->` and `&&`→`&`)
arrive as quick-fixes. Secondary locations (an unterminated string's
opening quote) arrive as related information.

## Error explanations (RFC 0010)

Hovering a squiggle shows the diagnostic's **explanation summary** — the
meaning paragraph from the error index, right in the hover:

> **NML2007** — *Missing required field.* Fields are required unless
> marked `?` or given a default; the instance omits one.
>
> *Run `nml explain NML2007` for the full entry.*

The summary appears after any regular hover content for the position (or
alone, highlighting the diagnostic's range). Content comes from the same
embedded index as `nml explain` — offline, always in sync with the
binary, covering every stable code.

The **full entry** opens in-editor: every coded diagnostic offers an
**Explain NML2007** code action (the 💡 lightbulb) that renders the
complete index section — meaning, runnable examples, the fix — as a
markdown preview beside your code. The **NML: Explain a Diagnostic
Code** palette command opens the same entries for codes you *can't*
hover (CI output, a teammate's log): it lists every code with its
summary, searchable by either. Explanations always come from the exact
server that produced the diagnostic — a provider tool (`nudge lsp`)
explains with its own binary's index — so error and explanation can
never version-skew, in any channel, offline.

For other LSP clients: the server emits the code action only to clients
that declare a command id in `initializationOptions.explainCommand`, and
serves the content over two custom methods — `nml/explain { code } →
{ markdown } | null` and `nml/explainIndex {} → [{ code, summary }]`.
Web links on codes (`codeDescription`) join at publish day — the last
tier of RFC 0010.

## Completion

Schema-driven: block keywords from the resolved schema context, fields
with types and defaults, enum variants, `oneof` discriminators, union
variants in the `as`-type slot, language keywords, and directive names
under a bound package — each drawn from the same candidate sets the
validator checks.

## Navigation and hover

Go-to-definition for references, keywords, and model fields; hover
documentation from schema definitions and leading `//` comment blocks;
document symbols. A bound document's position `(0,0)` hover shows its
schema-package binding (package, version, content hash, binding).

## Formatting

`nml fmt`'s canonical, comment-preserving formatting as the document
formatter — including canonical `" & "` conjunction spacing and `as`
annotation preservation.

## Schema packages

Documents covered by a schema package ([RFC 0030 lineage — see the RFC
index](../rfcs/README.md)) validate against the package's composed
schemas automatically; `.model.nml` files feed the workspace registry in
open mode.
