# RFC 0016 — Exact Decimal Numbers

- **Status:** Implemented

## Summary

Replace `Number { Int(i64), Float(f64) }` with a single exact decimal
representation. Every `number` literal becomes exactly the value the author
wrote, or a hard error — and the acceptance rule is *value-based*: a literal
parses if and only if its value is a finite IEEE 754-2019 **decimal128**
value. Binary floating point disappears from the data model and survives
only as explicit, correctly-rounded conversions at the serde/query edge.

This closes the one exactness leak in the language: integers already error
rather than round (`NML0014`), money is integer minor units, the CST is
lossless — but a dotted literal silently becomes the nearest binary64, so
`taxRate = 0.20` does not store `0.20`. After this RFC, "NML numbers are
exact" is true without qualification, and the misleading docs (tutorial
01/02, integration.md, README, `spec/types.md`) become correct prose.

The concrete downstream win is diff fidelity: today `Value::semantic_eq`
compares through f64, so numerically *different* edits beyond 2^53 — e.g.
`9007199254740993.0 → 9007199254740992.0` — falsely classify as cosmetic
in RFC 0032 reload diffing. Exact numbers make `semantic_eq` exactly right:
value changes always classify as changes, and scale-only edits
(`2.50 → 2.5`) remain cosmetic — now with the written scale actually
preserved end-to-end instead of erased at decode.

Positioning: CUE and Ion store config numbers as arbitrary-precision GDA
decimals but either round silently past an implementation bound (CUE) or
are unbounded (Ion, EDN — a DoS surface). nml adopts the GDA value model
with a **published finite contract and an error-on-inexact posture**: the
nml number domain *is* the finite decimal128 value space (minus −0), and a
number is rejected exactly when representing it would be inexact or out of
range — never when its value is representable. No surveyed config language
does both.

> Review note: this is revision 3 (amended). Revision 2 absorbed a
> three-lens adversarial review (numerics/spec, security/feasibility,
> ergonomics/UX); revision 3 absorbed a second round targeting rev-2's
> new material — correcting the storage/clamp rule (now total over
> subnormals and zero), the coercion grammar (liberal data grammar,
> saturating exponent parse, pinned error surface), the foreign-f64
> conversion rule (shortest-round-trip), unified u128 serde ladders,
> the three-variant diagnostic payload, `NumberError`'s definition and
> single shared parse core, the `10^6112` fixture (it must parse), and
> the semantic_eq motivation (scale-only edits are already cosmetic
> today; the f64 defect is false-cosmetic classification of real
> changes). A third round then verified the document two ways: a
> line-by-line code-truth sweep (all ~40 file:line refs exact, all
> numerics recomputed, external citations checked) and a **clean-room
> implementation spike built from this RFC's text alone — 31/31 fixture
> tests passed on the first run** — whose recorded ambiguities are now
> pinned in place (zero's scale floor, error precedence, `integral`'s
> definition, `try_new`/`try_from_f64` contracts, value-level round-trip,
> the `10^100` stored member, wrap-fixture lengths). The spike, synced to
> those amendments and re-verified green, is preserved at
> a scratch crate as the P0 starting point. That spike has since been
> **deleted**: P0 shipped, and an un-gated second copy of the digit
> logic — which by then lagged the shipped code, missing the
> extreme-magnitude conversion guards — is a liability, not a reference.
> `crates/nml-core/src/decimal.rs` is the single home.

## 1. Design

### 1.1 Value space, representation, and the storage rule

The nml number domain is exactly: **finite decimal128 values, excluding
−0** — every value `±c × 10^e` with `c ≤ 10^34 − 1` and quantum exponent
`e ∈ [−6176, 6111]`. NaN and infinities are ungrammatical (digits and one
dot), so the domain is total-ordered and `Eq`-clean.

A new module `crates/nml-core/src/decimal.rs` owns the numeric core.
`Number` becomes a struct; the enum and the int/float distinction are
deleted:

```rust
/// An exact decimal: value = coeff × 10^(−scale).
///
/// Invariants (every constructor validates BEFORE narrowing widths;
/// counts are computed in usize):
///   |coeff| ≤ 10^34 − 1          (≤ 34 significant digits)
///   scale ∈ [−6111, 6176]        (⇔ exponent −scale ∈ [−6176, 6111])
///   coeff == 0 ⇒ scale ∈ [0, 6176]   (no negative-scale zeros — they
///                                     would Display as leading-zero junk)
/// Negative zero is unrepresentable by construction: the sign lives in
/// `coeff`, and two's-complement i128 has no −0.
pub struct Number {
    coeff: i128,
    scale: i16,
}
```

`types.rs` re-exports it (`pub use crate::decimal::Number;`) so the
`nml_core::types::Number` path used by nudge keeps working. Derives:
`Copy` (nudge derefs it — plugin_webhook_config.rs:208), `Clone`, `Debug`;
`Eq`/`Ord`/`Hash`/`Serialize`/`Deserialize` are manual (§1.3, §1.7).
`Number::ZERO` is a public const (P1 uses it for the decode placeholder,
cst/value.rs:128).

**The storage rule (normative, total).** For a nonzero value, let
`c_min` be its coefficient with all trailing zeros stripped, `d` the
digit count of `c_min`, and `s_min` the matching scale (the *normalized*
form — unique per value). The value's constructible scales form the window

```
W = [ max(s_min, −6111),  min(s_min + 34 − d, 6176) ]
```

(each step up from `s_min` appends one trailing zero to the coefficient;
the budget allows `34 − d` of them; both ends clamp to the invariant).
**Acceptance**: the value parses iff `d ≤ 34` and `W` is nonempty; when
both fail (40 significant digits at 10^7000), `TooManyDigits` takes
precedence.
**Storage**: `scale = clamp(written_scale, W)`, where the written scale is
the fraction-digit count of the source form (negative for
exponent-shifted coercion inputs, §1.4). **Zero**: always accepted;
`scale = clamp(written_scale, 0, 6176)`; the coefficient is 0. (The floor
is 0, not −6111: negative-scale zeros — reachable only via
exponent-form coercion like `"0e12"` — would render as leading-zero
strings under the Display algorithm; the invariant forbids the case at
the source, so `"0e999999999999"` stores `(0, 0)`.)

Worked consequences (all are §2 fixtures):

- `2.50` → `(250, 2)`; `8080.0` → `(80800, 1)` — written scale preserved
  whenever it fits; the `{n:.1}` Display hack (types.rs:105) dies because
  the information it faked is now real.
- `10^34` (35 written digits, `d = 1`) → `W = [−34, min(−1, 6176)]`,
  scale `clamp(0, W) = −1` → `(10^33, −1)`. Clamped members render as
  plain integer digits, so no display information is lost.
- `10^6144` → `(10^33, −6111)` parses; `10^6145` → `W` empty → error
  (`TooLarge`). The rejection boundary is the decimal128 maximum
  (≈ 9.999×10^6144), **not** 10^34 or any digit count of the source.
- `0.` + 6150 zeros + `5` + 33 zeros (subnormal band; written scale
  6184): `c_min = 5`, `d = 1`, `s_min = 6151`,
  `W = [6151, min(6151 + 33, 6176)] = [6151, 6176]`, stored scale
  `clamp(6184, W) = 6176` → `(5×10^25, 6176)`. Parses exactly (rev 2
  rejected this representable value).
- `0.` + 6200 zeros → zero, scale `clamp(6200, …) = 6176` → `(0, 6176)`.
- `10^−6177` (`0.` + 6176 zeros + `1`) → `s_min = 6177 > 6176`, `W`
  empty → error (`TooSmall`).

**Why bounded-custom instead of a crate.** nml performs *no arithmetic* on
numbers (verified across the workspace) — only parse, store, compare,
format, convert — so a decimal arithmetic engine is dead weight and
supply-chain surface in a parser handling untrusted multi-tenant config.
Each candidate fails a hard constraint: `rust_decimal` rounds silently in
`from_str` and caps scale at 28; `fastnum` requires MSRV 1.94 (nml pins
1.86); `dec` is C FFI (decNumber); `bigdecimal`/`dashu` are heap-per-value
and need external caps to be DoS-safe. A ~400-line zero-dependency core
(`i128 + i16`, stack-only, `Copy`, O(1) memory per value) is exactly as
large as the published contract. nml-core stays at thiserror + serde +
rowan; **zero new dependencies, including dev-deps** (§2).

### 1.2 The published contract and security posture

- **Acceptance is value-based**: a number is rejected exactly when its
  value is not a finite decimal128 value — too many significant digits
  (inexact), too large, or too small — and never when it is. This is
  stricter than every surveyed system (CUE and rust_decimal round
  silently at their bounds; Ion/EDN are unbounded; IEEE itself rounds on
  conversion) while never rejecting a representable value, extending
  nml's integer posture ("out-of-range is an error, never a silently
  rounded float") to the whole domain.
- **Consequences for integers**: any integer with ≤ 34 significant digits
  and magnitude ≤ the decimal128 maximum parses — `u64`/`u128`-typed
  config fields above `i64::MAX` become expressible for the first time
  (previously unwritable: NML0014 from 19 digits up, past `i64::MAX`).
  Honesty note: `u128`
  coverage extends to values with ≤ 34 significant digits, not
  `u128::MAX` (39 digits) — the spec bound, stated, not hidden.
- **DoS posture**: the literal grammar has no exponent short-form, so the
  `1e1000000000` blow-up class (Jackson `maxNumberLength`, Johnzon's
  scale cap, decimal CVE-2026-32686) is unrepresentable in documents;
  the coercion grammar (§1.4) admits exponents but parses them with
  saturating accumulation — no digit-count limit, no allocation, no
  numeric overflow, regardless of exponent length. Parse work is **linear in source
  length** (the leading/trailing-zero scan is O(n); the caps bound
  per-value *storage and downstream* cost, not the unavoidable single
  scan). All digit/width counts are computed in `usize` and validated
  against the caps **before** narrowing into `i128`/`i16` — a
  `frac.len() as i16` narrowing bug would silently wrap `0.` + 65 538
  digits into scale 2; §2 pins fixtures at both the i16 (32 768) and u16
  (65 536/65 538) wrap boundaries, asserting the error *kind*, to kill
  that class. Error paths bound echoed literals (`echo_capture`, 33
  chars), which is also why diagnostic counts are structured payload
  fields (§1.5), never derived from the echo.

### 1.3 Equality, ordering, hashing (normative)

The survey shows two half-solutions in the wild: Ion's representational
equality (`1.5 ≠ 1.50` — surprising in config dedup), and Java
BigDecimal's `equals`/`compareTo` incoherence (HashMap and BTreeMap
disagree). Python (and post-1.6 Clojure) got it right: **numeric equality
with a coherent hash, plus a total order on representations as an explicit
secondary comparator**. nml commits to that pair in the spec:

- `Eq`: numeric — `1.5 == 1.50 == 1.500`, `2 == 2.0`. Full `Eq` (no NaN).
- `Ord`: numeric total order, `Eq`-consistent (cohort members compare
  `Equal`). **Normative algorithm**:
  1. If both are zero → `Equal`. If one is zero → the nonzero side's sign
     decides. (Zero first: it has no meaningful adjusted exponent; the
     naive rule orders `0 > 0.005`.)
  2. Different signs → negative < positive.
  3. Same sign: compare **magnitudes** by adjusted exponent
     `digits(|coeff|) − 1 − scale` (computed in i32); if equal, align
     coefficients and compare; then **negate the result if both are
     negative**.
  Overflow-freedom: alignment happens only when adjusted exponents tie,
  which forces the scale gap to equal the digit-count gap (≤ 33), so the
  aligned coefficient has ≤ 34 digits < 10^34 ≪ `i128::MAX` ≈ 1.7×10^38.
  Exact, allocation-free, total.
- `Hash`: over the value's **mathematical normalized pair**
  `(c_min, s_min)` (trailing zeros stripped; zero hashes as `(0, 0)`),
  computed arithmetically in `(i128, i32)` — explicitly **not** routed
  through `try_new`: the normalized pair of a valid Number may lie
  outside the constructible window (`10^6144` = `(10^33, −6111)`
  normalizes to `(1, −6144)`), and that is fine — it is a pure cohort
  invariant, which is all a hash needs. `hash(1.5) == hash(1.50)`;
  Eq/Hash coherent; `Number` becomes a valid map key (impossible today).
- `Number::total_cmp(&self, &Self) -> Ordering` — GDA *compare-total*
  restricted to our domain, named per the `f64::total_cmp` convention:
  numeric order first; within a cohort the member with the **larger
  exponent ranks higher for positives** (`12.0 < 12`, per GDA/Python
  `compare_total`), flipped for negatives (`−12 < −12.0`), and zeros
  rank positive-style (`0E+1 > 0E−1` in GDA terms: smaller scale ranks
  higher). Documented deviation from IEEE `totalOrder`: no −0 or NaN
  ranks (unrepresentable). The deterministic tiebreak for
  representation-sensitive consumers.

`Value::semantic_eq` (diff.rs) inherits numeric equality — scale-only
edits stay cosmetic (as today), and value edits beyond 2^53 stop
classifying as cosmetic (today's f64 fall-through equates
`9007199254740993.0` with `9007199254740992.0` — a real
misclassification in RFC 0032 reload diffing, fixed here).

### 1.4 Grammar, parsing, and string coercion

Lexer rules are untouched (digits and dots, `Dash` composition, money's
trailing-currency form); `cst/value.rs::parse_number` remains the single
literal-parse site, now a thin adapter over the shared core (§1.6). Two
grammar-adjacent decisions:

- **Literals: trailing-dot forms are rejected.** `1299.` (and
  `1299. JPY`) lex as one token today and slip through `f64::from_str`;
  the exact parser rejects them as `NML0013` with a machine-applicable
  suggestion (drop the dot). Deliberate legacy-quirk removal; no corpus
  contains one (§4). Authored source stays strict:
  `-? digits ( '.' digits )?`.
- **String coercion is a deliberately liberal data grammar** (`de.rs::
  coerce_to_number`). Env values are machine-emitted *data*, not
  authored literals — Postel for data, strict for source. Today
  `"1e-6"`, `"+1"`, `".5"`, `"1."` all coerce (via the f64 detour);
  the new grammar keeps accepting them, exactly:

  ```
  [+-]? ( digits ( '.' digits? )? | '.' digits ) ( [eE] [+-]? digits )?
  ```

  converted **exactly** — mantissa digits with the exponent folded into
  the written scale (which may go negative: `"1.5e3"` → `(15, −2)`,
  value 1500) — under the same value-based acceptance rule and storage
  clamp as literals. `"1e-6"` → `(1, 6)` exactly, *better* than today's
  f64 rounding. The exponent field parses by **saturating accumulation**
  (leading zeros harmless, `"1e00042"` fine; `"0e999999999999"` → zero,
  accepted, storing `(0, 0)`; `"1e999999999999"` → saturates →
  `TooLarge`): linear, allocation-free, no numeric overflow, no
  digit-count heuristics (accumulator width is immaterial at ≥ i32; the
  reference uses i64). One deliberate liberal-grammar consequence,
  fixtured in §2: `"1.e5"` is valid data (`digits '.' ε` mantissa) →
  `(1, −5)`.
  Intentional tightenings, documented in §4: `"inf"`/`"nan"` (accepted
  today, admitting IEEE infinities into the data model — a latent bug)
  are rejected; >34-significant-digit strings error instead of silently
  rounding.
- **De-band errors locate by field path, never by value.** Redaction
  removed the accidental locator that echoed values provided, so the
  deserializer now builds a path as nested access unwinds — ``field
  `database`: field `port`: u8 value out of range``, ``field `ports`:
  element 2: …`` — strictly better than the pre-RFC state, where these
  errors carried neither a path nor a span (implementation-review
  follow-through).
- **Coercion failures stay in the serde band** (they are data errors at
  deserialization time, not parse diagnostics): `coerce_to_number`
  returns the shared core's error, and de.rs embeds its Display text
  (verbatim per §1.5's normative messages) — `expected u32, got string
  (number exceeds 9999999999999999999999999999999999 × 10^6111, the
  largest exact NML number)` — in the existing `Error::De` shape.
  **The coerced value itself is never echoed** (implementation-review
  finding): the resolver erases provenance — resolved `$ENV` secrets
  arrive as plain `Value::String`s — so any coerced string could be a
  credential; the reason text diagnoses, the caller's key/span context
  locates. No NML code is minted for the de band (unchanged posture);
  the structured counts ride the embedded `NumberError` message.

### 1.5 Diagnostics (structured, per house style)

`NML0013 InvalidNumber` — unchanged meaning (malformed literal: `1.2.3`),
plus the trailing-dot case, which carries a distinct signal so the
suggestion machinery can act (`with_suggestion`, drop-the-dot, over the
literal span).

`NML0014 NumberOutOfRange` — generalized, with the structured payload the
house style demands (`MoneyErrorKind::Precision`, `UnicodeEscapeIssue`
precedent). The payload enum lives in decimal.rs and is embedded (error.rs
already depends on its sibling modules; decimal.rs stays dependency-free):

```rust
// decimal.rs — span-free, embedded by both NmlError and de.rs messages
pub enum NumberError {
    /// Not a number at all (shared-core reject; CST maps to NML0013).
    Malformed,
    /// `1299.` — valid digits, trailing dot (NML0013 + drop-dot fix).
    TrailingDot,
    /// Out of the decimal128 domain (NML0014).
    Range(NumberRangeIssue),
}

pub enum NumberRangeIssue {
    /// > 34 *value-significant* digits (the normalized coefficient's
    /// count — sign, leading zeros, and strippable trailing zeros
    /// excluded), so storing the value would be inexact. Parse-path
    /// only.
    TooManyDigits { got: usize, integral: bool },
    /// `try_new` only: the RAW pair's coefficient has > 34 digits. Kept
    /// distinct because the counts diverge — `try_new(10^34, 0)` is
    /// rejected (35-digit coefficient) even though the VALUE has one
    /// significant digit and constructs as `(10^33, −1)`; labeling the
    /// raw count "significant" would be false, and normalizing before
    /// counting produced a self-contradictory "1 significant digit;
    /// at most 34". The parse path cannot produce this variant: its
    /// clamp already stores every representable value.
    CoefficientTooWide { got: usize },
    /// `try_new` only: the pair's scale is outside [−6111, 6176] as
    /// handed in. Distinct from TooLarge/TooSmall for the same reason —
    /// those speak about the VALUE domain, and `(1, −6112)` is an
    /// invalid pair whose value 10^6112 is representable.
    ScaleOutOfRange { got: i16 },
    /// `try_new` only: coeff == 0 with negative scale — the third
    /// invariant as the third raw-pair kind (zero IS representable at
    /// every scale in [0, 6176]; the pair is not). Checked before the
    /// scale window so every negative-scale zero gets the zero rule in
    /// one step. Malformed is grammar-only again.
    NegativeScaleZero { got: i16 },
    /// Magnitude above the decimal128 maximum (W empty on the low side).
    TooLarge,
    /// Nonzero magnitude below 10^−6176 (W empty on the high side).
    TooSmall,
}
```

Worked messages (normative for `NumberError`'s Display, the error-index
rewrite, and the de-band embedding):

- `TooManyDigits { got: 38, integral: false }` →
  `number has 38 significant digits; NML numbers hold at most 34 (values
  are stored exactly — NML never rounds)`
- `TooManyDigits { integral: true, .. }` → same, plus the surviving prose
  from today's index entry: quote it as a string if it is an identifier
  (account numbers usually are).
- `CoefficientTooWide { got: 35 }` → `coefficient has 35 digits; NML
  coefficients hold at most 34 (fold trailing zeros into the scale —
  the value itself may be representable)` — API-surface only; never a
  diagnostic, since no parse can produce it.
- `ScaleOutOfRange { got: −6112 }` → `scale -6112 is outside NML's scale
  range [-6111, 6176] (renormalize the pair — the value itself may be
  representable)` — likewise API-surface only. When a pair violates both
  bounds, `CoefficientTooWide` wins (mirrors parse's digits-before-window
  precedence).
- `NegativeScaleZero { got: −5 }` → `zero cannot carry negative scale -5
  (it would render as leading-zero digits); zero stores at any scale in
  [0, 6176] — scale 0 is canonical` — likewise API-surface only, and
  checked before the scale window (most-specific-diagnosis-first), so
  `(0, −6112)` reports the zero rule in one step rather than a
  "renormalize" detour through the window.
- `TooLarge` → `number exceeds 9999999999999999999999999999999999 ×
  10^6111, the largest exact NML number`
- `TooSmall` → `number is closer to zero than 10^-6176, the smallest
  exact NML number`
- `Malformed` → `invalid number` (the NML0013 house wording);
  `TrailingDot` → `number ends with a decimal point; remove the "." or
  add fraction digits` (drives the machine-applicable suggestion).

Payload semantics, pinned: `got` counts *value-significant* digits
(`digits(c_min)`); `integral` means the **value** is integral
(`s_min ≤ 0`), not that the source form lacked a dot — the flag selects
the quote-it-as-a-string hint, which only makes sense for
identifier-like integers. When an input violates both acceptance
conditions, `TooManyDigits` is reported (§1.1 precedence).

Counts are payload fields because `echo()` truncates literals at 32 chars
— the message must never need the echo to be informative. Rendering
note: the RFC's prose uses typographic `×`/`—` for readability; the
shipped `Display` texts use the repo's ASCII message convention
(`x 10^6111`, `--`) and are pinned verbatim by `error_messages_pinned`. Suggestion
policy: rounding is never proposed; no `with_fix` exists for true
precision loss (dropping value-changing digits fails error.rs:326's
"provably preserve intent" bar); the stability.md fixer expectation is
explicitly waived here, in the RFC and the index entry.

**Code-continuity argument** (stability.md: codes are never renumbered,
never reused): NML0014's semantic identity — "this number exceeds what
NML can represent exactly" — is unchanged; the representable range
*grew*. Old triggers in (2^63, 10^34) stop erroring (widening); no input
moves to a *different* code; the index entry's first paragraph (the
`explain_summary` surface) is rewritten to state the decimal128 contract.
Generalization, not reuse; the CHANGELOG entry (§3 P5) records it, and
in-repo `expect-error` fixtures are updated in P1.

### 1.6 One parse core, conversions, and the Rust API

**One shared, `const` digit-parsing core.** Rev 2 left four parse entry
points (literal, `FromStr`, coercion, `num!`) with unstated sharing;
rev 3 pins it: decimal.rs exposes a single
`const fn from_scan(...) -> Result<Number, NumberError>` implementing
§1.1's storage rule, plus a `const fn parse_literal(&str)` over the
strict grammar. Entry points are thin adapters: `FromStr` wraps
`parse_literal`; `cst/value.rs::parse_number` wraps it and maps
`NumberError` → `ParseErrorKind` + `Span` (spans also attach at the
money and CST adapters, which wrap the same core errors);
`coerce_to_number` wraps the liberal-grammar extension (exponent
folding; `parse_coercion` itself is `const` — "runtime" describes where
the data arrives, not the fn); `num!` invokes `parse_literal` in const context. No
mirrored digit logic anywhere. (Const bona fides: const while-loops,
const panic, inline-const all predate MSRV 1.86.)

Renames follow the API guidelines (`as_` = cheap view; these are
conversions):

| Old | New | Semantics |
|---|---|---|
| `as_i64() -> Option<i64>` | `to_i64() -> Option<i64>` | exact integral in-range, else None (never truncates). *Value*-based: `25.0` → `Some(25)`, matching today's `float_to_exact_i64` posture — the serde ladders (§1.7) stay *form*-based; the two rules serve different layers. |
| — | `to_u64() / to_i128() / to_u128()` | same, wider. Magnitude rescaling runs in **u128 with checked 10^k steps** (sign applied per target): `2×10^38` = `(2, −38)` is a valid `u128` but overflows i128 — an i128 intermediate would spuriously reject it. |
| `as_f64() -> f64` | `to_f64() -> f64` | **correctly rounded**: format plain digits, parse via std (Eisel–Lemire **plus the correctly-rounded fallback**, rust-lang/rust#86761 — the fallback, not E–L alone, guarantees hard cases) |
| — | `to_f32() -> f32` | **direct** `parse::<f32>` of the canonical digits. Never `to_f64() as f32`: double rounding is real — `16777217.0000000001` → 16777216 via f64, 16777218 direct (verified). Also fixes the latent double-rounding in today's `deserialize_f32` (de.rs:777). |

`Value::as_f64`/`as_i64` and `ValueQuery::as_f64`/`as_i64` rename in
lockstep (`to_f64`/`to_i64`); the "lossy above 2^53" rustdoc moves with
them.

Construction: `From<i64>`, `Number::from_u64` (inherent, not `From<u64>`
— an implementation-time correction: a second integer `From` impl makes
plain integer literals ambiguous at every `impl Into<Number>` call site,
so `Value::number(8080)` would stop inferring; the inherent method keeps
the u64 path exact and infallible without polluting inference),
`TryFrom<i128>`, `TryFrom<u128>`,
`Number::try_new(coeff: i128, scale: i16) -> Result<Number, NumberError>`,
`FromStr` (`Err = NumberError`), `Number::ZERO`, and the ergonomics
linchpin below. `try_new`'s contract, pinned: it validates the §1.1
invariants **raw** — no clamping, no normalization (storage-rule
canonicalization is the parser's job, not the constructor's) — and may
therefore construct valid cohort members the parser never produces
(`(1, −100)` is legal; the parse of `10^100` stores `(10^33, −67)`).
Violations map to `Range(..)` by side (coefficient too wide →
`CoefficientTooWide` with the RAW digit count — not `TooManyDigits`,
whose value-significant count would mis-describe a raw pair; scale
beyond either bound → `ScaleOutOfRange` with the raw scale — not the
value-domain `TooLarge`/`TooSmall`, false for pairs like `(1, −6112)`
whose value is representable) —
and the zero-invariant (`coeff == 0` with negative scale) has its own
kind, `NegativeScaleZero`, checked BEFORE the scale window so every
negative-scale zero gets the zero rule in one step (pinned in
`try_new_contract`); a positive out-of-window scale on zero reports
`ScaleOutOfRange`, which stays truthful. `Malformed` is grammar-only.

```rust
pub use nml_core::num;    // rust_decimal `dec!` precedent
let t = num!(2.5);        // expands to: const { match Number::parse_literal(
                          //   stringify!(2.5)) { Ok(n) => n,
                          //   Err(_) => panic!("invalid nml number literal") } }
let v = Value::number(num!(0.20));
```

Inline-const (stable 1.79) makes invalid literals compile errors.
`stringify!` preserves the written form (`2.50` keeps its scale —
verified through token capture); negative literals work (`num!(-2.5)`);
underscored literals (`num!(1_000.5)`) are compile errors by design —
the macro mirrors the literal grammar, documented. `num!` exists because
`From<f64>` is deleted (§1.9) and 34 float-literal `Value::number(2.5)`
test sites need a replacement *better* than `"2.5".parse().unwrap()`
noise.

**Foreign binary→decimal rule** (used by serde's f64 fallback, §1.7):
`Number::try_from_f64` is defined as the **shortest decimal that
round-trips to the same f64** (Rust's f64 Display digits), parsed
exactly; NaN/∞ reject as `Malformed`. The naive "exact expansion" reading is wrong by
construction — `0.1_f64`'s exact decimal value has 55 significant digits
and would reject as `TooManyDigits`; shortest-round-trip is the canonical
decimal a human would have written.

Comparisons: `PartialEq<i64>` stays and gains `PartialOrd<i64>`
(threshold code: `n > 1`). `PartialEq<f64>` is deleted — under exact
decimals `n == 0.1_f64` would be *correctly false* and perpetually
surprising; decimal thresholds use `num!`: `n > num!(1.0)`.

### 1.7 The serde contract

Three tiers, each with a stated rounding posture. Binary floating point
survives **only** in tier 2 and the fraction-form `deserialize_any` rung.

1. **Integer targets — exact or error** (unchanged posture, wider
   reach): `number_to_int` re-derives from `(coeff, scale)` via the u128
   magnitude path (§1.6); fractional and out-of-range still error.
   `deserialize_i128`/`deserialize_u128` are added; `u64` no longer
   funnels through `i64` — closing the gap where `u64 > i64::MAX` was
   undeserializable. (The deleted de.rs:131–137 check is the
   *fractional-part* rejection — rev 1 mislabeled it as NaN defense; its
   replacement derives fractionality from scale; its message
   interpolation changes from `{f}` float text to exact display.)
2. **`f64`/`f32` targets — correctly rounded, documented-lossy**:
   `to_f64()`/`to_f32()` per §1.6 (single rounding each; RFC 8259 §6's
   binary64 interop *expectation* — cited as guidance, not mandate).
3. **`Number` fields — exact by construction**: `Number: Deserialize` is
   part of the type's core correctness — without it,
   `struct P { temperature: Number }` would silently detour through f64
   *inside nml's own bridge*. Mechanism: the toml-`Datetime`/
   serde_json-`Number` private-token handshake
   (`deserialize_newtype_struct` with a crate-private name; the nml
   Deserializer intercepts — de.rs:866 already forwards
   `newtype_struct`, so the intercept is local). The intercept answers
   with the **compact member encoding** `{coeff}e{-scale}` — ≤ ~42
   bytes for every member where the plain form of extreme-scale values
   is ~6 KB (an amplification lever the security review flagged), and,
   unlike the plain form, it round-trips the stored cohort *member*
   exactly, not just the value. Foreign formats fall through to a
   visitor accepting i64/u64/i128/u128 (exact), f64 (via
   `try_from_f64`, shortest-round-trip), and str — parsed with the
   §1.4 liberal **data** grammar (which is what decodes the handshake
   encoding, and gives foreign machine-emitted strings the same Postel
   treatment as every other coerced string; scoped to the `Number`
   visitor only — general `Value` deserialization does not treat
   strings as numbers). The `#[serde(tag)]`/`flatten`
   buffering pitfall (serde_json #505) is documented on the impl **and
   pinned by a regression test** (the pin exercises `#[serde(untagged)]`
   — the same serde `Content` buffering machinery all three attributes
   share) — no nudge nml-path struct uses tag/flatten today (verified),
   so the caveat is latent, not live.

**`Serialize for Number` never emits binary floating point.** The derived
untagged emit dies with the enum. The rule is *form-based* (stored
scale), deliberately mirroring `deserialize_any` — a value-based
"integral → integer" rule would emit `25` for `25.0` and destroy the
scale this RFC makes real:

- **integer form** (`scale ≤ 0`; such members are display-identical to
  plain integer digits, so nothing is lost): fits i64 → `serialize_i64`;
  else fits u64 → `serialize_u64`; **else the exact plain string**;
- **fraction form** (`scale > 0`, even when numerically integral: `25.0`
  emits `"25.0"`) → the exact plain string (`Display`).

Rationale: rev 1's "else `to_f64()`" silently corrupted the very integers
§1.2 legalizes (`2^64 + 1` → emitted as a *different integer*, verified)
and erased scale from the one machine-readable dump. The **128-bit rungs
this section originally specified were then removed too** (audit
finding): serde_json *writes* a 128-bit integer but cannot read one back
— its `deserialize_any` has no 128-bit rung, so the value returned
through `visit_f64` and `Number → JSON → Number` silently lost
exactness, and the wire form was not even idempotent across one
store-and-reload cycle. A form only half the ecosystem can carry is the
silent-loss trap this RFC exists to remove, so **round-trip exactness
wins over looking like a number**: `u64` is the widest integer every
mainstream format actually round-trips, and beyond it the exact string
decodes through the visitor's §1.4 data grammar everywhere — including
formats with no integer type at all. The property is now pinned by a
per-member `Number → JSON → Number` round-trip test.

Under this rule the `nml parse` dump is exact and scale-preserving
(`2.50` → `"2.50"`, `8080.0` → `"8080.0"`, `8080` → `8080`, and a
25-digit integer → its exact digits quoted); the dump shape change is a
documented compat item (§4) and doubles as the exact machine-readable
egress — no bespoke wire mode needed.

**`deserialize_any` preserves today's form-based behavior**: stored
scale > 0 (author wrote a fraction) → `visit_f64` — the residual
documented-lossy edge for untyped consumers; scale ≤ 0 (integer form) →
`visit_i64`, else `visit_u64`, else `visit_i128`, else `visit_u128`,
else — integer form beyond u128 — a **hard error** naming the typed
alternatives (`Number` field, string capture). Never a silent f64 for an
integer-form value; serde-`Content`'s missing 128-bit variants
(serde #1717) make untagged-enum capture of >u64 integers a documented
limitation, not a rounding hole. Ladder geometry, by construction:
`deserialize_any` climbs i64 → u64 → i128 → u128 → error; the **emit**
ladders (`Serialize`, §4's dump) stop at u64 — i64 → u64 → exact
string — because emitting 128-bit rungs that half the JSON ecosystem
cannot read back is the silent-loss trap this section exists to close.

Env-string coercion feeds this same machinery through §1.4's exact
grammar: `$ENV.PORT = "9007199254740993"` survives exactly (existing
guarantee), `$ENV.RATE = "1e-6"` now arrives exact instead of
f64-rounded.

### 1.8 Money on the shared substrate (DRY)

`money.rs::parse_minor_units` (~60 lines of sign/dot-split/pad/checked
scaling) and `format_display` are re-implemented over the decimal core:
parse the amount digits through the shared core, validate
`scale ≤ currency exponent` (`NML3002`, counts from the structured
payload), rescale `coeff × 10^(exponent − scale)` **in i128** (max
≈ 10^38, inside i128 — the intermediate width is stated), then
`i64::try_from` narrows checked. `Money`'s public shape (`amount: i64`
minor units + currency + exponent, `Eq`, serialization) is wire-stable;
`Money::to_number()` is the sanctioned exact accessor.

**Error-band mapping, pinned**: *amount-domain* failures never surface
syntax-band codes — every decimal-core `Range(..)` failure inside money
parsing re-wraps as `MoneyErrorKind` (malformed → `InvalidAmount`,
NML3000; too many digits / too large → `OutOfRange`, NML3003).
Implementation-time cleanup: `MoneyErrorKind::InvalidFraction` is
retired — with one shared parse core there is no separate fraction
parse to fail, so the variant became unproducible dead code; its inputs
(`1.2.3 USD`) classify as `InvalidAmount` under the same NML3000. One carve-out, stated: the **trailing-dot malformation is a
literal-layer rejection** that fires before money parsing begins —
`1299. JPY` gets NML0013 + the drop-dot suggestion, same as bare
`1299.` (it is a form defect of the token, not of the amount). Boundary
shifts documented, not hidden: a 20-digit USD amount errors as NML3003
where today it dies as NML3000 at `whole_str.parse::<i64>()`
(money.rs:161); `-92233720368547758.08 USD` (exactly `i64::MIN` minor
units), rejected today by the abs-then-negate dance, becomes valid — a
one-value widening.

### 1.9 Removals (no legacy retained)

Deleted outright — each is faked information, a trap, or now-unreachable:

- `Number::Int` / `Number::Float` variants and all pattern sites (64
  lines, the overwhelming majority in `#[cfg(test)]`; production sites
  are de.rs, cst/value.rs incl. the `:128` placeholder → `Number::ZERO`,
  and the types.rs impl itself).
- `float_to_exact_i64` (types.rs:57); the `{n:.1}` Display hack
  (types.rs:105).
- `From<f64>` and `PartialEq<f64>` (semantic traps under exactness;
  §1.6's `num!` + renames absorb the 34 + 1 affected sites).
- The fractional-part float check in `number_to_int` (de.rs:131–137),
  re-derived from scale.
- The i64-then-f64 string fallback in `coerce_to_number` — replaced by
  §1.4's exact coercion grammar (which also removes the accidental
  acceptance of `"inf"`/`"nan"` env values into the data model).
- Trailing-dot literal acceptance (`1299.`) — rejected with a suggestion.

Kept, with renames per §1.6, because nudge compiles against them:
`to_i64`/`to_f64` (né `as_*`), `Display`, `Serialize`,
`TryFrom<&Value> for f64`, `Value`/`ValueQuery` accessors. (Amendment:
the `i64` impl was later replaced by a single `i128` rung — the width
every integer target through `u64` narrows from exactly — so the
typed-error flavor has no band the value domain can outgrow; consumers
migrated in the same change.)

### 1.10 Formatter and LSP

- fmt keeps rendering through `Display`, which now preserves scale —
  `2.50` survives `nml fmt` (today it collapses to `2.5`). **Display
  algorithm, pinned**: sign (from coeff), then — `scale ≤ 0`: the
  coefficient digits followed by `−scale` zeros; `scale > 0`: the
  digits split at `scale` from the right, zero-padded through the
  integer position (`(5, 3)` → `"0.005"`; `(0, 2)` → `"0.00"`;
  parse of `-0.00` displays `"0.00"`). **Fixed-point property, stated**:
  every output of today's formatter re-formats to itself under the new
  formatter (`2.5` → `2.5`, `N.0` → `N.0`, integers unchanged) with
  exactly one exception — `-0.0`, which today renders `-0.0` and
  henceforth `0.0`; §2 pins the old-output→new-fmt round-trip table
  including this case. Leading zeros still canonicalize (`007` → `7`).
- LSP: the `number` primitive hover string (in `nml-lsp/src/server.rs`;
  line refs rot — search for `Exact decimal`) becomes
  "**number** — exact decimal (up to 34 significant digits); integers
  and decimals never round" (as shipped). A "simplify number" code action (LSP-side, value-preserving,
  machine-applicable) rewrites a literal to its minimal cohort form
  (`8080.000` → `8080`) — the counter-lint for *unwanted* trailing
  zeros now that fmt preserves them; authored precision like `2.50` is
  simply never auto-touched. Rev 1's "hover shows the exact stored
  value" was new unspecced LSP functionality — cut; follow-up (§5).
- `nml-validate` needed **one** change (the original "no code change"
  claim did not survive the border sweep). It consumes numbers via
  `Display`/`to_u64`, not variant matches, but `formatVersion`'s reader
  returned `None` for anything outside `u64` — and since the widening
  made such a literal *parse* rather than die at NML0014, that `None`
  read downstream as "missing `formatVersion`" while silently skipping
  RFC 0030's degradation gate, i.e. the wall-of-noise outcome that
  contract exists to prevent. It now **saturates**: any `formatVersion`
  outside `u64` — too large, negative, or fractional — is definitionally
  not a version this build supports, so the gate fires with the one
  precise error instead of a misleading "missing" or a validation wall.
  Separately, `set<number>` duplicate detection tightens (see §4).

## 2. Verification

**No new dependencies, including dev-deps.** The MSRV (1.86) and
minimal-versions CI jobs compile `--all-targets` — dev-deps are inside
both gates — and proptest's tree and minimal-versions hygiene would put
the gates at risk for zero functional gain. The repo already has the
right house pattern: in-tree seeded property loops
(`fuzz_termination_losslessness_bounded`, cst/mod.rs:1450;
`fuzz_eol_insensitivity…`, :1539 — xorshift-seeded plain `#[test]`s).
P4 extends it:

- **In-tree property harness** (seeded, deterministic, plain `#[test]`):
  parse→Display→parse identity across generated cohorts — **value-level**
  (the reparse is `Eq`-equal): member-level identity holds for
  literal-produced members but provably not for coercion's
  negative-scale members (`(15, −2)` displays `"1500"`, which stores
  `(1500, 0)`); `Eq ⇒ Hash` coherence; `Ord`
  totality/antisymmetry/transitivity and `Eq`-consistency; `total_cmp`
  refines `cmp` (incl. the zero cohort); `to_i64`/`to_u128`/`to_f64`
  round-trip properties; storage-rule determinism (same value + written
  scale ⇒ same member). The equality **oracle is digit-string
  comparison** (align adjusted exponents textually) — an independent
  implementation path with no width limits (an i128 cross-multiply
  oracle overflows at scale gaps > 4 and would degenerate into testing
  the implementation with itself).
- **cargo-fuzz** (`fuzz/` crate; `exclude = ["fuzz"]` added to the
  workspace so nightly-only tooling stays outside the MSRV/
  minimal-versions/3-OS gates): `number` (both the literal and coercion
  grammars plus round-trip/hash invariants), `document` (full parse +
  the lossless-CST byte-identity invariant), `money`, and `format` (the
  formatter's round-trip/idempotency invariant) — every defined target
  runs in the smoke job. The CI smoke seeds from `tests/fixtures/` and
  **accumulates a persistent coverage corpus across runs** via
  first-party `actions/cache` (SHA-verified pin; run-scoped save keys
  with a prefix restore-key) — continuous-fuzzing-style progress with
  zero third-party action surface.
- **Edge tables** (unit tests): the §1.1 worked-consequence set
  (`10^34` → `(10^33, −1)`; `10^6144` parses, `10^6145` `TooLarge`;
  the subnormal clamp case; `0.`+6200 zeros → `(0, 6176)`; `10^−6177`
  `TooSmall`); 34-digit boundaries both sides; the narrowing-wrap
  killers with **error-kind assertions** at fraction lengths of exactly
  32 768 (`0.` + 32 767 zeros + `1` — the i16 wrap, → −32 768) and
  65 536 / 65 538 (the u16 wraps, → 0 / 2 — the last reproducing
  §1.2's scale-2 example);
  `9007199254740993` (2^53+1) through every conversion;
  `16777217.0000000001` through `to_f32` (double-rounding sentinel);
  `i64::MIN`; `-0.00` (displays `"0.00"`); `2^64 + 1` and
  `2×10^38` through Serialize/deserialize_any/`to_u128` (the u128 rung,
  from both source forms: coercion `"2e38"` stores `(2, −38)`, the
  literal stores `(2×10^33, −5)` — same value, same rung); `10^100`
  (literal) → parses `(10^33, −67)`, Serialize = exact string,
  deserialize_any = hard error; JSON `0.1` through the foreign-f64
  fallback (must equal `num!(0.1)`); coercion strings `"1e-6"`,
  `"1E5"`, `"+1"`, `".5"`, `"1."`, `"1.e5"` → `(1, −5)` accepted,
  `"1e00042"` accepted, `"0e999999999999"` → `(0, 0)`,
  `"1e999999999999"` → `TooLarge`, `"inf"`/`"nan"` rejected;
  `1299.` and `1299. JPY` rejected with suggestion; tag/flatten
  regression for `Number` fields; the real-world corpus literals
  (`0.05/0.08/0.1/0.19/0.20/0.7`) exact end-to-end; `2.50` fmt
  round-trip; diff-cosmetic classification (`1.5 ↔ 1.50` cosmetic;
  `9007199254740993.0 ↔ …92.0` **not** cosmetic — the f64-era bug); the
  old-fmt-output fixed-point table incl. the `-0.0` exception.
- Existing gates stay green: `just docs-test` (error-index bidirectional
  check picks up the NML0014 rewrite), 3-OS matrix, MSRV job.

## 3. Phases

Each phase lands green and reviewable on its own — including P0: nml-core
is a library, so the new pub module is reachable API from the moment it
merges (pub items in a pub module are never `dead_code`).

- **P0 — decimal core**: `decimal.rs` (repr, invariants, the §1.1
  storage rule via the shared const parse core, `NumberError`/
  `NumberRangeIssue`, cmp/total_cmp/Eq/Hash, Display/FromStr,
  `to_*` conversions incl. the u128 magnitude path, `try_from_f64`
  (shortest-round-trip), `try_new`/`From`/`TryFrom`, `Number::ZERO`),
  the `num!` const macro, and the full §2 property/edge suite for the
  module. `types.rs` gains the re-export.
- **P1 — cut-over** (the breaking PR, ~90 mechanical sites, mostly
  tests): types.rs rewrite + deletions (§1.9); `parse_number` as
  adapter + `NumberError` → `ParseErrorKind`/Span mapping;
  de.rs (three tiers, unified ladders, liberal coercion grammar with
  saturating exponents, de-band error embedding, `Number`
  Serialize/Deserialize incl. the handshake — part of the type's serde
  correctness, not separable); `NumberOutOfRange { issue }` +
  error-index rewrite + fixture updates (incl. query.rs:509, which
  lives in nml-core); fmt/LSP/validate render sites; all four crates
  green.
- **P2 — money on the substrate**: §1.8 incl. the pinned error-band
  mapping and the trailing-dot carve-out; money tests extended.
- **P3 — fmt/LSP UX**: fmt fixed-point table; the hover string; the
  "simplify number" code action.
- **P4 — verification infra**: in-tree property harness, `fuzz/` crate
  + workspace exclude + CI smoke job.
- **P5 — docs truth sweep** (complete list): `spec/types.md` §number
  rewrite (the authoritative "exact i64 / IEEE double" prose; its
  phantom `<integer, min, max>` constraint promise folds into the
  facets follow-up, §5); README.md:62 (`port: f64` example) and :88
  ("exact `i64` semantics"); tutorial 01 (:55–57), 02 (:20 + money
  segue reframed: money adds *currency rules*, not exactness), **07
  and its runnable app** (`port/retries/pool_size/trial_days: f64` —
  teaches the anti-pattern); docs/guides/parse-and-query.md:38–39;
  integration.md :66/:104/:111/:320–321 (":321's 'truncated to
  integer' comment is false even today")/:332;
  deserialize-with-serde.md; cookbook comment; lib.rs:24 +
  query.rs:22–23 doctests; de.rs:923 example; types.rs/query.rs
  rustdoc; error-index NML0013/NML0014 bodies (first paragraph = the
  `explain_summary` surface, per §1.5's normative messages);
  **CHANGELOG entry** (stability.md mandates one); spec fixture pairs
  at the 34-digit and decimal128-boundary cases.
- **P6 — nudge follow-up** (separate repo, small): `number_to_json`
  (plugin_webhook_config.rs:296) — `to_i64`/`to_u64` fast path emits
  exact JSON integers; integer-form beyond u64 → explicit
  `UnsupportedValueKind`-style error (precedent at :243) — **never a
  silently rounded integer to a WASM guest**; fraction-form →
  `from_f64` (the guests read f64 from JSON; that floor is theirs,
  documented). Renames: `to_f64`/`to_i64` at
  plugin_webhook_config.rs:297/:300, types.rs:5443/:5513/:5552,
  workflow/capability.rs:2487 (the `f64::try_from` sites at
  types.rs:6327/6335/6350 compile unchanged). No-code-change but
  output-relevant: the two Display sites (config_reload.rs:717 reload
  diffs, workflow/parser.rs:542 plugin config strings) render written
  scale — no current corpus literal is affected (swept), noted for
  operators. Nothing else changes — verified: no `Number::Float`
  patterns, no nml-path tag/flatten structs, no ValueQuery use in
  nudge.

## 4. Compatibility

- **Compile-time**: semver-major for nml-core (pre-1.0, path deps).
  Dependents: nudge + nudge-cli only; the kept-API table (§1.6/§1.9)
  covers every call site; expected downstream diff is P6 alone.
- **Documents that parse differently**:
  (a) dotted literals become exact — strictly more correct at every
  edge; (b) literals whose *value* is outside the decimal128 domain
  (>34 value-significant digits, above ~10^6145, below 10^−6176),
  previously silently f64-rounded, now error — none exist in any corpus
  (nml, nudge, site/, fixtures — swept); (c) integer literals in
  (2^63, 10^34) — and larger trailing-zero forms up to the decimal128
  maximum — now parse (previously NML0014): strictly widening;
  (d) trailing-dot literals (`1299.`, `1299. JPY`) now error with a
  fix-suggestion — none in any corpus; (e) trailing-zero-overflow
  literals now parse exactly (rev 1 would have rejected them).
- **Runtime data (env coercion)**: today's accepted forms keep working
  — `"+1"`, `".5"`, `"1."`, and scientific notation now coerce
  *exactly*. Tightenings: `"inf"`/`"nan"` rejected (previously admitted
  IEEE infinities); out-of-domain magnitudes error instead of rounding.
  Called out separately because corpora sweeps cannot cover env values.
- **Serialization surfaces**: `nml parse` JSON dumps emit fraction-form
  numbers as exact strings (`2.50` → `"2.50"`, `8080.0` → `"8080.0"`)
  and integer-form values as JSON integers up to `u64` (beyond:
  exact string); `Number`'s untagged-derive shape is gone. The CST
  differential test is shape-agnostic (same serializer both sides —
  verified). **Display/`to_string` is itself a compat surface** anywhere
  downstream stringifies numbers (nudge reload diffs, plugin config
  strings): trailing-zero fractions render with written scale. Scope,
  precisely: the **nudge** corpus has none (swept — its decimals are
  `0.05/0.1/0.7`), so no operator-visible output changes; nml's own
  corpora do contain `taxRate = 0.20` (spec/examples/pricing.nml:17,
  tests/fixtures/valid/pricing.nml:21), whose rendering changes
  `0.2` → `0.20` — in-repo fixtures/examples, updated in P1/P5.
- **deserialize_any**: form-based visiting preserved (`2.0` still visits
  f64); new behavior only above u64 (exact integer visits) and above
  u128 (hard error).
- **`deserialize_f32`**: single-rounding fix means some f32 fields
  deserialize to a *different, more correct* value than today's
  double-rounded path (sentinel in §2).
- **fmt**: old formatter output is a fixed point of the new formatter
  except `-0.0` → `0.0` (documented; no `--check` mode exists, so blast
  radius is save-churn only). `2.50`-class literals stop being
  collapsed — a change only where information was previously destroyed.
- **`set<number>` duplicate detection tightens** (audit finding):
  element identity is now numeric, so a set holding both `- 8080` and
  `- 8080.0` — two spellings of one value, accepted pre-RFC because the
  old `Int`/`Float` variants compared unequal — is a
  `DUPLICATE_SET_ELEMENT` load error. Correct under the value model
  (a set cannot hold one value twice) and fail-safe on reload
  (keep-last-good), but it is a hard failure at boot for such a config.
  No corpus contains one.
- **Money**: NML3000→NML3003 boundary shift for >i64 amounts;
  `i64::MIN` minor units accepted (one-value widening); trailing-dot
  money literals now NML0013 (previously parsed). Code definitions
  unchanged.
- **MSRV/deps**: 1.86 unchanged; zero new deps of any kind; fuzz crate
  excluded from the workspace.

## 5. Non-goals and follow-ups

- Exponent/underscore literal syntax (separable ergonomic RFC; coercion
  strings already accept exponents where it matters).
- Arbitrary precision (bounded-exact is the point).
- Numeric schema facets (min/max/precision) — future RFC; `spec/
  types.md`'s dangling `<integer, min, max>` promise folds into it.
- Literal-value hover in the LSP (cut from §1.10; needs its own design).
- A guest-facing exact-decimal JSON mode for WASM plugin config (the
  host-side floor is now exact; the guest contract change is a nudge
  RFC when a plugin needs it).
