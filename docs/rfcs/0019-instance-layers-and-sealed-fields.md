# RFC 0019 — Instance composition (`uses`) and sealed fields

- **Status:** Slice 1 implemented (same-file `uses` composition; the
  promised `resolve`/`binding`/`diff --resolve-layers` verbs, the
  language-guide chapter and `import` are not built) — spec **TERMINAL
  SEAL 2026-08-28** after nine review rounds (16 reviewer passes: grounding, plan, adversarial
  security, adversarial semantics, coherence audit, DX walkthrough,
  seal certification, implementation dry-run + spec-execution trace,
  post-fold certification with de-accretion, a zero-defect terminal
  certification, and a final author full-read — 10 rounds / 17 passes)
- **Date:** 2026-08-27
- **Crates:** nml-core (parser, AST, schema, layers, diff), nml-validate,
  nml-lsp, nml-cli
- **Depends on:** RFC 0012 (schema universe); RFC 0020 (imports/exports/
  file scope) for all cross-file instance visibility
- **Origin:** Tape (UI-automation flow library) multi-tenant flow
  composition; generalizes to any embedder that ships a base config and
  accepts tenant overlays (nudge workflows, service definitions, automation
  flows).

## Summary

An instance block may declare a **`uses`** clause in its header — ordered
references to other instance blocks of the **same model** that are composed
bottom-up, with the declaring body on top, into one **resolved** instance
before validation and deserialization. Schema fields carry merge-policy
directives; **`#sealed`** is the language builtin: the first layer to assign
a sealed field fixes it; any higher layer that assigns it is rejected.

```nml check
model step:
    name string+
    action string #sealed
    locator string

model flow:
    entrypoint string #sealed
    steps []step #identity

flow memberLookup:
    entrypoint = "search"
    steps:
        - search:
            action = "type"
            locator = "#q"
        - submitSearch:
            action = "click"
            locator = "#submit"

flow cuXyzMemberLookup uses memberLookup:
    steps:
        - submitSearch:
            locator = "#search-button"    // the tenant delta: their button differs
```

The base supplies `entrypoint` and the step list; the declaring body
overlays `submitSearch` by **identity**, re-targeting its `locator` while
`entrypoint` stays sealed at the base and `action` stays sealed inside
the item — the tenant changes *where* the step points, never *what* the
step does.

**Terminology:** the *concept* is layering — docs and APIs speak of layers,
layer stacks, and the lowest layer. The *syntax* is `uses`, in the
declaration header, parallel to how `is` is the syntax for the trait-mixin
concept. The authorization surface is accordingly named for the concept
(layer grants, `LayerGrant`), not the keyword.

## Motivation

NML already composes at other levels:

| Mechanism | Level | What merges |
|---|---|---|
| `trait` + `is` | Schema | Field definitions into a model |
| `.shared:` | Instance body | Defaults onto list items in one scope |
| `field = ArrayName` | Document | Array-declaration references (RFC 0013) |

None of these solve **cross-file, identity-keyed instance overlays** — the
pattern every multi-tenant embedder eventually needs:

- A vendor ships a canonical `flow memberLookup:` (or `service Api:`).
- A tenant changes one named step, one endpoint, or one timeout — not the
  whole file.
- Security fields (entrypoints, capability grants, compatibility metadata)
  must **not** be escalated by a tenant overlay.

Today's workarounds are all bad: fork the whole file per tenant (thousands of
near-identical copies that drift), abuse `model`-as-mixin (leaks vocabulary,
RFC 0011, and still doesn't merge named list items across files), or hide the
merge in application code (no editor validation, no semantic diff, no stable
diagnostics; every embedder reinvents the same bug farm).

`uses` makes overlay composition a **language concern** with one merge
engine, one diagnostic set, and one LSP story. `#sealed` makes authorization
boundaries **schema-declared** instead of convention.

### Why `uses` — the keyword decision

Candidates were tested in the only position the clause occupies, the
declaration header. NML headers are **declarative sentences** with
conjugated verbs — `model resource is accessControlled:`, `oneof email by
provider:` — and the winning keyword had to keep that shape.

| Candidate | Verdict |
|---|---|
| **`uses`** | **Adopted** — "cuXyz **uses** memberLookup" is a complete declarative sentence, exactly the `is` shape; direction-neutral (direct refs: listed order left to right, declaring body wins; transitive stacks: C3 — *Resolution pipeline*); consistent with the `is`/`by`/`as` clause family |
| `using` | Participle fragment; no other NML clause reads that way — rejected on family consistency |
| `layers` | Best *noun* for the concept, ambiguous as a header verb ("memberLookup layers vendor/base" — which is on top?); an earlier body-property draft also reserved a field name. Kept as vocabulary, rejected as syntax |
| `extends` / `inherits` | OOP subclass connotation; imply downward field flow — the wrong mental model for sealed fields |
| `amends` | Closest prior art (Pkl) and directionally precise, but off-register for the clause family and awkward with multi-ref lists |

### Header-only: no body form, no reserved names

The `uses` clause exists **only** in the declaration header. There is no
body-property form. Consequences:

- **No field name is reserved.** A model may freely declare fields named
  `layers`, `stack`, or `uses` — the clause is declaration syntax, like
  `is`, and can never collide with instance properties.
- **Embedder data stays ordinary.** A catalog model that stores a layer
  stack *as data* (see *Embedder guidance*) is a plain `[]model` field with
  no parser involvement.

## Design

### Syntax

```
<keyword> <Name> uses <LayerRef> [, <LayerRef>]*:
    <local fields...>
```

Composition order: `layers[0] ⊕ layers[1] ⊕ … ⊕ local body` — bottom-up,
later wins (subject to per-field merge policy). That is the whole story
for direct refs; when refs carry their own `uses` clauses, the
transitive stack is the C3 linearization of the DAG, which preserves
every clause's listed order or refuses (NML2077 — see *Resolution
pipeline*). A declaring body may be empty (pure stack assembly).

### Layer references — statically resolvable, always

A `<LayerRef>` is a **bare identifier** naming an in-scope instance (a
local block or an import binding). It resolves to exactly one instance
before composition (RFC 0020 — cross-file refs require an explicit
`import` and an `export` on the target). Any instance of the same model is
a valid base — there is no special "base block" type. Rename at import
with `as`; consts stay value aliases and are not layer refs — and
`template` declarations are consts in every way that matters here (they
register into the same const-value bucket in the symbol table and
produce a string, never an instance), so they are not layer refs either;
there is no overlay mechanism hiding in templates:

```nml fragment
myobject X:
    myvar = "abcd"

myobject Y uses X:
    // resolved: Y.myvar = "abcd"; Y's own fields overlay on top
```

```nml fragment
import memberLookup as vendorBase from "vendor/skylight/member-lookup.flow.nml"

flow cuXyz uses vendorBase:
    steps: ...
```

(Import syntax and `export` visibility — RFC 0020.)

**Design principle: everything in the language is resolvable at
`nml check` time.** Template strings, runtime parameters, content hashes,
and tenant routing are embedder data (see *Embedder guidance*), never
`uses` syntax. This keeps the CLI deterministic, the LSP honest, and the
security analysis tractable. Shared values across layers are expressed with
`const` (both layers reference the same constant), not by interpolating one
layer's fields inside another's expressions — cross-layer interpolation is
deferred (see *Deferred*). One scope truth: consts are file-scoped (the
`SymbolTable` is per-file, and the resolver's const lookup is built from
one file's symbols), and RFC 0020 v1 imports instances only — so const
sharing works between same-file layers today and across files only once
const imports land (RFC 0020, *Deferred*). Until then a cross-file shared
value is simply the base's field value, or embedder data.

Resolution is **fail-closed** (NML2059). The referenced block must declare
the **same model keyword** as the composing block (NML2062). `uses` is an
*instance* clause: a schema definition (`model` / `trait` / `enum`)
carrying a `uses` clause is rejected at schema extraction under the same
code — NML2062 covers both "wrong target kind" and "wrong declaring kind"
(the parser accepts the clause anywhere a header parses, resiliently; the
semantic layer draws the line).

### Resolution pipeline

Embedders and `nml check` share one primitive:

```rust
/// Compose a stack. `refs` are the declaring clause's LISTED refs, in
/// authored order — linearization (C3, transitive `uses` included) is
/// this function's job, not the caller's. Both callers pass listed refs:
/// the `uses` walker passes the clause's refs; catalog assembly passes
/// its pin list in stack order. Neither pre-linearizes; there is one
/// linearizer, in here.
///
/// `declaring` identifies the composing instance — its defining file is
/// what `grants` is keyed by for the ROOT grant (site grant, stack-wide
/// allow/deny bounds, depth limit), so the primitive can authorize
/// itself; without it, step 1 below is unimplementable.
///
/// `root` is a model OR oneof name — the dispatch `apply_defaults`
/// already does (`defaults.rs` tries `index.model(root)` then
/// `index.oneof(root)`), so oneof-rooted instance blocks compose too.
/// (Note the citation is `apply_defaults`, deliberately:
/// `apply_positional` silently no-ops on a non-model root today, which
/// is why oneof normalization goes through the effective arm's *model*
/// name — see the discriminator pre-pass in step 3.)
///
/// Return shape: the pair, matching `load_schema` — never `Result`,
/// because warnings (NML2079, NML2084, compose-time NML2076) ride
/// successful compositions and an `Err` arm has nowhere to put them.
/// Compose is total and best-effort, like the parser: it collects all
/// diagnostics in one pass. Step 1–2 failures (unresolvable ref, cycle,
/// NML2077, any denial) yield `None` — fail closed, no artifact.
/// Step 3–4 policy violations yield diagnostics PLUS a best-effort
/// instance with the offending contribution skipped, so the LSP's
/// resolved peek and override hover keep working at the moment the
/// author most needs them; `nml resolve` refuses to print an artifact
/// (nonzero exit) whenever an error-severity diagnostic exists — the
/// artifact of record is clean or absent.
///
/// `local` and `root` are the escape hatch for an un-indexed buffer
/// (an editor's dirty document); when `declaring` is indexed they must
/// agree with the index and the indexed forms win.
pub fn resolve_layers(
    index: &SchemaIndex,
    instances: &InstanceIndex,
    declaring: InstanceId,
    root: &str,
    refs: &[InstanceId],
    local: &Body,
    grants: &dyn LayerGrantProvider,
) -> (Option<ResolvedInstance>, Vec<Diagnostic>);

/// Identity of a composed instance: defining file + declaration name.
/// Names are file-scoped (RFC 0020), so diamonds dedupe by this pair,
/// never by name alone. `source_path` is
/// the canonical workspace-relative form (pipeline P2 produces an owned
/// `String`), so the index **interns** canonical paths; a catalog
/// caller building ids from `layerPin.path` keeps those strings alive
/// across the call.
pub struct InstanceId<'a> {
    pub source_path: &'a str,
    pub name: &'a str,
}

/// The instance index owns the parsed `File`s and resolves an
/// `InstanceId` to the declaring `BlockDecl` plus a `Document`
/// constructed over the owned file per call. The document is required,
/// not a convenience: RFC 0013
/// array-declaration references are file-local (`Document::array_body`
/// reads its own file's declarations), so each layer's normalization
/// (step 3) inlines against its *own* document, never the composing
/// file's. The index also carries, per file: the **scope table**
/// (RFC 0020 — local names + import bindings) and each declaration's
/// **parsed `uses` refs**, because linearization resolves every
/// transitive layer's bare refs through *that file's* scope, inside
/// this call. Export bits (RFC 0020) live here too.
///
/// A composed instance spans files, but `Span` is byte offsets into ONE
/// file with no file identity (`span.rs`) — so the resolved artifact
/// carries provenance explicitly. This table is load-bearing, not
/// decorative: it is what makes NML2060's `related` span, resolved-
/// validation attribution, `nml diff --resolve-layers`, the LSP's
/// resolved peek, and the embedder audit hash all point at the right
/// file and line.
pub struct ResolvedInstance {
    /// Invariant: exactly one entry per field name (see step 4).
    pub body: Body,
    /// Field path → origin. List items key by the identity **pair
    /// (kind, token)** — the same key the merge uses; keying by token
    /// alone would collide the very kinds NML2063 keeps apart. Entries
    /// reuse `diff::Origin` (`File { file, span } | Default`) so the
    /// provenance vocabulary and `--resolve-layers` never drift.
    pub origins: ProvenanceTable,
}

/// Authorization is two-grant: per authoring site (step 1) and per the
/// root grant over the linearized stack (step 2). A transitive stack
/// spans files under different bindings, so compose takes a provider.
/// Keying: this RFC *introduces* the canonical workspace-relative source
/// path as the one naming convention. Today `load_schema` tuple names are
/// caller-chosen attribution labels with no convention (basenames in
/// `nml check`, absolute paths in the LSP universes, logical `[]schema`
/// names in package composition) — those call sites migrate to the
/// canonical form as part of this work (implementation plan, item 0).
///
/// The lookup returns a STATE, not an `Option` — NML2064's three
/// message forms must variously name the governing binding and its
/// manifest file, both ambiguous claimants, or the unclaiming manifest
/// root; an `Option<&LayerGrant>` can construct none of that.
pub enum GrantLookup<'a> {
    /// A binding governs the file and carries a grant.
    Granted { grant: &'a LayerGrant, binding: &'a str, manifest: &'a str },
    /// A binding governs the file and carries no `layers:` grant.
    NoGrant { binding: &'a str, manifest: &'a str },
    /// Two or more manifests claim the file — denied, naming both.
    Ambiguous { manifests: Vec<&'a str> },
    /// No binding governs the file; the context default applies
    /// (closed universe → denied, open developer context → permissive).
    Unbound { open_context: bool },
}

pub trait LayerGrantProvider {
    fn grant_for(&self, source_path: &str) -> GrantLookup<'_>;
}
```

Order of operations:

1. **Authorize each clause** — checks answerable *per clause, without
   the linearized stack*: the clause's authoring site has a grant at all
   (else NML2064), and each of the clause's **listed** refs passes that
   site grant's allow/deny rules (else NML2065). Site authorization is
   evaluated **at each clause's authoring site** (a base's own `uses`
   clause is checked against the base file's binding, not the top
   file's), and ref rules are checked against the **referenced
   instance's defining file path**. An `import … as` rename can never
   launder a denied path: grants see the defining path, not the local
   binding name. The step numbering is **logical layering, not temporal
   phasing**: transitive clauses are only discoverable by loading
   layers, so site checks run *during* step 2's discovery, as each
   clause surfaces. Discovery always runs to completion so one pass
   reports every site denial, cycle, and order contradiction together;
   compose (step 3 onward) never runs if any step-1/2 error exists.
2. **Load, linearize, authorize the stack** — refs resolve
   **transitively**: a layer's own `uses` clause pulls its bases into
   the stack, with **each distinct `InstanceId` composed exactly once**
   (identity is `(source_path, name)`, never name alone, so two files
   that both export `memberLookup` stay distinct). The stack is the
   **C3 linearization** of the reference DAG — with one orientation
   fact an implementer lifting C3 from the MRO literature must not
   miss: **NML's precedence is the mirror of Python's**. An MRO puts
   the *first*-listed parent highest; NML composes bottom-up with the
   *last*-listed ref (and then the local body) winning. Concretely: run
   the C3 merge over the **reversed** ref lists, or equivalently read
   every C3 result back-to-front. Getting this wrong is not cosmetic —
   `C uses D, B` where `D uses B` linearizes *cleanly* in Python's
   orientation and **must be NML2077 here** (C's clause puts `B` above
   `D`; `D`'s own clause puts `D` above its base `B` — contradictory).
   **NML2077 fires whenever no linearization exists that preserves
   every clause's declared order** — that one-clause case, and the
   sibling-subtree case where two listed refs' own stacks order a
   shared pair oppositely (see *Alternatives* for why plain DFS
   postorder was rejected). The fix is always local and deterministic:
   reorder or drop the contradicting ref. A consistent diamond (`C uses
   B, D`, both building on `A`) composes `A` once at the bottom — no
   spurious `#append` conflicts from shared ancestry. Stack order is
   load-bearing for security (it decides which assignment holds a seal
   and which draws NML2060), which is why an inconsistent order is an
   error and never a heuristic. **Stack-level authorization runs here**,
   on the linearized result, because only now do its subjects exist:
   the root grant's `allowRefs`/`denyRefs` bound **every composed
   layer** — every element of the stack *except the declaring instance
   itself* (a file needs no permission to be itself; same-file refs ARE
   composed layers and do match the root grant — see *Same-trust-domain
   composition* for the operator consequence) — and the depth limits
   count **distinct instances in the linearized stack, the declaring
   instance included**: the Summary's `base ⊕ overlay` is depth 2,
   vendor → product → tenant → local is depth 4, and one counting rule
   serves both `maxStackDepth` and the language cap 16 (NML2066; C3
   over ≤16 layers is trivially cheap). Cycles are NML2061; every layer
   must declare the same model keyword (NML2062).
3. **Normalize each layer**, in the shipped pipeline's order — RFC 0013
   array-reference inlining against **that layer's own document** first
   (array refs are file-local: `Document::array_body` reads its own
   file's declarations), then positional/identity materialization, then
   `.shared:` merge. That is `from_document_defaulted`'s order
   (`inline_array_references → apply_positional →
   apply_shared_properties`), and both orderings within it are
   load-bearing: inlining must precede positional materialization so
   that items arriving from an array declaration get bodies before the
   shared merge (a bodiless scalar item deliberately ignores shared
   properties — inverting the order silently drops an array
   declaration's `.shared:` values); materializing before the shared
   merge is what lets an item's own token beat a list-wide shared
   property (RFC 0005 §10). Normalization also erases **spelling
   variance the policies must not see**: a `Property` carrying an array
   literal on a list/set-typed field (`denyHosts = ["a", "b"]`) and a
   modifier's inline form (`|deny = [@x]`) both rewrite to the block
   spelling's real AST shape — a `NestedBlock { name, body }` holding
   the `ListItem` sequence (the shape `inline_array_references` already
   synthesizes for array refs), preserving any existing type annotation
   and synthesizing none. The element mapping is load-bearing:
   `Value::Role` → `ListItemKind::Role`, `Value::Reference` →
   `ListItemKind::Reference`, every other value → a bodiless
   `Shorthand` — mapping roles or references to `Shorthand` would make
   `|deny = [@ops]` and `|deny:` + `- @ops` a cross-kind pair at an
   equal token (NML2063), the exact inverse of the spelling invariance
   this step exists to provide. List policies (`#identity`, `#append`)
   then apply to the *field*, post-normalization, regardless of how a
   layer spelled the value; a modifier's type-annotation form is inert
   and passes through like `FieldDefinition`. Oneof-typed bodies need
   the **discriminator pre-pass**: the effective arm at each oneof
   position (root and nested) is computed *before* per-layer
   materialization, by folding the layers' authored discriminator
   properties bottom-up from the schema default over the
   array-ref-inlined bodies (inlining is arm-independent and can itself
   introduce a discriminator, so it runs first) — then each layer
   normalizes against the stack's effective arm at that position, by
   passing the arm model's name where the shipped positionalizer would
   have consulted the layer's own default-filled discriminator and
   materialized items against the wrong arm. Without the pre-pass,
   step 3 would circularly depend on step 4's accumulator (see
   *Variant-typed values*). A layer's `.shared:` is
   consumed by its own normalization and **never applies to items
   another layer adds** — a layer's body means the same thing composed
   or alone; cross-layer defaults belong in the schema (field defaults),
   layer-local defaults in `.shared:`. A list entry that normalizes to
   **zero items** — a `.shared:`-only block, an empty array literal
   (`tags = []` is valid authored NML today), an empty nested block — is
   dead weight in a composing layer: it does not count as supplying the
   list, and because an author may have *meant* it as "clear the base
   list" (which no merge operation expresses), it is always diagnosed
   (NML2079, warning), never silently ignored.
4. **Compose** field-by-field using each field's merge policy from the
   schema (default: overlay). For list-shaped fields, "the upper layer
   supplies the list" means it contributes **at least one item after
   normalization** — a zero-item entry is a no-op for overlay purposes,
   never a wholesale emptying (emptying a list has no spelling, by
   design; see *Deferred*). The composed body maintains one invariant
   the three downstream consumers silently disagree on today (the
   validator tolerates duplicate entries, `diff` last-wins them, serde
   **errors** on them): **exactly one entry per field name — replace in
   place at the base entry's position, never append-and-shadow** — and it
   is built with `Body::with_entries`, never `Body::fresh`, **with the
   establishing layer's body as the receiver** — the lowest layer that
   supplies the value, or the switching layer after a legal variant
   switch. The attractive "later wins" reading of the receiver is the
   wrong one: `with_entries` copies the *receiver's* type annotation,
   and the RFC 0015 rule fixes the variant at the establishing layer
   (dropping or mis-sourcing it changes which union variant every
   downstream consumer resolves; see *Variant-typed values* — including
   the one case where the engine must *synthesize* an annotation no
   layer authored). Every composed entry records its origin in the
   `ProvenanceTable`.
5. **Validate** the resolved instance against the model (existing
   `SchemaValidator` — its code unchanged; which passes fire does change:
   normalization consumed `SharedProperty` entries, so the validator's
   pre-merge shared-property union checks never fire on the resolved form.
   That coverage is not lost — each layer file is still checked standalone
   under its own binding, where those passes run; the resolved instance
   validates the normalized, strictly more concrete form). Required-field
   errors fire on the **resolved** instance, attributed to the top
   declaration's name span; all other resolved-validation diagnostics
   attribute through the provenance table to the layer file that supplied
   the offending entry.
6. **Deserialize** via `from_body_defaulted` when the embedder asks for
   typed output (defaults apply here, on the resolved body — so schema
   defaults reach layer-added items uniformly).

Unauthorized or unresolved layers never reach compose — fail closed at
steps 1–2.

### Merge policies

Declared on **schema fields** as directives. The language defines four
builtins. These are the language's first **language-interpreted**
directives — a deliberate carve-out from the opaque-directive rule, argued
in *Alternatives* — and that carve-out is structural, not just semantic:
today directive *names* are checked only by the LSP against
manifest-declared vocabularies (the CLI never checks them), and the
builtin meta-package is deliberately excluded from vocabulary coverage —
so the builtins get their own path: interpreted by the compose engine
(where NML2060 fires at check time, CLI and LSP alike), surfaced at the
three vocabulary consumption sites (see *Editor surface* — the
unknown-name check matters most, or `#sealed` itself reports as
unknown), and their four names become reserved: a package manifest
declaring a directive named `sealed`, `identity`, `append`, or `overlay`
is a load error (NML2082) with a rename suggestion — its own code because the fix differs from a reserved type name's.

| Directive | Field shapes | Semantics |
|---|---|---|
| *(none)* / `#overlay` | scalars, objects, lists | Later layer wins on conflict. Scalar assignments replace; nested blocks **always deep-merge**, recursively, per the nested model's own field policies — there is deliberately **no whole-object replacement form** (that is the deferred `#replace`). This closes sealed-field laundering at the object level: a nested `#sealed` field a lower layer fixed can be neither reassigned nor *dropped* by rewriting its parent object. The one principled exception is a variant change — a value whose *type* differs across layers cannot deep-merge — and even there the seal backstop forbids a switch that would discard an assigned `#sealed` field; see *Variant-typed values*. |
| `#sealed` | any | **Write-once from the bottom:** the first (lowest) layer to assign the field fixes it; any assignment in a higher layer or the declaring body is NML2060 — **even an assignment of the identical value** (restatement is the drift hazard: it silently decouples the moment the base changes). In a one-layer stack this reduces to "only the base may set it"; in vendor → product → tenant stacks it lets a template deliberately leave a sealed field open for the next tier to fix. There is no sealing-by-omission — absence is not a write, so a sealed field no layer assigns stays open at every tier (see the JSON Merge Patch rejection for why unset stays inexpressible). |
| `#identity` | lists of identity-bearing items | Merge list items by **identity** — the pair **(item kind, token)**, defined once in *Item identity* below. Matching body-bearing items merge recursively per the item model's own field policies (a `#sealed` field inside the item stays sealed; unstated fields pass through); bodiless items are immutable; items only in lower layers are preserved. An upper-layer item matching **no** base identity is NML2067 unless the field also grants `#append`. |
| `#append` | `[]T`, `set<T>` | Later layers may only **add** items. Under `#append` **alone**, an item matching an existing identity is NML2063; for plain scalar items there is no identity to redefine — append is concatenation, and duplicates are permitted exactly as lists permit them everywhere (for `set<T>`, see below). |
| `#identity #append` | lists of identity-bearing items | The one sanctioned pair, and the flagship multi-tenant shape: an upper item matching a base identity merges per `#identity` (NML2063 does **not** fire); an upper item matching no base identity is added per `#append` (NML2067 does **not** fire). The lone-policy errors belong to the lone policies — **except** a cross-kind match at an equal token, which stays NML2063 under *every* identity-keyed policy: kind is part of the key precisely so a shadow cannot slip in as an "addition". |

**List directives are grants — nothing is implicit.** Each list directive
grants upper layers one specific right: `#identity` grants *override a
named base item*, `#append` grants *add new items*, both together grant
both, and a bare (overlay) list grants *replace wholesale*. An overlay
item that matches no base identity in an `#identity`-only list is a loud
NML2067, never a silent addition. This closes the silent-injection paths
that implicit additions (Kubernetes strategic-merge behavior) leave open:
a typo'd identity gets a did-you-mean instead of becoming a stray
executable step; a base rename breaks overlays at compose, not in
production; and a machine author (an LLM repair loop proposing overlay
patches) cannot inject new items unless the schema explicitly grants
`#append`. Corollary: item names in `#identity` lists are part of the
base's **public contract** — renaming one is a breaking change, exactly
like renaming a field.

**Item identity — the canonical definition.** Identity is the pair
**(item kind, token)**, and this paragraph is the single normative
statement of it. The *kind* is the item's `ListItemKind`, four-valued:
**named** (`- search:`), **scalar-keyed** (`- "/api"` — the RFC 0005
positional key; its body is genuinely optional, and the bodied and
bodiless spellings are the **same kind**, a bodiless one merging as an
empty body — so an upper `- "/api": timeout = 60s` composes config onto
a base's bare `- "/api"`), **reference** (`- SomeRef`), and **role**
(`- @ops/deploy`). The *token* compares by the differ's relation,
`Value::semantic_eq` — `- 8080:` and `- 8080.0:` are one identity, `-
30s:` and `- 30000ms:` are one identity. Rules, in order:

- Identity keyed on list *position* does not exist — positional keying
  is the drift hazard the policy closes.
- A **cross-kind** match at an equal token — a named `- search:` against
  a scalar `- "search"`, a named body at a reference item's token,
  either direction — is NML2063, never a merge: kinds are part of the
  key, and the loud error's fix is to match the base's spelling. So a
  vendor's `- auditStep` reference can never be shadowed by a tenant's
  `- auditStep:` body.
- **Reference and role items are immutable** under `#identity`: bodiless
  by construction, their only legal match is an identical restatement
  (a no-op); anything else is NML2063.
- A **duplicate identity within one layer's list**, under any
  identity-keyed policy, is NML2063 — the merge key must be unique
  before it can be merged on.
- `#identity` requires something to key and something to merge: it is
  rejected at schema load (NML2068) on plain scalar lists (`[]string` —
  value-keyed scalars have no body) and on `set<T>` (set elements *are*
  their values; `#append`-as-union and overlay are the set policies).
- NML2067's did-you-mean discloses **named** identities only —
  scalar-keyed tokens are values (URLs, selectors) and are never echoed
  (normative requirement 4).

NML2068 is the general **merge-policy declaration validator**, and it
also rejects incoherent combinations: `#sealed` composes with none of the
other three on the same field (write-once and grows-upward contradict;
write-once and merge-by-identity contradict — seal the *item fields*, not
the list, when that is the intent), `#identity`/`#append` are list/set
policies and are rejected on scalar and object fields, and `#overlay` is
the explicit spelling of the default and combines with nothing. One
policy per field — with exactly one sanctioned pair, `#identity #append`
(override named items *and* add new ones) — checked where the schema
loads.

**`#sealed` inside items binds only under `#identity`.** In a
bare-overlay list, an upper layer that supplies the list *replaces* it —
base items are dropped wholesale, and the upper layer's items are newly
*authored*, not overridden, so item-level `#sealed` fields never engage.
A schema that seals item fields while leaving the list bare-overlay is
promising a protection it does not deliver; schema load warns (NML2076).
The fix is to grant the list `#identity` (and optionally `#append`),
which is what makes item seals reachable.

**Placement of additions.** Additions always compose at the back: base
items keep their positions, each successive layer's additions follow, and
authored order is kept within a layer. There is deliberately no placement
knob — one deterministic rule, nothing to memorize for multi-layer stacks,
and stable resolved order for `diff_config`. Lists whose order carries
execution semantics put order in the data instead (next paragraph), which
is strictly more expressive than any merge-time placement rule.

**Removal is data too.** *Targeted* removal of an individual base item —
"drop `search`, keep the rest" — has no spelling under **any** policy
(see *Deferred*); bare overlay drops base items only as a side effect of
wholesale replacement, which is precisely why `#identity` and `#append`
exist for lists whose entries must survive. Where tenants legitimately
need to disable a base step, the vendor grants it in data — `enabled
bool = true` on the item model, `#sealed` where disabling must be
forbidden — so a disable is a reviewable field write in the overlay, not
a structural hole in the resolved artifact.

**Order-sensitive lists: order is data.** When list position would carry
execution semantics, the schema makes order explicit on the item:

```nml fragment
model stage:
    name string+
    order number(min = 50) #sealed
    wasm string

model pipeline:
    stages []stage #append
```

The consumer sorts by `order` at load (consumer-interpreted semantics, the
same division of labor as `#live`/`#restart`), and the sort **must be
stable**: NML's resolved order is merge order (base items before
additions), so on equal keys base items keep precedence. This subsumes
every placement need a knob could express, with properties a knob cannot
have: **per-item** position (one addition early *and* one late;
interleaving between base items), **quantified grants** via numeric facets,
and **visible order in the artifact**: a reviewer reads where an item runs
without knowing merge mechanics.

The facet binds *every* layer, the base included — so the grant works as
floor-plus-stable-tie-break: with `min = 50`, the base's mandatory-first
stages sit **at the floor** (`order = 50, 60, …`); an upper layer cannot
type a value below 50 (NML2057, exact-decimal enforcement, RFC 0018), and
an upper layer writing exactly `50` still sorts *after* the base's `50` by
stability. Nothing can schedule before the base's floor stages — the
invariant is the floor and the tie-break together, **plus the seal**.
`#sealed` on `order` is not decoration: under `#append` alone base items
are immutable anyway, but the moment the list also grants `#identity`
(the natural evolution — "tenants may retarget a named stage"), an
unsealed `order` would let an overlay legally push a base stage's order
to `900` — floor respected, mandatory-first gate now running *last*.
Sealing `order` closes that while costing additions nothing: `#sealed`
constrains overriding, never authoring, so an added item still places
itself freely at or above the floor. Two consequences follow: the base
must **assign** `order` explicitly on every item (there is no
sealing-by-omission, and a default is not an assignment — which is why
the field declares no default: a defaulted `#sealed` field draws
NML2076), and re-positioning a base stage becomes a base-file
change, which is exactly where that authority belongs. Order-sensitive
*scalar* lists promote their items to a small model (`name string+`,
`order number`) to carry the field; the positional marker keeps the common
case one line.

One honesty note on the floor: NML2057 checks literals and `const`
references (faceted consts cannot dodge bounds) but not `$ENV`-sourced
values or array-declaration references — normative requirement 5 below
carries the resulting obligation, and the schema should keep
`order`-class fields literal-valued.

**Why identity, not position.** Without `#identity`, list merge is either
whole-list replacement (an overlay copies the entire list to change one
item — the forking problem at list granularity) or positional (item N
overlays item N), which silently misaligns the moment a lower layer
inserts an item: an overlay written against `steps[1] = submit` lands on
a newly inserted `confirmDialog` instead. Identity keys the merge on the
item's identity token, so the override follows the item wherever it
moves — and an override that no longer matches anything is NML2067 at
compose, never a silent misfire. (What each other policy closes is its
table row's semantics — the table is canonical.)

**Policies nest.** An `#identity` item merge recurses per the item model's
own field policies, so granularity composes: `steps []step #identity` with
`action string #sealed` and `locator locator` on the `step` model means an
overlay may re-target a named step's locator but can never change what the
step *does* — the common multi-tenant shape ("their button has a different
label") expressed entirely in the schema. Replace-the-whole-item semantics
were rejected here: they are incompatible with nested `#sealed` (restating
a required sealed field to replace the body would itself violate the seal);
wholesale replacement is the deferred `#replace` policy.

**`#sealed` on a field no layer sets:** if the schema marks it required,
ordinary required-field validation fires on the resolved instance.

**Items added by an upper layer** (under an identity-keyed policy this
requires an `#append` grant; under bare overlay every supplied item is
authored by whichever layer supplied the list): an item introduced by
layer N treats N as that item's lowest layer — field policies (including
`#sealed`) constrain *overriding*, never *authoring*, so the adding
layer may set any field on its own new item. What a newly-authored item
is permitted to *do* is the consumer's capability check, not the merge
engine's concern.

**`#append` on `set<T>`:** composes as union — re-adding an element already
present in a lower layer is a set no-op, not NML2063 (matching set
semantics; only identity-bearing `[]T` items can be *redefined*, and only
that is an error). Set uniqueness itself (NML2030) is enforced at
resolved-instance validation as today, and a duplicate introduced purely
by the union of two layers attributes through the provenance table to the
layer that re-added it.

**Modifiers (`|allow`, `|deny`):** merge with overlay semantics by default;
`#append` (deny lists only grow upward) and `#sealed` apply as declared —
the directive rides the modifier's type annotation, which already carries
directives (`|block []string? #live` parses today). Three implementation
truths the one-liner hides: a modifier value has **three** spellings —
inline (`|deny = [@x]`, the spec's canonical form), block (a
`Vec<ListItem>`, not a `Body`), and type-annotation — and step 3
normalizes the inline form to the same item sequence as the block form
so `#append` on a deny list binds regardless of spelling, while the
type-annotation form is inert pass-through; list-policy compose over
modifier items runs as a parallel arm of the same engine (one semantics,
two walkers); and modifiers are deliberately serde-invisible (the
deserializer drops them — consumers read `BodyEntryKind` directly), so a
merged `|deny` is observed on the resolved AST, which is exactly what
embedders already walk and what `nml resolve` prints. Modifier names themselves are package vocabulary,
not language builtins — the merge policies attach to whatever names the
package declares.

**Variant-typed values (oneof, unions, arms).** Merge is body-directed,
and three NML constructs choose *which type a body even is* from its
content — so variant identity composes before fields do. One governing
rule covers all three before their specifics: **replacement cannot
launder a seal.** Variant-typed compose is the only place an upper layer
may *discard* a lower body rather than merge into it — and a discarded
body's `#sealed` assignments would otherwise vanish without ever being
"reassigned", with the replacement counting as fresh authoring that
seals do not constrain. So: **any variant switch or whole-value
replacement that would discard a lower body containing an assigned
`#sealed` field — at any depth, recursively: a seal buried in a nested
object inside the arm counts, or the laundering vector reopens one
level down — is NML2060**, attributed to the switching layer's
discriminator assignment, `as` annotation, or arm-set entry, with the
**lowest, then first-in-document-order** sealed assignment as the
related span (the message states the count when more than one sealed
field would be discarded). This binds the oneof arm switch,
the union `as` switch, and the arm-set wholesale replacement equally.
NML2076 extends to **oneof**-typed positions for legibility — both a
oneof-typed *field* and a oneof used as an *instance root* (the lint
hangs on the oneof declaration in the root case, since there is no
field): arm models declaring `#sealed` fields warn at schema load
unless the discriminator is sealed. Unions are exempt from the lint —
they have no discriminator to seal, so the warning would be
unsatisfiable by construction; for them the backstop is the only
guard, which is why it is a rule and not a lint. The specifics:

- **`oneof` fields and oneof-rooted instances.** The arm is chosen by the
  discriminator *property*, and the body is a flat bag of discriminator
  plus arm fields. Per-field overlay across an arm switch would be
  silently wrong (base `provider = "google"` fields surviving under
  `provider = "azure"` and validating as unknown properties — or worse,
  passing). Rule, in terms of an **effective arm accumulated bottom-up**:
  the accumulator starts at the schema's `default_discriminator` when one
  is declared (the defaulter injects it only at deserialize time, *after*
  compose — so compose consults the schema, never waits for injection); a
  layer that omits the discriminator inherits the current effective arm
  and its fields deep-merge into it per the arm model's policies; a layer
  that states the discriminator at its current effective value also
  deep-merges; **a layer that states a different value switches the arm
  and replaces the value wholesale** (subject to the governing seal
  backstop) — nothing of the lower arms' bodies survives, and the
  switching layer (plus layers above it) must satisfy the new arm's
  required fields on the resolved instance. The
  accumulator's starting point is what makes the default-arm case safe: a
  base authored against the default arm without restating the
  discriminator still *has* a variant identity, so an upper layer's
  explicit different arm is a switch (base fields dropped), never a merge
  of two arms' vocabularies. A
  schema that must forbid arm switching seals the discriminator —
  `#sealed` on the discriminator field is the natural spelling, no new
  mechanism.
- **RFC 0015 nominal unions.** Variant selection is body-shape-inferred
  unless an `as` annotation pins it — and merging two bodies can change
  the shape, silently flipping the variant. Rule: the **lowest layer
  that supplies the value establishes the variant** (its `as` annotation,
  else its shape), and the resolved body carries that variant as an
  explicit annotation from then on (so the merged shape can never
  re-infer differently — the same reason step 4 names the receiver of
  `Body::with_entries`). When the establishing layer *inferred* its
  variant from shape, its body carries no annotation for `with_entries`
  to copy — the engine **synthesizes** one (name = the resolved
  variant; span = the establishing layer's body span), a deliberate
  authored-by-no-one identifier that exists precisely so the merged
  shape can never re-infer. Layers above merge into the effective variant;
  **only an authored `as` naming a different variant switches** — an
  un-annotated upper body never switches regardless of its shape (a
  mis-shaped merge surfaces as ordinary validation errors on the
  resolved instance, attributed through provenance — loud, not silent).
  A switch replaces wholesale, same as an arm switch, subject to the
  governing seal backstop above.
- **RFC 0007 arm-set fields (`(K -> V)`).** Arm order carries first-match
  semantics, so "additions at the back" would quietly dead-letter an
  overlay's arm behind a base `else`. v1 composes arm-set fields by
  overlay only — a layer that states the field replaces the whole arm
  set (subject to the governing seal backstop: a replacement discarding
  arm bodies with assigned `#sealed` fields is NML2060); per-arm
  identity merge is deferred until a consumer needs it.

Sealing the discriminator remains the oneof spelling for "no switching
at all"; the backstop is the floor beneath it, protecting assigned
seals even where switching is otherwise acceptable-by-design.

**Entry kinds, exhaustively.** A body holds seven entry kinds, and the
engine defines all of them, not just the three the examples show:
`Property` and `NestedBlock` compose per field policy (above); `ListItem`
per list policy; `SharedProperty` is consumed by its own layer's
normalization (step 3) and never crosses layers; `Modifier` composes as
its own arm (above); `Arm` per the arm-set rule (above);
`FieldDefinition` entries are inert in bound instance files (closed
vocabulary already diagnoses them, NML2026) and pass through from the
layer that declares them without merging.

### Authorization: composition is an import capability

Composition pulls another block's full content into the declaring instance.
Where instance files are untrusted input (tenant files under a strict
package binding, RFC 0012; nudge RFC 0030), an unconstrained `uses` clause is a
privilege-escalation surface. This RFC treats import authorization as part
of the design, not an embedder afterthought.

#### Threat model

Actors: **platform operators** (trusted — own vendor bases, catalogs,
capability manifests), **vendors** (semi-trusted — own base instances),
**tenants** (untrusted — author overlay files only), the **runtime**
(trusted — assembles and executes), and **machine authors** such as LLM
repair loops (untrusted — output must be validated and policy-checked like
tenant input).

| Threat | Vector |
|---|---|
| Privilege escalation via import | Tenant writes `uses adminTransferFlow` to inherit destructive content |
| Inherit-then-tweak | Overlay one innocuous field on a powerful base; the resolved instance runs the base's full content |
| Cross-tenant import | importing another tenant's file to compose against its content |
| Sealed-field laundering | Reach a sealed outcome through an unsealed nested field |
| Confused deputy | Runtime binds a weak capability while the resolved instance contains stronger operations |
| Denial of service | Reference cycles, pathological stack depth, huge merges |
| Silent item injection | Typo'd or stale identity (or an LLM-hallucinated name) in an overlay lands as a new executable list item — closed by NML2067: additions require an explicit `#append` grant |
| Information disclosure | Diagnostics or LSP reveal base content an overlay author shouldn't read |

`#sealed` answers *"an upper layer cannot change this field."* It does
**not** answer *"this author may import that base."* Both are required.

#### Layer grants: a property of the binding

The package manifest **already** binds schemas to files by glob
(`[]validator validators`, RFC 0012; nudge RFC 0030). Composition authority is a
property of that same binding — not a parallel policy list with a second
matcher. A binding without a `layers:` grant **denies composition**
(fail-closed by absence); a binding with one grants exactly what it
states:

```nml fragment
[]schema schemas:
    - tape:
        file = "schemas/tape.model.nml"

[]validator validators:
    - tenantFlows:
        files:
            - "tenants/**/*.flow.nml"
        schemas:
            - tape
        strict = true
        // no `layers:` grant → any `uses` clause here is NML2064

    - vendorFlows:
        files:
            - "vendor/**/*.flow.nml"
        schemas:
            - tape
        strict = true
        layers:
            allowRefs:
                - "vendor/**"
            maxStackDepth = 4
```

```rust
/// Attached to a validator binding; a binding without one denies
/// composition (`GrantLookup::NoGrant`).
pub struct LayerGrant {
    /// Target allowlist: globs over the referenced instance's defining
    /// file path (same matcher as the binding's `files`); empty = deny all.
    pub allow_refs: Vec<String>,
    /// Deny wins over allow (NML2065).
    pub deny_refs: Vec<String>,
    /// Maximum distinct instances in one linearized stack, the
    /// declaring instance included (NML2066).
    /// `None` = no grant-level cap; the language hard cap still applies.
    pub max_stack_depth: Option<u32>,
}
```

The grant is ordinary schema on the builtin meta-package
(`crates/nml-validate/assets/package.model.nml`), not a parser special:

```nml fragment
model layerGrant:
    allowRefs []string
    denyRefs []string?
    maxStackDepth number(min = 1)?

model validator:
    name string+
    files []string
    schemas []schema
    strict bool?
    layers layerGrant?
```

Two deliberate absences keep this DRY:

- **No `allowComposition` boolean.** An empty allowlist already means
  "deny all", so a boolean would be a second representation of the same
  state — and once absence-of-grant is the deny, an explicit "no" is
  noise. One source of truth: grant present = what it says; grant absent
  = denied.
- **No separate policy-selection algorithm.** Whichever binding governs a
  file for validation also governs its composition authority — one
  matcher, one selection rule, inherited from the package loader's
  existing (implemented and tested) semantics: first match in declaration
  order, shadowed bindings diagnosed, root-relative paths. A file governed
  by no binding gets the context default: closed universe → denied, open
  context (a developer's own repo, `nml check` without a binding) →
  permissive. Unbound has a **third case** the resolver produces today:
  when two packages both claim a file, resolution warns and falls through
  to unbound. Under grants that ambiguity is not "open" — an
  ambiguously-claimed file is **denied** composition (fail-closed), so a
  crafted second manifest can never launder a file out of its governing
  binding into the permissive default.

**Resolution inputs must not be author-writable.** Binding selection is
only as trustworthy as the inputs that pick the binding, and today two
of them are ordinary workspace files an untrusted author could commit.
Three hardening rules close that class:

- **The workspace root is fixed once per invocation** — from `--root`,
  the editor's workspace, or the **outermost** manifest/`nml-project.nml`
  above the invocation target — never re-derived per content file by a
  nearest-ancestor walk. Otherwise a tenant commits an empty
  `tenants/cu-xyz/nml-project.nml` and re-roots glob matching: their
  file's relative path becomes `member-lookup.flow.nml`, the operator's
  `tenants/**` binding no longer matches, and first-match hands them
  whatever catch-all binding remains.
- **A project config or package manifest that lives inside content
  another manifest's binding claims is content, not configuration** —
  its `autoAssociate`, pins, and bindings are ignored for resolution and
  its presence is diagnosed (NML2080, warning). Otherwise `autoAssociate = false` in a
  one-line tenant-committed `nml-project.nml` detaches the tenant's
  subtree from the operator's binding into the permissive unbound
  default — the exact laundering the ambiguous-claim rule exists to
  stop, reached through a side door.
- **A nearer manifest declaring the same package name as an outer one is
  an ambiguous claim (denied), never a shadow.** Today's resolver
  dedupes known packages by name, nearest first — under grants, that
  "nearest wins" would let a tenant-committed same-named manifest
  replace the operator's binding wholesale, with a grant of the
  tenant's choosing, while producing exactly one claim and so never
  tripping the two-claimant ambiguity rule.

**Grant globs are meta-validated like `files` globs.** The manifest
loader's glob rules — `**` must be a whole segment, segment count capped
at `MAX_PATTERN_SEGMENTS` — apply to `allowRefs` and `denyRefs` at
manifest load, as errors with a stable code (NML2081 — today's `files`
meta-validation is codeless, and a security-load-bearing rule without a
code has no `nml explain` entry by construction). This is not symmetry for its own sake: at match
time an over-cap glob simply matches nothing, which is fail-closed for an
allow rule but **fail-open for a deny rule**. Rejecting the malformed
glob at load closes that asymmetry before it can exist. (Today the
meta-validation loop is hard-coded to `files`; extending it is part of
item 4 in the plan.)

**Depth is bounded everywhere.** Each clause's grant limit bounds the
linearized stack **rooted at that clause** (local reasoning: a vendor's
`maxStackDepth = 4` bounds the vendor's own composition, not every stack
it later participates in). Independently, a **language-level hard cap of
16** backs NML2066 in all contexts, including permissive ones with no
grant and grants that omit `maxStackDepth` — a runaway generator cannot
stack-bomb the checker in a developer repo, the same defensive stance the
glob matcher takes with its segment cap (`MAX_PATTERN_SEGMENTS = 64`) and
the parser with `MAX_DEPTH = 64`. Sixteen is well above any vendor →
product → tenant stack; it exists to bound merge work, not to encode a
product rule.

**Matching is path-based.** Ref rules match against the **defining file
path of the referenced instance**, never its name: instance names are
chosen by content authors and could be crafted to satisfy a name-based
rule, while paths are fixed by repository layout the operator controls
and gates through code ownership. One glob vocabulary covers the subject
(the binding's `files`) and the target (`allowRefs` / `denyRefs`).

**The root grant bounds the stack.** Two grants apply to every composed
layer, by intersection: the **authoring site's** grant governs each
clause's own listed refs (step 1 — local reasoning, mirroring
`maxStackDepth`), and the **root clause's** grant — `allowRefs` *and*
`denyRefs` both — bounds **every composed layer in the linearized
stack** (the declaring instance excepted, per step 2), wherever it
entered. Site-only checking would make any
generously-granted binding a confused deputy: a vendor whose grant
allows `**` writes a one-line wrapper around `admin/transfers.flow.nml`
— and a tenant allowed `vendor/**` composes the wrapper, pulling admin
content into their stack while the tenant's own grant is never
consulted for it. Making only *deny* transitive would still leave that
open for every operator who wrote an apparently-exhaustive allowlist
with no deny (the common spelling — and the one this RFC's own "empty
allowlist means deny all; grant present = what it says" principle
teaches as complete). So the root grant is the statement of **what
content may reach a resolved artifact rooted in that binding's files**,
and it travels with the whole stack: a facade can attenuate nothing and
launder nothing. The cost is honesty in manifests — a grant of
`vendor/catalog/**` whose catalog flows build on `vendor/lib/**` must
say so — and the diagnostic makes the fix obvious. A stack-level
NML2065 has **two denial modes with two message forms**: a
**deny-veto** (an allow rule admitted the path, a `denyRefs` entry
vetoed it — named by index, `denyRefs[2]`) and an **allow-miss** (no
`allowRefs` entry admits the layer — the dominant mode, since "empty
allowlist means deny all" makes absence the default deny, and there is
no rule index to cite: the message says "no allowRefs entry of binding
`<name>` admits this layer"). Both forms name the **entering ref** —
the *root clause's* listed ref through which the denied layer arrived,
whose span lies in the checked file itself (never a span inside a
vendor file the author may not be able to open, and never the vendor's
local binding names). Disclosure of the denied layer's own path
follows from the mode: in a deny-veto the **root grant's** allow
already admits the path, so naming it discloses nothing new; in an
allow-miss the path is never named (a deny must not become an
enumeration channel for the paths it protects — the threat model's
information-disclosure row). "The composer's grant" always means the
**root** grant, never an intermediate layer's.

**Path pipeline (fail-closed).** Its steps are lettered P1–P4 to keep
them distinct from the resolution pipeline's numbered steps:

- **P1.** The authored string must be a quoted relative path — no
  scheme, no leading `/`, no drive prefix (else NML2069; authored path
  strings exist only in `import` declarations — RFC 0020 — and embedder
  catalogs; `uses` refs are bare identifiers, and grant globs are
  meta-validated at manifest load, NML2081).
- **P2.** Canonicalize **through the filesystem**, not lexically:
  resolve the **existing ancestor chain** through the platform realpath
  and collapse the remaining (not-yet-verified) components lexically —
  enough to defeat `..` and symlink games without probing the leaf,
  which matters because existence must not be revealed before the grant
  allows the path (see P3). Re-relativize against the workspace root;
  `/` only (callers map `\` first, same as `glob_match`). A result
  outside the workspace root is rejected (NML2069). Lexical `.`/`..`
  collapse alone is never enough — it cannot see that
  `tenants/cu-xyz/lib` is a symlink to `../../admin`, and a deny
  matched against the authored spelling would never say `admin`.
- **P3.** Match grant globs against that canonical **realpath-derived**
  form, byte-exact (no case folding — a case-insensitive filesystem
  must not widen a rule). **Existence is probed only after P3 allows
  the path** — a disallowed path diagnoses identically whether or not
  the file exists (no existence oracle; RFC 0020, *Paths and
  resolution*).
- **P4.** Closed bindings additionally reject content reached **through
  any symlinked component** — leaf or ancestor — so a link can never
  relocate content into a differently-trusted subtree even transiently
  (NML2083; P2 already maps the target out of the subtree; P4 makes the
  attempt loud instead of quietly re-scoped).

P2 before P3 is load-bearing twice over: a deny of `admin/**` must catch
`admin/../admin/secret.nml` *and* `tenants/cu-xyz/lib/secret.nml` where
`lib` links out — both only after resolution. Grants live only in the
manifest — the trusted binding — so content files can never author
authority.

P2 and P4 are new machinery, and they must live where **both**
front-ends reach them: today the system performs no path canonicalization
at all (`glob_match` splits on `/` verbatim), and symlink policy exists
only at the LSP's filesystem boundary (workspace indexing and
watched-file eligibility) — nothing shared, nothing the CLI runs. The
pipeline lands once, in shared code (`nml-validate`), and both the CLI
and the LSP call it (implementation plan, item 0).

**Same-trust-domain composition is not an escalation.** A block that
`uses` another block the same author wrote (same file or same subtree)
resolves to content the author could have typed inline — composition
grants no authority there, so denying it protects nothing. Leaving tenant
bindings grant-less is a *legibility* choice ("tenant files contain zero
`uses` clauses" is trivially auditable, and platform-assembled stacks make
tenant-side composition unnecessary), not a security requirement.
Operators who want tenant-side DRY add a grant scoped to the tenant's own
subtree (a `layers:` grant whose `allowRefs` is `tenants/cu-xyz/**`) with
no new mechanism.

Note the pre-claim trick this does **not** open: a tenant cannot fix a
sealed field by assigning it in their own same-file base. Seals are
write-once over the **full linearized stack at assembly** — when the
platform composes `vendorBase` under the tenant's blocks, transitive
linearization places the vendor base at the true bottom, its earlier
assignment wins, and the tenant base's assignment is NML2060. There is no
ordering an overlay author controls that puts their content below the
platform's. Cycles and depth abuse within a granted subtree remain caught
mechanically (NML2061, NML2066), and an `import … as` rename is caught
because ref rules check the target's defining file path, not the local
binding name.

#### Normative embedder requirements

1. Untrusted instance files **must** validate under a binding with no
   `layers:` grant unless the operator explicitly grants composition.
2. Runtimes **must** assemble multi-tenant stacks from trusted embedder
   data (a catalog the platform authors), never from an untrusted file's
   own `uses` clause.
3. Embedders executing resolved instances **should** verify the resolved
   content against a separately-bound capability (post-resolve
   attenuation), and **should** record a content hash of the resolved
   instance in their audit trail.
4. Diagnostics shown to overlay authors **should not** echo base-layer
   content beyond the violating span. (**Named** item identities in
   `#identity` lists are deliberately exempt: they are the base's public
   contract — NML2067's did-you-mean discloses names, never values.
   Scalar-keyed identities are *not* exempt: their tokens are values —
   URLs, selectors, internal paths — and the hint never echoes them,
   which also forecloses probing base content by typing near-miss keys.)
5. Embedders whose schemas use numeric facets as authorization floors
   (the order-as-data pattern) **must** validate with resolved-facet
   checking enabled on their trusted lane
   (`SchemaValidator::with_env_resolution`) — the unresolved lane
   deliberately never reads `$ENV`, so a floor on an env-sourced value is
   unchecked without it.

### Diagnostics (stable codes)

| Code | Meaning |
|---|---|
| NML2059 | `uses` ref does not resolve (did-you-mean over in-scope instance names of the same keyword; NML2073 instead when a grant-allowed export of the same name **and keyword** exists outside this file) |
| NML2060 | a `#sealed` field a lower layer already fixed is violated. Three message forms: **differing value** — the standard form; **equal value** (`semantic_eq`) — "already sealed to this same value by `<layer>:<line>`; restating it would silently decouple if the base changes", with a machine-applicable *delete this assignment* suggestion (sole-candidate, `nml fix`-eligible; on a sealed field this form **takes precedence over** NML2084 — one span, one diagnostic, the sealed one); **seal backstop** — a variant switch or whole-value replacement would discard a lower body that assigned the field, attributed to the switching discriminator assignment / `as` annotation / arm-set entry, with the sealed assignment as the `related` span |
| NML2061 | `uses` reference cycle |
| NML2062 | two message forms: `uses` target is a different model keyword (did-you-mean over in-scope same-keyword instances), or `uses` on a schema definition (fix: delete the clause) |
| NML2063 | illegal identity redefinition — three message forms, one per case: redefinition under `#append` without `#identity` ("the schema does not grant overriding; ask for `#identity`"); a cross-kind match at an equal token, any kinds (named vs scalar-keyed vs reference/role, either direction — "match the base's spelling"), including replacing a bodiless reference/role item; a duplicate identity **within one layer's list** ("delete the duplicate") |
| NML2064 | composition not permitted — three message forms: no `layers:` grant on the governing binding (names the binding **and the manifest file that declares it**; not fixable from a content file — an operator change); the file is **ambiguously claimed** (names *both* claiming manifests: "remove or narrow one claim" — the grant may exist and still not govern); or **no binding governs the file in a closed universe** (names the manifest root; fix: add a `files` glob that claims it — an operator change). All end by pointing at `nml binding <file>` |
| NML2065 | `uses` ref denied by the layer grant — two message forms: **deny-veto** names the binding and the vetoing rule **by index** (`denyRefs[2]` — grant rules are unnamed strings; `nml binding` prints the same indices) and may name the path (the allow already admits it); **allow-miss** names the binding and states no `allowRefs` entry admits the layer, never naming the path. Stack-level denials additionally name the entering ref (the root clause's listed ref, span in the checked file) |
| NML2066 | a language or grant bound exceeded — the message names **which** bound and the actual measure: the grant's `maxStackDepth` (an operator change), the language stack cap 16, or the import-closure cap 256 files (both author-side restructures) |
| NML2067 | overlay item matches no base identity in an `#identity` list without `#append` (did-you-mean over the base's **named** identities only — scalar-keyed tokens are values, never echoed) |
| NML2068 | invalid merge-policy declaration (schema load): `#identity` on a list with no mergeable identity — message text: "seal the *item fields*, not the list, when that is the intent"; incoherent combinations (`#sealed` with any other; list policies on non-collections) |
| NML2076 | warning (schema load): seals that cannot engage — a bare-overlay list whose item model declares `#sealed` fields; a **oneof** — as a field type or as an instance root (the lint hangs on the oneof declaration in the root case) — whose arm models declare `#sealed` fields without a sealed discriminator (unions are exempt: they have no discriminator to seal, and the governing backstop is their guard); or `#sealed` on a field **with a schema default** ("a default is not an assignment; this field stays open until some layer writes it") |
| NML2077 | no consistent linearization (C3 merge fails). The message names the contradicting pair and the clause that forces it, in the teaching shape: "`B` is already a base of `D` (declared at `<file>:<line>`); listing it after `D` would place it above `D`". The single-clause case carries a machine-applicable *remove the contradicting ref* suggestion — note redundancy and contradiction are orthogonal: the same ref listed in the dependency-consistent position is redundant but legal, and deliberately silent; only the contradicting position errors. The sibling-subtree case offers the two reorderings as non-auto-applied hints |
| NML2079 | warning: a composing layer's list entry normalizes to zero items (`.shared:`-only block, empty array literal, empty block) — it does not supply the list, and if "empty the base list" was the intent, that operation has no merge spelling |
| NML2080 | warning: a project config or package manifest inside content claimed by another manifest is inert — content, not configuration (see *Resolution inputs must not be author-writable*) |
| NML2081 | manifest load error: malformed grant glob in `allowRefs`/`denyRefs` (`**` must be a whole segment; segment cap) — load-time because an over-cap deny would be fail-open at match time |
| NML2082 | manifest load error: a declared directive name collides with a reserved builtin (`sealed`, `identity`, `append`, `overlay`) — rename suggestion |
| NML2083 | closed binding rejects content reached through a symlinked path component (pipeline P4) |
| NML2084 | warning: an overlay assignment restates the effective lower value unchanged (`semantic_eq`) — a dead delta that will silently decouple when the base changes; the copy-the-whole-body anti-pattern this feature exists to eliminate. Evaluated per field assignment on **scalar and object fields under overlay or `#sealed` policy** — never on `#append`/`#identity` lists, whose per-item semantics differ (a duplicate scalar append is legal by the `#append` row and must not warn) |
| NML2085 | a union-typed position discarded a contribution that can neither merge into the establishment in force nor switch it (errata E2–E4): a whole-value spelling over a body establishment, an un-annotated body over a structural one, or a scalar↔list cross — loud, never silent |
| NML2086 | an internal composition invariant was violated; the layer's contribution is not composed, or composition fell back to a local fold and the plan was ignored — fail safe and loud, never silently wrong (please report the input) |

(NML2069–NML2075 and NML2078 are RFC 0020's; 2076–2077 and 2079–2084 are
this RFC's. 2056 is a retired allocation and stays retired.)

Compose errors attribute to the **assigning span** in the offending layer,
with a `related` span on the assignment that first fixed the field
(NML2060) or the grant rule (NML2065) when helpful. Spans carry no file
identity, so cross-file attribution is explicit: every compose
diagnostic sets `Diagnostic.source` to its span's owning layer path,
and `Related` gains the same field (today both renderers assume
same-file related spans — a breaking-but-necessary change, plan
item 2). **Each diagnostic has one home**: a violation intrinsic to a
sub-stack (exhibited when that sub-root's own file is checked) is
reported in full there, and a taller stack that contains it reports one
**summary diagnostic at its own ref token** — "layer `<name>` does not
compose (NML2067 at `<file>`); fix it there" — primary span in the
checked file, no duplication across files, and no echo of layer content
into files whose authors may not read it (requirement 4).

**Recovery paths are part of the contract.** Every code above follows
the house rule that the diagnostic teaches its own fix (`nml explain`,
did-you-means, machine-applicable suggestions where sole-candidate) —
with two deliberate asymmetries. First, the denial family (NML2064,
NML2065, and RFC 0020's NML2070) is *not fixable by the author who sees
it*. Those
messages therefore always name the governing binding and its manifest
file, state plainly that the change is an operator's, and end by
pointing at `nml binding <file>` — the difference between a wall and a
doorway is telling the user whose door it is. Second, the one-home
**summary** form names the file where the fix lives rather than
teaching it in place — consistent with requirement 4, since the summary
only names a path the root grant already admitted, the same disclosure
logic as the deny-veto form.

### Editor surface

- **Completion** after `uses`: in-scope instance names of the same model
  keyword — in-scope symbols only, **export**-visible, grant-filtered
  (RFC 0020).
- **Go to definition** on a `<LayerRef>`.
- **Code lens** on composed blocks: "Resolved from N layers" → peek the
  resolved body (read-only virtual document).
- **Diagnostics at compose**, not only on resolved output — NML2060
  squiggles the overlay's illegal assignment directly.
- **Override hover** — hovering an overlay's assignment shows the value it
  overrides and the layer that supplied it (from the provenance table);
  the reviewer reads the delta without opening the base.
- **Directive hovers** for `#sealed` / `#identity` / `#append` / `#overlay`.
  These ride a **new builtin-directive path**, not the package vocabulary:
  the builtin meta-package is deliberately excluded from vocabulary
  coverage (its empty vocabulary would turn every operator directive into
  an unknown-name error), so the four builtins are merged into every
  vocabulary outcome at the three consumption sites — the unknown-name
  check, hover, and completion.

### CLI

```
nml check <file>                    # composes `uses` stacks before validation (default on)
nml validate <file>                 # definitions-only, unchanged (no compose)
nml diff a.nml b.nml                # NEW verb; --resolve-layers to diff resolved meaning
nml resolve <file> [--block <name>] # NEW: print the composed instance as canonical NML
                                    #      --provenance: field→layer table; --contract: the vendor view
nml binding <file>                  # NEW: binding, grant, imports
```

Surface honesty: `check` and `validate` exist; `diff`, `resolve`, and
`binding` are **new subcommands**. `nml diff` wraps the existing
`diff_config` library API (which today has no CLI caller) — and
`--resolve-layers` requires the provenance work, because `diff`'s
`Origin` pairs each body with one file and its spans are offsets into
that one file; a composed body diffed without provenance would attribute
every inherited entry to the wrong file with confidence. `check`'s
compose-on-check loads the checked file plus its **`uses`-referenced**
transitive import closure (RFC 0020 — visited-set-terminated, capped at
256 files, NML2066), each file parsed and normalized under its own
binding.

All three new verbs, and compose-on-check itself, are gated on one
refactor named nowhere else: **binding resolution today lives only in the
LSP** (`nml check` has zero package awareness — it never touches the
package module). The resolver core — manifest discovery, pins,
auto-association, first-match binding — moves into `nml-validate` so the
CLI and LSP share one implementation. The "one matcher, one selection
rule" property this design leans on is exactly the property that
duplication would forfeit; the extraction is item 0 of the plan, not an
afterthought.

`nml binding` is the forward direction of NML2065's deny-time explanation —
the `opa eval --explain` of this design. It names the governing binding,
shows why it matched, and prints the effective layer grant (or "no grant —
composition denied"), so manifest authors inspect authority instead of
guessing.

`nml resolve` is the `kustomize build` / `helm template` of this design —
the "show me the final artifact" command that overlay review workflows are
built on. It composes the stack, applies nothing else (no env resolution),
and prints canonical NML via the formatter's AST path (`nml_fmt::format`
walks the semantic AST, so a synthesized body prints with no green tree
behind it — no second printer, no drift against `nml fmt`). The output is
comment-free by construction (comments live in CST trivia the composed
artifact never had) and prints the **normalized spelling** — an inline
`denyHosts = ["a", "b"]` comes back as the block form, because
normalization rewrote it before compose. That is a feature, stated
plainly: the artifact of record has one canonical spelling per shape.
Two consequences: `nml diff --resolve-layers` compares normalized
artifacts (in particular, a normalized `set<T>` no longer hits the
differ's `Value::Array`-matching `SetDelta` path and diffs element-wise
unless the diff adapter re-detects sets from the schema — plan item 5);
and `nml resolve` exits nonzero and prints **no artifact** when any
error-severity diagnostic exists — best-effort bodies serve the LSP's
peek and hover, never the artifact of record. Provenance fills that gap better than comment
salvage would — `nml resolve --provenance` prints a field-path →
layer `file:line` table alongside the artifact, straight from the
`ProvenanceTable`, so a reviewer reads both *what* the resolved instance
says and *which layer* said each line of it.

`nml resolve --contract <file>` faces the **vendor** — the persona every
other surface skips. This RFC states that a base has a public contract
(exported names, the identity tokens of its `#identity` lists, its
sealed surface) and that renaming a named item is a breaking change;
`--contract` is what makes that sentence operational instead of
aspirational. Per exported block it prints: the export name and model
keyword; each identity-merged list's identity tokens; and each
`#sealed` field marked **closed** (assigned here — fixed for every
higher tier) or **open** (deliberately left for the next tier to fix —
the vendor→product→tenant story made visible). All of it derives from
machinery this RFC already builds (schema index, instance index, export
bits, provenance); diffing two `--contract` runs in CI turns "renaming
a named item is a breaking change" from a documentation sentence into a
failing check.

## Embedder guidance (non-normative): dynamic stacks are data

Everything dynamic lives in embedder schemas as ordinary fields — no parser
involvement, no reserved names. Tape's catalog pattern:

```nml fragment
model layerPin:
    path string+               // file-addressed; the file holds one flow instance
    hash string?               // content-address pin, verified at assembly

model catalogEntry:
    name string+
    stack []layerPin           // ordinary data — platform-authored
    capability string          // resolved by the runtime against its capability registry
```

```nml fragment
catalogEntry memberLookupCuXyz:
    stack:
        - "vendor/skylight/member-lookup.flow.nml":
            hash = "sha256:a3f8c2..."
        - "tenants/cu-xyz/member-lookup.flow.nml"
    capability = "core-readonly"
```

(The path rides the `+` positional marker — a scalar-keyed item with an
optional body for the pin; no `path =` spelling needed.)

The runtime substitutes any template parameters, verifies pins, applies its
layer grants, then calls the same `resolve_layers` primitive. One scope
rule keeps the trusted lane honest: catalog assembly addresses layer
*files* by path, but when a catalog-supplied layer carries its own
`uses` clause, those bare refs resolve through **that file's own scope
table** (its imports and same-file names, RFC 0020) — exactly as
`nml check` would resolve them — never through an ambient name lookup.
The catalog picks the files; it does not get a looser name-resolution
mode than the language, or the trusted lane would quietly reinstate the
ambient-global namespace RFC 0020 exists to eliminate. Content
hashes belong here — in-repo `uses` refs live in the closed, validated
universe and need no pins; pins matter when stacks are assembled at runtime
across packages. Pins are **algorithm-prefixed digests over raw file
bytes** (`sha256:` today; the prefix buys algorithm agility, as OCI and
sigstore learned): raw-byte hashing means any change — even
formatting-only — re-pins, which is exactly right for a review-gating pin,
and never ties pin stability to a parser or formatter version. Verify and
parse the **same bytes**: read the layer once, hash it, then hand those
bytes to the parser (no verify-then-reread TOCTOU window). Pins address
**files**, and the convention is one flow instance per file — the right
review granularity anyway; if a consumer ever needs multi-instance files,
a block-selector field on `layerPin` is the additive fix. Tenant selection, per-environment stacks, and A/B routing
are all catalog concerns.

## Examples

The tenant-overlay shape is the Summary example above; these cover the
failure modes. Cross-file examples carry their `import` lines — RFC 0020
requires them, and an example that would not check is worse than none.

### Sealed-field violation

```nml fragment
import memberLookup from "vendor/skylight/member-lookup.flow.nml"

flow hijacked uses memberLookup:
    entrypoint = "adminPanel"    // NML2060 — entrypoint is #sealed at the base
```

### Multi-layer stack (trusted context)

```nml fragment
import memberLookup as vendorBase from "vendor/skylight/member-lookup.flow.nml"
import atriumVariant from "vendor/skylight/atrium.flow.nml"

flow cuXyzMemberLookup uses vendorBase, atriumVariant:
    steps:
        - submitSearch:
            locator = "#search-button"
```

### Policy denial (no grant on the governing binding)

```nml fragment
// tenants/cu-xyz/foo.flow.nml — binding has no `layers:` grant, so even
// a same-file ref is NML2064 (an import line would draw it first)
flow draft:
    entrypoint = "search"

flow mine uses draft    // NML2064 — composition not permitted here
```

## Alternatives considered

- **Other keywords (`using`, `layers`, `extends`/`inherits`, `amends`)
  and a body-property form** — argued in full in *Why `uses`* above; the
  body-property form is additionally the source of the reserved-name
  problem (a field models could never declare) and gives two spellings
  for one semantic.
- **DFS postorder with dedupe instead of C3 linearization** —
  topologically sound but precedence-blind across sibling subtrees: in
  `tenant uses vendorX, productY` where `vendorX uses slowDefaults,
  fastDefaults` and `productY uses fastDefaults, slowDefaults`,
  postorder silently hands the shared pair whichever order the
  *first-listed* sibling declared, inverting `productY`'s own declared
  precedence even though it is listed later and should win. C3 refuses
  (NML2077) instead of guessing — stack order decides who holds a seal,
  so it is never resolved by heuristic.
- **Application-only merge (Tape-owned)** — every embedder duplicates
  diagnostics, LSP, and diff; violates RFC 0012 parity (CLI and editor must
  agree).
- **`#sealed` as an opaque consumer directive** — merge enforcement belongs
  in the compose pass; opaque directives cannot produce NML2060 at the
  assigning span.
- **`#prepend` and placement arguments (`#append(front)`,
  `#identity(front)`)** — rejected in two rounds. A standalone `#prepend`
  puts placement at the *merge site*: for first-match-wins lists, an
  overlay prepending a catch-all entry disables every base entry while
  triggering no `#sealed`, `#identity`, or `#append` diagnostic —
  precedence *taken* by a less-trusted author. A schema-granted placement
  argument fixes the authority problem but is dominated by order-as-data
  (see *Order-sensitive lists* in the design): "append(front)" is
  self-contradictory English; the grant is binary and field-global (cannot
  express one early and one late addition, or interleaving); multi-layer
  front placement needs a memorized precedence rule; and it would be the
  vocabulary's only parameterized directive, adding argument-value
  validation and completion machinery for a strictly weaker capability
  than a faceted `order` field provides with zero new surface.
  Identity-relative placement ("right after `login`, wherever it moves")
  remains the deferred anchored-insertion design.
- **Implicit additions under `#identity` (Kubernetes strategic-merge
  behavior)** — an unmatched patch item becoming an addition turns typos,
  base renames, and LLM-hallucinated names into silently injected
  executable items. NML fails closed instead (NML2067); additions are a
  separate, explicit `#append` grant.
- **CUE-style commutative unification** — elegant (order-independent,
  conflicts are always errors) but the wrong primitive for overlays:
  multi-tenant composition *is* directional — "the tenant's value wins on
  this field" is the product requirement — and expressing overrides in a
  commutative lattice requires CUE's default/disjunction machinery, which
  trades one merge engine for a subtler one. NML keeps directional
  compose and puts the safety in schema grants; `#append`'s
  conflicts-are-errors is the unification insight applied exactly where
  it fits.
- **Value-site precedence markers (Nickel merge priorities, Jsonnet
  `+:`)** — whether a field merges or replaces is decided by the overlay
  author at the value site, so the base cannot constrain it. NML puts
  the policy on the schema field, authored where trust lives, bounded by
  the type system, once for every overlay.
- **JSON Merge Patch (RFC 7386) null-as-delete** — the standard trick for
  expressing removal in an overlay. Structurally impossible in NML:
  `Value` has no null and absence of an entry is the only absence — and
  that is kept deliberately, because a delete spelled as a value is
  invisible to schema policy (`#sealed` could not intercept it). Removal
  stays data (`enabled`) or deferred syntax (`#replace`).
- **Kustomize-style strategic merge patches** — too much syntax for v1;
  `#identity` + `#overlay` cover the common cases with less surface.
- **`const` as a `LayerRef`** — consts are value aliases (`const Port = 8000`);
  treating `const BASE = memberLookup` as an instance pointer conflates two
  namespaces and duplicates `import … as`. Rejected; rename at import.
- **Template/parameterized refs in `uses`** — rejected to preserve static
  resolvability; dynamic assembly is embedder data (see guidance above).

## Compatibility

Pre-1.0: additive syntax. Files without `uses` are unchanged. Models
without merge directives default to overlay. No **field** name is
reserved. Four **directive** names (`sealed`, `identity`, `append`,
`overlay`) become language-reserved: a package manifest that already
declares one of them as an opaque directive fails to load (NML2082) with
a rename suggestion — loud and immediate, the pre-1.0 policy for the
rare collision, rather than silently reinterpreting an existing
vocabulary.

**Changing a merge policy after instances exist is a fleet-wide semantic
change**, and the asymmetry deserves stating: *tightening* (adding
`#sealed` or `#append`) is loud — existing overlays that violate the
new policy draw NML2060/2063 at next compose — but *loosening* (bare
list → `#identity`) changes every existing overlay's resolved meaning
(replace becomes merge) with **zero file edits**, and its diagnostics
are only partial: NML2067 fires where an overlay item now matches no
base identity, and NML2084 fires where items copied verbatim for the
old whole-list replacement now merge as dead deltas — but an overlay
whose items all match and differ recomposes silently into a new
meaning. So the rule is procedural, and the tools ship in this RFC:
re-resolve the fleet and run `nml diff --resolve-layers` across the
policy change before shipping it.

## Documentation

When implemented, update:

- `spec/models.md` — `uses` clause, merge-policy directives, compose
  semantics
- `spec/syntax.md` — header-clause grammar, `<LayerRef>` forms
- `docs/language-guide.md` — new "Compose instances with `uses`" chapter
- `docs/tutorial/` — extend chapter 4 (compose and reuse)
- `docs/guides/` — `resolve-layers.md` cookbook recipe **plus its paired
  runnable binary** `docs/guides/examples/cookbook/examples/resolve_layers.rs`
  — every guide has one, and CI builds and runs them all; extend
  `directive-vocabulary.md` with the builtin merge directives (a new
  *builtin* category — that page documents only manifest-declared
  vocabulary today); a security guide section on binding layer grants for
  multi-tenant embedders
- `crates/nml-core/assets/error-index.md` — NML2059–NML2068 + 2076–2077
  + 2079–2084 entries; 2069–2075 and 2078 are RFC 0020's.
  (`docs/errors/README.md` is a 7-line stub pointing there; the index is
  embedded in the binaries for `nml explain`, and CI enforces ascending
  `## NML####` order, bidirectional constants↔sections, and fence-census
  rules on it)
- `CHANGELOG.md` — `Unreleased` entry
- **This RFC's fences** — proposed-syntax blocks are tagged `fragment`
  (`docs_test.py` must not execute them); the Summary example is
  retagged `check` and runs as an executed regression test. The
  policy-denial example needs a manifest and cannot run as a
  single-file doc fence; it and the cross-file fences (imports,
  manifest fragments) land as `tests/fixtures/layers/` and
  `tests/fixtures/imports/` files instead, and stay `fragment` here.

## Implementation plan

Grounding (verified against the crates, 2026-08-27): field directives
already exist on both the syntactic AST (`ast::FieldDefinition.directives`)
and the schema model (`model::FieldDef.directives`, doc'd "opaque —
consumers interpret") — the vocabulary arrived with nudge's RFC 0032
(`#live`/`#restart`; note nudge and nml share a 4-digit RFC space, so
qualify cross-repo citations) — so merge policies need no new AST;
`BlockDecl.extends` is the exact parser precedent for a header clause
(`uses` is a sibling of `is`, parsed by a one-token `at_kw("is")` test);
all four `ListItemKind`s carry identity (`Named`, scalar-keyed
`Shorthand` — whose body is `Option<Body>`, both spellings one kind —
`Reference`, and `Role`); empty bodies and even the trailing colon are already optional
(the formatter relies on it for `is`-only headers), so pure stack
assembly parses today; binding selection (first match, shadow-warning,
root-relative globs) is implemented and tested in nml-validate;
`SymbolTable` already enforces per-file declaration uniqueness (NML1000);
the reserved-name precedent is `RESERVED_TYPE_CONSTRUCTORS`
(`set`/`map`, rename suggestion) in the schema loader; diagnostic codes
NML2059–2084 are free (highest allocated is 2058; 2056 is a retired gap
that stays retired). The genuinely new structures are the **instance
index** (item 2), the **provenance table** (item 2), and the shared
**binding-resolution core** (item 0).

0. **Shared resolution core** — extract binding resolution (manifest
   discovery, pins, auto-association, first-match `binding_for`) from
   `nml-lsp::PackageResolver` into `nml-validate` so CLI and LSP share
   one implementation; introduce the canonical workspace-relative source
   naming and migrate the four `load_schema` caller conventions
   (basename, absolute path, logical name) onto it; land the path
   pipeline (canonicalize, workspace containment, symlink rejection)
   there. This includes defining **workspace root** in shared code — the
   concept is LSP-only today (client-supplied roots, or a
   nearest-ancestor walk; the CLI has no root notion at all) — per the
   once-per-invocation, outermost-ancestor rule in *Resolution inputs
   must not be author-writable*. Canonical `InstanceId` paths are
   meaningless without it.
1. **AST / parser** — `uses` header clause (contextual, like `is`);
   `LayerRef` is a bare identifier. Mechanics: new `SyntaxKind::Uses`
   inserted **before** `Error` (the `repr(u16)` transmute bound depends
   on it) and named in the total `describe()` match; `uses_clause()`
   between the two `reject_decl_annotation()` calls so `X uses Y as Z:`
   diagnoses correctly; typed-CST accessor with the positional `.skip(1)`
   trick `Extends` uses; the three `BlockDecl` struct-literal test
   fixtures (identity, schema_index, nml-validate schema tests) gain the
   field.
2. **`nml_core::layers`** — the cross-file **instance index** (keyed by
   `InstanceId` = source path + name, resolving to declaring `BlockDecl`
   **and defining `Document`**, with export bits — RFC 0020; new —
   schemas compose across sources today, instances do not),
   `resolve_layers` (taking the declaring `InstanceId` so the primitive
   authorizes itself), `ResolvedInstance` + `ProvenanceTable`, per-layer
   normalization (pipeline order per step 3, including array-literal and
   modifier-inline expansion to `ListItem`s — the spelling-invariance
   step), C3 linearization in NML's reversed orientation with the
   NML2077 consistency check, site and stack grant checks, merge-policy
   dispatch (the modifier `Vec<ListItem>` arm, the four-kind identity
   rules including cross-kind NML2063, and the variant rules with the
   seal backstop and the discriminator pre-pass), language hard cap 16.
   Dry-run-established build notes: the **identity key is new code, not
   a lift** — the differ's `ElemId`/`ElemKey` are two-valued *by
   design*, folding Named/Reference/Role into one `Name` (reusable for
   path rendering, never for matching — widening them in place would
   change diff semantics); `inline_array_references` needs a `pub`
   wrapper (it is body-taking already — exactly the per-layer shape);
   `Related` gains `source: Option<String>` plus `with_related_in`, and
   both renderers (CLI path lookup, LSP same-uri mapping) change —
   `Related` is not `#[non_exhaustive]`, so this is the one breaking
   field add; step 5 and `nml resolve` share a synthetic
   `File { [Declaration::Block] }` wrapper (validator and formatter
   both take `&File`; all parts public and constructible).
   **Refresh strategy: rebuild the
   whole index eagerly on any instance-file change.** The measured scale
   licenses brute force — the entire workspace today holds ~456
   top-level declarations across 185 instance files (the busiest app
   directory has nine), far under the LSP's existing 10,000-file eager
   text index — and eager rebuild sidesteps needing a reverse-dependency
   graph that does not exist (see item 7). The thousand-tenant-file
   fleet the design anticipates lives behind **catalog assembly**, which
   composes per-stack from named files and needs no workspace-wide
   index at all; the eager rebuild serves the editor/CLI workspace
   scale. Revisit only if a real fleet measures otherwise.
3. **Schema extraction** — merge-policy directives on `FieldDef`;
   NML2068 (policy-declaration validation, incoherent combinations
   included) and all three NML2076 seal-reachability lints (bare-overlay
   lists, oneofs — field-typed and instance-rooted alike —
   sealed-with-default) at schema load.
4. **`nml-validate`** — `layerGrant` on the builtin `package.model.nml`
   `validator` model with a new nested-block grant extractor (the
   meta-schema addition is load-bearing: extraction ignores unknown keys,
   so strict meta-validation is what catches typos); grant-glob
   meta-validation (same `**`/segment-cap rules as `files`, as load
   errors — emitted through `PackageError::Manifest { errors:
   Vec<Diagnostic> }`, since the variant the analogous `files` checks
   use, `Inconsistent`, carries no code field and NML2081/2082 need
   one); deny on grant absence and on ambiguous claims; `layers:
   Option<LayerGrant>` as a plain field add on the public
   `ValidatorBinding` (which derives `Eq`, so `LayerGrant` must too) —
   the thing that actually drops the binding today is the *LSP's*
   `Resolution::Bound` payload, which keeps only a binding name and an
   `Arc<SchemaValidator>` and moves to nml-validate under item 0
   anyway, gaining the grant and manifest path `GrantLookup` needs;
   builtin merge-directive names reserved at manifest load (NML2082,
   rename suggestion on collision, the `RESERVED_TYPE_CONSTRUCTORS`
   shape). Manifest `content_hash` already frames the full text, so
   grant edits re-key every cache — no new invalidation.
5. **CLI** — compose-on-check over the `uses`-referenced import
   closure; the `resolve` subcommand (compose + `nml_fmt::format`,
   `--provenance` table, `--contract` vendor view); the `binding`
   subcommand (governing-binding and grant explanation, rule indices
   matching NML2065/NML2070 messages, both claimants in the
   ambiguous-claim case); `diff` as a new verb over `diff_config` with
   `--resolve-layers` provenance-aware attribution and set-aware
   element pairing: normalization moves a `set<T>` out of the
   `Property{Value::Array}` shape `SetDelta` matches on, so the
   adapter pairs normalized `ListItem`s against `is_set` field types.
6. **Formatter** — `nml-fmt` re-emits from the semantic AST, so an
   unemitted clause is *silently deleted* by `nml fmt`: emit `uses`
   (mirroring the `is` no-colon empty-body rule) with round-trip tests,
   in the same change that adds the parser production — never later.
7. **LSP** — completion (grant-filtered), go-to-def, compose diagnostics,
   resolved peek, override hover; builtin-directive merge at the three
   vocabulary consumption sites (unknown-name check, hover, completion —
   the builtin package is excluded from vocabulary coverage by design, so
   this is a new path, not a manifest entry). **Invalidation is the item
   this list would otherwise hide**: the diagnostics cache is keyed
   `(text, generation)` and the LSP has *no* cross-file fan-out — a base
   instance edit clears only the base's own cache entry, so an open
   overlay would keep serving stale compose diagnostics until refocused.
   The manifest `content_hash` re-keying covers *grant* edits only, not
   this new base→overlay dependency. Fix: any instance-file change that
   touches the index bumps the resolver generation (the epoch already in
   the cache key), invalidating every cached entry — coarse, correct,
   already wired, and cheap at the measured scale (item 2's eager
   rebuild).
8. **Fuzzing** — a `layers` fuzz target following the `validate.rs`
   doctrine (that target exists precisely because "the largest logic
   surface had no fuzzing"): never panic on arbitrary input; span
   integrity for every diagnostic and suggestion (`check_spans` is
   copy-ready); the compose analogue of format idempotence —
   `resolve_layers` output formatted, re-parsed, and re-composed is a
   fixed point; bounded work under adversarial stacks (cycles, the hard
   cap 16); and the closed-set error-message guard where grant denials
   echo paths. The path pipeline (`..` collapse, workspace containment)
   is its own classic target. New `[[bin]]` entries in `fuzz/Cargo.toml`.
9. **Fixtures** — `tests/fixtures/layers/` happy paths plus one file per
   diagnostic code and per rule with teeth: grant-denial (site and
   stack-level, including the facade case), ambiguous-claim, both
   NML2077 contradiction classes (including the machine-applicable
   redundant-ref fix), the seal backstop on all three variant forms,
   spelling normalization (`denyHosts = [...]` and `|deny = []` under
   `#append`), cross-kind identity NML2063 (all three message cases),
   zero-item NML2079, the array-declaration `.shared:`-through-compose
   ordering case, the equal-value NML2060 deletion fix, restatement
   NML2084, all three NML2076 lints, and the manifest-side codes
   (NML2080–NML2083).

See RFC 0020 for import/export scope, additional diagnostics (NML2069–
NML2075 and NML2078), and fixtures under `tests/fixtures/imports/`.

## Deferred

- **Cross-layer value interpolation** — referencing an inherited field
  inside an overlaying expression (`other = "{{myvar}}-suffix"`). Opens
  evaluation-order and cycle questions; `const` covers the shared-value
  case statically today for same-file layers (both reference the same
  constant — consts are file-scoped; see *Layer references*).
- **A known non-win to be honest about:** the heaviest real duplication
  in the workspace today is *prompt-template* duplication — near-twin
  workflow files each carrying four large `template` blocks. v1 dedupes
  neither: templates are not instances (no `uses`) and RFC 0020 v1
  imports instances only. Const/template imports (RFC 0020, *Deferred*) are what unlocks it; this RFC's machinery
  is deliberately not stretched to cover strings.
- **Cross-model fragments** — a generic "values bundle" composable into
  instances of different models. The same-keyword constraint (NML2062)
  deliberately blocks this; revisit only with a concrete consumer.
- **Strategic merge patches** (path-addressed `patch:` blocks) — power
  users; `#identity` may be enough for years.
- **`#replace` policy and item removal** — "tenant removes a base step"
  needs explicit, auditable syntax; no consumer yet (the `enabled`-flag
  pattern covers disabling in data today). Note the stronger fact: unset
  is inexpressible as a merge operation, not merely deferred (see the
  JSON Merge Patch rejection for the structural argument); an *authored*
  empty list exists (`tags = []`) but as an overlay it is a zero-item
  entry — a no-op plus NML2079, never an emptying. Any future removal
  syntax must be a marker the schema can police, never a value.
- **Identity aliases for renames** — a vendor renaming a named item
  currently breaks overlays loudly (NML2067, the best available behavior);
  an alias/migration mechanism ("was `submitSearch`") is coherent but
  needs fleet-scale usage to justify.
- **Layer refs across schema-package boundaries** — needs cross-package
  import paths; pins and provenance live in embedder data until then (RFC
  0020 covers in-repo imports only in v1).
- **Signed stack attestations** — embedder concern (catalog signatures);
  revisit if a language-level hook proves necessary.
- **`nml fmt` canonicalization** of `uses` ref order — not in v1.

## Security notes

`#sealed` guards field integrity; binding layer grants guard import
authority; post-resolve capability attenuation (embedder) guards
execution. All three are required in multi-tenant deployments — no single
mechanism is sufficient. Secrets remain `secret`-typed references resolved
through `ValueResolver`: compose (pipeline steps 1–4) and validation
(step 5) operate on **unresolved** bodies — `$ENV` and secret resolution
happen only inside step 6's deserialize, exactly where
`from_body_defaulted` runs the resolver today, and the validator's
unresolved lane is engineered to never read `$ENV` — so layer compose
merges reference shapes and never touches secret material. The
provenance table is part of the security story, not just ergonomics: it
is what lets an audit trail say *which trust tier wrote each effective
field* of the artifact that ran.

## Errata (implementation-discovered, 2026-08-29)

Composition of union-typed positions (RFC 0015 meets this RFC) was
built and reviewed to fixpoint after this text was sealed. The engine
is the authority for the following refinements; each is pinned by a
test named in `crates/nml-core/src/layers.rs`:

- **E1 — codes.** NML2085 (discarded union contribution) and NML2086
  (internal composition invariant) join the stable-codes table (rows
  appended below).
- **E2 — whole values over a named establishment.** A scalar/array
  spelling above a named variant can neither merge nor switch (`as`
  has no scalar spelling): it is discarded LOUDLY as NML2085, not
  left to "ordinary validation errors" — no merge exists to validate.
- **E3 — un-annotated bodies over a structural establishment.** The
  dual case (a keyed body above a scalar/list value) is likewise
  NML2085.
- **E4 — per-shape structural establishment.** A scalar value and a
  list value are distinct establishments; a scalar↔list cross is
  NML2085 (one collapsed bucket made the winner depend on spelling).
  Scalar variants of different scalar types overlay like any scalar.
- **E5 — ambiguity carve-out.** Where RFC 0015's D2 oracle refuses to
  pick a variant, composition refuses too: an ambiguous lowest body
  composes model-less and un-annotated (no synthesized annotation), so
  NML2052 fires on the composed view exactly as on the raw one.
- **E6 — pin.** An authored `as` above an ambiguous group RESOLVES it
  (the group joins under that variant and the pinning layer's
  identifier becomes the annotation); it is not a switch and is not
  seal-judged — nothing was displaced. At a `#sealed` position the pin
  is a second assignment and the seal rejects it.
- **E7 — zero-item entries.** At a union position that admits items
  (one with a list or set variant — a set variant is reachable by array
  literal only, never by block shape), `= []`, an empty block and a
  `.shared`-only block never supply and never establish (NML2079's
  contract); a keyed or annotated block is a model body and a write,
  seal included; a position only zero-item entries supply survives as
  `= []`. Block-shaped items resolve, normalize and are seal-judged
  under the FIRST `List` variant — the resolver's own selection — never
  under a set variant that happens to precede it (a set-only union with
  block items is raw-invalid, NML2032; its items are neither judged nor
  promised by NML2076 — the POSITION seal still applies: at a `#sealed`
  set-only union the item-bearing block is a write, and a switch above
  it is a second assignment, NML2060).
- **E8 — bogus `as` in a dependent layer.** Reported as NML2051 by the
  merge (the composed view replaces the annotation before validation)
  and treated as un-annotated; a list variant's element name gets the
  honest form (list variants are selected by shape, never named).
- **E9 — list-establishment switches.** A switch away from a
  list-value establishment is judged over the list the displaced
  compose would carry — the bare-list winner — under the union's list
  variant, with list-level `.shared` writes distributed and each item's
  identity token materialized.
- **E10 — NML2068 union-element form.** `#identity` on a
  union-element list is rejected with its own wording (item identity
  across variants is not yet defined).
- **E11 — NML2076 arm 1** covers union elements and a union's own list
  variant (with honest advice: `#identity` is not grantable at a union
  list position). The "unions are exempt" sentence above concerns arm 2
  (the discriminator lint) only.
- **E12 — type-annotation modifiers in instance bodies** are
  declarations, not values: inert like `FieldDefinition` entries, they
  neither compose as values nor seal, satisfy no required field, and
  pass through ahead of the composed value — the composed view's
  CANONICAL order (declare, then assign), whatever order the author
  wrote; `nml fmt` stays order-preserving on source — so the validator
  still checks them on the composed view. The last declaration of a
  field wins, within a layer and across them.
- **E13 — nothing under a `#sealed` position is planned.** Write-once
  is judged by `seal_write` alone and the merge never consults the plan
  there; planning it handed normalization a REJECTED upper layer's
  variant for the surviving lowest body (its own findings vanished, its
  body normalized under a foreign vocabulary). Likewise list ITEMS are
  their own scope at every position: block-shaped items at a union
  position normalize under the first `List` variant's element model,
  under a bracketed item path (`slot[w]`) the plan never writes — so a
  nested union inside a discarded item can never read the winning
  variant's plan for a same-named field. Items resolve their vocabulary
  PER ITEM from the element type (a model directly; a oneof under the
  arm the item states, else the default; a union under the variant the
  item's annotation or shape selects, none when the D2 oracle calls it
  ambiguous) — the Positionalizer's reading — so model, oneof and union
  elements are peers on every pass, and the block-modifier spelling
  (`|steps:` with item bodies) normalizes like the block spelling.
- **E14 — one name-resolution order.** A model before a `oneof` of the
  same name (`SchemaIndex::nameable`, the validator's order) on EVERY
  pass — the plan, normalization, the positionalizer, the merge and the
  seal scans, at the root and at every nested position. A colliding
  name is NML1000/NML2016 at load, but composition still runs over the
  loaded schema; two orders once planned a position under one reading
  and merged it under the other.
- **E15 — the head rule** (RFC 0023 Part B). The receiver rule extends
  from the body to the composed ENTRY: a composed entry carries the
  span, the name identifier and the provenance row of the head of its
  surviving group — the base when nothing switched, the switching layer
  after an accepted switch, unchanged by a pin or a rejection — at
  every route (the entry keeps the base SLOT). Two switching
  dependents' findings keep two homes under the one-home dedup key,
  and NML2085's item-scope `established here` note follows the
  establishing item.
- **E16 — non-string discriminators** (RFC 0023 Part C). Stripping is
  by NAME (the validator's own reading), selection stays string-valued:
  the surviving group's non-string discriminator entries pass through
  the composed view after the canonical entry (first, when none
  exists), each drawing NML2042 at its author's span; at the NML2054
  shape the union field never sees a non-string entry (NML2042 replaces
  NML2085); NML2084 no longer fires on a non-string restatement. The
  one-entry-per-field invariant reads: one VALUE entry per field name;
  validator-facing passthroughs sit beside it, and every passthrough of
  the non-string kind is accompanied by an error-severity finding — no
  artifact of record may be derived from a composed body while an
  error-severity finding exists. (One carve-out an artifact gate must
  honor: validator DEPTH TRUNCATION, NML2044, is a warning that stops
  checking before the discriminator does — unreachable through the
  parser, whose nesting fence errors first, but reachable from a
  constructed AST — so a gate treats NML2044 as blocking too.)
- **E17 — NML2060 counts fields** (RFC 0023 Part D). The backstop
  message counts DISTINCT SEALED FIELDS (`(and N more field[s])`), with
  the assignment count when it exceeds the fields (`(M assignments)`);
  one `sealed here` note per distinct assignment (the first four), the
  first the lowest-then-document hit; notes carry `Related.source` and
  every renderer locates a note in its OWN file.
- **E19 — status and fences.** This RFC's status is *Slice 1
  implemented*; the instruction to retag the policy-denial example was
  withdrawn (it needs a manifest and cannot run single-file).
