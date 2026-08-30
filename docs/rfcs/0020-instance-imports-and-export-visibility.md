# RFC 0020 — Instance imports, exports, and file scope

- **Status:** Proposed — **draft sealed for implementation** with RFC
  0019 (shared ten-round review arc — TERMINAL SEAL 2026-08-28)
- **Date:** 2026-08-27
- **Crates:** nml-core (parser, AST, scope, layers), nml-validate, nml-lsp,
  nml-cli
- **Depends on:** RFC 0012 (schema universe), RFC 0019 (`uses`, layer
  grants). Supersedes RFC 0012's flat cross-file **instance** visibility;
  schema-definition names keep RFC 0012 semantics (NML2009).
- **Origin:** Tape and other multi-trust embedders need explicit cross-file
  visibility; flat global instance names are a footgun without imports.

## Summary

Cross-file instance references are **scoped by file**. A block is visible
outside its defining file only when marked **`export`**. Another file
brings exported symbols into scope with a top-level **`import`**
declaration naming a **static workspace-relative path**. A
`uses` clause (RFC 0019) may target only same-file instances or symbols
explicitly imported in that file — always, with no ambient global mode. Instance names are unique **per file**, not
globally: every cross-file reference carries its source path, so exported
names may repeat across files and importers disambiguate with `as`.

```nml fragment
// vendor/skylight/member-lookup.flow.nml
export flow memberLookup:
    entrypoint = "search"
    steps:
        - search:
            action = "type"
        - submitSearch:
            action = "click"

flow internalDraft:
    steps: ...
```

```nml fragment
// vendor/skylight/atrium.flow.nml
import memberLookup from "vendor/skylight/member-lookup.flow.nml"

export flow atriumVariant uses memberLookup:
    steps:
        - submitSearch:
            locator = "#atrium-submit"    // entrypoint stays sealed at the base
```

`internalDraft` is file-private — private as a **name**: it cannot be
imported or referenced from another file, though a block that composes
an *exported* wrapper over it still receives its content (see *Security
notes*). `memberLookup` is importable because it is exported.
`atriumVariant` composes with `uses` only after import brings
`memberLookup` into scope.

## Motivation

RFC 0019 adds `uses` for cross-file instance composition. RFC 0012 gives
open contexts one flat namespace of instance names. That combination
creates two problems:

1. **Ambient authority** — without imports, an author can compose against
   instances they never declared a dependency on, as long as the name
   exists somewhere in the check universe (RFC 0012).
2. **Opaque provenance** — `uses memberLookup` does not say *where*
   `memberLookup` lives; dotted names (`vendor.memberLookup`) look like
   namespaces but are spoofable author-chosen identifiers.

Path-qualified `uses` (`uses "vendor/foo.nml"::memberLookup`) is the
wrong fix: it conflates composition with addressing and still lacks a
real visibility model. **Import is the right primitive**; `uses` stays
composition-only.

This RFC is embedder-agnostic. Vendor/tenant is one mapping; admin/user,
platform/product, and maintainer/consumer use the same mechanism.

| Role (abstract) | Typical import posture |
|---|---|
| Operator | Authors manifest + catalog; does not need imports in content |
| Maintainer | Exports bases; may import shared bases within trusted subtree |
| Author (low-trust) | Often no imports and no `uses`; platform assembles stacks |
| Runtime | Loads by path (catalog), applies grants, composes, executes |

## Design

### Syntax decision — what reads best in NML

NML top-level declarations are **short declarative sentences** (`const
Port = 8000`, `flow cuXyz uses base:`). Header clauses use everyday
verbs (`is`, `by`, `uses`, `as`). Imports are file-level declarations,
not header clauses on instances.

Candidates evaluated:

| Form | Verdict |
|---|---|
| `import { memberLookup } from "path"` | Familiar (JS/TS), but braces are not NML list syntax at top level — rejected on family consistency |
| `use memberLookup from "path"` | Collides mentally with `uses` composition — rejected |
| `from "path" import memberLookup` | Valid but inverted vs NML's `name verb …` habit — rejected |
| `import memberLookup of "path"` | On-family but uncommon in config languages — rejected |
| **`import memberLookup from "path"`** | **Adopted** — reads as English, static path obvious, one symbol per common case |
| **`import a, b from "path"`** | **Adopted** — two or three symbols from one file without a block |
| **`import from "path":` block** | **Adopted** for four or more symbols — path once, list below |
| **`import X as Y from "path"`** | **Adopted** — `as` already in the language (RFC 0015) |
| `uses vendor.memberLookup` without import | Fake namespace; rejected (RFC 0019) |
| `uses "path"/memberLookup` | Path-qualified composition; rejected — use import + `uses` |

**Export** uses a declaration prefix (like `[]` on array declarations),
not a trailing clause:

```nml fragment
export flow memberLookup:
```

Default is **file-private** — secure default; only exported instances are
cross-file visible.

**`export` applies to instance blocks only (v1).** Schema definitions and
instances share one `BlockDecl` node (the distinction is a keyword-text
test), so the grammar happily parses `export model foo:` — the language
rejects it instead: `export` on a schema definition (`model` / `trait` /
`enum`), an array declaration, a `const`, a `template`, or a `oneof` is
**NML2078**. Schema visibility already has its own mechanism (the package
manifest's `[]schema`), and const/template exports wait for symbol
imports (*Deferred*).

### Grammar

```
ImportDecl      ::= 'import' ImportClause 'from' Path
                  | 'import' 'from' Path ':' INDENT ImportListItem+ DEDENT

ImportClause    ::= ImportName (',' ImportName)*
ImportName      ::= Identifier ('as' Identifier)?
ImportListItem  ::= '-' ImportName

Path            ::= string literal — workspace-relative, forward-slash,
                    canonical, byte-exact (same rules as layer grants)

ExportDecl      ::= 'export' ( BlockDecl | ArrayDecl | ConstDecl
                             | TemplateDecl | OneofDecl )
                    — every non-Block alternative is the NML2078 error
                      form: it parses cleanly so the diagnostic fires on
                      one clean node instead of a shattered header
```

**Three arities, one family** — pick by count, not by taste:

| Count | Form |
|---|---|
| 1 symbol | `import memberLookup from "path"` |
| 2–3 symbols, same file | `import a, b from "path"` |
| 4+ symbols, same file | `import from "path":` block with `-` items |

Block items use **`-` list syntax** (same as `[]resource` array declarations),
not bare-indented names — one list vocabulary across top-level declaration
bodies.

`import` and `export` are **contextual**, like `else` and `as` — they are
not reserved model or field names. The parser decides by lookahead at the
start of a declaration. Mechanics grounding: declaration dispatch is a
bare `current_text()` first-token match (the `const`/`template`/`oneof`
arm) — **not** `at_kw`, which requires no preceding newline and is
therefore always false at declaration position. The lookahead this
table needs is **more than `nth(1)`**: `Parser::nth` returns a
`SyntaxKind` only (no text, and it discards the per-token newline
flag), and rows two and three below both begin `import Ident`, so a
one-token kind peek cannot separate them. The specified mechanism is a
**bounded same-line scan**: after `import`, scan forward while the
tokens are `,` / `as` / an `Ident` **other than the text `from`**, on
the same line (`from` itself lexes as `Ident` — there is no `From`
kind — so it must be the scan's stopping text, not part of the consumed
run); the declaration is an `ImportDecl` iff the scan stops on `from`
followed by a `String` — else it is a `BlockDecl` whose keyword is
`import`.
(The scan is bounded by the line and allocates nothing; the existing
one-token precedents — `else`+arrow, `set`+`<` — establish contextual
dispatch, not this scanner, which is new.) Two ordering constraints are
load-bearing: `ImportDecl` must be dispatched **before** the
`BlockDecl` fallback, because the header-level
`reject_decl_annotation()` pass would otherwise consume `import X as
Y`'s `as` as a stray annotation; and the marker-based event parser
means the fallback needs no backtracking — start one marker, complete
it as either `ImportDecl` or `BlockDecl`.

| First tokens | Parse |
|---|---|
| `import from "…":` | `ImportDecl` (block form) |
| `import Ident (as Ident)? (, Ident (as Ident)?)* from "…"` | `ImportDecl` (clause form, via the same-line scan) |
| `import Ident :` / `import Ident uses …` / `import Ident is …` | `BlockDecl` whose keyword is `import` |
| `export Ident Ident` (keyword + name, then `:` / `uses` / `is` / **line end** — the bodyless colon-less block is legal) | export prefix + `BlockDecl` |
| `export Ident :` | `BlockDecl` whose keyword is `export` |
| `export` + `[` / `const` / `template` / `oneof` | `ExportDecl` wrapping the parsed declaration, so **NML2078 has a production to fire on** — without these rows the export prefix on an array, const, template, or oneof falls into `block_decl` and shatters into garbage declarations instead of one clean diagnostic |

`from` is special only after `import`. `uses` is special only in a
declaration header, sibling of `is` — a model may still declare a field
named `uses`.

Examples:

```nml fragment
import memberLookup from "vendor/skylight/member-lookup.flow.nml"

import memberLookup, orderLookup from "vendor/skylight/bases.flow.nml"

import memberLookup as vendorBase from "vendor/skylight/member-lookup.flow.nml"

import from "vendor/skylight/bases.flow.nml":
    - memberLookup
    - orderLookup as vendorOrders
    - loanLookup
    - payeeLookup
```

**Declaration order.** `import` declarations are top-level, conventionally
placed **before** any instance block that references them (same placement
rule as `const`). `nml fmt` groups imports at the top of the file, sorts by
path then symbol, and preserves `as` aliases — so provenance stays visible
in review without authors hand-maintaining order.

Paths are matched as **canonical, forward-slash, workspace-relative
strings, byte-exact** (RFC 0019 path pipeline); closed bindings reject
symlinked target files. A path that is absolute, escapes the workspace
after `..` normalization, or names the importing file itself is
**NML2069** — and so is a grant-allowed path that does not exist
(existence is probed only after the grant allows; *Paths and
resolution*).

### Paths and resolution

- Resolved at `nml check` time against the workspace / package root,
  through RFC 0019's path pipeline (P1 syntactic form → P2 canonicalize
  and contain → P3 grant match → existence last; P4 symlink rejection).
- **The grant check runs before the existence check, and their outcomes
  never mix** — otherwise the pair of codes is a workspace-wide
  filesystem oracle: `import x from "admin/transfers.flow.nml"` (exists
  → denied) versus `"admin/transfrs.flow.nml"` (absent → unresolved)
  would let an author enumerate the operator's private tree by
  spelling. A path the grant does not allow is **NML2070 whether or not
  the file exists**, and the existence probe is never run for it.
  Canonicalization of a not-yet-verified path resolves the existing
  ancestor chain through the filesystem and collapses the remainder
  lexically — enough to defeat `..` and symlink games without touching
  the leaf.
- An **allowed** target must then exist, not be the importing file, and
  remain inside the workspace (NML2069).
- Imported name must refer to an **`export`** instance block in the target
  file (NML2071 if missing or not exported). Model-keyword matching is
  enforced where the symbol is **used** (`uses` — NML2062), not at import
  time — an import binds a name into scope; keyword agreement is a
  property of the composition that consumes it.
- Non-exported instances in the target file are invisible cross-file
  (NML2071) — `export` gates visibility, not merely imports.
- Import does not re-export transitively: importing `atriumVariant` does
  not import `memberLookup` through its `uses` stack.
- An import binding nothing in the file references is **NML2075**, a
  warning — stale dependency edges stay visible in review and the LSP
  offers a remove action. (`nml fmt` never removes them: formatting must
  not change the scope table.)
- **Import cycles between files are permitted.** An import only binds
  names, and export lists are syntactic — building a file's scope table
  never recurses into its imports' imports, so name binding needs no
  topological order. But scope tables are not the only traversal:
  compose-on-check walks the transitive import closure (RFC 0019, CLI),
  and *that* walk is over exactly this graph — so the rule is stated
  for it, not assumed away: the closure walk carries a **visited set
  keyed by canonical path** (cycles terminate structurally), the
  closure is restricted to **imports actually referenced by a `uses`
  clause** — which makes NML2075 load-bearing, not cosmetic: an unused
  import costs a warning, never a workspace walk — and a
  **language-level cap of 256 files** per closure bounds the work the
  way the depth cap 16 bounds a stack (both report as NML2066, which
  names the bound that tripped). Composition cycles remain NML2061 at
  the instance level.

### File scope

Each file has a **scope table** of instance names available for
`LayerRef` resolution:

| Source | In scope for `LayerRef` |
|---|---|
| Same file | Any local instance (export not required) |
| Other file | Only symbols brought in by an `import` declaration |

Cross-file targets must be **`export`** in their defining file — `internalDraft`
in another file is never visible, imported or not.

**Instance names are unique per file**, as a member of the existing
per-file declaration namespace (NML1000 — `SymbolTable::find_duplicates`
already covers blocks, arrays, consts, templates, oneofs). They are unique
*only* per file. Across files, names may repeat — even among exports —
because every cross-file reference names its source path at the import
site; there is no ambient namespace left for a collision to corrupt.
Import bindings join the same per-file namespace: a local declaration and
an import binding may not share a name (NML2074); two imports may not bind
the same local name (NML2072) — resolve either with `as`. Mechanically
these are the existing NML1000 collision — `SymbolTable` keys by bare
name across every declaration kind, and import bindings simply register
into it — but they carry dedicated codes because the recovery is `as`,
not rename-the-block: the fix the diagnostic teaches is different, so
the code is different. This replaces the
flat instance universe: a thousand tenant files can each declare `flow
memberLookup:` matching the vendor's canonical name, and the catalog
addresses each by path.

`LayerRef` (RFC 0019) resolves from scope as a **bare identifier** — a
local instance or an import binding. Consts are not layer refs (`import …
as` is the rename).

No qualified `alias.name` in `uses` — rename at import with `as` keeps
`uses` a flat, readable clause:

```nml fragment
import memberLookup as vendorBase from "vendor/skylight/member-lookup.flow.nml"

flow tenantFlow uses vendorBase:
    ...
```

### Interaction with `uses` (RFC 0019)

| Step | Mechanism |
|---|---|
| Visibility | `export` on source; `import` in consumer |
| Authorization | `layers:` grant on the importing file's binding (absence → NML2064; path mismatch → NML2070) |
| Composition | `uses` (unchanged merge semantics, RFC 0019) |

Order when both apply:

1. **Grant** — binding has a `layers:` grant at all (else NML2064 for
   `import` and `uses`). Diagnose **once**, at the first illegal statement
   in the file (the `import` if present, otherwise the `uses`) — not on
   every subsequent clause.
2. **Import** — path allowed? (NML2070 — checked first, and emitted
   without running the existence probe; see *Paths and resolution*)
   then: file exists and is not self? (NML2069) symbol exported?
   (NML2071) → bind name in file scope.
3. **`uses` / cross-file ref** — ref in scope? If not: **NML2073** when at
   least one **export** of that name **and the same model keyword**
   exists on a path the grant **would allow** (code action: add import —
   the keyword filter keeps the action from importing a symbol the
   `uses` would immediately reject as NML2062); **NML2059** otherwise (did-you-mean
   over in-scope names only — a denied or grant-less path is not hinted,
   matching RFC 0019's "do not echo base-layer content" rule). Then grant
   on the target's defining path at the `uses` site (RFC 0019 step 1 —
   NML2065 if denied) → compose per RFC 0019.

Same-file `LayerRef`s never require `import` or `export` — visibility rules
apply only across file boundaries.

A facade file imports a base and exports a wrapping instance that `uses`
it (true re-export is deferred) — and a facade **attenuates nothing and
launders nothing**: RFC 0019's root grant bounds every composed layer of the
composer's linearized stack (the declaring instance excepted), so content a composer's own grant would
not admit stays inadmissible when it arrives wrapped. An import
referenced by nothing is NML2075. A file may `uses` only symbols in
scope.

Platform/runtime catalog assembly (embedder data) loads layers by **path**
directly, selects the instance in each file, and composes — the catalog
needs no `import` scope table *to pick its pinned files*. A pinned
layer's own `uses` refs still resolve through that layer file's scope
table, exactly as `nml check` would resolve them (RFC 0019, *Embedder
guidance*) — path-addressed pinning is operator trust; it is not a
looser name-resolution mode.

### Authorization: grants on import paths

Import paths are checked against the **importing file's** binding layer
grant (same matcher as RFC 0019 `allowRefs` / `denyRefs` — globs over
defining file paths). Checked at the `import` statement, fail-closed:

```nml fragment
[]validator validators:
    - maintainers:
        files:
            - "bases/**/*.nml"
        schemas:
            - myapp
        strict = true
        layers:
            allowRefs:
                - "bases/**"

    - authors:
        files:
            - "users/**/*.nml"
        schemas:
            - myapp
        strict = true
        // no layers: grant → import and uses both NML2064
```

Deny wins over allow. An `import … as` rename cannot launder a path:
grants see the canonical (P2-resolved) import path, never the local
binding name. **`import`
requires a `layers:` grant** on the governing binding — same gate as
`uses` (NML2064 when absent); NML2070 is only when a grant exists and the
path falls outside `allowRefs` / inside `denyRefs`.

A file governed by **no binding** follows RFC 0019's context default:
closed universe → denied; open developer context (`nml check` in a repo
with no manifest) → permissive, so imports work out of the box while
developing and tighten the moment a binding governs the file.

### Diagnostics (stable codes)

| Code | Meaning |
|---|---|
| NML2069 | import path does not resolve |
| NML2070 | import path denied by layer grant — fires whether or not the target exists (no existence oracle); names the binding and a rule *identifier*, never the glob text (glob patterns are operator content; `nml binding` shows them to manifest authors) |
| NML2071 | cross-file symbol not found in target file or not `export` — deliberately one message for both (never confirms a private name exists), with a did-you-mean over the target file's **exported** names only: exports are the public contract (the same carve-out NML2067 makes for named identities), so the hint corrects a typo'd export and, by omission, tells a vendor "you forgot `export`" without naming any private symbol |
| NML2072 | duplicate import binding (same local name imported twice) |
| NML2073 | `uses` target is not in file scope, but an exported instance of that name and model keyword exists on a grant-allowed path (code action: add import) |
| NML2074 | local declaration name conflicts with import binding |
| NML2075 | unused import binding (warning) |
| NML2078 | `export` on a non-instance declaration (schema definition, array, const, template, oneof) |

Compose/import errors attribute to the **import or `uses` span**; NML2073
offers a code action ("import … from …") when a grant-allowed export
exists. Multiple candidates open a picker; a denied path is never
suggested. Because the code action is LSP-only by construction (see the
implementation plan), the **message text itself renders the exact line
to add** in the sole-candidate case — `add: import memberLookup from
"vendor/skylight/member-lookup.flow.nml"` — so the fix survives in CI
logs and reaches machine authors (the LLM repair loops the threat model
names) that never see an editor.

### Editor surface

- **Import completion** after `from "` — paths allowed by grant (prefix-filtered)
- **Symbol completion** in `import` clause — exported instances from resolved path
- **Completion after `uses`** — in-scope symbols only (local + imported,
  grant-filtered)
- **Code action** on NML2073 — add missing import
- **Unused import** (NML2075) — rendered faded; "remove import" code action
- **Go to definition** on imported symbol → defining export site
- **Go to definition, references, and rename all learn the scope rule** —
  all three resolve through the file's scope table (local, then import
  bindings, honoring `export`), or the editor and the language disagree
  — destructively, in rename's case (grounding in plan item 6).
- **Code lens** on `import` — "imports N symbols from path"

### CLI

`nml binding <file>` prints the governing binding, effective layer grant,
and resolved import table (path → local binding). `nml resolve` composes
`uses` stacks after scope validation.

## Examples

### Maintainer composes within trusted subtree

```nml fragment
import baseLookup from "bases/skylight/base-lookup.flow.nml"

export flow atriumVariant uses baseLookup:
    steps:
        - submitSearch:
            locator = "#atrium-submit"
```

### Author file — no cross-file visibility

```nml fragment
// users/alice/profile.flow.nml — authors binding has no layers: grant
flow aliceProfile:
    steps:
        - welcome:
            action = "click"
```

No import, no `uses`, no ambient base symbols. Platform catalog stacks
this file onto a base at runtime (RFC 0019 embedder guidance).

### Import denied (no grant)

```nml fragment
// users/alice/evil.flow.nml — authors binding has no layers: grant
import adminTransfer from "admin/transfers.flow.nml"   // NML2064
```

### Import denied (path outside grant)

```nml fragment
// bases/evil.flow.nml — maintainers binding grants bases/** only
import adminTransfer from "admin/transfers.flow.nml"   // NML2070
```

### Uses without import

```nml fragment
flow x uses memberLookup:    // NML2073 — memberLookup not imported
    ...
```

## Alternatives considered

- **Ambient instance universe (cross-file refs without imports), whether
  always-on or as a per-binding mode** — rejected: prod/dev drift (works in
  dev, breaks when bindings tighten); provenance absent from source; every
  production language requires explicit imports; one rule everywhere is
  simpler to implement, teach, and audit. A mode knob would also have
  forced instance names back to global uniqueness — losing file-scoped
  naming, the biggest ergonomic win of this design.
- **Flat global namespace without `export`** — ambient footgun; `export`
  gates all cross-file visibility.
- **Path-qualified `uses`** — import semantics smuggled into composition;
  rejected; import + flat `uses` is cleaner.
- **Bare-indented block items (no `-`)** — inconsistent with `[]resource`
  and every other top-level list in NML; block imports use `-` items.
- **Wildcard import (`import from "path": *`)** — export surface too broad;
  rejected for v1.
- **A new duplicate-name code for instances** — per-file uniqueness already
  exists as NML1000 (`SymbolTable`). Import collisions get NML2072/2074
  because recovery is `as`, not a second uniqueness rule.

## Compatibility

Pre-1.0 additive syntax. Cross-file instance references without `import`
become **NML2073** (when a grant-allowed export of the same name and
keyword exists — else NML2059), and imports of unexported targets become
**NML2071** — migration adds `export` to published instances and
`import` lines to consumers.

Instance names become **file-scoped**. RFC 0012's one-universe uniqueness
continues to govern schema definition names (NML2009); instance names
participate in the existing per-file declaration namespace (NML1000).
Nothing breaks: no global instance-name check exists today (instances are
not indexed across files), so this codifies reality while
`export`/`import` makes it safe.

## Documentation

When implemented, update:

- `spec/syntax.md` — `import`, `export`
- `spec/models.md` — scope rules with `uses`
- `docs/language-guide.md` — "Import and export instances"
- `docs/guides/` — multi-trust composition guide (with RFC 0019 security
  section)
- `crates/nml-core/assets/error-index.md` — NML2069–NML2075 + NML2078
  entries (`docs/errors/README.md` is a stub pointing there; CI enforces
  ascending order and bidirectional constants↔sections on the index)
- `CHANGELOG.md` — `Unreleased`
- RFC 0019 — cross-reference scope rules; retag `fragment` fences

## Implementation plan

Grounding: `ConstDecl` is the parser precedent for a contextual
first-token declaration (the bare `current_text()` match in
`declaration()` — not `at_kw`, which is always false at declaration
position; see *Grammar*); `import`/`export` join that match **with
lookahead** so they do not steal model keywords — a courtesy
`const`/`template`/`oneof` never got (a block keyword named `const`,
`template`, or `oneof` is already silently unusable; the new arms must
not add to that class).
The `export` flag lives on **`Declaration`** (the span-carrying wrapper),
not on `BlockDecl` — the prefix must parse on *every* declaration kind
so NML2078 can fire on one clean node (the grammar's non-Block
`ExportDecl` alternatives), and a flag on the wrapper needs no second
wrapper variant; `SymbolTable` already owns per-file
uniqueness (NML1000); binding loader already has first-match semantics
and path globs (RFC 0019); instance index from RFC 0019 plan extended
with export bits and per-file scope. `DeclarationKind` gains its sixth
variant (`Import`), and every exhaustive match over it follows.

1. **AST / parser** — `ImportDecl`, `export` flag on `Declaration` (with
   the NML2078 non-instance check), contextual lookahead for
   `import`/`export`; new `SyntaxKind` variants inserted before `Error`
   (the `repr(u16)` transmute bound) and named in the total `describe()`
   match. **Formatter in the same change**: `nml-fmt` re-emits from the
   semantic AST, so an unhandled `ImportDecl` or `export` prefix is
   silently deleted by `nml fmt` — emission plus round-trip tests land
   with the parser production, never after.
2. **`nml_core::scope`** — per-file scope table (local + imported); unused
   binding detection (NML2075).
3. **Instance index** — keyed by `InstanceId` (source path, name); export
   bits.
4. **`nml_core::layers`** — NML2073/NML2059 scope check before compose;
   grant check on import paths (NML2070); path pipeline.
5. **`nml-validate`** — wire scope into check pipeline.
6. **LSP** — import path/symbol completion; NML2073 import code action
   (prefers one-liner for single symbol, comma form for 2–3, block for
   4+); scope-aware definition/references/rename. The grounding that
   makes rename the urgent one: today all three are flat name scans over
   **every indexed document** (the LSP eagerly reads the whole workspace
   tree at startup), first-hit-wins for definition — and file-scoped
   names make same-name-across-files the *norm* (a thousand tenant files
   each declaring `flow memberLookup` is the design's headline win), so
   today's rename would rewrite every one of them in a single
   `WorkspaceEdit`. Two hosting truths for the code action: it is
   **LSP-only, never `nml fix`** — the batch fixer's sole-candidate
   splicer rejects any replacement containing a newline *by construction*
   (a documented security property), and its strict
   must-reduce-diagnostics acceptance would revert an insertion that
   surfaces NML2075 or downstream compose errors; and it needs a **new
   CST edit primitive** — `insert_entry_at_path` inserts into block
   bodies only, and nothing today inserts a top-level declaration at
   file head (item 8's import grouping needs the same primitive).
7. **CLI** — extend `nml binding` with import table.
8. **`nml fmt`** — group imports at file top; sort by path then symbol.
   The formatter **never rewrites a path string**: canonicalization is a
   *check-time resolution*, not a text transform — realpath-rewriting in
   the formatter would make `nml fmt` filesystem-dependent
   (non-deterministic across machines, symlinks, half-created trees)
   and would silently *pin* an import to a link's current target,
   changing the file's meaning: the scope-table invariant broken by the
   formatter itself. Never drops unused imports (NML2075 is a
   diagnostic, not a formatter rewrite). Because grouping reorders
   top-level declarations, NML2072/NML2074 collisions attribute to the
   **import binding** regardless of textual order, so formatting cannot
   flip which declaration is "the duplicate".
9. **Fixtures** — `tests/fixtures/imports/` plus one file per diagnostic,
   the existence-oracle case (denied path, present and absent, same
   NML2070), and an import-cycle closure fixture.

## Deferred

- **Re-export** (`export import foo from "path"`) — facade packages.
- **Import type/schema symbols, consts, and templates** — v1 is
  instance-only; schema sharing stays in package manifest `[]schema`.
  Const imports are the gating item for RFC 0019's cross-file
  shared-value pattern (consts are file-scoped today, so "both layers
  reference the same constant" works only same-file until this lands) —
  and `template` declarations ride the same decision, not a separate one:
  they register into the same const-value bucket in the symbol table.
  Template imports are also the item that dedupes the workspace's
  heaviest real duplication (near-twin workflow files sharing large
  prompt templates — RFC 0019, *Deferred*).
- **Cross-package imports** — pins and catalog (RFC 0019); in-repo paths only in v1.

## Security notes

Import closes the ambient-global footgun: cross-file visibility requires
`export` on the source and `import` in the consumer — one rule, no mode
switch. Combined with RFC 0019 layer grants (path allow/deny) and embedder
catalog assembly (cross-trust routing), four independent layers apply:

1. **Export** — what may be **named** from another file. This is a
   naming scope, not confidentiality: composing an exported block
   exposes its entire transitive stack's *content* and provenance to
   the composer (an exported wrapper over a file-private base resolves
   to the base's fields, and `nml resolve --provenance` attributes
   them). Content that must stay unreadable belongs on paths the
   composer's root grant does not admit — that is layer 3's job, and
   only layer 3's.
2. **Import** — explicit dependency edge into a file
3. **Grant + merge policy** — what content may reach a resolved
   artifact, and what may change
4. **Catalog + capability** (embedder) — what runs in production

Diagnostics are part of the boundary: NML2071 does not distinguish
"missing" from "not exported"; NML2073 / completion never hint a path
the grant would deny. Overlay authors cannot probe private or
out-of-grant names through error text.
