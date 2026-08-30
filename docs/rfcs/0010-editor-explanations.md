# RFC 0010 — In-Editor Error Explanations (Three Tiers)

- **Status:** **Tiers 1–2 implemented** (2026-07-24) — tier 2 adds the
  full entry in-editor: `explain_document`/`explain_index` (one composer,
  one `sections()` iterator), `nml/explain` + `nml/explainIndex` custom
  methods, the negotiation-gated "Explain NML0000" code action, the
  `nml-explain:` content provider + palette QuickPick in the extension,
  and `nml explain --list`. Tier 3 (`codeDescription.href`) remains for
  publish day. Tier-1 record below; tier-2 converged design in §1.2.
- Previously: **Tier 1 implemented** (2026-07-24) — `explain_summary` (with
  relative-link stripping), the text-validated per-document diagnostics
  cache (registry/config-aware invalidation), and the hover compose point
  (async-block wrapper; explanation-only hovers carry the diagnostic's
  range). Tiers 2–3 remain proposed. History: an early `explain_summary`
  was built and **removed by review** — a public primitive with no consumer
  violates the no-speculative-API precedent; it landed here *with* its
  consumer, which is also where its open design question (relative links
  dangling in hover context) was answered by measurement — 2 of 82 first
  paragraphs, stripped in the one splitter.
- **Builds on:** [RFC 0008](./0008-unified-diagnostics-error-codes.md)
  (embedded index, `diagnostic::explain`), the `nml/schemaInfo` custom-
  method precedent.
- **Crates touched:** `nml-lsp` (hover, custom request), `editors/vscode`
  (tier 2 — the first extension work of this arc; E2E harness exists).

## 1. Three tiers, one source

All content derives from the embedded index (`diagnostic::explain`) —
compile-time static, no user input in rendered markdown, offline always.

1. **Hover augmentation (bounded).** When the hover position intersects a
   coded diagnostic, append the index section's **first paragraph only**
   (meaning line, not examples — hover real estate is precious) plus "run
   `nml explain NML2007` for the full entry," after normal hover content.
   The paragraph extraction lives beside `explain()` in
   `nml_core::diagnostic` (`explain_summary`) — one splitter, never
   re-derived per consumer.

   **Tier-1 converged design** (three grounding rounds, 2026-07-23 —
   every point below is grounded in code or measured on the index):

   - **Cache:** per-document, keyed by `uri`, entry carries the source
     **text it was computed from** — reads validate `entry.text ==
     current` (stale-insert race: an in-flight compute finishing after
     `on_change` must read as a miss, not serve stale ranges). No LSP
     version numbers (`on_change` discards them today; text identity is
     exact with zero plumbing). Invalidated on change/close; filled
     lazily by whichever consumer computes first. Side effect banked:
     the pull handler's *Unchanged* path stops re-validating (today it
     recomputes fully just to derive `result_id`).
   - **Four invalidation sources, not three** (the fourth found by the
     existing e2e harness, not by design review): document text (entry
     compare), schema-registry rebuilds and project-config changes
     (wholesale clears at their choke points), and **resolution state**
     — an out-of-band `schema sync` or on-disk manifest change alters
     diagnostics with no buffer edit. The resolver carries a monotonic
     **generation** bumped whenever a stat/fingerprint guard observes
     real change; entries record it, and reads run the (cheap,
     stat-guarded) resolve first so the generation is current. Pinned by
     `out_of_band_store_publish_heals_on_repull`.
   - **Store events:** `validate_document → resolve_document` queues
     store-health events; the fill path drains them (same
     `drain_store_events`, either consumer) so delivery stays prompt.
   - **Compose point, by construction:** the existing hover body (≈40
     exits, including the `(0,0)` binding-summary early return — where
     file-start diagnostics live) becomes `hover_base(…) ->
     Option<Hover>` untouched; the public `hover()` is base +
     augmentation + compose. No base hover + a coded diagnostic ⇒
     explanation-only hover with the **diagnostic's range**.
   - **Multi-diagnostic policy:** narrowest range first, dedup by code,
     cap 3.
   - **Links (the formerly open question, now measured):** exactly 2 of
     82 first paragraphs carry relative markdown links — the splitter
     strips relative links to their text and keeps absolute ones (useful
     at publish day). No length cap: measured max 544 chars (NML2026),
     median far below; the sections are review-guarded content.
   - **Coverage:** all 82 coded sections become hover content on day
     one.
2. **Full entry in-editor: the content-provider pattern.** A new custom
   request `nml/explain { code } → { markdown }` (argument validated by the
   same `explain()` lookup; unknown → null), plus an extension
   `TextDocumentContentProvider` on an `nml-explain:` URI scheme, plus a
   diagnostic-ranged code action "Explain NML2007" that opens
   `nml-explain:NML2007.md` as a rendered document. VS Code-native shape;
   the scheme structurally cannot read arbitrary files.

   **Tier-2 converged design** (four grounding rounds, 2026-07-24 — every
   point grounded in code or measured on the index; all verified by the
   implementation's tests):

   - **Negotiation, not assumption:** the action is emitted only to
     clients that declared a command id
     (`initializationOptions.explainCommand`, read at `initialize` beside
     the existing capability reads) — an editor without the command never
     receives an unexecutable action. Any client can declare its own id;
     helix/neovim keep hover + CLI. Pinned by
     `explain_code_action_is_negotiation_gated`.
   - **One composer:** `explain_document` joins `explain`/`explain_summary`
     in `nml_core::diagnostic` — the CLI's `nml explain` and the LSP's
     `nml/explain` render the identical document. All derivations ride one
     private `sections()` iterator. The heading interpolates the **matched
     section head**, never the caller's string (injection-proof by
     construction); the link policy applies line-wise outside fences.
   - **The same-binary invariant:** explanations come from the exact
     server that emitted the diagnostic — a provider tool explains with
     its own embedded index — so error↔explanation version skew is
     structurally impossible, in every channel, offline. (Tier 3's web
     links inherently lose this; one more reason they stay garnish.)
   - **Action semantics:** derived purely from the round-tripped
     `context.diagnostics`, filtered to `source == "nml"` (other
     extensions' diagnostics share ranges), deduped by code, appended
     after real quick-fixes, **kind stays empty** — it fixes nothing, and
     a client filtering `only: [quickfix]` must not receive it.
   - **Discoverability (the round-2 win):** `nml/explainIndex {} →
     [{ code, summary }]` — derived from the index itself, so the
     test-only `codes::ALL` stays test-only — feeds the "NML: Explain a
     Diagnostic Code" palette QuickPick (searchable by code *or* summary;
     deliberately flat — band grouping would promote the allocation bands
     into wire API) and `nml explain --list`.
   - **Wire conventions:** `schema_info`'s — tolerant `Value` params,
     malformed input answers as data (`{"error": …}`), unknown code is
     `null` (a miss, not a fault), case-normalized at the boundary.
   - **Degradation is readable:** every provider failure (no client, an
     older server without the methods) renders as a markdown document
     pointing at the CLI — the user asked to read something; they never
     get a thrown error.
   - **Content findings (round 3):** the index's own relative links had
     broken in its docs/errors/ → assets/ move (fixed, normalized to the
     asset home); a docs-test guard now resolves every relative link, and
     a unit tripwire (`index_sections_are_fence_safe`) guarantees no
     fenced line can ever truncate a section for any consumer.
   - **E2E proof, both wires:** the Rust harness pins the JSON-RPC
     contract; the extension suite drives provider → custom request →
     real WASM server → canonical `# NML0013` document, and asserts the
     negotiated action, in a real headless VS Code.
3. **`codeDescription.href`** joins at publish day (anchors exist) — web
   link as garnish, never the meal.

## 2. Documentation (required)

`docs/reference/editors.md` gains the explanation surfaces; CHANGELOG; the
extension walkthrough demonstrates hover + explain.
