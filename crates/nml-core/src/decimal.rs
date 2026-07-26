//! The exact decimal numeric core (RFC 0016).
//!
//! NML's number domain is exactly the **finite IEEE 754-2019 decimal128
//! values, excluding −0**: every value `±c × 10^e` with `c ≤ 10^34 − 1` and
//! quantum exponent `e ∈ [−6176, 6111]`. A number is rejected precisely when
//! representing it would be inexact or out of that range — never when its
//! value is representable. There is no binary floating point in the data
//! model; f64/f32 exist only as the explicit, correctly-rounded conversions
//! [`Number::to_f64`](crate::decimal::Number::to_f64) / [`Number::to_f32`](crate::decimal::Number::to_f32).
//!
//! This module is deliberately dependency-free and arithmetic-free: NML
//! parses, stores, compares, formats, and converts numbers — it never does
//! math on them — so the entire core is representation + one shared `const`
//! parse path + total ordering + formatting. See RFC 0016 §1.1–§1.7 for the
//! normative rules; the fixture names in the tests mirror the RFC's worked
//! consequences.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Invariant: `|coeff| ≤ 10^34 − 1` (≤ 34 significant digits).
const COEFF_ABS_MAX: u128 = 9_999_999_999_999_999_999_999_999_999_999_999;
/// Invariant: `scale ∈ [−6111, 6176]` (⇔ exponent `−scale ∈ [−6176, 6111]`).
const SCALE_LO: i64 = -6111;
const SCALE_HI: i64 = 6176;

/// The private serde handshake name (RFC 0016 §1.7 tier 3): [`Number`]'s
/// `Deserialize` asks for a newtype struct with this name; NML's own
/// `Deserializer` recognizes it and answers with the compact exact member
/// encoding `{coeff}e{-scale}` (≤ ~42 bytes for any member, member-exact
/// through the §1.4 data grammar), so `struct P { temperature: Number }`
/// never detours through f64.
/// Foreign formats treat the newtype as transparent and fall through to the
/// visitor's i64/u64/i128/u128/f64/str arms.
///
/// Known limitations (inherited from the serde_json `Number` / toml
/// `Datetime` pattern this follows): inside `#[serde(tag = …)]` /
/// `#[serde(flatten)]` containers, serde's internal buffering does not
/// preserve the handshake, so exact capture degrades to the foreign-format
/// arms (serde_json#505-class) — covered by a regression test in `de`.
/// And the handshake is name-based: a downstream `Deserialize` impl that
/// deliberately requests this newtype name receives the exact decimal
/// string instead of transparent passthrough — an author-opt-in behavior
/// change, not a config-triggerable one (same property as the serde_json
/// and toml precedents).
pub(crate) const NUMBER_NEWTYPE_TOKEN: &str = "$nml::private::Number";

/// An exact decimal: value = `coeff × 10^(−scale)`.
///
/// The stored `(coeff, scale)` pair is the *cohort member* chosen by the
/// storage rule (RFC 0016 §1.1): the written scale, clamped into the value's
/// constructible window — so `2.50` keeps scale 2 and displays `"2.50"`,
/// while `10^34` (35 written digits) stores `(10^33, −1)` and still displays
/// all 35 digits. Equality, ordering, and hashing are **numeric**
/// (`1.5 == 1.50`); [`Number::total_cmp`] is the representation-sensitive
/// tiebreak.
///
/// Negative zero is unrepresentable by construction: the sign lives in
/// `coeff`, and two's-complement `i128` has no −0. Zeros additionally hold
/// `scale ∈ [0, 6176]` (no negative-scale zeros).
#[derive(Copy, Clone, Debug)]
pub struct Number {
    coeff: i128,
    scale: i16,
}

/// Span-free parse/construction error for the numeric core (RFC 0016 §1.5).
///
/// The CST layer is the only place these gain spans and NML codes
/// (`Malformed`/`TrailingDot`/`BadSeparator` → NML0013, `Range` →
/// NML0014); the serde
/// layer embeds the `Display` text verbatim in its own error band.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NumberError {
    /// Not a number at all (`1.2.3`, `0x10`, empty, …).
    Malformed,
    /// Valid digits with a trailing decimal point (`1299.`) — rejected with
    /// a machine-applicable drop-the-dot suggestion at the CST layer.
    TrailingDot,
    /// A misplaced `_` digit separator (`1__0`, `1_`, `1_.5`). Separators
    /// are legal only between two digits — one spelling per grouping,
    /// stricter than Rust. The CST layer attaches a machine-applicable
    /// strip-the-separators fix (provably value-preserving: separators are
    /// spelling, never value).
    BadSeparator,
    /// The value is outside the decimal128 domain.
    Range(NumberRangeIssue),
}

/// Why a value falls outside the decimal128 domain (RFC 0016 §1.5).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NumberRangeIssue {
    /// More than 34 *value-significant* digits (the normalized
    /// coefficient's count — sign, leading zeros, and strippable trailing
    /// zeros excluded): storing the value would be inexact. Takes
    /// precedence when a value also violates the exponent range.
    /// Parse-path only; a raw pair rejected by [`Number::try_new`] is
    /// [`NumberRangeIssue::CoefficientTooWide`] instead.
    TooManyDigits {
        /// Value-significant digit count (`digits(c_min)`).
        got: usize,
        /// The **value** is integral (`s_min ≤ 0`) — selects the
        /// quote-it-as-a-string hint for identifier-like integers.
        integral: bool,
    },
    /// [`Number::try_new`] only: the coefficient of the pair *as handed
    /// in* has more than 34 digits. Distinct from `TooManyDigits`
    /// because the two counts diverge — `try_new(10^34, 0)` is rejected
    /// (35-digit coefficient) even though its VALUE has one significant
    /// digit and constructs fine as `(10^33, −1)`; `try_new` validates
    /// the raw pair, never normalizes (see its contract). The parse
    /// path cannot produce this: its clamp already stores every
    /// representable value.
    CoefficientTooWide {
        /// Digit count of `|coeff|` as handed in.
        got: usize,
    },
    /// [`Number::try_new`] only: the pair's scale is outside
    /// `[−6111, 6176]` as handed in. Distinct from `TooLarge`/`TooSmall`
    /// for the same reason `CoefficientTooWide` is distinct from
    /// `TooManyDigits` — those speak about the VALUE domain, and a raw
    /// pair can be invalid while its value is representable
    /// (`(1, −6112)` is rejected; the equal value 10^6112 constructs as
    /// `(10, −6111)`). When a pair violates both the coefficient and
    /// scale bounds, `CoefficientTooWide` wins — mirroring the parse
    /// path's digits-before-window precedence. The zero-invariant is
    /// checked before this window, so a NEGATIVE-scale zero never
    /// reaches here ([`NumberRangeIssue::NegativeScaleZero`] owns that
    /// case in one step); a zero at a scale above 6176 does report this,
    /// truthfully.
    ScaleOutOfRange {
        /// The scale as handed in.
        got: i16,
    },
    /// [`Number::try_new`] only: `coeff == 0` with a negative scale —
    /// the third invariant, and the third raw-pair kind (the VALUE,
    /// zero, is representable at every scale in `[0, 6176]`; the PAIR
    /// is not, because a negative-scale zero would render as
    /// leading-zero digits). Checked before the scale window so that
    /// every negative-scale zero — `(0, −5)` and `(0, −6112)` alike —
    /// gets the zero rule in ONE step, never a "renormalize" detour
    /// through the window bounds. Completes the 1:1 invariant↔kind
    /// taxonomy; `Malformed` is grammar-only again.
    NegativeScaleZero {
        /// The scale as handed in.
        got: i16,
    },
    /// Magnitude above the decimal128 maximum, ≈ 9.999×10^6144.
    TooLarge,
    /// Nonzero magnitude below 10^−6176.
    TooSmall,
}

impl fmt::Display for NumberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NumberError::Malformed => f.write_str("invalid number"),
            NumberError::TrailingDot => f.write_str(
                "number ends with a decimal point; remove the \".\" or add fraction digits",
            ),
            NumberError::BadSeparator => {
                f.write_str("misplaced digit separator: `_` is allowed only between two digits")
            }
            NumberError::Range(issue) => issue.fmt(f),
        }
    }
}

impl fmt::Display for NumberRangeIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NumberRangeIssue::TooManyDigits { got, integral } => {
                write!(
                    f,
                    "number has {got} significant digits; NML numbers hold at most 34 \
                     (values are stored exactly -- NML never rounds)"
                )?;
                if *integral {
                    write!(
                        f,
                        "; if this is an identifier such as an account number, \
                         quote it as a string"
                    )?;
                }
                Ok(())
            }
            NumberRangeIssue::CoefficientTooWide { got } => write!(
                f,
                "coefficient has {got} digits; NML coefficients hold at most 34 \
                 (fold trailing zeros into the scale -- the value itself may be \
                 representable)"
            ),
            NumberRangeIssue::ScaleOutOfRange { got } => write!(
                f,
                "scale {got} is outside NML's scale range [-6111, 6176] \
                 (renormalize the pair -- the value itself may be representable)"
            ),
            NumberRangeIssue::NegativeScaleZero { got } => write!(
                f,
                "zero cannot carry negative scale {got} (it would render as \
                 leading-zero digits); zero stores at any scale in [0, 6176] \
                 -- scale 0 is canonical"
            ),
            NumberRangeIssue::TooLarge => f.write_str(
                "number exceeds 9999999999999999999999999999999999 x 10^6111, \
                 the largest exact NML number",
            ),
            NumberRangeIssue::TooSmall => {
                f.write_str("number is closer to zero than 10^-6176, the smallest exact NML number")
            }
        }
    }
}

impl std::error::Error for NumberError {}

/// `(a * b) % m` without overflow: residues are < 10^34 < 2^113, so the
/// double-and-add accumulator stays under 2^114. Deterministic, loop
/// length fixed by bit-width — no bignum, no allocation (RFC 0018).
const fn mulmod(mut a: u128, mut b: u128, m: u128) -> u128 {
    let mut acc: u128 = 0;
    a %= m;
    while b > 0 {
        if b & 1 == 1 {
            acc = (acc + a) % m;
        }
        a = (a + a) % m;
        b >>= 1;
    }
    acc
}

/// `10^e % m` by square-and-multiply over [`mulmod`] (RFC 0018).
const fn powmod10(mut e: u32, m: u128) -> u128 {
    let mut base = 10u128 % m;
    let mut acc = 1u128 % m;
    while e > 0 {
        if e & 1 == 1 {
            acc = mulmod(acc, base, m);
        }
        base = mulmod(base, base, m);
        e >>= 1;
    }
    acc
}

const fn digit_count_u128(mut v: u128) -> u32 {
    let mut n = 1u32;
    while v >= 10 {
        v /= 10;
        n += 1;
    }
    n
}

const fn clamp_i64(v: i64, lo: i64, hi: i64) -> i64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// The two digit regions of a scanned literal (integer part, then fraction
/// part), by **physical byte range**. Regions may contain `_` separators
/// (already placement-validated by [`scan_digit_run`]); the cursor helpers
/// below iterate the conceptual `int ++ frac` digit sequence while hopping
/// separators and the inter-region gap — amortized O(1) per digit, so
/// separator-laden input parses in O(n) (never O(n²): a parse runs on
/// untrusted input, and quadratic digit access would be a DoS lever).
/// Const-compatible: no closures, no allocation, no range subslicing.
#[derive(Copy, Clone)]
struct DigitRegions {
    int_start: usize,
    int_end: usize,
    frac_start: usize,
    frac_end: usize,
}

impl DigitRegions {
    /// Physical position of the first digit, or `usize::MAX` when empty.
    const fn first(&self, b: &[u8]) -> usize {
        self.settle(b, self.int_start)
    }

    /// Normalize a physical position to the next digit at-or-after it:
    /// hop separators and the inter-region gap. `usize::MAX` = exhausted.
    const fn settle(&self, b: &[u8], mut p: usize) -> usize {
        loop {
            if p >= self.int_start && p < self.int_end {
                if b[p] == b'_' {
                    p += 1;
                    continue;
                }
                return p;
            }
            if p >= self.int_end && p < self.frac_start {
                p = self.frac_start;
                continue;
            }
            if p >= self.frac_start && p < self.frac_end {
                if b[p] == b'_' {
                    p += 1;
                    continue;
                }
                return p;
            }
            return usize::MAX;
        }
    }

    /// Physical position of the next digit after the digit at `p`.
    const fn next(&self, b: &[u8], p: usize) -> usize {
        self.settle(b, p + 1)
    }

    /// Physical position of the last digit, or `usize::MAX` when empty.
    /// The backward mirror of [`Self::first`]/[`Self::next`], used only by
    /// the trailing-zero trim (which walks strictly backward).
    const fn last(&self, b: &[u8]) -> usize {
        let mut p = self.frac_end;
        loop {
            if p > self.frac_start {
                if b[p - 1] == b'_' {
                    p -= 1;
                    continue;
                }
                return p - 1;
            }
            if p <= self.frac_start && p > self.int_end {
                p = self.int_end;
                continue;
            }
            if p > self.int_start {
                if b[p - 1] == b'_' {
                    p -= 1;
                    continue;
                }
                return p - 1;
            }
            return usize::MAX;
        }
    }

    /// Physical position of the digit before the digit at `p`.
    const fn prev(&self, b: &[u8], p: usize) -> usize {
        // Reuse the backward walk with a temporarily clipped view.
        let clipped = DigitRegions {
            int_start: self.int_start,
            int_end: if p <= self.int_end { p } else { self.int_end },
            frac_start: self.frac_start,
            frac_end: if p > self.frac_start {
                p
            } else {
                self.frac_start
            },
        };
        clipped.last(b)
    }
}

/// Scan a digit run that may contain `_` separators, starting at `i`.
/// Returns `(end, digit_count)` — `end` is the first byte past the run,
/// `digit_count` excludes separators. The placement rule is stricter than
/// Rust's, one spelling per grouping: a separator is legal **iff flanked
/// by digits on both sides** (never leading, trailing, doubled, or
/// dot-adjacent — the flanks close all four). A leading `_` is judged by
/// context (`leading_sep_err`): at a literal's start it terminates the run
/// (identifiers own `_`-leading spellings, so `_1` is never a number's
/// problem to diagnose), but after a `.` or an exponent marker no
/// identifier can follow, so there it is the misplaced-separator error.
const fn scan_digit_run(
    b: &[u8],
    start: usize,
    leading_sep_err: bool,
) -> Result<(usize, usize), NumberError> {
    let mut i = start;
    let mut digits = 0usize;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            digits += 1;
            i += 1;
        } else if c == b'_' {
            if i == start {
                if leading_sep_err {
                    return Err(NumberError::BadSeparator);
                }
                break;
            }
            if i + 1 >= b.len() || !b[i + 1].is_ascii_digit() {
                return Err(NumberError::BadSeparator);
            }
            i += 1;
        } else {
            break;
        }
    }
    Ok((i, digits))
}

/// The storage rule (RFC 0016 §1.1, normative and total), over scanned
/// source parts. `exp` is the already-accumulated (saturating) exponent for
/// coercion inputs; 0 for literals. The digit regions contain ASCII digits
/// and placement-valid `_` separators (spelling only — every count below is
/// digit-only, so separators can never perturb significance, scale, or the
/// 34-digit acceptance). All counts run in usize/i64 and are validated
/// against the caps BEFORE narrowing into i128/i16 (§1.2 — the
/// narrowing-wrap bug class).
const fn from_scan(
    b: &[u8],
    neg: bool,
    regions: DigitRegions,
    int_digits: usize,
    frac_digits: usize,
    exp: i64,
) -> Result<Number, NumberError> {
    // Written scale: fraction-digit count with the exponent folded in
    // (negative for exponent-shifted coercion inputs).
    let ws: i64 = (frac_digits as i64).saturating_sub(exp);
    let all_len = int_digits + frac_digits;

    // Leading-zero trim: forward cursor walk counting zeros.
    let mut leading = 0usize;
    let mut first_pos = regions.first(b);
    while first_pos != usize::MAX && b[first_pos] == b'0' {
        leading += 1;
        first_pos = regions.next(b, first_pos);
    }
    if leading == all_len {
        // Zero: always accepted; scale = clamp(ws, 0, 6176). The floor is 0
        // (not −6111): negative-scale zeros would render as leading-zero
        // strings, so the invariant forbids them — "0e999999999999" → (0, 0).
        return Ok(Number {
            coeff: 0,
            scale: clamp_i64(ws, 0, SCALE_HI) as i16,
        });
    }
    // Trailing-zero trim: backward cursor walk.
    let mut trailing_n = 0usize;
    let mut last_pos = regions.last(b);
    while b[last_pos] == b'0' {
        trailing_n += 1;
        last_pos = regions.prev(b, last_pos);
    }
    let trailing = trailing_n as i64;
    let d = all_len - leading - trailing_n; // digit count of c_min
    let s_min = ws.saturating_sub(trailing);
    let integral = s_min <= 0;

    // Acceptance: d ≤ 34 and W nonempty; TooManyDigits takes precedence
    // when both fail.
    if d > 34 {
        return Err(NumberError::Range(NumberRangeIssue::TooManyDigits {
            got: d,
            integral,
        }));
    }

    // W = [max(s_min, −6111), min(s_min + 34 − d, 6176)]
    let w_lo = if s_min > SCALE_LO { s_min } else { SCALE_LO };
    let w_hi = {
        let h = s_min.saturating_add((34 - d) as i64);
        if h < SCALE_HI { h } else { SCALE_HI }
    };
    if w_lo > w_hi {
        return Err(NumberError::Range(if s_min > SCALE_HI {
            NumberRangeIssue::TooSmall
        } else {
            NumberRangeIssue::TooLarge
        }));
    }

    let scale = clamp_i64(ws, w_lo, w_hi);

    // c_min fits i128: d ≤ 34 < 39 digits. Forward cursor from the first
    // significant digit, consuming exactly `d` digits (interior zeros
    // included; the trimmed trailing zeros are never visited).
    let mut c: i128 = 0;
    let mut p = first_pos;
    let mut consumed = 0usize;
    while consumed < d {
        c = c * 10 + (b[p] - b'0') as i128;
        consumed += 1;
        p = regions.next(b, p);
    }
    // Append scale − s_min ∈ [0, 34 − d] trailing zeros.
    let mut k = scale - s_min;
    while k > 0 {
        c *= 10;
        k -= 1;
    }
    if neg {
        c = -c;
    }
    Ok(Number {
        coeff: c,
        scale: scale as i16,
    })
}

impl Number {
    /// The canonical zero, `(0, 0)`.
    pub const ZERO: Number = Number { coeff: 0, scale: 0 };

    /// The stored cohort member's coefficient (`value = coeff × 10^(−scale)`).
    pub const fn coeff(&self) -> i128 {
        self.coeff
    }

    /// The stored cohort member's scale. `scale > 0` is fraction form (the
    /// author wrote fraction digits); `scale ≤ 0` is integer form.
    pub const fn scale(&self) -> i16 {
        self.scale
    }

    /// Parse the strict **literal** grammar: `-? digits ( '.' digits )?`
    /// (RFC 0016 §1.4). Exactly `-? digits '.'` → [`NumberError::TrailingDot`]
    /// (so the CST can suggest dropping the dot); anything else malformed →
    /// [`NumberError::Malformed`]. `const`, so [`num!`](crate::num) can
    /// reject invalid literals at compile time.
    pub const fn parse_literal(s: &str) -> Result<Number, NumberError> {
        let b = s.as_bytes();
        let mut i = 0usize;
        let neg = if !b.is_empty() && b[0] == b'-' {
            i = 1;
            true
        } else {
            false
        };
        let int_start = i;
        let (int_end, int_digits) = match scan_digit_run(b, i, false) {
            Ok(r) => r,
            Err(e) => return Err(e),
        };
        if int_digits == 0 {
            return Err(NumberError::Malformed);
        }
        i = int_end;
        let mut frac_start = i;
        let mut frac_end = i;
        let mut frac_digits = 0usize;
        if i < b.len() && b[i] == b'.' {
            i += 1;
            frac_start = i;
            (frac_end, frac_digits) = match scan_digit_run(b, i, true) {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
            i = frac_end;
            if frac_digits == 0 {
                return if i == b.len() {
                    Err(NumberError::TrailingDot)
                } else {
                    Err(NumberError::Malformed)
                };
            }
        }
        if i != b.len() {
            return Err(NumberError::Malformed);
        }
        let regions = DigitRegions {
            int_start,
            int_end,
            frac_start,
            frac_end,
        };
        from_scan(b, neg, regions, int_digits, frac_digits, 0)
    }

    /// Parse the liberal **coercion** grammar for machine-emitted data
    /// (env values; RFC 0016 §1.4):
    /// `[+-]? ( digits ( '.' digits? )? | '.' digits ) ( [eE] [+-]? digits )?`
    ///
    /// Converted exactly — mantissa digits with the exponent folded into the
    /// written scale — under the same value-based acceptance as literals.
    /// The exponent accumulates **saturating**: linear, allocation-free, no
    /// numeric overflow regardless of exponent length (`"0e999999999999"` →
    /// zero; `"1e999999999999"` → [`NumberRangeIssue::TooLarge`]).
    pub const fn parse_coercion(s: &str) -> Result<Number, NumberError> {
        let b = s.as_bytes();
        let mut i = 0usize;
        let mut neg = false;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            neg = b[i] == b'-';
            i += 1;
        }
        let int_start = i;
        let (int_end, int_digits) = match scan_digit_run(b, i, false) {
            Ok(r) => r,
            Err(e) => return Err(e),
        };
        i = int_end;
        let mut frac_start = i;
        let mut frac_end = i;
        let mut frac_digits = 0usize;
        if i < b.len() && b[i] == b'.' {
            i += 1;
            frac_start = i;
            (frac_end, frac_digits) = match scan_digit_run(b, i, true) {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
            i = frac_end;
            // The `'.' digits` alternative requires fraction digits when the
            // integer part is absent.
            if int_digits == 0 && frac_digits == 0 {
                return Err(NumberError::Malformed);
            }
        } else if int_digits == 0 {
            return Err(NumberError::Malformed);
        }
        let mut exp: i64 = 0;
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            i += 1;
            let mut eneg = false;
            if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                eneg = b[i] == b'-';
                i += 1;
            }
            let (e_end, e_digits) = match scan_digit_run(b, i, true) {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
            if e_digits == 0 {
                return Err(NumberError::Malformed);
            }
            let mut acc: i64 = 0;
            while i < e_end {
                if b[i] != b'_' {
                    acc = acc.saturating_mul(10).saturating_add((b[i] - b'0') as i64);
                }
                i += 1;
            }
            exp = if eneg { acc.saturating_neg() } else { acc };
        }
        if i != b.len() {
            return Err(NumberError::Malformed);
        }
        let regions = DigitRegions {
            int_start,
            int_end,
            frac_start,
            frac_end,
        };
        from_scan(b, neg, regions, int_digits, frac_digits, exp)
    }

    /// Direct construction from a raw `(coeff, scale)` pair, validating the
    /// invariants **raw** — no clamping, no normalization (storage-rule
    /// canonicalization is the parser's job, not the constructor's). May
    /// therefore construct valid cohort members the parser never produces
    /// (`(1, −100)` is legal; parsing `10^100` stores `(10^33, −67)`).
    pub const fn try_new(coeff: i128, scale: i16) -> Result<Number, NumberError> {
        if coeff.unsigned_abs() > COEFF_ABS_MAX {
            // The RAW pair is what failed (no clamping, no
            // normalization — see the contract above), so the error
            // names the coefficient's own width, not the value's
            // significant digits; the variant doc has the worked case.
            return Err(NumberError::Range(NumberRangeIssue::CoefficientTooWide {
                got: digit_count_u128(coeff.unsigned_abs()) as usize,
            }));
        }
        // Zero first, most-specific-diagnosis-first: a negative-scale
        // zero's root cause is the zero rule, and routing it through the
        // window check sent authors on a two-step detour ((0, −6112) →
        // "renormalize" → (0, −6111) → rejected again).
        if coeff == 0 && scale < 0 {
            return Err(NumberError::Range(NumberRangeIssue::NegativeScaleZero {
                got: scale,
            }));
        }
        // Scale bounds are raw-pair rejections too: `(1, −6112)` is an
        // invalid PAIR while its value 10^6112 is representable (as
        // `(10, −6111)`), so borrowing TooLarge/TooSmall's value-domain
        // wording ("exceeds the largest exact NML number") would be
        // false — the same honesty rule that separates
        // CoefficientTooWide from TooManyDigits.
        if (scale as i64) < SCALE_LO || (scale as i64) > SCALE_HI {
            return Err(NumberError::Range(NumberRangeIssue::ScaleOutOfRange {
                got: scale,
            }));
        }
        Ok(Number { coeff, scale })
    }

    /// The value's mathematical normalized pair `(c_min, s_min)` in
    /// `(i128, i32)` — computed arithmetically, deliberately **not** routed
    /// through [`Number::try_new`]: the normalized pair of a valid Number
    /// may lie outside the constructible window (`10^6144` = `(10^33,
    /// −6111)` normalizes to `(1, −6144)`), and that is fine — it is a pure
    /// cohort invariant, which is all [`Hash`] needs. Zero → `(0, 0)`.
    fn normalized(&self) -> (i128, i32) {
        if self.coeff == 0 {
            return (0, 0);
        }
        let mut c = self.coeff;
        let mut s = self.scale as i32;
        while c % 10 == 0 {
            c /= 10;
            s -= 1;
        }
        (c, s)
    }

    /// Exact decimal divisibility (RFC 0018): `true` iff `self / m` is
    /// an integer — decided on the normalized pairs with modular
    /// arithmetic, never through binary floating point (where
    /// `0.3 / 0.1` famously is not 3). A predicate, not arithmetic: it
    /// returns a boolean, rounds nothing, allocates nothing, and keeps
    /// the RFC 0016 no-arithmetic-engine posture intact.
    ///
    /// `m == 0` is `false` (nothing is a multiple of zero; RFC 0018's
    /// loader rejects `multipleOf = 0` before this is ever asked).
    /// Zero is a multiple of everything nonzero. Sign is irrelevant
    /// (`-0.2` is a multiple of `0.1`); divisibility is value-based,
    /// so every cohort spelling answers identically.
    pub fn is_multiple_of(&self, m: &Number) -> bool {
        if m.coeff == 0 {
            return false;
        }
        if self.coeff == 0 {
            return true;
        }
        let (cv, sv) = self.normalized();
        let (cm, sm) = m.normalized();
        let cv = cv.unsigned_abs();
        let cm = cm.unsigned_abs();
        // v/m = (cv/cm) x 10^(sm - sv); integer iff the power shifts
        // cv onto a multiple of cm.
        let d = i64::from(sm) - i64::from(sv);
        if d >= 0 {
            // cm | cv * 10^d — via residues: coefficients are < 10^34
            // (< 2^113), whose products overflow u128, so the multiply
            // is a double-and-add mulmod; the power is square-and-
            // multiply (d <= 12352, bounded by the scale window).
            mulmod(cv % cm, powmod10(d as u32, cm), cm) == 0
        } else {
            // (cm * 10^-d) | cv. |cv| < 10^34, so a divisor at or
            // beyond 10^34 — or one that overflows the checked
            // multiply — already exceeds cv: false without computing.
            let k = (-d) as u32;
            if k >= 34 {
                return false;
            }
            match cm.checked_mul(10u128.pow(k)) {
                Some(div) => cv % div == 0,
                None => false,
            }
        }
    }

    /// The minimal member of this value's cohort with `scale ≥ 0`:
    /// value-preserving trailing-zero removal (`8080.000` → `8080`,
    /// `2.50` → `2.5`, `0.00` → `0`; integer forms are already minimal).
    /// The single home for cohort simplification — the LSP's
    /// "Simplify number" action rewrites literals to this member's
    /// Display form.
    pub fn simplified(&self) -> Number {
        let mut coeff = self.coeff;
        let mut scale = self.scale;
        while scale > 0 && coeff % 10 == 0 {
            coeff /= 10;
            scale -= 1;
        }
        Number { coeff, scale }
    }

    /// GDA *compare-total* restricted to NML's domain, named per the
    /// [`f64::total_cmp`] convention (RFC 0016 §1.3): numeric order first;
    /// within a cohort the member with the larger exponent (smaller scale)
    /// ranks higher for positives (`12.0 < 12`), flipped for negatives
    /// (`−12 < −12.0`); zeros rank positive-style. Deviation from IEEE
    /// `totalOrder`: no −0 or NaN ranks (unrepresentable). The deterministic
    /// tiebreak for representation-sensitive consumers.
    pub fn total_cmp(&self, other: &Self) -> Ordering {
        match self.cmp(other) {
            Ordering::Equal => {
                let ord = other.scale.cmp(&self.scale); // smaller scale ⇒ Greater
                if self.coeff < 0 { ord.reverse() } else { ord }
            }
            o => o,
        }
    }

    /// Whether the value is integral (no fractional part) — value-based
    /// and form-independent: `(250, 1)` = 25.0 is integral. Used by the
    /// serde bridge to word fractional-vs-range rejections precisely.
    pub(crate) fn is_integral(&self) -> bool {
        self.normalized().1 <= 0
    }

    /// Integral magnitude via the u128 path with checked 10^k scaling
    /// (RFC 0016 §1.6). Value-based integrality: fraction-form members whose
    /// value is integral (`(250, 1)` = 25.0) convert; `None` means a real
    /// fractional part or overflow — never truncation.
    fn integral_magnitude(&self) -> Option<(bool, u128)> {
        if self.coeff == 0 {
            return Some((false, 0));
        }
        let neg = self.coeff < 0;
        let mut m = self.coeff.unsigned_abs();
        let mut s = self.scale as i32;
        while s > 0 {
            if m % 10 != 0 {
                return None;
            }
            m /= 10;
            s -= 1;
        }
        while s < 0 {
            m = m.checked_mul(10)?;
            s += 1;
        }
        Some((neg, m))
    }

    /// The exact `i64`, when the **value** is integral and in range
    /// (`25.0` → `Some(25)`); `None` otherwise — never truncates.
    pub fn to_i64(&self) -> Option<i64> {
        let (neg, m) = self.integral_magnitude()?;
        if neg {
            if m == (i64::MAX as u128) + 1 {
                Some(i64::MIN)
            } else if m <= i64::MAX as u128 {
                Some(-(m as i64))
            } else {
                None
            }
        } else {
            i64::try_from(m).ok()
        }
    }

    /// The exact `u64`, when the value is integral, non-negative, in range.
    pub fn to_u64(&self) -> Option<u64> {
        let (neg, m) = self.integral_magnitude()?;
        if neg {
            return None;
        }
        u64::try_from(m).ok()
    }

    /// The exact `i128`, when the value is integral and in range.
    pub fn to_i128(&self) -> Option<i128> {
        let (neg, m) = self.integral_magnitude()?;
        if neg {
            if m == (i128::MAX as u128) + 1 {
                Some(i128::MIN)
            } else if m <= i128::MAX as u128 {
                Some(-(m as i128))
            } else {
                None
            }
        } else {
            i128::try_from(m).ok()
        }
    }

    /// The exact `u128`, when the value is integral, non-negative, in range.
    pub fn to_u128(&self) -> Option<u128> {
        let (neg, m) = self.integral_magnitude()?;
        if neg {
            return None;
        }
        Some(m)
    }

    /// The value's adjusted exponent, `floor(log10 |v|)` for nonzero
    /// values — the extreme-magnitude gate for the binary conversions.
    fn adjusted_exponent(&self) -> i32 {
        digit_count_u128(self.coeff.unsigned_abs()) as i32 - 1 - self.scale as i32
    }

    /// The **correctly rounded** `f64` (round-to-nearest-even): formats the
    /// plain digits and parses via std, whose Eisel–Lemire path plus
    /// correctly-rounded fallback (rust-lang/rust#86761) guarantees hard
    /// cases at any digit count. Documented-lossy: values beyond f64's range
    /// overflow to ±∞ per IEEE conversion semantics.
    ///
    /// Extreme magnitudes short-circuit without materializing the plain
    /// form (security review): a 7-byte coercion input like `"1e-6176"`
    /// must not cost a ~6 KB format + slow-path parse per conversion.
    /// The cutoffs sit far outside any rounding ambiguity: adjusted
    /// exponent ≥ 310 ⇒ |v| ≥ 10^310 > f64::MAX (±∞); ≤ −350 ⇒
    /// |v| < 10^−349, under half the smallest subnormal (±0).
    pub fn to_f64(&self) -> f64 {
        // No `coeff != 0` guard: zero's adjusted exponent is `−scale ∈
        // [−6176, 0]`, so the ≥310 branch is unreachable for it and the
        // ≤−350 branch returns +0.0 — correct. Guarding cost `0e-6176`
        // (a 7-byte coercion input) a 6 KB materialization per call:
        // 494× slower than the same-shaped nonzero, i.e. the very
        // amplification this shortcut exists to close.
        {
            let ae = self.adjusted_exponent();
            if ae >= 310 {
                return if self.coeff < 0 {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                };
            }
            if ae <= -350 {
                return if self.coeff < 0 { -0.0 } else { 0.0 };
            }
        }
        self.to_string()
            .parse::<f64>()
            .expect("plain decimal digits always parse as f64")
    }

    /// The **correctly rounded** `f32`, via a **direct** f32 parse of the
    /// canonical digits — never `to_f64() as f32`, which double-rounds
    /// (`16777217.0000000001` → 16777216 via f64, 16777218 direct).
    /// Same extreme-magnitude short-circuit as [`Number::to_f64`], at
    /// f32's range (≥ 10^40 ⇒ ±∞; < 10^−59 ⇒ ±0).
    pub fn to_f32(&self) -> f32 {
        {
            let ae = self.adjusted_exponent();
            if ae >= 40 {
                return if self.coeff < 0 {
                    f32::NEG_INFINITY
                } else {
                    f32::INFINITY
                };
            }
            if ae <= -60 {
                return if self.coeff < 0 { -0.0 } else { 0.0 };
            }
        }
        self.to_string()
            .parse::<f32>()
            .expect("plain decimal digits always parse as f32")
    }

    /// Foreign binary→decimal conversion (RFC 0016 §1.6): the **shortest
    /// decimal that round-trips to the same f64** (Rust's f64 `Display`
    /// digits), parsed exactly — the canonical decimal a human would have
    /// written. The exact binary expansion would be wrong by construction
    /// (`0.1_f64` expands to 55 significant digits). NaN/∞ →
    /// [`NumberError::Malformed`].
    pub fn try_from_f64(f: f64) -> Result<Number, NumberError> {
        if !f.is_finite() {
            return Err(NumberError::Malformed);
        }
        // f64 Display renders shortest-round-trip digits positionally (no
        // exponent form), which is exactly the strict literal grammar.
        Number::parse_literal(&format!("{f}"))
    }
}

/// The pinned Display algorithm (RFC 0016 §1.10): sign (from `coeff`);
/// `scale ≤ 0` → the coefficient digits followed by `−scale` zeros;
/// `scale > 0` → the digits split at `scale` from the right, zero-padded
/// through the integer position (`(5, 3)` → `"0.005"`, `(0, 2)` → `"0.00"`).
/// Output always re-parses to an `Eq`-equal Number (plain form only — never
/// exponent notation).
impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.coeff < 0 {
            f.write_str("-")?;
        }
        let mag = self.coeff.unsigned_abs().to_string();
        if self.scale <= 0 {
            f.write_str(&mag)?;
            for _ in 0..(-(self.scale as i32)) {
                f.write_str("0")?;
            }
            Ok(())
        } else {
            let s = self.scale as usize;
            if mag.len() > s {
                let (int_part, frac_part) = mag.split_at(mag.len() - s);
                write!(f, "{int_part}.{frac_part}")
            } else {
                f.write_str("0.")?;
                for _ in 0..(s - mag.len()) {
                    f.write_str("0")?;
                }
                f.write_str(&mag)
            }
        }
    }
}

/// The normative three-step ordering (RFC 0016 §1.3): zeros first, then
/// sign, then same-sign magnitudes by adjusted exponent with bounded
/// coefficient alignment on ties (≤ 34 aligned digits — inside i128 by
/// construction). Exact, allocation-free, total, `Eq`-consistent.
impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        // Step 1: zeros (no meaningful adjusted exponent).
        match (self.coeff == 0, other.coeff == 0) {
            (true, true) => return Ordering::Equal,
            (true, false) => {
                return if other.coeff > 0 {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (false, true) => {
                return if self.coeff > 0 {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (false, false) => {}
        }
        // Step 2: different signs.
        let sneg = self.coeff < 0;
        let oneg = other.coeff < 0;
        if sneg != oneg {
            return if sneg {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        // Step 3: same sign — magnitudes by adjusted exponent
        // digits(|coeff|) − 1 − scale; tie → align (gap = digit-count gap
        // ≤ 33, so the aligned coefficient stays < 10^34) and compare;
        // negate if both negative.
        let a = self.coeff.unsigned_abs();
        let b = other.coeff.unsigned_abs();
        let ae = digit_count_u128(a) as i32 - 1 - self.scale as i32;
        let be = digit_count_u128(b) as i32 - 1 - other.scale as i32;
        let mag = match ae.cmp(&be) {
            Ordering::Equal => {
                let sa = self.scale as i32;
                let sb = other.scale as i32;
                if sa < sb {
                    let aa = a
                        .checked_mul(10u128.pow((sb - sa) as u32))
                        .expect("alignment bounded by the 34-digit invariant");
                    aa.cmp(&b)
                } else if sb < sa {
                    let bb = b
                        .checked_mul(10u128.pow((sa - sb) as u32))
                        .expect("alignment bounded by the 34-digit invariant");
                    a.cmp(&bb)
                } else {
                    a.cmp(&b)
                }
            }
            o => o,
        };
        if sneg { mag.reverse() } else { mag }
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Numeric equality: `1.5 == 1.50 == 1.500`, `2 == 2.0` (RFC 0016 §1.3).
impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Number {}

/// Hash over the mathematical normalized pair — coherent with the numeric
/// `Eq` (`hash(1.5) == hash(1.50)`), making `Number` a valid map key.
impl Hash for Number {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let (c_min, s_min) = self.normalized();
        c_min.hash(state);
        s_min.hash(state);
    }
}

/// Exact integer comparison ergonomics (`n == 3`, `n > 1`); the f64
/// equivalents are deliberately absent — under exact decimals
/// `n == 0.1_f64` would be correctly-but-surprisingly false. Decimal
/// thresholds use [`num!`](crate::num): `n > num!(1.0)`.
impl PartialEq<i64> for Number {
    fn eq(&self, other: &i64) -> bool {
        *self == Number::from(*other)
    }
}

impl PartialOrd<i64> for Number {
    fn partial_cmp(&self, other: &i64) -> Option<Ordering> {
        Some(self.cmp(&Number::from(*other)))
    }
}

/// Strict literal grammar (`-? digits ( '.' digits )?`), same as
/// [`Number::parse_literal`].
impl FromStr for Number {
    type Err = NumberError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Number::parse_literal(s)
    }
}

/// The one integer `From` impl, deliberately: a second (`From<u64>`)
/// would make plain integer literals ambiguous at every
/// `impl Into<Number>` call site (`Value::number(8080)`). Wider exact
/// construction is [`Number::from_u64`] / [`TryFrom<i128>`] /
/// [`TryFrom<u128>`].
impl From<i64> for Number {
    fn from(v: i64) -> Self {
        Number {
            coeff: v as i128,
            scale: 0,
        }
    }
}

impl Number {
    /// Exact `u64` construction (infallible: 20 digits ≤ the 34-digit
    /// budget). Inherent rather than `From` to keep integer-literal
    /// inference unambiguous at `Into<Number>` call sites.
    pub const fn from_u64(v: u64) -> Number {
        Number {
            coeff: v as i128,
            scale: 0,
        }
    }
}

impl TryFrom<i128> for Number {
    type Error = NumberError;
    fn try_from(v: i128) -> Result<Self, NumberError> {
        let mag = v.unsigned_abs().to_string();
        let b = mag.as_bytes();
        let regions = DigitRegions {
            int_start: 0,
            int_end: b.len(),
            frac_start: b.len(),
            frac_end: b.len(),
        };
        from_scan(b, v < 0, regions, b.len(), 0, 0)
    }
}

impl TryFrom<u128> for Number {
    type Error = NumberError;
    fn try_from(v: u128) -> Result<Self, NumberError> {
        let mag = v.to_string();
        let b = mag.as_bytes();
        let regions = DigitRegions {
            int_start: 0,
            int_end: b.len(),
            frac_start: b.len(),
            frac_end: b.len(),
        };
        from_scan(b, false, regions, b.len(), 0, 0)
    }
}

/// The serialize ladder (RFC 0016 §1.7): **never binary floating point,
/// and never a form the ecosystem cannot read back exactly.**
///
/// Integer form (`scale ≤ 0`) emits a native integer only through
/// **`u64`/`i64`** — the widest range every mainstream format actually
/// round-trips. Beyond that, and for every fraction form (`scale > 0`,
/// even when numerically integral: `25.0` → `"25.0"`), the value is the
/// exact plain string.
///
/// The `i128`/`u128` rungs were deliberately **removed** after an audit
/// (they were in the RFC's first draft): serde_json *writes* 128-bit
/// integers but its `Deserializer` cannot read one back — `deserialize_any`
/// has no 128-bit rung, so a 20-digit integer returned through `visit_f64`
/// rounded, and `Number → JSON → Number` silently lost exactness. A wire
/// form only half the ecosystem can carry is precisely the silent-loss
/// trap this RFC exists to remove, so exactness wins over looking like a
/// number.
///
/// Honest limits of the remaining rungs (measured, not assumed): TOML's
/// integers are i64 by spec, so the `u64` rung fails to *serialize* there
/// — loudly, and the string rung above it does not. And a `Number` cannot
/// be deserialized from RON at all: the handshake token is not a valid RON
/// identifier. Both are properties of those formats meeting the private
/// token, not silent-loss paths: nothing rounds, everything errors.
impl Serialize for Number {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.scale <= 0 {
            if let Some(v) = self.to_i64() {
                return serializer.serialize_i64(v);
            }
            if let Some(v) = self.to_u64() {
                return serializer.serialize_u64(v);
            }
        }
        serializer.collect_str(self)
    }
}

struct NumberVisitor;

impl<'de> Visitor<'de> for NumberVisitor {
    type Value = Number;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an exact NML number")
    }

    /// Self-describing foreign formats (serde_json) answer the token
    /// request by wrapping the inner value in `visit_newtype_struct`;
    /// unwrap and re-dispatch to the numeric/string arms below.
    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Number, D::Error> {
        deserializer.deserialize_any(NumberVisitor)
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Number, E> {
        Ok(Number::from(v))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Number, E> {
        Ok(Number::from_u64(v))
    }

    fn visit_i128<E: serde::de::Error>(self, v: i128) -> Result<Number, E> {
        Number::try_from(v).map_err(E::custom)
    }

    fn visit_u128<E: serde::de::Error>(self, v: u128) -> Result<Number, E> {
        Number::try_from(v).map_err(E::custom)
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Number, E> {
        Number::try_from_f64(v).map_err(E::custom)
    }

    /// Strings parse with the liberal §1.4 **data** grammar (Postel for
    /// machine-emitted forms), which serves double duty: NML's own
    /// Deserializer answers the handshake with the compact member
    /// encoding `{coeff}e{-scale}` (O(40) bytes for ANY member — the
    /// plain form of extreme-scale values is ~6 KB — and, unlike the
    /// plain form, it round-trips the stored cohort member exactly, not
    /// just the value), and foreign-format strings get the same
    /// treatment as every other coerced string.
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Number, E> {
        Number::parse_coercion(v).map_err(E::custom)
    }
}

/// Exact capture (RFC 0016 §1.7 tier 3): negotiates the
/// `NUMBER_NEWTYPE_TOKEN` handshake with NML's own `Deserializer` (which
/// answers with the compact member encoding `{coeff}e{-scale}`); foreign
/// formats treat the newtype as transparent and hit the visitor's
/// numeric/string arms — integers exactly, floats via the
/// shortest-round-trip rule, strings via the liberal §1.4 data grammar
/// (which decodes both the handshake encoding and the plain strings
/// [`Number`]'s own `Serialize` emits).
impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_newtype_struct(NUMBER_NEWTYPE_TOKEN, NumberVisitor)
    }
}

/// Compile-time-checked exact decimal literal (RFC 0016 §1.6; the
/// `rust_decimal::dec!` precedent):
///
/// ```rust
/// use nml_core::{num, decimal::Number};
/// const RATE: Number = num!(0.20);
/// assert_eq!(num!(2.50).to_string(), "2.50");   // written scale preserved
/// assert!(num!(-1.5) < num!(1));
/// ```
///
/// Invalid literals — malformed, out of the decimal128 domain, or
/// underscored (`num!(1_000.5)`; the macro mirrors the literal grammar,
/// which has no separators) — are **compile errors** via const evaluation.
#[macro_export]
macro_rules! num {
    ($lit:literal) => {
        const {
            match $crate::decimal::Number::parse_literal(::core::stringify!($lit)) {
                ::core::result::Result::Ok(n) => n,
                ::core::result::Result::Err(_) => {
                    ::core::panic!("invalid NML number literal")
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn lit(s: &str) -> Number {
        Number::parse_literal(s).unwrap_or_else(|e| {
            panic!(
                "literal {:?}… (len {}) failed: {e:?}",
                &s[..s.len().min(40)],
                s.len()
            )
        })
    }
    fn co(s: &str) -> Number {
        Number::parse_coercion(s).unwrap_or_else(|e| {
            panic!(
                "coercion {:?}… (len {}) failed: {e:?}",
                &s[..s.len().min(40)],
                s.len()
            )
        })
    }
    fn parts(n: Number) -> (i128, i16) {
        (n.coeff(), n.scale())
    }
    fn p10(n: u32) -> i128 {
        10i128.pow(n)
    }
    fn pow10_str(n: usize) -> String {
        format!("1{}", "0".repeat(n))
    }
    fn subnormal_str() -> String {
        format!("0.{}5{}", "0".repeat(6150), "0".repeat(33))
    }
    fn hsh(n: &Number) -> u64 {
        let mut h = DefaultHasher::new();
        n.hash(&mut h);
        h.finish()
    }

    // ---- §1.1 worked consequences -------------------------------------

    #[test]
    fn written_scale_preserved() {
        assert_eq!(parts(lit("2.50")), (250, 2));
        assert_eq!(parts(lit("8080.0")), (80800, 1));
    }

    #[test]
    fn pow34_clamps() {
        // 10^34: d = 1, s_min = −34, W = [−34, −1], clamp(0) = −1.
        assert_eq!(parts(lit(&pow10_str(34))), (p10(33), -1));
    }

    #[test]
    fn pow6144_parses_pow6145_too_large() {
        assert_eq!(parts(lit(&pow10_str(6144))), (p10(33), -6111));
        assert_eq!(
            Number::parse_literal(&pow10_str(6145)).unwrap_err(),
            NumberError::Range(NumberRangeIssue::TooLarge)
        );
    }

    #[test]
    fn pow6112_parses() {
        // s_min = −6112 < −6111, W = [−6111, −6079], clamp(0) = −6079.
        assert_eq!(parts(lit(&pow10_str(6112))), (p10(33), -6079));
    }

    #[test]
    fn subnormal_clamp() {
        // "0." + 6150 zeros + "5" + 33 zeros → (5×10^25, 6176).
        assert_eq!(parts(lit(&subnormal_str())), (5 * p10(25), 6176));
    }

    #[test]
    fn zero_long_fraction() {
        assert_eq!(parts(lit(&format!("0.{}", "0".repeat(6200)))), (0, 6176));
    }

    #[test]
    fn below_min_too_small() {
        let s = format!("0.{}1", "0".repeat(6176));
        assert_eq!(
            Number::parse_literal(&s).unwrap_err(),
            NumberError::Range(NumberRangeIssue::TooSmall)
        );
    }

    // ---- §2 edge tables ------------------------------------------------

    #[test]
    fn boundaries_34_digits() {
        let n34 = "9".repeat(34);
        assert_eq!(parts(lit(&n34)), (COEFF_ABS_MAX as i128, 0));
        let n35 = "9".repeat(35);
        assert_eq!(
            Number::parse_literal(&n35).unwrap_err(),
            NumberError::Range(NumberRangeIssue::TooManyDigits {
                got: 35,
                integral: true
            })
        );
        let f34 = format!("0.{}", "9".repeat(34));
        assert_eq!(parts(lit(&f34)), (COEFF_ABS_MAX as i128, 34));
        let f35 = format!("0.{}", "9".repeat(35));
        assert_eq!(
            Number::parse_literal(&f35).unwrap_err(),
            NumberError::Range(NumberRangeIssue::TooManyDigits {
                got: 35,
                integral: false
            })
        );
        assert_eq!(
            Number::parse_literal("1234567890123456789012345678901234.5").unwrap_err(),
            NumberError::Range(NumberRangeIssue::TooManyDigits {
                got: 35,
                integral: false
            })
        );
    }

    #[test]
    fn narrowing_wrap_killers() {
        // Fraction lengths at the i16 wrap (32 768) and u16 wraps
        // (65 536 / 65 538): the error KIND must be TooSmall — never a
        // wrong-value parse via a scale wrap.
        for frac_len in [32768usize, 65536, 65538] {
            let s = format!("0.{}1", "0".repeat(frac_len - 1));
            match Number::parse_literal(&s) {
                Err(NumberError::Range(NumberRangeIssue::TooSmall)) => {}
                other => panic!(
                    "fraction length {frac_len}: expected TooSmall, got {:?}",
                    other.map(parts)
                ),
            }
        }
    }

    #[test]
    fn two_pow_53_plus_1_conversions() {
        let n = lit("9007199254740993"); // 2^53 + 1
        assert_eq!(n.to_i64(), Some(9007199254740993));
        assert_eq!(n.to_u64(), Some(9007199254740993));
        assert_eq!(n.to_i128(), Some(9007199254740993));
        assert_eq!(n.to_u128(), Some(9007199254740993));
        assert_eq!(n.to_string(), "9007199254740993");
        // to_f64 is correctly rounded (ties-to-even → 2^53), documented-lossy.
        assert_eq!(n.to_f64(), 9007199254740992.0);
        assert_eq!(n.to_f64(), 9007199254740993i64 as f64);
    }

    #[test]
    fn double_rounding_sentinel_f32() {
        let n = lit("16777217.0000000001");
        let direct: f32 = "16777217.0000000001".parse().unwrap();
        assert_eq!(direct, 16777218.0f32, "direct f32 parse");
        assert_eq!(n.to_f32(), direct, "to_f32 must equal direct f32 parse");
        assert_eq!(n.to_f64() as f32, 16777216.0f32, "double-rounded path");
        assert_ne!(n.to_f32(), n.to_f64() as f32);
    }

    #[test]
    fn i64_min_roundtrip() {
        let n = lit("-9223372036854775808");
        assert_eq!(n.to_i64(), Some(i64::MIN));
        assert_eq!(Number::from(i64::MIN), n);
        assert_eq!(n.to_string(), "-9223372036854775808");
        assert_eq!(n.to_u64(), None);
    }

    #[test]
    fn negative_zero_normalizes() {
        let n = lit("-0.00");
        assert_eq!(parts(n), (0, 2), "sign discarded, written scale kept");
        assert_eq!(n.to_string(), "0.00");
        assert_eq!(n, lit("0.00"));
        assert_eq!(n, lit("0"));
        assert_eq!(n, Number::ZERO);
        assert_eq!(hsh(&n), hsh(&lit("0")));
        assert_eq!(hsh(&n), hsh(&lit("0.00")));
    }

    #[test]
    fn leading_zeros_canonicalize() {
        assert_eq!(parts(lit("007")), (7, 0));
        assert_eq!(lit("007").to_string(), "7");
    }

    #[test]
    fn zero_negative_scale_forbidden() {
        // Negative-scale zeros are unrepresentable by invariant: coercion
        // clamps zero's scale to a floor of 0, so zero members round-trip
        // member-identically and Display never emits leading-zero junk.
        let z5 = co("0e5");
        assert_eq!(parts(z5), (0, 0));
        assert_eq!(z5.to_string(), "0");
        assert_eq!(
            parts(Number::parse_literal(&z5.to_string()).unwrap()),
            (0, 0)
        );
        assert!(Number::try_new(0, -5).is_err());
        assert_eq!(parts(lit("0.00")), (0, 2));
    }

    // ---- coercion ------------------------------------------------------

    #[test]
    fn coercion_accepts() {
        assert_eq!(parts(co("1e-6")), (1, 6));
        assert_eq!(parts(co("1E5")), (1, -5));
        assert_eq!(co("1E5").to_i64(), Some(100_000));
        assert_eq!(parts(co("+1")), (1, 0));
        assert_eq!(parts(co(".5")), (5, 1));
        assert_eq!(parts(co("1.")), (1, 0));
        assert_eq!(parts(co("1e00042")), (1, -42));
        assert_eq!(parts(co("1.5e3")), (15, -2));
        assert_eq!(co("1.5e3").to_i64(), Some(1500));
        let z = co("0e999999999999");
        assert_eq!(z, Number::ZERO, "0e<huge> is zero");
        assert_eq!(parts(z), (0, 0));
    }

    #[test]
    fn coercion_rejects() {
        for s in [
            "inf", "nan", "", ".", "e5", "NaN", "Infinity", "-", "+", "--1", "1.2.3", "0x10", "1e",
            "1e+", "1 ",
        ] {
            assert_eq!(
                Number::parse_coercion(s).unwrap_err(),
                NumberError::Malformed,
                "coercion {s:?}"
            );
        }
        assert_eq!(
            Number::parse_coercion("1e999999999999").unwrap_err(),
            NumberError::Range(NumberRangeIssue::TooLarge)
        );
    }

    #[test]
    fn liberal_grammar_dot_exponent_corner() {
        // `digits '.' ε [eE] digits` matches, so "1.e5" coerces (§1.4).
        assert_eq!(parts(co("1.e5")), (1, -5));
        assert_eq!(
            Number::parse_coercion("1.e").unwrap_err(),
            NumberError::Malformed
        );
    }

    #[test]
    fn trailing_dot_and_strict_rejects() {
        assert_eq!(
            Number::parse_literal("1299.").unwrap_err(),
            NumberError::TrailingDot
        );
        assert_eq!(
            Number::parse_literal("-1299.").unwrap_err(),
            NumberError::TrailingDot
        );
        assert_eq!(
            Number::parse_literal("0.").unwrap_err(),
            NumberError::TrailingDot
        );
        for s in ["+1", ".5", "1e5", "1.5e3", "", ".", "inf", "nan", "1299.x"] {
            assert_eq!(
                Number::parse_literal(s).unwrap_err(),
                NumberError::Malformed,
                "literal {s:?}"
            );
        }
    }

    // ---- `_` digit separators (RFC 0017 follow-up) ----------------------

    /// Placement rule, stricter than Rust: `_` legal iff flanked by two
    /// digits — never leading, trailing, doubled, or dot-adjacent — in
    /// both grammars and every digit region (int, frac, exponent).
    #[test]
    fn separators_accepted_where_digit_flanked() {
        assert_eq!(lit("1_000"), lit("1000"));
        assert_eq!(lit("1_000_000"), lit("1000000"));
        assert_eq!(lit("-1_000.2_5"), lit("-1000.25"));
        assert_eq!(lit("1_2.5"), lit("12.5"));
        // Written scale is digit-only: separators never perturb it.
        assert_eq!(lit("2.5_0").to_string(), "2.50");
        // Coercion grammar: mantissa and exponent regions alike.
        assert_eq!(co("1_000e1_0"), co("1000e10"));
        assert_eq!(co("+9_007_199_254_740_993"), co("9007199254740993"));
        for s in ["1__000", "1_", "1_.5", "1._5", "1_000_"] {
            assert_eq!(
                Number::parse_literal(s).unwrap_err(),
                NumberError::BadSeparator,
                "literal {s:?}"
            );
            assert_eq!(
                Number::parse_coercion(s).unwrap_err(),
                NumberError::BadSeparator,
                "coercion {s:?}"
            );
        }
        // A LEADING `_` is not a number's problem at all (identifiers own
        // that spelling) — the run terminates instead, and the caller's
        // empty-digits rule reports Malformed.
        assert_eq!(
            Number::parse_literal("_1").unwrap_err(),
            NumberError::Malformed
        );
        // Exponent-region violations are the same rule.
        assert_eq!(
            Number::parse_coercion("1e1__0").unwrap_err(),
            NumberError::BadSeparator
        );
    }

    /// The differential property: for ANY valid literal, inserting
    /// flank-legal separators anywhere yields the IDENTICAL stored member
    /// — same coefficient, same scale (separators are spelling, never
    /// value). Deterministic sweep over a value grid × every legal single
    /// insertion point, plus an all-positions insertion; hand-rolled in
    /// the cst fuzz-battery style.
    #[test]
    fn separator_transparency_differential() {
        let grid = [
            "0",
            "7",
            "42",
            "1000",
            "9007199254740993",
            "0.25",
            "12.50",
            "123456.789",
            "-8080",
            "-0.125",
            &"9".repeat(34),
        ];
        for base in grid {
            let expected = lit(base);
            let bytes: Vec<char> = base.chars().collect();
            // Every legal single insertion point: between two digits.
            for i in 1..bytes.len() {
                if bytes[i - 1].is_ascii_digit() && bytes[i].is_ascii_digit() {
                    let mut s: String = bytes[..i].iter().collect();
                    s.push('_');
                    s.extend(&bytes[i..]);
                    let got = lit(&s);
                    assert_eq!(got, expected, "{s}");
                    assert_eq!(
                        got.total_cmp(&expected),
                        Ordering::Equal,
                        "same member, not merely same cohort: {s}"
                    );
                }
            }
            // Saturated form: a separator at EVERY legal position.
            let mut sat = String::new();
            for (i, c) in bytes.iter().enumerate() {
                sat.push(*c);
                if c.is_ascii_digit() && bytes.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
                    sat.push('_');
                }
            }
            assert_eq!(lit(&sat), expected, "{sat}");
            assert_eq!(lit(&sat).total_cmp(&expected), Ordering::Equal, "{sat}");
        }
    }

    /// `num!` takes Rust's own underscore idiom (stringify! preserves the
    /// separators, so the const path exercises the transparent core).
    #[test]
    fn num_macro_accepts_separators() {
        const N: Number = crate::num!(1_000_000);
        assert_eq!(N, lit("1000000"));
        const F: Number = crate::num!(1_234.5_6);
        assert_eq!(F, lit("1234.56"));
        assert_eq!(F.to_string(), "1234.56");
    }

    // ---- §1.3 Eq / Ord / Hash / total_cmp -------------------------------

    #[test]
    fn eq_hash_cohorts() {
        assert_eq!(lit("1.5"), lit("1.50"));
        assert_eq!(lit("1.50"), lit("1.500"));
        assert_eq!(lit("2"), lit("2.0"));
        assert_eq!(hsh(&lit("1.5")), hsh(&lit("1.50")));
        assert_eq!(hsh(&lit("2")), hsh(&lit("2.0")));
        // Clamped member: normalized pair (1, −6144) lies outside the
        // constructible window; hash must be computed arithmetically.
        let big_lit = lit(&pow10_str(6144));
        let big_co = co("1e6144");
        assert_eq!(parts(big_co), (p10(33), -6111));
        assert_eq!(big_lit, big_co);
        assert_eq!(hsh(&big_lit), hsh(&big_co));
    }

    #[test]
    fn ord_basics() {
        assert!(
            lit("0") < lit("0.005"),
            "zero-first step (naive rule fails)"
        );
        assert!(lit(&pow10_str(6144)) > lit(&pow10_str(34)));
        assert_eq!(lit("1.5").cmp(&lit("1.50")), Ordering::Equal);
        assert!(lit("-2.5") < lit("-2.4"));
        assert!(lit("-2.5") < lit("2.5"));
        assert!(lit("0.7") > lit("0.19"));
        assert!(lit("-0.00") < lit("0.005"));
        assert!(lit("9007199254740993") > lit("9007199254740992"));
    }

    #[test]
    fn int_comparison_ergonomics() {
        assert!(lit("2.0") == 2);
        assert!(lit("2.5") != 2);
        assert!(lit("2.5") > 2);
        assert!(lit("-2.5") < 2);
        assert!(num!(1.5) > num!(1.0));
    }

    #[test]
    fn total_cmp_cohorts() {
        assert_eq!(lit("12.0").total_cmp(&lit("12")), Ordering::Less);
        assert_eq!(lit("-12").total_cmp(&lit("-12.0")), Ordering::Less);
        // Zeros positive-style: smaller scale ranks higher.
        assert_eq!(lit("0.00").total_cmp(&lit("0")), Ordering::Less);
        assert_eq!(co("0e5").total_cmp(&lit("0")), Ordering::Equal);
        assert_eq!(lit("12").total_cmp(&lit("12")), Ordering::Equal);
    }

    // ---- §1.7 serde ladders (through real serde, serde_json dev-dep) ----

    #[test]
    fn serialize_ladder() {
        let json = |n: &Number| serde_json::to_string(n).unwrap();
        assert_eq!(json(&lit("8080")), "8080");
        assert_eq!(json(&lit("-9223372036854775808")), "-9223372036854775808");
        assert_eq!(json(&lit("18446744073709551615")), "18446744073709551615");
        // Beyond u64 → exact STRING, not a 128-bit integer: serde_json can
        // write those but cannot read them back (no 128-bit rung in
        // `deserialize_any`), so an integer wire form would silently lose
        // exactness on the return trip. Strings round-trip everywhere.
        assert_eq!(
            json(&lit("18446744073709551617")),
            "\"18446744073709551617\""
        );
        let big = format!("2{}", "0".repeat(38));
        assert_eq!(json(&co("2e38")), format!("\"{big}\""));
        assert_eq!(json(&lit(&big)), format!("\"{big}\""));
        assert_eq!(
            json(&lit(&pow10_str(100))),
            format!("\"{}\"", pow10_str(100))
        );
        // Fraction form → exact string, scale preserved, even when integral.
        assert_eq!(json(&lit("2.50")), "\"2.50\"");
        assert_eq!(json(&lit("25.0")), "\"25.0\"");
        assert_eq!(json(&lit("8080.0")), "\"8080.0\"");
    }

    /// The property the ladder exists for: `Number → JSON → Number` is
    /// **exact for every member**, including past u64 and at extreme
    /// scale (audit finding: 128-bit integer rungs broke this because
    /// serde_json cannot read them back).
    #[test]
    fn json_round_trip_is_exact_for_every_member() {
        for n in fixtures() {
            let wire = serde_json::to_string(&n).unwrap();
            let back: Number =
                serde_json::from_str(&wire).unwrap_or_else(|e| panic!("re-read of {wire}: {e}"));
            assert_eq!(back, n, "value round-trip via {wire}");
            // Idempotent: a second cycle must not drift.
            assert_eq!(serde_json::to_string(&back).unwrap(), wire, "stable wire");
        }
    }

    #[test]
    fn deserialize_foreign_formats() {
        // JSON floats arrive via visit_f64 → shortest-round-trip rule.
        let n: Number = serde_json::from_str("0.1").unwrap();
        assert_eq!(parts(n), (1, 1));
        assert_eq!(n, num!(0.1));
        // JSON integers arrive exactly.
        let n: Number = serde_json::from_str("9007199254740993").unwrap();
        assert_eq!(n.to_i64(), Some(9007199254740993));
        // Strings round-trip our own Serialize output via the literal grammar.
        let n: Number = serde_json::from_str("\"2.50\"").unwrap();
        assert_eq!(parts(n), (250, 2));
        // Serialize → Deserialize is value-identical across the ladder for
        // everything plain serde_json can represent losslessly (≤ u64 ints
        // and all exact strings).
        for s in ["8080", "2.50", "8080.0", "-0.7", "18446744073709551615"] {
            let orig = Number::parse_literal(s).unwrap();
            let wire = serde_json::to_string(&orig).unwrap();
            let back: Number = serde_json::from_str(&wire).unwrap();
            assert_eq!(orig, back, "round-trip {s}");
        }
        // Beyond u64 the wire form is an exact STRING, so the round trip
        // stays exact even though serde_json's number reader tops out at
        // u64 (this test previously pinned the lossy 128-bit-integer
        // behavior — an audit finding, now fixed at the ladder).
        let big = lit("18446744073709551617"); // 2^64 + 1
        let wire = serde_json::to_string(&big).unwrap();
        assert_eq!(wire, "\"18446744073709551617\"");
        let back: Number = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, big, "exact across the JSON boundary");
        // A bare 128-bit JSON integer from a FOREIGN producer is still
        // read at serde_json's own f64 floor — their limit, not ours,
        // and it can never come from our Serialize.
        let foreign: Number = serde_json::from_str("18446744073709551617").unwrap();
        assert_eq!(
            foreign,
            Number::try_from_f64(18446744073709551617.0_f64).unwrap()
        );
    }

    #[test]
    fn try_from_f64_shortest_roundtrip() {
        assert_eq!(Number::try_from_f64(0.1).unwrap(), lit("0.1"));
        assert_eq!(parts(Number::try_from_f64(0.1).unwrap()), (1, 1));
        assert_eq!(
            Number::try_from_f64(f64::NAN).unwrap_err(),
            NumberError::Malformed
        );
        assert_eq!(
            Number::try_from_f64(f64::INFINITY).unwrap_err(),
            NumberError::Malformed
        );
        assert_eq!(Number::try_from_f64(-0.0).unwrap(), Number::ZERO);
        assert_eq!(
            Number::try_from_f64(9007199254740992.0).unwrap(),
            lit("9007199254740992")
        );
    }

    #[test]
    fn pow100_stored_member() {
        // s_min = −100, d = 1, W = [−100, −67], scale = clamp(0, W) = −67.
        let n = lit(&pow10_str(100));
        assert_eq!(parts(n), (p10(33), -67));
        assert_eq!(n.to_string(), pow10_str(100));
        assert_eq!(n.to_u128(), None);
    }

    /// RFC 0018 §1.3/§2: exact divisibility. The headline pin is the
    /// f64 lie — `0.3 / 0.1` is not 3 in binary64, and every
    /// float-based `multipleOf` validator mis-answers it; the exact
    /// predicate cannot. Plus the full-window spread (no overflow),
    /// cohort independence, signs, zero on both sides, and a
    /// brute-force oracle over an exact-rational grid.
    #[test]
    fn is_multiple_of_exact() {
        let n = |s: &str| Number::parse_literal(s).unwrap();
        // The f64 lie, pinned.
        assert!(n("0.3").is_multiple_of(&n("0.1")));
        assert!(!n("0.25").is_multiple_of(&n("0.1")));
        // Zero on both sides.
        assert!(n("0").is_multiple_of(&n("0.1")));
        assert!(n("0.00").is_multiple_of(&n("7")));
        assert!(!n("5").is_multiple_of(&Number::ZERO));
        assert!(!Number::ZERO.is_multiple_of(&Number::ZERO));
        // Signs are irrelevant.
        assert!(n("-0.2").is_multiple_of(&n("0.1")));
        assert!(n("0.2").is_multiple_of(&n("-0.1")));
        assert!(n("-6").is_multiple_of(&n("-3")));
        // Cohort independence: divisibility is value-based.
        assert!(n("2.50").is_multiple_of(&n("0.05")));
        assert!(n("2.5").is_multiple_of(&n("0.050")));
        assert!(!n("2.50").is_multiple_of(&n("0.075")));
        // Integers and the unit grid.
        assert!(n("42").is_multiple_of(&n("1")));
        assert!(!n("2.5").is_multiple_of(&n("1")));
        assert!(!n("1").is_multiple_of(&n("2")));
        assert!(n("42").is_multiple_of(&n("42")));
        // Full-window spread: coarse value on the finest grid — and a
        // non-divisor at the same spread — both decide without
        // overflow.
        let big = n(&format!("1{}", "0".repeat(6144)));
        let tiny = n(&format!("0.{}1", "0".repeat(6175)));
        assert!(big.is_multiple_of(&tiny));
        let three_tiny = n(&format!("0.{}3", "0".repeat(6175)));
        assert!(!big.is_multiple_of(&three_tiny));
        // 34-digit coefficients through the mulmod path.
        let nines = n(&"9".repeat(34));
        assert!(nines.is_multiple_of(&n("3")));
        assert!(!nines.is_multiple_of(&n("7")));

        // Brute-force oracle: v = a/10^sa, m = b/10^sb over a dense
        // small grid; v multiple of m iff (a * 10^sb) % (b * 10^sa)
        // == 0 in plain i128 (small enough to be exact).
        for a in -60i128..=60 {
            for sa in 0u32..=2 {
                for b in 1i128..=25 {
                    for sb in 0u32..=2 {
                        let v = Number::try_new(a, sa as i16).unwrap_or(Number::ZERO);
                        let m = Number::try_new(b, sb as i16).unwrap();
                        let lhs = a.unsigned_abs() * 10u128.pow(sb);
                        let rhs = b.unsigned_abs() * 10u128.pow(sa);
                        let oracle = lhs % rhs == 0;
                        assert_eq!(
                            v.is_multiple_of(&m),
                            oracle,
                            "{a}e-{sa} multiple of {b}e-{sb}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn try_new_contract() {
        // Raw invariant validation, no clamping: members the parser never
        // produces are still legal.
        assert_eq!(parts(Number::try_new(1, -100).unwrap()), (1, -100));
        assert_eq!(Number::try_new(1, -100).unwrap(), lit(&pow10_str(100)));
        // A wide RAW pair reports the coefficient's own width — never
        // "significant digits", which would be self-contradictory here:
        // 10^34 has one significant digit and its VALUE constructs fine
        // from the normalized pair.
        assert_eq!(
            Number::try_new((COEFF_ABS_MAX as i128) + 1, 0).unwrap_err(),
            NumberError::Range(NumberRangeIssue::CoefficientTooWide { got: 35 })
        );
        assert_eq!(
            parts(Number::try_new(10i128.pow(33), -1).unwrap()),
            (10i128.pow(33), -1)
        );
        // Scale-side raw rejections carry the honest kind — never the
        // value-domain TooLarge/TooSmall (10^6112 IS representable).
        assert_eq!(
            Number::try_new(1, -6112).unwrap_err(),
            NumberError::Range(NumberRangeIssue::ScaleOutOfRange { got: -6112 })
        );
        assert_eq!(
            Number::try_new(1, 6177).unwrap_err(),
            NumberError::Range(NumberRangeIssue::ScaleOutOfRange { got: 6177 })
        );
        // Both violated: the coefficient wins (parse-path precedence).
        assert_eq!(
            Number::try_new(10i128.pow(34), 6177).unwrap_err(),
            NumberError::Range(NumberRangeIssue::CoefficientTooWide { got: 35 })
        );
        // Zero lattice: EVERY negative-scale zero reports the zero rule
        // in one step (most-specific-diagnosis-first — the window check
        // would send authors on a renormalize detour); a positive
        // out-of-window scale on zero stays ScaleOutOfRange (truthful).
        assert_eq!(
            Number::try_new(0, 6177).unwrap_err(),
            NumberError::Range(NumberRangeIssue::ScaleOutOfRange { got: 6177 })
        );
        for z in [-5i16, -6111, -6112] {
            assert_eq!(
                Number::try_new(0, z).unwrap_err(),
                NumberError::Range(NumberRangeIssue::NegativeScaleZero { got: z }),
                "zero at scale {z}"
            );
        }
        // In-window edges — pinned so a one-off narrowing of either
        // bound cannot pass silently.
        assert!(Number::try_new(0, 6176).is_ok());
        assert!(Number::try_new(0, 0).is_ok());
        assert!(Number::try_new(1, -6111).is_ok());
        assert_eq!(parts(Number::try_new(-5, 3).unwrap()), (-5, 3));
        assert_eq!(Number::try_new(-5, 3).unwrap().to_string(), "-0.005");
    }

    #[test]
    fn wide_int_construction() {
        assert_eq!(
            Number::try_from(2 * 10i128.pow(37)).unwrap().to_i128(),
            Some(2 * 10i128.pow(37))
        );
        assert_eq!(
            Number::try_from(u128::MAX).unwrap_err(),
            NumberError::Range(NumberRangeIssue::TooManyDigits {
                got: 39,
                integral: true
            })
        );
        assert_eq!(
            Number::try_from(10u128.pow(38)).unwrap().to_u128(),
            Some(10u128.pow(38))
        );
    }

    // ---- corpus literals -------------------------------------------------

    #[test]
    fn corpus_literals_exact() {
        let cases: [(&str, f64); 6] = [
            ("0.05", 0.05),
            ("0.08", 0.08),
            ("0.1", 0.1),
            ("0.7", 0.7),
            ("0.19", 0.19),
            ("0.20", 0.20),
        ];
        for (s, f) in cases {
            let n = lit(s);
            assert_eq!(n.to_string(), s, "Display round-trip for {s}");
            assert_eq!(n.to_f64(), f, "to_f64 equals the Rust f64 literal for {s}");
        }
    }

    #[test]
    fn extreme_magnitude_float_shortcuts() {
        // The short-circuit must agree with the format-path semantics.
        let tiny = co("1e-6176");
        assert_eq!(tiny.to_f64(), 0.0);
        assert!(!tiny.to_f64().is_sign_negative());
        let neg_tiny = co("-1e-6176");
        assert_eq!(neg_tiny.to_f64(), 0.0);
        assert!(neg_tiny.to_f64().is_sign_negative(), "IEEE: rounds to -0.0");
        assert_eq!(co("1e6144").to_f64(), f64::INFINITY);
        assert_eq!(co("-1e6144").to_f64(), f64::NEG_INFINITY);
        assert_eq!(co("1e6144").to_f32(), f32::INFINITY);
        assert_eq!(co("-1e-6176").to_f32(), 0.0);
        // Near the cutoffs the format path still runs and agrees with a
        // direct std parse (no misfire inside f64's live range).
        for s in ["1e-340", "1e-300", "9e307", "1e39", "-1e-59"] {
            let n = co(s);
            let via_std: f64 = format!("{n}").parse().unwrap();
            assert_eq!(n.to_f64(), via_std, "{s}");
        }
        assert_eq!(co("1e38").to_f32(), 1e38f32);
        assert_eq!(co("1e-45").to_f32(), 1e-45f32);
    }

    /// Zero must take the SAME short-circuit as any other extreme
    /// member — the guard that excluded it made `0e-6176` materialize a
    /// 6 KB plain form per conversion (audit: 494× the nonzero cost),
    /// reopening the amplification the shortcut closes. Correctness is
    /// unchanged: zero's adjusted exponent is −scale ∈ [−6176, 0].
    #[test]
    fn zero_takes_the_extreme_magnitude_shortcut() {
        for s in ["0e-6176", "0e-350", "0e-1", "0"] {
            let z = co(s);
            assert_eq!(z.to_f64(), 0.0, "{s}");
            assert!(!z.to_f64().is_sign_negative(), "{s}: +0.0");
            assert_eq!(z.to_f32(), 0.0, "{s}");
        }
        // A zero with maximum stored scale agrees with the format path.
        let deep = lit(&format!("0.{}", "0".repeat(6176)));
        assert_eq!(deep.to_f64(), 0.0);
        assert_eq!(deep.to_f64(), deep.to_string().parse::<f64>().unwrap());
        assert_eq!(deep.to_f32(), deep.to_string().parse::<f32>().unwrap());
    }

    #[test]
    fn simplified_minimal_cohort_member() {
        assert_eq!(parts(lit("8080.000").simplified()), (8080, 0));
        assert_eq!(parts(lit("2.50").simplified()), (25, 1));
        assert_eq!(parts(lit("2.5").simplified()), (25, 1), "already minimal");
        assert_eq!(parts(lit("0.00").simplified()), (0, 0));
        assert_eq!(parts(lit("8080").simplified()), (8080, 0));
        // Integer forms (scale ≤ 0) are untouched — Display is identical
        // across their cohort anyway.
        let clamped = lit(&pow10_str(34));
        assert_eq!(parts(clamped.simplified()), parts(clamped));
        // Value-preserving by definition.
        for s in ["8080.000", "2.50", "0.00", "-1.500"] {
            assert_eq!(lit(s).simplified(), lit(s), "{s}");
        }
    }

    #[test]
    fn num_macro_const() {
        const RATE: Number = num!(0.20);
        assert_eq!(RATE.to_string(), "0.20");
        assert_eq!(parts(num!(-2.5)), (-25, 1));
        assert_eq!(num!(2.50), num!(2.5));
        assert_eq!(num!(2.50).to_string(), "2.50");
        assert_eq!(num!(8080), lit("8080"));
    }

    // ---- fixture sweep: identity + order consistency ---------------------

    fn fixtures() -> Vec<Number> {
        vec![
            lit("2.50"),
            lit("8080.0"),
            lit("8080"),
            lit(&pow10_str(34)),
            lit(&pow10_str(6112)),
            lit(&pow10_str(6144)),
            lit(&subnormal_str()),
            lit(&format!("0.{}", "0".repeat(6200))),
            lit(&"9".repeat(34)),
            lit(&format!("0.{}", "9".repeat(34))),
            lit("9007199254740993"),
            lit("9007199254740992"),
            lit("16777217.0000000001"),
            lit("-9223372036854775808"),
            lit("-0.00"),
            lit("0.00"),
            lit("0"),
            lit("007"),
            lit("18446744073709551617"),
            lit("18446744073709551615"),
            lit(&format!("2{}", "0".repeat(38))),
            lit(&pow10_str(100)),
            lit("1.5"),
            lit("1.50"),
            lit("1.500"),
            lit("2"),
            lit("2.0"),
            lit("12"),
            lit("12.0"),
            lit("-12"),
            lit("-12.0"),
            lit("0.005"),
            lit("-2.5"),
            lit("-2.4"),
            lit("25.0"),
            lit("0.05"),
            lit("0.08"),
            lit("0.1"),
            lit("0.7"),
            lit("0.19"),
            lit("0.20"),
            co("1e-6"),
            co("1E5"),
            co("+1"),
            co(".5"),
            co("1."),
            co("1e00042"),
            co("1.5e3"),
            co("0e999999999999"),
            co("0e5"),
            co("2e38"),
            co("1e6144"),
            Number::try_from_f64(0.1).unwrap(),
            Number::try_new(1, -100).unwrap(),
        ]
    }

    #[test]
    fn parse_display_parse_identity() {
        // Value-level identity (§2): the reparse is Eq-equal. Member-level
        // identity is impossible for negative-scale members ((15, −2)
        // displays "1500", which stores (1500, 0)).
        for n in fixtures() {
            let s = n.to_string();
            let r = Number::parse_literal(&s).unwrap_or_else(|e| {
                panic!(
                    "re-parse of Display (len {}) of {:?} failed: {e:?}",
                    s.len(),
                    parts(n)
                )
            });
            assert!(
                n == r,
                "value identity: {:?} → len {} → {:?}",
                parts(n),
                s.len(),
                parts(r)
            );
        }
    }

    #[test]
    fn ord_eq_hash_total_consistency() {
        let fx = fixtures();
        for a in &fx {
            for b in &fx {
                let c = a.cmp(b);
                assert_eq!(
                    c == Ordering::Equal,
                    a == b,
                    "Eq/Ord {:?} vs {:?}",
                    parts(*a),
                    parts(*b)
                );
                assert_eq!(c.reverse(), b.cmp(a), "antisymmetry");
                if *a == *b {
                    assert_eq!(hsh(a), hsh(b), "Eq⇒Hash {:?} vs {:?}", parts(*a), parts(*b));
                }
                if c != Ordering::Equal {
                    assert_eq!(a.total_cmp(b), c, "total_cmp refines cmp");
                }
                assert_eq!(a.total_cmp(b).reverse(), b.total_cmp(a));
                // One-way f64 oracle: correctly-rounded conversion is
                // monotone, so a_f64 < b_f64 ⇒ a < b.
                if a.to_f64() < b.to_f64() {
                    assert_eq!(
                        c,
                        Ordering::Less,
                        "f64 oracle {:?} vs {:?}",
                        parts(*a),
                        parts(*b)
                    );
                }
            }
        }
        let mut sorted = fx.clone();
        sorted.sort();
        for i in 0..sorted.len() {
            for j in i..sorted.len() {
                assert_ne!(
                    sorted[i].cmp(&sorted[j]),
                    Ordering::Greater,
                    "sort order at {i},{j}"
                );
            }
        }
    }

    // ---- seeded property harness (RFC 0016 §2) ---------------------------
    //
    // House pattern (cst::tests::fuzz_*): deterministic xorshift64, plain
    // #[test], zero dev-deps. The equality/ordering oracle is DIGIT-STRING
    // comparison — an independent implementation path with no width
    // limits (an i128 cross-multiply oracle overflows at scale gaps > 4
    // and would degenerate into testing the implementation with itself).

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Textual numeric comparison over Display output (plain form only):
    /// zeros first, then sign, then integer parts by (length, lex), then
    /// fractions right-padded lexicographically.
    fn oracle_cmp(a: &Number, b: &Number) -> Ordering {
        fn split(s: &str) -> (bool, String, String) {
            let (neg, rest) = match s.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, s),
            };
            let (i, f) = rest.split_once('.').unwrap_or((rest, ""));
            (neg, i.trim_start_matches('0').to_string(), f.to_string())
        }
        let (sa, sb) = (a.to_string(), b.to_string());
        let (an, ai, af) = split(&sa);
        let (bn, bi, bf) = split(&sb);
        let a_zero = ai.is_empty() && af.bytes().all(|c| c == b'0');
        let b_zero = bi.is_empty() && bf.bytes().all(|c| c == b'0');
        match (a_zero, b_zero) {
            (true, true) => return Ordering::Equal,
            (true, false) => {
                return if bn {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (false, true) => {
                return if an {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (false, false) => {}
        }
        if an != bn {
            return if an {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let width = af.len().max(bf.len());
        let mag = ai
            .len()
            .cmp(&bi.len())
            .then_with(|| ai.cmp(&bi))
            .then_with(|| format!("{af:0<width$}").cmp(&format!("{bf:0<width$}")));
        if an { mag.reverse() } else { mag }
    }

    #[test]
    fn property_harness_generated_cohorts() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut pool: Vec<Number> = fixtures();
        for round in 0..2000usize {
            // Generate a random literal: sign, 1–40 digits, optional dot.
            let r = xorshift(&mut state);
            let digits = 1 + (r % 40) as usize;
            let mut s = String::new();
            if r & 0x100 != 0 {
                s.push('-');
            }
            let mut body = String::with_capacity(digits + 1);
            for k in 0..digits {
                let d = (xorshift(&mut state) % 10) as u8;
                body.push((b'0' + d) as char);
                let _ = k;
            }
            let dot = (xorshift(&mut state) % (digits as u64 + 1)) as usize;
            if dot > 0 && dot < digits && xorshift(&mut state) & 1 == 0 {
                body.insert(dot, '.');
            }
            s.push_str(&body);

            let Ok(n) = Number::parse_literal(&s) else {
                // Rejections must be range-kind (the grammar is valid by
                // construction) and deterministic.
                assert_eq!(
                    Number::parse_literal(&s).unwrap_err(),
                    Number::parse_literal(&s).unwrap_err(),
                    "round {round}: nondeterministic rejection of {s:?}"
                );
                continue;
            };
            // Round-trip: Display output reparses Eq-equal.
            let redisplayed = n.to_string();
            let r2 = Number::parse_literal(&redisplayed)
                .unwrap_or_else(|e| panic!("round {round}: reparse of {redisplayed:?}: {e:?}"));
            assert_eq!(n, r2, "round {round}: value round-trip of {s:?}");

            // Handshake compact-encoding round-trip (§1.7 tier 3): the
            // member — coefficient AND scale — survives exactly, for
            // every generated member. (Analytic argument: the decoder's
            // written scale is exactly the original scale, which lies in
            // the value's window by the member's own invariants, so the
            // clamp is the identity.)
            let encoded = format!("{}e{}", n.coeff(), -(n.scale() as i32));
            let back = Number::parse_coercion(&encoded)
                .unwrap_or_else(|e| panic!("round {round}: encoding {encoded:?}: {e:?}"));
            assert_eq!(
                (back.coeff(), back.scale()),
                (n.coeff(), n.scale()),
                "round {round}: member-exact handshake round-trip of {s:?}"
            );

            // Conversion round-trip theorems (RFC §2): every exact
            // extraction reconstructs the same VALUE, and the binary
            // edge is shortest-round-trip stable.
            if let Some(i) = n.to_i64() {
                assert_eq!(
                    Number::from(i),
                    n,
                    "round {round}: to_i64 round-trip of {s:?}"
                );
            }
            if let Some(u) = n.to_u128() {
                assert_eq!(
                    Number::try_from(u).unwrap(),
                    n,
                    "round {round}: to_u128 round-trip of {s:?}"
                );
            }
            let f = n.to_f64();
            if f.is_finite() {
                let back = Number::try_from_f64(f)
                    .unwrap_or_else(|e| panic!("round {round}: try_from_f64({f}): {e:?}"));
                assert_eq!(
                    back.to_f64(),
                    f,
                    "round {round}: shortest-round-trip stability of {s:?}"
                );
            }

            // Pairwise properties against a rolling pool.
            for m in pool.iter().take(24) {
                let c = n.cmp(m);
                assert_eq!(
                    c,
                    oracle_cmp(&n, m),
                    "round {round}: cmp disagrees with the digit-string oracle: {s:?} vs {m}"
                );
                assert_eq!(c.reverse(), m.cmp(&n), "round {round}: antisymmetry");
                assert_eq!(c == Ordering::Equal, n == *m, "round {round}: Eq/Ord");
                if n == *m {
                    assert_eq!(hsh(&n), hsh(m), "round {round}: Eq => Hash");
                }
                if c != Ordering::Equal {
                    assert_eq!(n.total_cmp(m), c, "round {round}: total_cmp refines cmp");
                }
            }
            pool.push(n);
            if pool.len() > 96 {
                pool.remove(0);
            }
        }
    }

    #[test]
    fn property_harness_coercion_grammar() {
        // Coercion inputs with exponents: value equality must match the
        // equivalent literal spelling, end to end.
        let mut state = 0xD1B5_4A32_D192_ED03u64;
        for round in 0..500usize {
            let r = xorshift(&mut state);
            let mantissa = 1 + (r % 20);
            let exp = (xorshift(&mut state) % 60) as i64 - 30;
            let s = format!("{mantissa}e{exp}");
            let Ok(n) = Number::parse_coercion(&s) else {
                panic!("round {round}: {s:?} must coerce (mantissa/exponent in range)");
            };
            // Independent spelling of the same value.
            let spelled = if exp >= 0 {
                format!("{mantissa}{}", "0".repeat(exp as usize))
            } else {
                let m = mantissa.to_string();
                let frac = (-exp) as usize;
                if m.len() > frac {
                    format!("{}.{}", &m[..m.len() - frac], &m[m.len() - frac..])
                } else {
                    format!("0.{}{}", "0".repeat(frac - m.len()), m)
                }
            };
            let lit_n = Number::parse_literal(&spelled)
                .unwrap_or_else(|e| panic!("round {round}: {spelled:?}: {e:?}"));
            assert_eq!(n, lit_n, "round {round}: {s:?} vs {spelled:?}");
            assert_eq!(hsh(&n), hsh(&lit_n), "round {round}: hash across spellings");
        }
    }

    // ---- §1.5 pinned messages --------------------------------------------

    #[test]
    fn error_messages_pinned() {
        assert_eq!(
            NumberRangeIssue::TooLarge.to_string(),
            "number exceeds 9999999999999999999999999999999999 x 10^6111, \
             the largest exact NML number"
        );
        assert_eq!(
            NumberRangeIssue::TooSmall.to_string(),
            "number is closer to zero than 10^-6176, the smallest exact NML number"
        );
        let m = NumberRangeIssue::TooManyDigits {
            got: 38,
            integral: false,
        }
        .to_string();
        assert!(
            m.starts_with("number has 38 significant digits; NML numbers hold at most 34"),
            "{m}"
        );
        assert!(
            !m.contains("account number"),
            "non-integral omits the string hint"
        );
        let mi = NumberRangeIssue::TooManyDigits {
            got: 38,
            integral: true,
        }
        .to_string();
        assert!(mi.contains("quote it as a string"), "{mi}");
        assert_eq!(
            NumberRangeIssue::CoefficientTooWide { got: 35 }.to_string(),
            "coefficient has 35 digits; NML coefficients hold at most 34 \
             (fold trailing zeros into the scale -- the value itself may be \
             representable)"
        );
        assert_eq!(
            NumberRangeIssue::ScaleOutOfRange { got: -6112 }.to_string(),
            "scale -6112 is outside NML's scale range [-6111, 6176] \
             (renormalize the pair -- the value itself may be representable)"
        );
        assert_eq!(
            NumberRangeIssue::NegativeScaleZero { got: -5 }.to_string(),
            "zero cannot carry negative scale -5 (it would render as \
             leading-zero digits); zero stores at any scale in [0, 6176] \
             -- scale 0 is canonical"
        );
        assert_eq!(
            NumberError::TrailingDot.to_string(),
            "number ends with a decimal point; remove the \".\" or add fraction digits"
        );
        assert_eq!(NumberError::Malformed.to_string(), "invalid number");
    }
}
