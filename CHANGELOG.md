# Changelog

## [0.1.0] - Unreleased

### Added

- **In-string machine repairs for raw policy characters
  (NML0017/NML0018)** — a control or invisible character inside a
  string literal now carries its value-preserving repair(s), the way
  NML0016's in-string CR already carried `\r`. Where one reading is
  provable the fix is singular and `nml fix` auto-applies it (a C0 or
  unmapped-C1 byte becomes its escape); where intent is genuinely
  ambiguous the alternatives are enumerated and NEVER auto-applied —
  the editor offers each as its own action, and the CLI counts the
  finding as not auto-fixable: NEL offers `\n` | `\u{85}` | `…` (the
  Windows-1252 double-decode reading — NEL's message hint teaches all
  three, mirroring its repair arms), the other 26 CP-1252-mapped C1 bytes
  offer escape | mojibake repair, LS/PS offer `\n` | escape, and bidi
  controls, interior BOMs, and tag characters offer remove | escape —
  with the remove arm itself judged: it is offered only where deletion
  provably removes just that character (a sentinel-marked decode
  comparison, plus a relex proving the deletion leaves the string ONE
  clean token — a FEFF holding two quote runs apart, or sitting
  between a CR and an LF, has no sound removal), and where removal is
  refused the set collapses to the singular escape fix, which then
  auto-applies. (`ParseErrorKind::InvisibleCharacter` gains a
  `remove_sound` field — technically breaking for external code
  matching that variant with named fields; `..` patterns are
  unaffected.)
  `nml fix` also converges capped floods in one run: when more
  same-message findings exist than the 128-diagnostic bound renders,
  the convergence gate now charges instances that re-surface from
  behind the cap against the truncation marker's exact reported count
  (previously such a round was read as failed and the run crawled
  one fix per round into its budget); the fix summary's
  "not auto-fixable" count no longer includes advisory info rows, and
  when findings were suppressed past the diagnostic limit the summary
  discloses them beside the count ("(N more suppressed past the
  diagnostic limit)") — hidden findings are unknowns, never folded
  into the classified count.
  Token-position characters still carry no repair (any rewrite is a
  structural guess), and every in-string repair is gated on decode
  itself: the escape is offered only where splicing it leaves the
  decoded value byte-identical, so a character in a multiline string's
  blank edge line, in a blank line whose blankness holds min-indent up,
  in the opening line's dropped padding, or glued to a preceding
  backslash refuses — the exact geometries where an applied escape
  silently changed the value. The judgment itself is bounded: a string
  token past 64 KiB is never decode-judged, so its policy diagnostics
  stand with no machine repair (fail-closed; hostile-only in practice —
  a PEM-sized blob is ~4 KB). Under the hood the fix-vs-did-you-mean textual
  heuristic is gone: each error kind now declares its repair class
  (`Repairs` — none, did-you-mean, singular fix, or alternatives), and
  diagnostic messages, repair text, and formatter output all share one
  escape-spelling function, so the advised and emitted spellings can
  never drift.

- **RFC 0019: instance layer composition (`uses`) and sealed fields** —
  slice 1 (the language kernel). An instance block may declare
  `uses <ref>, …` in its header; the stack linearizes by C3 (in NML's
  reversed orientation — an inconsistent order is NML2077, never a
  heuristic) and composes bottom-up under schema-declared merge
  policies: `#sealed` (write-once from the bottom, equal-value
  restatement included, with the seal backstop binding all three
  variant forms equally: oneof arm switches, union `as` switches
  (RFC 0015 — lowest supplying layer establishes the variant, authored
  or shape-inferred with a synthesized annotation on the resolved body;
  un-annotated upper bodies never switch), and arm-set wholesale
  replacement, judged through the one decision-trace fold),
  `#identity` (merge list items by the four-kind identity pair),
  `#append` (additions only), and the sanctioned `#identity #append`
  pair. `nml check` composes same-file stacks before validation
  (default on; overlays validate their *resolved* body) under the open
  developer context; binding-governed layer grants land with the shared
  resolver core. Schema load now validates policy declarations
  (NML2068) and warns on seals that cannot engage (NML2076, three
  shapes). New diagnostics NML2059–NML2068, NML2076–NML2077, NML2079,
  NML2084, each with an `nml explain` entry; `nml fmt` emits the new
  clause with round-trip coverage. `nml validate` covers `uses` clause
  refs under its unresolved-references contract (same NML2059 wording
  as `check`); compose-blind deserialization of a `uses`-bearing block
  fails closed (the raw body is not the effective config; the
  document-level entry included); and the 16-instance stack cap rejects
  over-wide clauses *before* the linearization merge, in linear time.
  One field is one field across its spellings: a modifier-declared
  field's property/block spellings merge (and seal) together with it.
  `nml fix` composes, so compose-side machine fixes (the equal-value
  NML2060 deletion, NML2077's remove-the-ref) actually apply; `nml
  validate` also owns NML2062's schema-definition form. The editor
  composes too: LSP diagnostics validate the RESOLVED body, so overlays
  show `check`'s findings, not phantom missing-required errors. Arm
  decisions have ONE authority — the pre-pass fold (seal backstop
  included, judged over displaced-arm-normalized bodies) records a
  per-layer trace the merge replays positionally, nested positions
  planned over the surviving parent group — and oneof-typed list
  elements merge arm-aware (seal enforcement inside identity items,
  through the modifier spelling too, with `+` tokens materializing
  through the item's arm). Item identity scopes pair by full identity
  (kind + token). Field and item lookups are mapped/bucketed, keeping
  compose near-linear in body width and list length (numeric item keys
  bucket by canonical value, not by type); the editor caps its
  per-keystroke diagnostic stream with a summary row. Object-typed
  fields always deep-merge their nested contributions, so no scalar or
  modifier spelling can discard a sealed nested body (schema-less and
  dangling-target nested groups still deep-merge structurally); the
  plan keys each path by the same first-wins field the merge resolves;
  and the seal backstop judges a write through the one shared predicate
  across every spelling — one coherent first-wins policy per duplicate
  field name everywhere. Diagnostics honor the denial family's full
  recovery contract (binding + manifest + `nml binding <file>` with the
  real path), NML2077 names order ROTATIONS across three or more
  clauses, NML2061 teaches its fix inline, the discovery-depth NML2066
  reports once with a real span, and the editor composes schema-less
  buffers structurally and carries the schema-load lints for mixed
  buffers. Header clauses never continue across newlines (a trailing
  comma errors and consumes nothing — the next declaration survives);
  `.shared` distributes into modifier-spelled item bodies like every
  other spelling; type-annotation modifiers survive composition; the
  block-form empty modifier draws NML2079 like its sibling spellings;
  clause findings dedup across dependent composes; and every
  `resolve_layers` failure carries a diagnostic. The seal backstop's
  "at any depth" contract is total: the scan descends union-typed
  interiors (every variant the group could establish, fail-closed),
  arm-set inline arm bodies, and oneof-TARGETED arm sets (each
  displaced arm judged under its own discriminator's arm model), and
  union list elements route identity-matched item groups through the
  union authority (seal enforcement, establishment, annotation
  synthesis) instead of merging model-less. Structural (scalar/list)
  union variants are first-class in compose: the lowest supplying
  layer establishes them too, an authored `as` switches away from
  them, and a contribution that can neither merge into the established
  variant nor switch it — a whole-value spelling over a named
  establishment, or an un-annotated body over a structural one — is
  discarded LOUDLY as the new NML2085 rather than silently dropped. A
  bogus `as` on a dependent layer is reported (NML2051) by the merge
  itself, since composition replaces the annotation before the
  validator can see the authored one. All three NML2060 backstop faces
  share one wording owner (position, switch target, count suffix,
  action tail, "sealed here" note), and the bare-overlay
  seals-cannot-engage lint (NML2076) sees union elements too. The
  structural bucket is per-shape: a scalar value and a list value are
  distinct establishments, a scalar↔list cross is a loud NML2085 (one
  bucket let the winner flip with the base's spelling), every
  union-typed group routes through the union authority regardless of
  spelling, and a switch away from a LIST-variant establishment is
  seal-judged over the displaced item bodies under the list variants'
  element models ("a structural group has no seals" is true only for
  scalars). Ambiguity is fail-closed end to end: a keyed body the D2
  oracle refuses composes model-less and un-annotated — composition
  never guesses a variant by source order nor synthesizes an annotation
  that would silence NML2052. The seal scan reaches union-typed LIST
  elements of displaced variants (item paths as `slot[w].field`);
  `nml check` seeds its validator dedup with the composed diagnostics
  (the merge-emitted NML2051 printed twice with a non-`uses` base);
  NML2076 covers a union's list variant with honest advice
  (`#identity` is not grantable there), and NML2068 gets a
  union-element wording. NML2085 leads with the position and its
  establishment and carries an "established here" related note; its
  switch hint never names a first-wins guess. A switch away from a
  list-variant establishment is judged over the displaced LIST — list-
  level `.shared` writes distributed, each item's identity token
  materialized (a positional `+` field is a write), oneof elements under
  the item's own arm, modifier-spelled item blocks included — and the
  judgment is memoized per unchanged group (N rejected switches over M
  sealed items were N full scans, a super-linear DoS axis). Every
  spelling of a union field reaches the union authority (the
  all-modifier short-circuit no longer bypasses it); zero-item entries
  (`= []`, an empty block) at union positions draw NML2079 and never
  establish; an authored `as` above an oracle-ambiguous group PINS it
  rather than switching (nothing was chosen to switch from); ambiguous
  interiors are seal-scanned under every oracle candidate, never the
  resolver's first-wins pick; normalization no longer guesses a
  variant for ambiguous bodies; NML2085 faces are keyed on the
  (establishment, supply) pair the fold recorded, so a discard before a
  later switch reports once; the NML2060 tail is one clause; NML2076
  leads name the shape; NML2051 names a list variant's element honestly
  (no did-you-mean) through one builder the validator and the merge
  share; `nml explain` never rewrites inline code as a link; and the new
  NML2086 names a violated internal composition invariant instead of
  composing silently wrong. The plan and the merge fold ONE supply set
  (every spelling, through one constructor), so a planned union trace
  always aligns and the local refold never judges bodies already
  normalized under the final variant (a fabricated refusal); a
  type-annotation modifier at a union position is a declaration that
  yields to the values (it never routes a group around the authority,
  never seals); a switch away from a list establishment is judged over
  the bare-list WINNER only; `.shared`-only blocks are zero-item entries
  raw and normalized alike, and an all-zero-item union position
  survives as `= []`; arrays keep their spelling at union positions (an
  empty block reads as an empty object downstream); a pin carries the
  pinning layer's own identifier; every believed-unreachable arm in the
  union faces fails LOUD (NML2086) rather than dropping or last-wins;
  NML2085 names an earlier entry in the same layer when that is the
  establishment, and advises resolving an ambiguous lower body; the
  NML2060 count reads `'field' (and N more)`; NML2079 speaks in union
  terms at union positions; and hover summaries of the long index
  entries are their first sentence. A keyed or annotated body at a `#sealed`
  union position that admits items is a WRITE (one zero-item predicate
  — `= []`, an empty modifier, an entry-less un-annotated block — owns
  NML2079, the seal exemption, the modifier overlay's no-op skip and the
  classifier's `Empty`); declarations (type-annotation modifiers) pass
  through beside the composed value on EVERY route and never desync the
  plan (one value predicate shared by the plan's gather and the merge's
  partition); the plan is the authority for union positions (every
  establishment recorded with the supplies' kinds; a misaligned planned
  position is NML2086, never a silent refold); a sealed union position
  still reports a dependent's bogus `as`; the seal scan dedups by hash
  (a wide judgment was O(hits²)); item-scope discards anchor on the
  item; a list establishment's "established here" follows the effective
  list; set and later list variants are unreachable by shape — neither
  judged nor promised by NML2076; a `[]` on a declared scalar modifier
  reaches the composed view (a type error, not a no-op); the validator
  no longer counts a type-annotation modifier as satisfying a required
  field (errata E12); NML2085 says "as an un-annotated body" and
  "resolve the establishing body"; the RFC 0019 code table gained
  NML2085/NML2086 rows; and hover summaries of NML2051/2052/2077/2086
  are their first sentence too. Nothing under a `#sealed` position is
  planned (its surviving body normalizes under its own variant, never a
  rejected upper layer's — errata E13); the plan strips a oneof's
  discriminator entries exactly as the merge does (a union field named
  like the discriminator no longer desynchronizes them); the zero-item
  predicate is total (`= []` included, one owner, typed or untyped);
  set variants count as list-admitting again (reachable by array
  literal); one seal-hit sink owns every dedup (`assigned_seals_into`);
  declarations are gathered as passthrough entries beside the composed
  value (`merge_field` is single-entry again, its dispatch one
  exhaustive match); the composed annotation's source is one replay
  field; a merged identity item keeps the BASE item's span (a
  three-layer chain's notes point at the base); NML2086 says whether
  the contribution was dropped or composed by a local fold; a
  structural establishment's note reads "in force here"; NML2079 names
  the zero-item entry generically; the editor's compose pass is
  guarded (an internal error degrades to raw-text findings plus
  NML2086, never a dark buffer); and a `.shared` line inside a modifier
  block is a parse error instead of a silent drop. The displaced-list
  seal judgment binds to the union's first `List` variant — the one
  block-shaped items resolve to — never to a `set<T>` variant that
  precedes it (a switch could discard sealed items silently); block-
  shaped items at a union position normalize under that variant's
  element model like a plain list's items (their nested zero-item
  entries are warned, their array-spelled lists re-spelled), under a
  bracketed item path the plan never writes; the plan, normalization
  and the merge resolve a name in ONE order (a model before a oneof of
  the same name — a colliding name was planned under one reading and
  merged under the other); the plan hides a oneof's discriminator from
  its supply gather instead of cloning stripped bodies; the ownership
  order of a field group is a table (`route_of`); declarations precede
  their value in the composed view (declare, then assign); the survivor
  rule, the discriminator-entry predicate and the establishing supply
  each have one owner; the seal sink dedups by (path, file, span); the
  invariant debug assertion sits at the compose boundary (the diagnostic
  and the fail-safe composition are observable in every build); the
  editor's guard anchors its NML2086 at the buffer start (a span-less
  finding was dropped before the editor saw it) and guards its fallback
  too; a non-item line in a modifier block names its own kind ("found a
  shared property") at its own position; the validator checks EVERY
  discriminator-named entry (a dependent's `kind = 5` was laundered
  behind the re-added string discriminator); and the provenance of an
  identity-merged item is its base position (its fields keep their own
  writers' rows); and an `nml fix` deletion takes its whole line, never leaving an
  indentation-only husk behind (later superseded by RFC 0023's
  structural resolver, below)
  — `nml explain NML2079` now covers union positions, and its hover
  summary is its first sentence. Then: the ONE resolution order holds
  at the ROOT too (`SchemaIndex::nameable`, model before oneof, total
  matches at every site — the plan and the merge HAD read a colliding
  root oneof-first while normalization, the positionalizer and the
  validator read the model; no longer); list items resolve their normalization
  vocabulary PER ITEM from the element type (`Vocab::Items`), so oneof-
  and union-element items are peers of model-element items (their
  zero-item entries are warned, their arrays re-spelled) and a block
  modifier's items normalize like a nested block's; machine deletions are STRUCTURAL
  (RFC 0023): one resolver in nml-core computes every fix's bytes for
  `nml fix` AND the editor's quick-fix by token walks over the lossless
  tree — entry rows, `uses` clauses, clause references, the colon rule
  on an emptied clause-carrying header — with every refusal
  per-suggestion and printed (`fix refused: …`), the
  structural-injection guard owned in the same place (the editor's
  quick-fix path had none), editor staleness settled by cache
  membership, and `nml fix`'s round gate a multiset decrement over the
  applied diagnostics (a repair that reveals the next finding lands;
  a failed round retries its first applied candidate alone, and the
  round budget scales with the finding count); a deletion targeting a
  row INSIDE a `.shared` block is refused outright — the row is
  distributed into every named item, and the block-form NML2060 fix
  silently stripped a shared default from every sibling; the injection
  guard's refusal set equals the render-escape set (controls, the
  Trojan-Source bidi controls, U+2028/U+2029), one predicate for the
  guard, the message renderer and the CLI's note lines; a
  composed entry carries the span, name and provenance of the HEAD of
  its surviving group (RFC 0019 E15 — the switching layer after an
  accepted switch, the base otherwise), so two switching dependents'
  findings keep two homes instead of collapsing onto the base's under
  the one-home dedup key, and NML2085's item-scope note lands on the
  establishing item; discriminator stripping is by NAME with non-string
  entries passed through beside the canonical one (E16 — each drawing
  NML2042 at its author's span; `kind = 6` no longer overlays
  `kind = 5`, and the NML2054 shape draws NML2042, not NML2085); the
  NML2060 backstop counts DISTINCT SEALED FIELDS with the assignment
  count when it exceeds the fields (E17 — `(and 1 more field)`,
  `(2 assignments)`), over a hashed structural identity whose scalar
  item keys are never printed or `Debug`-leaked, with one `sealed here`
  note per assignment carrying its own file (`Related.source` lands;
  both renderers locate a note in its own file, falling back loudly); a backstop
  rejection carries one `sealed here` note per discarded assignment
  (the first four); the validator materializes a named item's name into
  a oneof element's arm (`- a: kind = "a"` under `[]oo` with `name
  string+` read as a missing required field on every raw block);
  `FieldRoute` no longer defines identity-equality; plan keys are
  debug-asserted dotted (never an item scope); a test-only tamper seam
  proves the compose boundary's invariant assertion is live; and the
  NML0016 fix for a bare CR INSIDE a string literal is the `\r`
  escape, and a CR in token position has NO machine fix (`nml fix`
  deleted both — a silent value change inside a string, and glued
  lines on a CR-terminated old-Mac file).

### Changed

- **Source-character policy closed over its classes (NML0017/NML0018)**
  — a breaking tightening, landed pre-release. NML0017 now rejects
  every Unicode control character raw (general category Cc: C0, DEL,
  and the previously unpoliced C1 range — including the one-byte CSI
  terminal-escape introducer and NEL); NML0018 adds the U+2028/U+2029
  line/paragraph separators, so every Unicode line-boundary character
  outside LF/CRLF is now diagnosed. The banned set is deliberately a
  strict superset of rustc's (rustc reads LS/PS as whitespace and
  allows raw C1 in literals): NML values are echoed into terminals,
  logs, and generated configs. The implicit bidi marks (LRM/RLM/ALM)
  stay legal — ordinary RTL content. Raw NEL/LS/PS previously parsed
  clean inside strings and comments; the escapes (`\u{85}`,
  `\u{2028}`, `\u{2029}`) and `\n` are the spellings, and the error's
  hint teaches both. `nml fmt` previously re-rendered those escapes as
  RAW bytes (so a formatted file could newly fail) — it now emits the
  escapes, restoring "formatted output never carries a character the
  parser rejects" as a tested invariant. The Unicode tag block
  U+E0000–U+E007F (128 code points — a deprecated invisible mirror of
  ASCII) joins the NML0018 set: a raw tag sequence hides an ASCII
  payload inside what displays as ordinary text, and previously drew
  zero diagnostics anywhere; emoji tag sequences are content and are
  written with escapes (`\u{1F3F4}\u{E0067}…`).

- **Quoted-literal migration teaching extended (NML0001)** — quoted
  numbers (`port = "3000"`) and quoted bools (`admin = "true"`) against
  their typed fields now get the same replaced-syntax teaching error the
  quoted-duration migration ships: the machine-applicable de-quote fix,
  instead of the generic NML2008 mismatch. Fires only when the de-quoted
  text parses as the field's type; `$ENV.*` references are untouched.

- **Docs harness `expect-error` counts findings (multiset)** — the
  bracketed code contract upgraded from set to MULTISET equality:
  repetition is the count syntax (`[NML2057, NML2057]` = exactly two
  findings). The flip immediately surfaced 13 error-index fences whose
  samples demonstrated two same-code findings while declaring one — the
  drift class set-equality structurally hid; all are now declared true.

- **Bare faceted-domain fields carry `PrimitiveFacets::None`** — a bare
  `number`/`duration` field now serializes and matches exactly like a
  bare `string` field ("no facet list authored"); the domain is the type
  name's job. Audited safe in advance: package identity hashes source
  bytes only, extraction is never persisted or wire-crossed, and every
  behavioral consumer routes through `is_none()`. Representation pinned
  by a wire-shape test plus a `None ⟺ no authored list` invariant test.

- **LSP claims invalidation tightened; wasi leak budget** — watched-file
  CHANGED events no longer clear the package-claims memo (claim verdicts
  are functions of file existence, names, and manifest-hashed globs —
  never content), and invalidation is root-scoped to the changed paths.
  The wasi `read_dir` leak workaround now counts forgotten handles and
  refuses past a documented budget — bounding the memory tail under
  `wasm-wasi-core`'s reuse-free fd table and any fd-capped host, with
  callers degrading to the existing honest `Truncated` path.

- **Inline arm targets (RFC 0007 §6.2)** — `@selector -> Name:` + indented body is a
  first-class arm RHS form, fully validated against `V`, recursed by identity/
  defaults/diff, with LSP descent, document symbols, inline-target completion
  snippets, string/enum arm-selector completion, and a parse-time diagnostic for
  the `-> "name":` mistake.

- **Compound duration literals (RFC 0017 amendment)** — durations now
  support attached compound syntax (`1h30m`, `5m2s`) and inline-spaced
  forms (`1h 30m`). The lexer suffix-run rule tokenizes glued compounds
  as alternating `Number`/`Ident` pairs; duplicate units in source
  diagnose as `NML3007` with a merge fix; dangling magnitudes are
  `NML3008`; the `NML3005` fractional fix now respells at the authored
  granularity (`1.5h` → `1h30m`, not `90m`). Coercion text (`$ENV`)
  silently merges duplicates and decomposes exact fractions. Wire JSON
  now carries a `segments` array (breaking, pre-1.0).

- **VS Code extension tooling** — migrated from npm to a pnpm 11 workspace
  (root `pnpm-lock.yaml`, supply-chain policy in `pnpm-workspace.yaml`).
  Contributors: `corepack enable && pnpm install`, then `just verify-ext`.
  Toolchain gate: `pnpm run check:toolchain` (VS Code API floor, lockfile
  alignment, Node 22+, Corepack-pinned pnpm).

- **Duration facets (RFC 0018 §3 deferral closed)** — `duration` is now
  a legal facet carrier beside `number`: `timeout duration(min = 1s,
  max = 2h, multipleOf = 250ms)`. One generic engine (`Facets<T:
  FacetDomain>`; `NumberFacets`/`DurationFacets` are aliases, and
  `FieldType::Primitive` now carries domain-tagged `PrimitiveFacets`)
  keeps the two families byte-identical in behavior: bounds compare
  semantically (`min = 1000ms` ≡ `min = 1s`; nanos-exact, never
  floats), `multipleOf` is unit-blind nanos divisibility, and defaults,
  config values, unions, and collections all face the same walk on
  every surface (generated default/value parity grid extended over the
  duration rows). Declaration rules (NML2058): bounds must be duration
  LITERALS (`min = 5s` — a unitless bound teaches the shape), a
  duration bound on a `number` field errors symmetrically, ranges are
  judged semantically across units, and `multipleOf = 0s` is rejected
  like its numeric zero sibling.

- **The editor calls the loader** — `.model.nml` buffers now get
  cross-definition schema validation in the editor: unknown/wrong-kind
  `is` targets, inheritance and reference cycles, post-inheritance
  shorthand arity, oneof/enum integrity, reserved type-constructor
  names, cross-source duplicates, and declared-default checks, all by
  running `nml_validate::loader::load_schema` — the same entry every
  CLI verb and embedder uses — over the buffer's covering package
  (`[]schema`, manifest order, buffer-first reads) or its
  directory-mates when uncovered, keeping only the buffer's own
  source-stamped findings. CLI/editor parity is by identity, not by
  test; `nml-lsp`'s separate buffer-scoped default check is deleted as
  redundant. The workspace index walk now also skips `target/`
  (`cargo package` copies crate sources there, `.nml` fixtures
  included).

- **Durations are literals (RFC 0017)** — `30s` parses to a typed
  `Value::Duration` (magnitude + unit, faithful storage, **semantic**
  equality: `30s == 30000ms`, so sets and reload diffs treat the two
  spellings as one value). Units are `h`/`m`/`s`/`ms`/`us`/`ns` — the
  **complete** ladder, closed by construction (`ns` is the value
  domain's own resolution floor; `h` the largest exact unit; calendar
  units are permanently excluded), so `DurationUnit` is exhaustively
  matchable forever. Unsigned integer
  magnitudes only, domain-bounded at decode (`NML3004` unknown unit,
  `NML3005` fractional magnitude, `NML3006` out of domain — all with
  machine-applicable fixes where one exists); `NML2029` is retired to a
  tombstone. A duration-typed field deserializes to
  `std::time::Duration` from every provenance: literals directly, and
  `$ENV`-resolved strings through `coerce_to_duration`, joining the
  existing `de` coercion family (never-echo rule inherited). The quoted
  spelling `"30s"` in a duration-typed field is an `NML0001` migration
  with a mechanical fix, applied in bulk by the new `nml fix`.

  *This reverses the earlier in-cycle removal of `Value::Duration`.* The
  removal's premise — no code path could produce the variant — was
  correct while durations had no grammar; this RFC adds the grammar and
  removes the premise. `Value::Path` **stays removed**: a path's
  meaningful typing is safety (containment, traversal), none of it
  knowable at parse time — that belongs in the consumer's newtype.
- **`Value::Path` removed** — the variant was unreachable by
  construction: paths are *quoted* strings in the grammar with no lexeme
  of their own, so only schema validation can judge path-ness, and no
  code path ever produced the variant (its `PrimitiveType` doc already
  said "treated as a string at runtime" — path-typed validation never
  even accepted `Value::Path`). Path-typed fields keep validating
  exactly as before; consumers keep receiving `Value::String`. Dead
  match arms across the workspace went with it.
- **Oneof arm values report every bad escape** — bare string-literal
  positions (oneof discriminator/arm values) now use the total decoder:
  all escape errors surface at once (rustc-style, matching property
  values) and the U+FFFD-recovered text is kept, so lenient surfaces
  retain the arm's identity instead of an empty string.

- **List validation: spelling parity & the item-shape matrix** — the two
  spellings of a list field (`f = [v]` and `f:` + `- v`) now validate
  identically, by construction and by CI-enforced test. — one contract,
  *raw is transport, escaped is content*, enforced end to end:
  - New diagnostics: bare CR (NML0016; inside a string the machine
    fix is the `\r` escape), raw
    C0/DEL control characters (NML0017), bidirectional controls and
    interior U+FEFF — the Trojan Source defense, rustc's banned set
    (NML0018). A leading U+FEFF is accepted as a BOM.
  - New escapes: `\r`, `\s` (protected space, as in Java text blocks),
    and `\u{…}` (1–6 hex digits, as in Rust/Swift) — the sanctioned
    spelling for every character the policy bans raw. `\u{7B}\u{7B}`
    writes a literal `{{` (template detection reads raw text, so escapes
    can never smuggle a template expression).
  - Multi-line strings adopt the Java text-block order: dedent is
    computed on source lines *before* escapes are interpreted, so
    escaped newlines/whitespace are content, never indentation. Content
    must begin on the line after the opening `"""` (NML0019), an
    own-line closing `"""` must align with the content (NML0020,
    machine-fixable — and gated on termination: an unterminated
    string's trailing blank line is a recovery artifact, not the
    closing quotes, so it reports only the missing delimiter), tabs
    may not appear in body indentation
    (NML0005 extended), and `\` before a line break is Java/Swift line
    continuation. Line endings are transport: LF and CRLF documents are
    byte-identical in value (fuzz-verified).
  - Value decoding is now *total*: every malformed escape in a string is
    reported at once (rustc-style) with U+FFFD recovery, via the new
    `decode_value_all` / `ValueErrors` API; the strict single-error API
    is unchanged.
  - The formatter can never emit a document the parser rejects: edge
    spaces render as `\s`, `"""` runs and template braces are broken by
    escaping, tabs render as `\t`.
  - A fallback chain in a list position gets a teaching diagnostic
    (NML0021) instead of a misleading pipe-modifier parse error — with
    recovery, in both list spellings. Chains stay property-position;
    the `const`-name idiom (`const K = $ENV.A | $ENV.B`, then
    `keys = [K]`) is the supported way to use one as an element.

- **Numbers are exact decimals (RFC 0016)** — `Number { Int(i64),
  Float(f64) }` is replaced by a single exact decimal (34 significant
  digits, the finite IEEE 754-2019 decimal128 value space): `taxRate =
  0.20` now stores exactly 0.20, written scale survives formatting and
  serialization (`2.50` stays `2.50`), and integers parse to 34 digits
  (previously hard-capped at `i64`). Anything outside the exact domain is
  a parse error — NML never rounds. Breaking API changes: `Number` is a
  struct (gains `Eq`/`Ord`/`Hash`, the `num!(…)` const literal macro,
  `total_cmp`, `to_i128`/`to_u128`, `try_from_f64`); `as_i64`/`as_f64`
  are now `to_i64`/`to_f64` on `Number`, `Value`, and `ValueQuery`;
  `From<f64>` and `PartialEq<f64>` are removed (use `num!`);
  `deserialize_i128`/`u128` are supported and `u64 > i64::MAX`
  deserializes exactly; `Serialize` never emits binary floats (fraction
  forms serialize as exact strings); env-string coercion accepts
  scientific notation exactly and rejects `inf`/`nan`; f32 fields are
  single-rounded (the old path double-rounded). `NML0014` generalizes
  from "integer out of `i64`" to "number outside the exact decimal
  domain" with a structured payload; trailing-dot literals (`1299.`) are
  now `NML0013` with a machine-applicable fix. Money parses through the
  same decimal core (`Money::to_number()` is new; exactly-`i64::MIN`
  minor units are now accepted; >`i64` amounts error as `NML3003` rather
  than `NML3000`). The typed-error integer extraction is now the single
  wide rung `TryFrom<&Value> for i128` (replacing the `i64` impl):
  every integer target through `u64` narrows exactly from it via
  `T::try_from` (a `u128` consumer uses `Number::to_u128`), so
  nothing funnels through `i64` and falsely rejects the
  `(i64::MAX, u64::MAX]` band; `Value::to_u64` covers the probe flavor.
  Programmatic `Number::try_new` rejections carry their own raw-pair
  kinds (`CoefficientTooWide`/`ScaleOutOfRange`/`NegativeScaleZero` —
  one per invariant, never diagnostics); `Malformed` is grammar-only.

- **Rust edition 2021 → 2024** across the workspace (one inherited line; the
  MSRV stays 1.86, which predates the flip and supports edition 2024). The
  audited migration surface was 12 sites, all rowan node-handle temporaries
  in `cst/` whose 2024 drop/scoping changes are semantically inert — each
  verified individually, and the natural `if let` forms kept over the
  migration tool's defensive rewrites. Code delta: one `use<>` capture
  bound in a test helper. Formatting adopts the 2024 style edition
  (mechanical sweep, separate from the semantic change). Downstream crates
  on any edition are unaffected.

- **`nml_core::de::from_block` is now `from_body`** — it always took a
  `&Body`, and every other deserialization entry point is named for its
  input type (`from_value`, `from_body_resolved`, the `defaults` family);
  the naming rule is now stated normatively in the `de` module docs.
  Pre-publish rename, no deprecation alias. Also: `nml_core::File` is
  re-exported on the crate facade beside `parse` (its return type), and
  `nml-validate` re-exports the core facade essentials (`parse`, `File`,
  `Document`, `ValueResolver`, `Diagnostic`, `Severity`, `SchemaIndex`,
  the defaults family) so the common parse → validate → defaults →
  deserialize flow is a single dependency.

- **Declared defaults are checked where the schema is loaded** — a
  field's `= default` is now type-checked by `load_schema` (and so by
  both CLI verbs, schema packages, and any embedder), instead of only
  by surfaces that happened to validate a file containing the
  definition. A type-wrong default in a `--schema` directory used to
  load clean and materialize into runtime config; it is now an error at
  load. **API change**: `SchemaValidator::validate()` no longer reports
  default diagnostics — call `nml_validate::schema::default_diagnostics`
  (or just use `load_schema`, which does). The check runs *before*
  inheritance resolution, so one declared default yields exactly one
  finding however many models inherit it.

### Added

- **Numeric schema facets (RFC 0018)** — `number` fields constrain
  their value range first-class in the type:
  `port number(min = 1, max = 65535)`, with `exclusiveMin`/
  `exclusiveMax` and `multipleOf`. Enforcement is exact through the
  RFC 0016 decimal core — boundary comparisons cannot lie and
  `multipleOf` is exact decimal divisibility (`0.3` IS a multiple of
  `0.1`; float-based validators famously disagree), via the new
  `Number::is_multiple_of` predicate (modular arithmetic, no bignum,
  no rounding). Facets apply element-wise to `[]number`/`set<number>`,
  hold field defaults to the same rule, render canonically in fmt and
  hover, and come with two diagnostics: `NML2057` (value violates a
  facet) and `NML2058` (invalid facet declaration). Schema packages
  need no format change — packages carry schema source, and a
  pre-facet parser rejects the new syntax loudly rather than silently
  under-validating. Wire-shape note for external `nml parse` JSON
  consumers: `Named` and `Primitive` are struct variants now
  (`{"Primitive": {"ty": "Number", "facets": …}}`).

- **`nml fix` — the batch fixer** (RFC 0017 §4.1): applies
  machine-applicable suggestions in bulk (`nml fix [--schema <dir>]
  [--dry-run] <path>...`, directories walked for `.nml`). A suggestion is
  applied only when it is the **sole candidate for its span** (so RFC
  0015's N-mutually-exclusive-fixes rule holds by construction), edits
  splice highest-offset-first via the new span-splice primitive
  (`cst::edit::splice`), and every round is re-checked before acceptance
  — a round that does not strictly improve the file is discarded, and
  writes are atomic. This is the missing half of the stability policy's
  "breaking changes ship with fixers" commitment: the `=>` → `->` and
  quoted-duration migrations are now bulk-appliable (and the repo's own
  examples were migrated with it).

- **The proof surface**: a [case study](docs/case-study.md) describing how
  a production workflow platform embeds NML end to end (schema packages +
  embedded `<tool> lsp`, diff-driven `#live`/`#restart` reload, config as
  a security surface — attribution generic until the platform's launch),
  and a [footprint page](docs/footprint.md) with measured, reproducible
  numbers: 19 packages for core parse+serde embedding (no async stack),
  ~0.75 MiB release example binary, corpus parse throughput via a
  committed measurement example (`measure_parse`).

- **The cookbook** (`docs/guides/`): fourteen task-oriented recipes for
  embedding NML as a library — parse/query, serde deserialization, custom
  secret resolvers, schema defaults, CI validation, semantic diff +
  `#live`/`#restart` classification, format-preserving edits, collect-all
  errors, directive vocabularies, schema packages & the store, embedding
  the language server, idempotent formatting, schema testing, and a
  migrate-from-TOML guide whose side-by-side claims are *executed*: the
  same config is parsed from TOML and NML into one struct and asserted
  equal in CI. Every recipe is a compiled example (or test) in the new
  `nml-cookbook` workspace crate; the docs harness runs all of them and
  verbatim-syncs every page listing against the compiled source.

- **MSRV: measured, declared, and enforced — Rust 1.86.** The floor was
  established by an audited bisect (every build target, the
  `wasm32-wasip1` server, and doctests, each candidate resolved with the
  MSRV-aware resolver): nml's own code compiles below 1.86, but the
  `icu_*` dependency family (via `url`/`idna` under `tower-lsp`) sets
  1.86 as the honest floor of the resolvable dependency set. CI now
  builds on exactly that toolchain across Linux/macOS/Windows (the job
  reads the version from `Cargo.toml` — one line to bump, and bumps are
  minor-version events per `docs/stability.md`), `Cargo.lock` is now
  committed (current cargo-team guidance; CI runs `--locked`, so a
  dependency release can never break `main` silently), and three more
  gates joined the reusable pipeline: rustdoc with warnings denied,
  `cargo package` publish-readiness for every publishable crate, and a
  nightly-resolver minimal-versions check proving the declared dependency
  bounds are real. Running the supply-chain gate locally also surfaced a
  latent failure: the unpublished tutorial apps tripped cargo-deny's
  license check — now exempted via `private = { ignore = true }`
  (licensing applies to what ships). Locally: `just msrv`.

- **Full error explanations in-editor (RFC 0010 tier 2)**: every coded
  diagnostic offers an **Explain NML0000** code action that renders the
  complete error-index entry — meaning, runnable examples, the fix — as
  a markdown preview beside the code, and the **NML: Explain a
  Diagnostic Code** palette command opens the same entries for codes you
  can't hover (CI output, a teammate's log), listing all of them
  searchable by code or summary. One composer
  (`nml_core::diagnostic::explain_document`) shapes the entry for both
  the editor and `nml explain`; explanations always come from the exact
  server binary that produced the diagnostic, so error and explanation
  can never version-skew. The action is negotiation-gated
  (`initializationOptions.explainCommand`) — LSP clients that registered
  no command never receive an unexecutable action; the content rides two
  new custom methods, `nml/explain` and `nml/explainIndex`. Also:
  `nml explain --list` (every code with its summary, grep-able), the
  index's own relative links repaired after its move to
  `crates/nml-core/assets/` (now guarded by a docs-test link resolver),
  and a tripwire guaranteeing no fenced line can truncate an index
  section.

- **In-editor error explanations (RFC 0010 tier 1)**: hovering a
  diagnostic shows its error-index **summary** — the meaning paragraph,
  never the examples — after any regular hover content (or alone, on the
  diagnostic's range), with a pointer to `nml explain` for the full
  entry. One new primitive, `nml_core::diagnostic::explain_summary`,
  derives the summary from the same embedded index as `explain`
  (relative links stripped so nothing dangles in hover context); all 82
  documented codes are covered on day one. Behind it, the language
  server gains a per-document **diagnostics cache** validated against
  the exact buffer text it was computed from (an in-flight compute that
  races an edit can never serve stale ranges), invalidated by document
  edits, schema-registry rebuilds, and project-config changes — which
  also makes the document-pull's *Unchanged* path recompute-free.

- **The parse-error taxonomy is closed (RFC 0009)**: every syntax
  diagnostic derives its message, stable code, and any machine-applicable
  fix from a payload-carrying kind — the transitional prose carrier and
  the `Lex`/`Parse` variant split are deleted, and `NmlError` is
  `Syntax` + `Money` with no `String` field anywhere. New stable codes
  `NML0002`–`NML0015` (unexpected-token with expected/found/context,
  unterminated string, unexpected character, tab-in-indent, the
  offside-rule dedent — which lists the open columns — nesting limits,
  set separator, reserved `map`, unknown type constructor, duplicate
  directive, string escapes, invalid/out-of-range numbers, `$NS.key`
  references) and `NML3002`/`NML3003` (money precision / out-of-range;
  `NML3000` narrows to malformed amounts) — every one with a CI-verified
  index example.
- **Parser errors carry token-width spans** (editor squiggles cover the
  offending token, not a caret between characters); recovery cascades at
  one position coalesce into a single "expected X or Y" report; and the
  fixers engine grows `&&`→`&` and `set<a, b>`→`set<a | b>`, plus
  did-you-means for unknown type constructors and variable namespaces.
- **Diagnostics are honest and hardened**: truncation is never silent
  (every layer counts what it drops; one `info` line reports the exact
  suppressed total), every render path escapes control characters (file
  content cannot smuggle terminal escapes into CLI output or logs), and
  `Diagnostic` gains related information — `note:` lines in the CLI,
  spec-native `relatedInformation` in the editor — with unterminated
  strings pointing back at their opening delimiter as the first producer.

- **Total diagnostic code coverage**: every validator- and package-emitted
  finding now carries a stable code with a verified error-index entry —
  the Phase 4 sweep. New: `NML2036`–`NML2042` (arms and `oneof` instance
  rules: duplicate/unreachable arms, key/target mismatches,
  missing/invalid discriminators), `NML2043` union-list shorthand,
  `NML2044` validation-truncated advisory, `NML2045` role-written-as-string
  (now **machine-fixable** — the quick fix strips the quotes and adds
  `@`), `NML2046`–`NML2048` membership advisories, `NML2049` dropped item
  key and `NML2050` arm-shorthand mismatch (identity's materialization
  findings are now `Diagnostic`-native, each coded at its source — and
  with its last producer converted, the prose-carrying
  `NmlError::Validation` variant is deleted), and `NML4000` fully-shadowed
  package validator — the first code in the packages band. Six more
  type-mismatch sites joined `NML2008`.
- **Schema-driven block-keyword completion** (the keyword twin of RFC
  0003's field completion): the editor offers block keywords from the
  document's resolved schema context — its bound package (closed
  vocabulary, RFC 0012) or the scope registry plus the document's own
  definitions — concrete models and oneofs only, labeled with `schema`
  provenance. And **editor collision parity**: an open-mode document
  redefining a registry name gets the same `NML2009` the CLI reports
  (`.model.nml` registry sources are exempt from self-collision).
- `nml check --strict` with an empty schema universe is now a **usage
  error** ("--strict has nothing to enforce") instead of silently
  degrading to parse-only checking — the CI-points-at-the-wrong-path
  trap, closed.

- **Self-validating files — one namespace (RFC 0012)**: `nml check`
  composes one schema universe from the `--schema` directory *plus the
  checked file*, so `model cache:` above `cache Foo:` fully types `Foo`
  with no flags — in the CLI and in the editor's lenient mode alike.
  Splitting a file into a schema directory no longer changes semantics; a
  name declared in both places is `NML2009`, never a silent shadow.
  Package-bound validation is the deliberate opposite — a **closed
  vocabulary**: in-file definitions under a binding type nothing, cannot
  mint keywords past `--strict`, and draw `NML2026` (warning lenient,
  error strict) instead of today's silence. Closedness follows provenance
  (`SchemaPackage::validator`); there is no knob.
- **Document-scope deserialization (RFC 0013)**:
  `from_document_defaulted` materializes top-level array-declaration
  references (`endpoints = monitoredEndpoints`) — shared properties and
  items inlined — before the serde pipeline runs, so the modular layout
  and typed structs stop being a trade-off; `Document::array_body(name)`
  exposes the referenced declaration directly.
- **Shared properties and modifiers are validated** (previously invisible
  even under `--strict`): unknown `.prop` names get the unknown-property
  treatment with a did-you-mean at the `.prop` token (for `oneof` and
  union elements alike, flagged only when no variant defines the name —
  with the did-you-mean drawn across every variant's fields), shared
  values type-check against the element's field (union elements: against
  the defining variants, through the standard union value check), and a
  model that declares modifier fields (`|allow []role?`) becomes its
  blocks' modifier vocabulary — `|alow` is caught with a suggestion even
  with no project/package modifier list.
- New coded diagnostics with error-index entries: `NML2027` duplicate enum
  variant (both authored forms normalize), `NML2028` empty enum, `NML2029`
  invalid duration (the spec grammar, enforced by the same
  `nml_core::types::parse_duration` consumers use — `"30x"` no longer
  passes), `NML2030` duplicate set element (one emitter replaces three
  hand-written sites), `NML2031` non-arm entry in an arms body, `NML2032`
  no union variant matches. `EnumDef`/`OneOfDef`/`ModelDef` all carry
  their declaring source for `file:line:col` attribution.
- `PackageError` and `StoreError` implement `std::error::Error` — `?`
  works in embedders' `main`.
- **The definition verbs can never disagree**: `nml validate` runs the
  same definition-side body pass `nml check` runs
  (`SchemaValidator::validate_definitions` — one code path, so a future
  definition check can't split the verbs). Surfaced by review: schema
  *defaults* (`duration = "5x"`) and RFC 0007 §4.3 type-shape violations
  previously erred under `check` while `validate` said ok.
- More formerly-uncoded diagnostics now carry stable codes with verified
  index entries: `NML2033` type composition with no instance form (arm
  sets in impossible positions; multi-arm-set unions), `NML2034` field
  definition outside a model, `NML2035` routing arms inside a schema
  declaration.
- The formatter's blank-line policy for shared properties is
  context-independent (nested lists now preserve the separator line the
  array form always kept).
- `NML2025`: a mixin listed twice in one `is` clause warns (the merge is
  idempotent; transitive diamonds stay silent).
- **Shared properties now scope to every body** (spec §Shared Properties
  clarified): a nested list's `.prop` merges into that list's items at any
  depth — previously the authored value was silently dropped in favor of
  the schema default in the serde pipeline (data loss). Precedence is
  unchanged: schema default → shared property → the item's own entry.
- **Positional items receive schema defaults** through the serde pipeline:
  a bare `- "value"` item's materialized body now defaults and merges like
  a named item's.

- **Traits are real (RFC 0011)**: `trait name:` declares a non-instantiable
  mixin — same field syntax as a model (defaults, markers, modifiers,
  directives all travel), composed with `is` by models and other traits,
  and never a block keyword, field type, or `oneof` arm target. Previously
  `trait` parsed but was silently ignored by schema extraction. Five new
  stable codes with error-index entries: `NML2020` unknown `is` target
  (with a machine-applicable did-you-mean at the target token — `is`
  targets were previously *silently* unresolved), `NML2021` non-composable
  `is` target, `NML2022` trait as a field type, `NML2023` trait as a
  `oneof` arm, `NML2024` trait instantiation (an error even in lenient
  mode). Strict-mode unknown-keyword suggestions (CLI and editor — the
  same validator) never offer a trait; `trait` joins the language-keyword
  completions.
- `nml validate` and `nml check` now run the full schema-finder pipeline
  on files that declare models/traits/enums/oneofs (reserved and duplicate
  names, composition, oneof integrity, positional arity, cycles) —
  previously these surfaced only through `check --schema`. `check` is a
  strict superset of `validate`; each finding is reported exactly once
  (the in-file composition twin defers when a loader pass covers the same
  content, and a file inside the `--schema` directory is covered by the
  directory load). Warnings report without failing the file. A
  self-contained file's own declarations resolve `is` targets — never
  falsely flagged against a foreign schema set.
- **Definition-anchored schema findings are source-attributed**: the
  loader stamps every model/trait/oneof with its declaring file, and
  composition, oneof-integrity, positional-arity, and cycle findings now
  render `file:line:col` instead of a directory-prefixed raw byte span.

- **Unified diagnostics model** (`nml-core::diagnostic`, RFC 0008): one
  `Diagnostic` type — severity (now incl. `Info`), span, source, structured
  suggestion, and a **stable error code** (`NML0000`-style, never renumbered
  or reused once released) — shared by the parser's error list, symbols,
  the validator, the LSP, and the CLI. One renderer, one LSP converter
  (replacing three hand-rolled bridges); the CLI prints rustc-style
  `error[NML2000]:` prefixes.
- New did-you-mean hints from core diagnostics: unresolved references
  (`DefaultPrt` → `DefaultPort`), unknown currency codes (`USE` → `USD`,
  from the ISO 4217 table), and unknown template namespaces — each with a
  machine-applicable quick-fix span.
- **Structured parse errors (RFC 0009 foundations)**: syntax errors carry a
  payload kind from which message, code, and fix derive; **`NML0001`
  Replaced syntax** is live — writing `=>` now yields a machine-applicable
  `->` fix in the CLI and as an editor quick-fix, with the error index
  carrying the migration ledger.
- **`nml explain NML2007`**: the error index is embedded in the binary
  (canonical file: `crates/nml-core/assets/error-index.md`, exposed as
  `nml_core::diagnostic::explain`), so explanations work offline; the CLI
  prints a rustc-style "for more information" hint after coded diagnostics.
- The oneof umbrella code split by fix-pattern: `NML2012` is now
  arm-references-unknown-model only, with `NML2015`–`NML2019` covering
  duplicate discriminator, name collision, bad default, non-enum
  discriminator type, and non-exhaustive enum arms.
- **Error index** (`docs/errors/README.md`): every stable code has a
  documented section, most with CI-verified examples; a bidirectional
  docs-test guard means a new code cannot ship without its documentation.
- `nml check --strict`: unknown properties and unmodeled keywords become
  errors (CI posture).
- Schema diagnostics from `nml check --schema` now locate as
  `file:line:col` against the attributed schema source instead of printing
  raw byte spans.
- **Changed:** every public findings API now returns `Vec<Diagnostic>` —
  `cst::parse_to_ast_all`, `cst::extract_schema`, the `SymbolTable` finders,
  and the schema-integrity finders (`find_oneof_errors`,
  `find_shorthand_errors`, `find_model_cycles`, `find_extends_cycles`) —
  with codes assigned (NML2007–NML2014 incl. missing-required-field,
  type-mismatch, duplicate-definition, reserved-type-name, cycle classes)
  and severity carried at the source; `nml_validate::diagnostics`/`::suggest` moved to
  `nml_core::diagnostic`/`::suggest` (no re-export shims); the LSP now
  reports duplicate/unresolved findings at their true severity (error),
  matching the CLI.
- **Unified did-you-mean engine** (`nml-core::suggest`): one OSA
  (restricted Damerau-Levenshtein) metric behind every suggestion, so
  transpositions (`"wran"` → `"warn"`) are caught; case-insensitive exact
  match wins outright; deterministic tie-breaking. Replaces the two divergent
  per-site suggesters.
- Did-you-mean suggestions (with machine-applicable quick-fix spans) at
  previously uncovered sites: unknown properties, unknown modifiers, unknown
  `oneof` discriminator values, and strict-mode unknown block keywords.
- `Diagnostic::rendered_message()`: the hint is derived from the structured
  suggestion by one renderer shared by the CLI, `Display`, and the LSP —
  producers no longer bake hint prose into messages. The `nml` CLI now prints
  hints for all suggesting diagnostics.

- Core parser with indentation-aware lexer
- AST types for all NML constructs (blocks, arrays, properties, modifiers, shared properties)
- Money type with ISO 4217 currency table and minor-unit storage
- Reference resolver with duplicate detection
- CLI with `parse`, `validate`, `fmt`, and `check` subcommands
- Canonical formatter with round-trip fidelity and idempotency
- LSP server with diagnostics, completion, hover, and go-to-definition
- VS Code extension with TextMate grammar for syntax highlighting
- Language specification (syntax, types, models, access control)
- Test fixtures for valid and invalid NML files

- **Template expressions**: `{{namespace.key}}` syntax in strings for dynamic interpolation
- **Fallback values**: `$ENV.KEY | "default"` pipe-chained fallback resolution
- **Template declarations**: `template Name:` for named string values
- **Const declarations**: `const Name = value` for file-level constants

- **Serde bridge** (`nml_core::de`): deserialize NML blocks into Rust structs
  - `from_block` -- deserialize a struct from an NML block body
  - `from_value` -- deserialize from a single NML value
  - `from_body_resolved` -- resolve + apply shared properties + deserialize pipeline
  - Recursive deserialization of nested blocks into nested structs
  - Named list item deserialization with automatic `name` field injection
  - Support for `Option<T>`, `Vec<T>`, enums, `camelCase` renaming

- **Value resolution** (`nml_core::resolve`):
  - `ValueResolver` with pluggable lookup (env vars or custom function)
  - Resolves `Value::Secret` (`$ENV.X`) and `Value::Fallback` chains
  - `resolve_body` / `resolve_array_body` for recursive resolution
  - `apply_shared_properties` -- merge `.key:` defaults into list items
  - `apply_array_shared_properties` -- same for array declarations

- **Query API** (`nml_core::query`):
  - `Document` wrapper with fluent block/property/nested lookups
  - `const_value`, `template_value`, `blocks`, `declarations` queries
  - `BlockQuery` and `ValueQuery` with `as_str`, `as_f64`, `as_bool` accessors

- **Value type conversions** (`nml_core::types`):
  - `TryFrom<&Value>` for `String`, `f64`, `i128`, `bool`, `Vec<String>`
  - Handles `Reference`, `RoleRef`, `Path`, `Duration`, `Secret` as string
  - `Value::as_str()`, `as_f64()`, `as_bool()`, `as_array()` accessors

- **Project configuration** (`nml-project.nml`):
  - `ProjectConfig` for schema files, template namespaces, modifiers, keywords
  - Auto-detected by LSP for workspace-aware validation

- **Model extraction** (`nml_core::model_extract`):
  - Extract model, enum, and trait definitions from parsed AST

- **LSP enhancements**:
  - Template expression validation (invalid namespace warnings)
  - Step reference validation in workflow files
  - Go-to-definition for keywords, references, and model fields

### Fixed

- `nml check`/`nml validate` did not detect const/template reference cycles
  (the LSP did) — now reported as `NML1002`, caught by the error index's own
  verified example
- Money `format_display` now correctly handles negative fractional amounts
  (e.g. `-$0.50` was previously displayed as `$0.50`)
- Serde bridge now uses `format_display()` for money values instead of raw
  minor units (previously serialized `1999 USD` instead of `19.99 USD`)
- Money values can now be deserialized into `String` fields via serde

### Documentation

- Language guide with complete feature coverage
- Integration guide for using NML in Rust projects
- Formal language specification with PEG grammar
- Template expression, fallback value, and const declaration documentation
- README rewritten around a CI-verified 30-second demo, an honest comparison
  table ("when NOT to use NML" included), and a Rust embed quickstart
- Docs verification harness (`just docs-test` + CI job): tagged ```` ```nml ````
  blocks in the Markdown docs run through the real CLI, including asserted
  error examples, so documentation examples cannot rot
- Per-crate READMEs, CONTRIBUTING (docs-required gate), SECURITY policy,
  PR template, RFC status index, and a stability & compatibility policy
  (`docs/stability.md`); workspace `rust-version` declared
- **Nine-chapter tutorial** (`docs/tutorial/`): one growing status-page
  config from first parse through schemas, composition, `oneof`, access
  control, the Rust embedding pipeline, a diff-driven reload classifier,
  and shipped schema packages. Every chapter's finished config is a
  CI-validated fixture; chapters 7–9 are workspace crates the docs harness
  compiles **and runs**, asserting the output printed on the page; full
  Rust listings on pages are CI-checked to be verbatim excerpts of the
  compiled programs (```` ```rust source=<file> ```` guard)
- **Spec legacy purge + role-conjunction docs**: the never-implemented
  angle-bracket constraints, the parenthesized composition form, the
  `[]@roleRef` element type, and the `&`-reference marker are gone from
  the spec (`&` now means conjunction); role conjunctions are documented
  in the spec grammar (a `RoleExpr` production plus a normative
  `" & "` canonical-form clause), the access-control semantics, and the
  language guide. The docs harness gains banned-pattern tripwires so
  none of the dead forms can be re-taught, and the pre-commit hook now
  guards RFC-number uniqueness and index parity.
