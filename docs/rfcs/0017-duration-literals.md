# RFC 0017 — Duration Literals (`30s` as a value, not a string)

- **Status:** Implemented (2026-07-26, from v2.1 — twice revised after
  execution-backed review; §9 records what changed and why). All eight
  rollout steps landed, including `nml fix` and the repo migration
  (fixer-applied for `.nml`, hand-edited for prose and type-level Rust).
  nudge adoption remains the follow-up its §7 describes.
- **Date:** 2026-07-26
- **Crates:** nml-core (parser, value decode, types, de, diff, fmt),
  nml-validate, nml-lsp, nml-cli
- **Docs in scope:** `spec/syntax.md`, `spec/types.md`, `spec/models.md`,
  `docs/language-guide.md`, `crates/nml-core/assets/error-index.md`,
  `editors/vscode/syntaxes/nml.tmLanguage.json`

## Summary

A duration becomes a **literal**, and — the part that makes it worth doing — a
duration-typed field holds a **typed duration however it was authored**:

```nml
service Api:
    requestTimeout = 30s          # literal
    retryBackoff   = 250ms
    sessionTtl     = $ENV.TTL     # resolved, then coerced to a duration
```

The parser produces `Value::Duration` for literals, and `de` coerces the
string shape `$ENV` resolution produces — joining the coercion family nml
already maintains for numbers and bools (§3.1). So a consumer receives a
`std::time::Duration` from every provenance at deserialization, and literals
compare semantically (`30s == 30000ms`) in the AST — where the reload differ
actually reads (§3.1 states the provenance asymmetry precisely).

## Motivation

**1. Semantic equality, which the reload path needs.** nudge's reload
classifier (its RFC 0032) diffs configuration to choose live-reload versus
restart. `"30s"` → `"30000ms"` is no semantic change, but a string diff
reports one and forces a needless restart. Typed durations compare by value.

**2. One grammar, not one per repo.** nml and nudge each centralize duration
parsing — nml's `parse_duration`, nudge's `parse_duration_str`, each
deliberately single-sourced within its own tree — but the two grammars
**already disagree**: nudge accepts `d` (days) and a bare integer (`"30"` =
30 s); nml accepts neither. The divergence is invisible today because the
value is a string on both sides. A literal makes the language the single
authority and the mismatch a compile-time fact rather than a runtime
surprise. (What that means for nudge's adoption, stated honestly: §7.)

**3. Consumers stop re-deriving.** A duration-typed field deserializes to
`std::time::Duration` (§6) instead of arriving as text each call site
interprets.

**4. It is the last scalar left behind.** Money is exact integer minor units;
RFC 0016 made numbers exact decimal128 with error-not-round, and numbers and
bools carry `de` coercions so `$ENV` cannot strip their typing. (Money does
not — it stringifies in both directions at `de`, having no std type to land
in; its `$ENV` story is symmetric-but-stringly, a gap this RFC notes and does
not widen.) Duration — a quantity people compare and convert — remains
unparsed text with no coercion, the only primitive still described in its own
doc comment as a "tooling hint."

*Not claimed:* cross-field ordering constraints (`retryMax > retryMin`).
`Value` implements no `Ord` and nml has no constraint syntax, so typing
durations does not make that expressible. It becomes *possible* for a future
constraint layer; it is not delivered here.

## Design

### §1 Grammar: money's shape, one new predicate

A duration literal is `Number Unit` where `Unit` ∈ {`h`, `m`, `s`, `ms`}.
Whitespace between them is insignificant, as for money.

**No lexer change.** `19.99USD` already lexes as `Number Ident` with no
separator, so `30s` does too. The only gate is the parser's `at_currency` →
`is_currency` (`cst/parser.rs:1008`, `:1109` — `len() == 3 && all
ascii_uppercase`). This RFC adds `at_duration` → `is_duration_unit` at that
same site, which buys two properties for free:

- **Newline scoping.** `at_currency` is `at(Ident) && !newline_before() && …`,
  pinned by `currency_does_not_cross_newline`. Adding the duration predicate
  there inherits it — and it matters *more* here than for money: a field
  named `s`, `m`, or `h` is plausible in real configuration, while one named
  `USD` is not, so a cross-line join would be live corruption. An equivalent
  `duration_does_not_cross_newline` pin ships with this RFC.
- **Disambiguation, total and structural.** Currency is exactly three
  uppercase letters; duration units are one or two lowercase. Disjoint by
  construction, no lookahead:

| Suffix shape | Meaning |
|---|---|
| 3 × `[A-Z]` | currency code |
| `h` `m` `s` `ms` | duration unit |
| anything else | `NML3004` (§5) |

**One predicate pair, one home.** `cst/value.rs`'s decoder does *not*
re-check `is_currency` — it treats any trailing `Ident` as a currency,
trusting the parser's gate. Both predicates therefore live in a single shared
module used by `cst/parser.rs` *and* `cst/value.rs`. Duplicating them would
recreate exactly the two-parsers-in-lockstep problem motivation §2 attacks.

**Case is meaningful.** `30S` is neither currency nor duration: `NML3004`
with a machine-applicable fix to `30s`. Rejecting rather than case-folding
keeps one spelling per value and leaves `M`/`m` unambiguous forever — a
language accepting both would owe an answer to "is `30M` minutes or months?",
and this one never has to.

### §2 Value: a typed quantity, with a hand-written equality

```rust
/// Exact duration: the authored integer magnitude and the authored unit.
/// Storage is faithful (never rescaled) so `fmt` renders `72h` as `72h`;
/// comparison is semantic via `total_nanos`.
#[derive(Debug, Clone, Copy)]
pub struct Duration { magnitude: u64, unit: DurationUnit }
```

**`PartialEq`, `Eq`, `Hash`, and `Ord` are MANUAL, over `total_nanos()` —
never derived.** This is the highest-risk line in the RFC, because `Money`
*derives* `PartialEq` (`money.rs:6`) and `Value::semantic_eq`'s fall-through
is `(a, b) => a == b` (`types.rs:170`). An implementer copying the money
precedent gets `30s != 30000ms`, the differ reports a spurious change, nudge
restarts needlessly — and **nothing fails to compile and no existing test
goes red**. The template to copy is `Number`, whose `Ord`/`Eq`/`Hash` are
hand-written over `normalized()` (`decimal.rs:750`, `:825`, `:829`). A pin
sits beside the existing `semantic_eq_numeric_pairs` test asserting
`30s ≡ 30000ms ≢ 31s`.

**The value domain is bounded at construction, so conversion cannot fail.**
`total_nanos() -> u128` is the comparison basis and is always safe
(`u64::MAX` hours ≈ 6.6e31 ns, far under `u128::MAX`). But
`std::time::Duration::MAX` is `u64::MAX` seconds, which `u64` *hours* and
*minutes* overflow. Rather than leak a fallible conversion into every
consumer, the decoder rejects out-of-domain magnitudes at parse time —
precisely as money rejects an amount exceeding `i64` minor units with
`NML3003`:

- **Domain:** `total_nanos() <= std::time::Duration::MAX`. Outside it:
  `NML3006`.
- `as_std() -> std::time::Duration` is therefore **infallible by
  construction**.
- The same check closes the magnitude-width gap: RFC 0016 admits integers up
  to 34 significant digits, so `12345678901234567890123s` is a valid `Number`
  that is not a valid duration. It is `NML3006` — not a panic, not a silent
  `None` from `Number::to_u64()`.

**Durations are unsigned.** `-30s` is `NML3006`. This *diverges* from money,
which permits negatives (`refund = -19.99USD` parses today), deliberately: a
negative `std::time::Duration` does not exist, and elapsed time has no
meaningful sign in configuration. The parser's `Dash` arm routes into the
same numeric path, so the diagnostic must be explicit rather than assumed.

**Zero is valid.** `0s` and `0ms` are legal and semantically equal.

### §3 Validation: a type check, not a format check

`NML2029` ("invalid duration \"…\": expected an integer with unit h, m, s, or
ms") exists only because format could not be trusted before validation. The
parser now guarantees it, so:

- **Format errors move to the literal layer** (`NML3004`, `NML3005`,
  `NML3006`) with spans on the offending suffix or magnitude.
- **`NML2029` is retired to a tombstone** — a diagnostic that cannot fire is a
  lie in the error index. Four sites: `diagnostic.rs:255`, the emitter
  (`nml-validate/src/schema.rs:2242`), `assets/error-index.md:817`, and its
  tests (`schema.rs:6162`).
- **A non-duration in a duration field is `NML2008`** (type mismatch — the
  code a string in a money field gets today), not a special case.

`PrimitiveType::Duration` and the `duration` keyword **survive unchanged**
(`types.rs:216`, `:233`); only `value_matches_primitive`'s arm flips. Schema
defaults use the literal: `requestTimeout duration = 30s` (verified — schema
defaults run the same value decoder money's defaults do).

### §3.1 Provenance-agnostic typing: extend `de`'s coercion family

A duration literal alone would be a **capability regression**, because two
supported shapes arrive as strings by construction:

- `sessionTtl = $ENV.TTL` — `resolve.rs:121` resolves a secret/reference to
  `Value::String` by design ("Resolved secrets become plain `Value::String`s").
- `timeout = "{{ env.T }}s"` — a template string, which
  `validate_primitive_value` deliberately skips format-checking today.
  (Precision: nml's resolver passes `TemplateString` through **unchanged** —
  `resolve_template_string_passthrough` pins it — interpolation belongs to
  the consumer at runtime. So the coercion below covers whatever *text*
  arrives; a raw template reaching `de` is an error for durations exactly as
  it already is for numbers. No new rule, the family's existing one.)

Both work now, neither is mechanically convertible to a literal, and the hole
is **not duration-specific**: `value_matches_primitive` blanket-accepts
`Reference | Secret` for *every* primitive, so `$ENV` erases typing for
number and bool fields too — theirs repaired at `de` by their coercions,
which is precisely the pattern durations join.

**nml already solved this, in the right layer, and durations must follow that
pattern rather than invent a second one.** `de` carries a coercion family —
`coerce_to_number`, `coerce_to_bool` — whose own doc states the rationale
verbatim: *"env vars resolve to `Value::String`, so `$ENV.PORT =
"9007199254740993"` must survive exactly"* (RFC 0016 §1.4, "Postel for
machine-emitted data"). It coerces at the one place that knows the **Rust
target type**, which is strictly more information than a schema pass has.

> **`coerce_to_duration` joins that family.** A duration-typed target accepts
> `Value::Duration` directly, or a `Value::String`/`Value::Secret` whose text
> parses as a duration — the same shape, the same failure style, the same
> security rule.

That security rule is inherited, not reinvented, and it matters: coercion
failures embed the *reason* but never the value, because "the resolver erases
provenance … ANY coerced string could be a resolved secret — echoing content
here would leak credentials into logs." A duration coercion that echoed
`$ENV.TTL`'s text would be a credential leak in a log line; following the
family means it cannot be.

**What this deliberately does not extend to: the differ.** A `$ENV`-fed
duration remains `Value::String` in the AST, so it diffs textually while a
literal-authored one diffs semantically. That asymmetry is correct rather than
regrettable — an `$ENV` value's source of truth *is* text, and an operator who
edits the variable changed that text. The reload win (motivation §1) lands
where reload actually reads: literals in `.nml` files. Making the AST itself
provenance-blind would require threading schema types into `resolve.rs`, which
has none — a new pass, a second coercion grammar, and a contradiction of the
DRY principle this section exists to honor.

### §4 Migration: one spelling, mechanically applied

Quoted duration **literals** become literals (`"30s"` → `30s`). Strings that
*cannot* be literals (`$ENV`, templates) are not migration targets — §3.1
handles them.

`NML0001` ("Replaced syntax … machine-applicable") plus a migration-ledger
entry is the existing mechanism, as RFC 0006 used for `=>` → `->`.

**The fixer must be schema-aware**: `"30s"` is a perfectly valid *string*, so
only a duration-typed field's value may be rewritten. That places it in
nml-validate (which has the schema), not nml-fmt (which does not).

#### §4.1 `nml fix` — the applier this migration requires

The CLI has `check` and `fmt`; there is **no** batch applier, so `NML0001`'s
promise currently terminates in an editor — and two prior migrations
(`=>` → `->`, `&&` → `&`) have waited on one. RFC 0006's was applied **by
hand**. This RFC ships the missing half:

```
nml fix [--schema <dir>] [--dry-run] <path>...
```

- **Applies a suggestion only when it is the sole candidate for its span.**
  RFC 0015 established that N mutually-exclusive fixes must never auto-apply;
  that rule holds. *Required upstream change:* `error.rs:475` classifies a
  replacement as `SuggestionKind::Fix` only when it is empty or whitespace, so
  `->` and `30s` are `DidYouMean` today. Either that gate widens or the
  applier keys on sole-candidacy rather than kind — this RFC chooses
  **sole-candidacy**, because kind should describe exclusivity (RFC 0015's
  axis), not applicability.
- **Splices highest-offset-first** within a file so earlier edits cannot
  invalidate later spans. No span-splice primitive exists yet (`cst/edit.rs`
  exposes only `insert_entry_at_path`); this adds one.
- **Re-checks after writing and reverts** if the diagnostic count did not
  drop — a fixer that can worsen a file is worse than none. Writes go through
  the existing atomic writer (`main.rs:452`).
- `--dry-run` prints a unified diff. (`diagnostic.rs:62` anticipates
  `--apply`; `--dry-run` is chosen so the default is the safe direction and
  the flag names the deviation.)
- New CLI surface: multi-path arguments and tree walking (`check` takes
  exactly one file today).

Benefit beyond durations: every ledger migration becomes bulk-appliable, and
`docs/stability.md:22`'s claim that "`nml fmt` … applies it for you" — false
today, since `format_source` errors on any parse error — becomes true of
`nml fix`.

**Measured surface, split by how it can actually be fixed:**

| Class | Count | Mechanism |
|---|---|---|
| `.nml` duration literals | ~34 | `nml fix` |
| Markdown prose/tables | ~27 | hand-edited (no fence-aware tool reaches table cells) |
| Rust test literals | ~18 (16 in `#[cfg(test)]`) | hand-edited; several **type-level**, e.g. `resolve.rs:979` asserts `Value::String("30s")`, `defaults.rs:1201` asserts a `String`-typed serde target |
| Deliberately-invalid fixture | 1 | **must not** be "fixed" (`tests/fixtures/invalid/bad-duration-default.model.nml`) |

RFC 0006's record warns never to regex-migrate near Rust; that warning applies
here.

### §5 Diagnostics

New codes land in the **3000–3999 band** (values & money) beside money's
`NML3000`–`NML3003`, because they come from the same decode layer
(`cst/value.rs`) and are exact analogues. The band comment
(`diagnostic.rs:113`) becomes "values, money & durations".

| Code | Condition | Fix |
|---|---|---|
| `NML3004` | unknown unit (`30x`, `30S`, `30sec`) | nearest of `h`/`m`/`s`/`ms`, machine-applicable when unambiguous |
| `NML3005` | non-integer magnitude (`30.5s`) | ~~equivalent in a finer unit (`30500ms`)~~ granularity-preserving compound (`30s500ms`, §10) |
| `NML3006` | out of domain: negative, or `total_nanos` > `Duration::MAX` | state the bound |
| `NML0001` | quoted duration literal in a duration field | the literal (`"30s"` → `30s`) |
| `NML2008` | non-duration value in a duration field | ordinary type mismatch |
| ~~`NML2029`~~ | *retired* → tombstone | points at `NML3004`/`NML3005` |

**Precedence:** `30.s` is caught by `NML0013` (trailing dot) *before* duration
decoding, exactly as `19.USD` is today. `NML3005` covers only well-formed
non-integers like `30.5s`.

### §6 Consumers

- **`de`** — a duration deserializes into `std::time::Duration` via
  `coerce_to_duration` (§3.1), accepting the typed variant *or* a coerced
  string. Mechanically, `deserialize_struct` is overridden for
  `name == "Duration"` to synthesize the `{secs, nanos}` map serde's stock
  `impl Deserialize for std::time::Duration` requests. This *deliberately
  differs* from money, which stringifies (`de.rs:900`:
  `visit_string(m.format_display())`) — money has no std type to land in;
  durations do, and landing there is the point. `"Duration"` is a public,
  collidable struct name (unlike the private `NUMBER_NEWTYPE_TOKEN` handshake
  at `de.rs:1040`); a user struct so named receives `{secs, nanos}` and fails
  with a field error — acceptable and documented. `Value::as_duration()` and
  `TryFrom<&Value> for std::time::Duration` ship alongside for direct use.
- **`diff`** — semantic equality means `30s` → `30000ms` is *no change*
  (motivation §1). `Ord` is not required by the differ (`lcs_pairs_by` takes
  an `eq` closure); it exists for future constraints.
- **`fmt`** — canonical form is **`30s`, attached**. This *diverges* from
  money, whose canonical form is spaced (`nml fmt` rewrites `19.99USD` →
  `19.99 USD`, matching `spec/syntax.md`'s examples). Deliberate: a currency
  code is a noun following a quantity, while a duration unit is a suffix in
  universal convention (systemd, Go, Prometheus, ISO-8601 all attach). Note
  `fmt` renders from the **decoded value** (`formatter.rs:521`), not the CST —
  so the authored *unit* survives because `Duration` stores it, while
  magnitude spelling is normalized (`030s` → `30s`). Unlike money, durations
  are never rescaled (`72h` stays `72h`, never `259200s`).
- **`lsp`** — hover shows the normalized total (`30s — 30,000ms`); completion
  offers unit suffixes after a bare number in a duration-typed field. Both
  need the catch-all arms at `server.rs:3234` (`format_value`) and `:2904`
  (`render_scalar`), which today render `...` / `None`.
- **Serialization** — `Value` derives `Serialize` and `nml parse` emits it as
  documented public JSON, so the wire shape is an API decision, not an
  accident: ~~`{"Duration":{"magnitude":30,"unit":"s"}}`~~ (amended with
  §10's compound storage to a canonical segments array —
  `{"Duration":{"segments":[{"magnitude":30,"unit":"s"}]}}`), with
  `DurationUnit` gaining `Serialize` (it has none today) and the unit
  rendered as its suffix.
  The CLI's wire-format rustdoc and its JSON pin
  (`tests/integration/cli_tests.rs:121`) update with it. There is no
  `Deserialize for Value`; the format stays one-way.
- **Editor grammar** — `nml.tmLanguage.json` has a `money-literal` rule and no
  duration rule while already listing `duration` as a type keyword (so the gap
  looks done at a glance). A `duration-literal` rule ships with this RFC.

### §7 Relationship to the 0.1.0 removal, and to nudge

**This RFC reverses a decision made in the current unreleased cycle**, and
says so plainly. `CHANGELOG.md` records `Value::Duration` and `Value::Path`
removed because "both variants were unreachable by construction … no code path
ever produced either variant." That premise was **correct**: durations had no
grammar, so nothing could construct them. This RFC removes the premise by
adding the grammar. The CHANGELOG entry is *reversed*, not appended — and
`Value::Path` stays removed (§8).

**Also relevant:** `Value` is not `#[non_exhaustive]` (though the crate uses
that attribute elsewhere), so adding a variant is semver-breaking for every
downstream matcher — including nudge's, which matches `Value` out of workspace
and is therefore *not* caught by nml's own `cargo check`.

**nudge adoption is a follow-up with its own breaking change, and this RFC
delivers none of nudge's win by itself.** nudge declares *zero*
`duration`-typed fields — its duration-shaped values are typed `string`
(`controlplane.model.nml:12 delay string = "5s"`,
`project.model.nml:62 interval string = "10s"`), so the migration is a no-op
there. When nudge re-types those fields, two of its accepted spellings stop
parsing: **`d` (days)** — used by `server.model.nml:395`'s `rotateAfter
"30d"` — and **bare integers** (`"30"` = 30 s). Either nml admits `d` (§8
argues against) or nudge converts (`30d` → `720h`). That decision belongs to
the adoption change, not this one — as does a one-time operational note: the
migration edit itself (`"30s"` → `30s`) changes the value's *variant*
(`String` → `Duration`), which the differ correctly reports as a change, so
each migrated field takes one restart-classified reload at adoption. Semantic
equality begins on the far side of that edit; it cannot retroactively cover
the edit that introduces it.

### §8 Non-goals, with reasons

- **No arithmetic** (`30s + 5m`). Values, not expressions; an expression layer
  is a separate RFC.
- **No calendar units** (`d`, `w`, `mo`, `y`). A day is not always 86,400
  seconds and a month has no fixed length; a configuration language that
  pretends otherwise ships timezone bugs to its users. `d` is the live request
  (nudge accepts it) and the answer is still no *without* a calendar model —
  `720h` is exact and unambiguous.
- ~~**No multi-unit or fractional forms** (`1h30m`, `1.5h`). Go and Prometheus
  accept both; this RFC takes the stricter subset for the reason RFC 0016
  chose error-over-rounding: one spelling per value, no accumulating parse
  rules, a diagnostic instead of a guess.~~ — **superseded post-
  implementation** (2026-08-01, §10): compound multi-unit literals
  (`1h30m`) were admitted; fractional magnitudes remain rejected in
  *source* (NML3005, now suggesting the compound respelling) while
  *coercion* text decomposes exact fractions (`"1.5h"` → `1h30m`),
  completing Go/Prometheus superset parity on the machine-input side
  without weakening the authored grammar.
- ~~**No sub-millisecond units** (`us`, `ns`)~~ — **superseded post-
  implementation**: `us` and `ns` were admitted (2026-07-26) precisely
  because the "future-additive" claim above was true for the *grammar*
  but false for the *Rust enum* — adding a variant later would break
  every downstream exhaustive match. Admitting them immediately closed
  the unit set **permanently at both ends**: `ns` is the resolution of
  the value domain itself (`total_nanos`, `std::time::Duration` — no
  finer unit can represent a representable value), and `h` is the
  largest exact unit (everything coarser is the calendar exclusion
  above). `DurationUnit` is therefore complete by construction, stays
  exhaustively matchable forever, and needs no `#[non_exhaustive]`
  hedge. ASCII `us` (the source-character policy rejects `µ` raw, as
  Go's ASCII fallback anticipates). The same change admitted `_` digit
  separators in all numeric literals (digit-flanked only, stricter than
  Rust; `NML0013` with a strip fix when misplaced; fmt canonicalizes
  them away — spelling, never value).
- **No `Value::Path` revival.** A path's meaningful typing is *safety*
  (containment, traversal rejection, portability), none of it knowable at
  parse time — no root, no filesystem context, and parsing must never touch
  the filesystem. That belongs in the consumer's newtype (nudge has
  `PortableProjectPath` and `open_within`). Symmetry with durations would be a
  mistake: durations have a total order and exact arithmetic; paths have
  neither.

## Prior art

`systemd` time spans, Prometheus intervals, and Go's `time.ParseDuration` all
use attached suffixed literals — the basis for §6's canonical form. Go and
Prometheus accept fractional and multi-unit values; §8 declines both. HCL and
JSON have no duration type, which is the status quo this leaves behind.

## Rollout

1. **`Duration` + `Value::Duration`** in nml-core: `#[derive(Debug, Clone,
   Copy)]` with **manual** `PartialEq`/`Eq`/`Hash`/`Ord` over `total_nanos()`;
   domain check; `as_std()`; `as_duration()`/`TryFrom`. Property tests for the
   equality/ordering laws and the domain boundary.
2. **Parser + decoder**: shared `is_currency`/`is_duration_unit` module;
   `at_duration` beside `at_currency`; decode arm; `NML3004`–`NML3006` with
   fixes; `duration_does_not_cross_newline` pin.
3. **Validation**: `duration` becomes a type check; `NML2029` retired across
   its four sites. **`coerce_to_duration`** joins `de`'s coercion family
   (§3.1) — beside `coerce_to_number`/`coerce_to_bool`, inheriting their
   never-echo-the-value security rule.
4. **Consumers** — each with the silent catch-all it must not miss:
   - `nml-validate/src/schema.rs:2629` — the `PrimitiveType::Duration` arm is
     a `matches!`, so a miss yields the nonsense *"expected duration, got
     duration"*.
   - `diff.rs:246` `render_key` — `other => format!("{other:?}")` would leak
     `Duration { magnitude: 30, unit: Seconds }` into operator output; the
     adjacent `Value::Secret` arm exists to stop exactly this class.
   - `nml-validate/src/schema.rs:2744` `duplicate_clarifier` and `:2782`
     `value_label` — `[30s, 30000ms]` in a `set<duration>` is correctly a
     duplicate, but reports two visibly different literals with no clarifier;
     duration is the same case as Number.
   - `nml-lsp/src/server.rs:3234`, `:2904` — hover / `render_scalar`.
   - Compiler-caught (safe): `types.rs:261`, `de.rs:865`, `formatter.rs:521`,
     `schema.rs:2786`.
   - Out of workspace, **not** caught by nml's `cargo check`:
     `nudge/src/types.rs:5798` `value_type_name`.
5. **Serialization** shape + `DurationUnit: Serialize` + CLI wire pin.
6. **`nml fix`** (§4.1) — span-splice primitive, sole-candidacy rule,
   re-check-and-revert, multi-path CLI.
7. **Docs**: error-index sections (CI-verified examples), `spec/syntax.md`,
   `spec/types.md`, `spec/models.md`, `docs/language-guide.md:138`'s type
   table, TextMate grammar, CHANGELOG reversal, migration-ledger entry.
8. **Migrate the repo with the fixer** — dogfooding it as the proof — then
   hand-edit the prose and type-level Rust sites per §4's table.

## §9 What changed under review

An execution-backed review found 11 defects and 15 gaps in v1. The substantive
ones, recorded so the reasoning is not lost:

- **`$ENV` and templates would have broken** — resolved secrets *are*
  `Value::String`, so deleting the string path was a capability regression.
  → §3.1, which v2 solved with a new pass and v2.1 replaced with the existing
  `de` coercion family (see below).
- **`as_std()` was unsound** and magnitude width unspecified (a valid 34-digit
  `Number` is not a valid `u64`). → one domain check at decode, mirroring
  money's `NML3003`; conversion is infallible by construction.
- **`PartialEq` would have been derived** by anyone copying `Money`, silently
  killing motivation §1 with no red test. → §2 mandates manual
  implementations and names `Number` as the template.
- **Wrong codes, wrong band** — `NML2003` is `UNKNOWN_DISCRIMINANT` (type
  mismatch is `NML2008`), and the new codes belong in the 3000s beside money's,
  not the 0000s. Codes are never renumbered once published, so this correction
  was free only before implementation.
- **`fmt`'s canonical form was backwards** — money's canonical form is
  *spaced* (`nml fmt` adds the space), so "no space, mirroring money" was
  self-contradictory. → attached form, with the divergence argued rather than
  assumed.
- **The losslessness argument was false** — `fmt` renders from the decoded
  value, not the CST.
- **Motivation overstated the disorder** — nudge has one centralized parser,
  not 17 re-derivations; the real defect is one cross-repo grammar mismatch.
  And "zero duration-typed fields" was true only because those fields are
  typed `string`, which means nudge's win arrives with its adoption change,
  not this RFC.
- **`nml fix` was vapor**, and its trigger was wrong: `NML0001` emits
  `DidYouMean`, not `Fix`, for non-whitespace replacements. → §4.1 specifies
  the command and keys on sole-candidacy.
- **The 0.1.0 removal went unacknowledged** — this RFC reverses a decision from
  the current cycle and now says so.

**v2.1 — the simpler design won.** v2 proposed a new schema-aware coercion
pass at the resolve→validate boundary. A further review found that `de`
**already** carries a coercion family (`coerce_to_number`, `coerce_to_bool`)
built for precisely this problem, documented with the same rationale
(`$ENV` resolves to `Value::String`) and carrying a security rule the new pass
would have had to reinvent (never echo a coerced value — it may be a resolved
secret). Durations now join that family instead: fewer moving parts, one
coercion grammar rather than two, and typing applied where the *Rust target
type* is known rather than merely the schema type. The pass also would not
have bought what it appeared to — see §3.1's differ note. The lesson worth
keeping: when a language already solves a problem in one layer, a second
mechanism is not thoroughness, it is drift.

## §10 Amendment: compound literals (2026-08-01)

§8's "no multi-unit forms" bullet is superseded. The single-component
grammar shipped, and the first real-world request was the form Go,
Prometheus, and systemd all accept: `1h30m`. The original argument — one
spelling per value — was already surrendered by unit choice (`90m` and
`5400s` were both legal); compound components add expressiveness, not
ambiguity, and canonicalization keeps one *rendered* spelling per
authored granularity.

**Grammar.** A duration literal is one or more `magnitude unit`
components (`1h30m`, `5m2s`). The lexer gains the **suffix-run rule**: an
identifier starting immediately after an ASCII digit scans letters only,
so `1h30m` tokenizes as alternating `Number`/`Ident` without touching
`sha256` or `19.99USD`. Zero components are spelling and drop (`1h0s` →
`1h`); all-zero is the canonical zero `0s`. No sign, no fractions in
source, no cross-unit carry (`60m` stays `60m`), rendering is attached
coarse→fine.

**Two policies, one build path.** Authored source is strict: a repeated
unit is **NML3007** with the merged whole-literal fix (`1h2h` → `3h`); a
dangling magnitude is **NML3008** (`1h30`, related-span on the break);
fractions remain **NML3005** — the fix now respells at the authored
granularity (`1.5h` → `1h30m`, `30.5s` → `30s500ms`; a compound reads
like what a person at that scale would write) and, where a machine fix
would drop sibling components, is withheld. Coercion
text (`de`'s family, `$ENV`) is Postel for machine-emitted input:
duplicates merge, exact fractions decompose (`"1.5h"` → `1h30m`,
`"0.5ms"` → `500us`; inexact `"0.5ns"` still rejects) — a Go superset,
per-component whitespace tolerated (`"1h 30m"`).

**Storage and wire.** `Duration` stores canonical integer segments
(coarse→fine, ≤ one per unit — the six-slot bound is structural) with
`total_nanos` cached at the construction gate; equality, ordering, and
hashing stay semantic over the total. The wire shape is the segments
array (see §6 as amended); pre-1.0, the break is recorded in the
CHANGELOG, not dual-shipped.

**Tooling (LSP).** Duration literals are a first-class CST node
(`DurationLiteral`); every editor surface derives from one query seam
(`duration_literal_at` / `duration_literals_in`). Hover and document
highlight are range-accurate over the literal (tight component spans,
UTF-16 positions); hover shows the breakdown with its human respelling
(`1h + 30m = 90m`) and the machine-comparable total. Signed literals
(`-5s` — durations are unsigned) highlight in full including the sign,
but hover, hints, and completions stay silent rather than present a
positive total for domain-invalid source. Unit completion is
CST-driven: bare magnitudes in duration-typed values and facets,
mid-compound segments with finer-unit ranking only, composed-literal
previews, verbatim digit separators (`1_000` completes to `1_000ms`),
and out-of-domain units withheld. Compound literals get inlay hints
(`= coarsest exact total`), semantic tokens (magnitudes as `number`,
suffixes as `durationUnit` with `superType: number`, both with the
`duration` modifier), and selection-range expansion.
`NML3005`/`NML3007` quick-fixes and `NML3008` related-information ride
the existing diagnostic suggestion wire unchanged.
